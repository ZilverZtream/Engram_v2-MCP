use engram_core::RelPath;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// Namespaced by project_id so multiple projects can coexist.
// Key format examples:
//   nodes:   "{project}\0{node_id}"
//   edges:   "{project}\0{edge_kind}\0{source}\0{target}"
//   adj_out: "{project}\0{edge_kind}\0{source}"  → JSON [{target, weight, updated_at_ms}]
//   adj_in:  "{project}\0{edge_kind}\0{target}"  → JSON [{source, weight, updated_at_ms}]
//   meta:    "{project}\0{key}"
static NODES: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes");
static EDGES: TableDefinition<&str, &[u8]> = TableDefinition::new("edges");
static ADJ_OUT: TableDefinition<&str, &[u8]> = TableDefinition::new("adj_out");
static ADJ_IN: TableDefinition<&str, &[u8]> = TableDefinition::new("adj_in");
static META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
static CENTRALITY: TableDefinition<&str, &[u8]> = TableDefinition::new("centrality");

/// Adjacency list entry (compact).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdjEntry {
    id: String, // target_id for adj_out, source_id for adj_in
    weight: u32,
    updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Search result co-occurrence (what v1 stored in search_sessions).
    CoOccurrence,
    /// Git temporal coupling (files that frequently change together).
    TemporalCoupling,
    /// Insight links (insight <-> sources).
    Insight,
    /// Static dependency edges (imports/calls/etc).
    Dependency,
    /// Anti-pattern links (e.g., reverted patches).
    AntiPattern,
    /// Structural containment (e.g., class contains method).
    Contains,
    /// Code imports/includes.
    Imports,
    /// SQL database calls (stored procs, inline SQL).
    SqlCalls,
}

impl EdgeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::CoOccurrence => "co_occurrence",
            EdgeKind::TemporalCoupling => "temporal_coupling",
            EdgeKind::Insight => "insight",
            EdgeKind::Dependency => "dependency",
            EdgeKind::AntiPattern => "anti_pattern",
            EdgeKind::Contains => "contains",
            EdgeKind::Imports => "imports",
            EdgeKind::SqlCalls => "sql_calls",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub node_id: String,
    pub node_type: String,
    pub name: String,
    pub namespace: String,
    pub language: String,
    pub file_path: RelPath,
    pub start_line: u32,
    pub end_line: u32,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub source_id: String,
    pub target_id: String,
    pub namespace: String,
    pub language: String,
    pub edge_kind: EdgeKind,
    pub weight: u32,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonValue>,
    pub updated_at_ms: u64,
}

#[derive(Clone)]
pub struct GraphStore {
    db: Arc<Database>,
}

type TargetMap = HashMap<String, Vec<(String, RelPath, String, Option<serde_json::Value>)>>;

impl GraphStore {
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(db_path)?;

        // Ensure tables exist by opening a write txn once.
        let wtx = db.begin_write()?;
        {
            let _ = wtx.open_table(NODES)?;
            let _ = wtx.open_table(EDGES)?;
            let _ = wtx.open_table(ADJ_OUT)?;
            let _ = wtx.open_table(ADJ_IN)?;
            let _ = wtx.open_table(META)?;
            let _ = wtx.open_table(CENTRALITY)?;
        }
        wtx.commit()?;

        Ok(Self { db: Arc::new(db) })
    }

    pub fn upsert_nodes(&self, project_id: &str, nodes: &[Node]) -> anyhow::Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        let wtx = self.db.begin_write()?;
        {
            let mut nt = wtx.open_table(NODES)?;
            for n in nodes {
                let key = format!("{project_id}\0{}", n.node_id);
                let val = serde_json::to_vec(n)?;
                nt.insert(key.as_str(), val.as_slice())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn upsert_edges(&self, project_id: &str, edges: &[Edge]) -> anyhow::Result<()> {
        if edges.is_empty() {
            return Ok(());
        }
        let wtx = self.db.begin_write()?;
        {
            let mut et = wtx.open_table(EDGES)?;
            let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
            let mut adj_in_t = wtx.open_table(ADJ_IN)?;

            for e in edges {
                let ekey = edge_key(project_id, &e.edge_kind, &e.source_id, &e.target_id);
                let val = serde_json::to_vec(e)?;
                et.insert(ekey.as_str(), val.as_slice())?;

                // Maintain OUT adjacency
                let out_key = adj_key(project_id, &e.edge_kind, &e.source_id);
                let mut out_list = read_adj_list(&adj_out_t, &out_key)?;
                upsert_adj_entry(&mut out_list, &e.target_id, e.weight, e.updated_at_ms);
                adj_out_t.insert(out_key.as_str(), serde_json::to_vec(&out_list)?.as_slice())?;

                // Maintain IN adjacency
                let in_key = adj_key(project_id, &e.edge_kind, &e.target_id);
                let mut in_list = read_adj_list(&adj_in_t, &in_key)?;
                upsert_adj_entry(&mut in_list, &e.source_id, e.weight, e.updated_at_ms);
                adj_in_t.insert(in_key.as_str(), serde_json::to_vec(&in_list)?.as_slice())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn set_meta(&self, project_id: &str, key: &str, value: &str) -> anyhow::Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut mt = wtx.open_table(META)?;
            let k = format!("{project_id}\0{key}");
            mt.insert(k.as_str(), value.as_bytes())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_meta(&self, project_id: &str, key: &str) -> anyhow::Result<Option<String>> {
        let rtx = self.db.begin_read()?;
        let mt = rtx.open_table(META)?;
        let k = format!("{project_id}\0{key}");
        let Some(v) = mt.get(k.as_str())? else {
            return Ok(None);
        };
        Ok(Some(String::from_utf8_lossy(v.value()).to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    /// Increment a directed edge weight.
    pub fn increment_edge(
        &self,
        project_id: &str,
        namespace: &str,
        language: &str,
        kind: EdgeKind,
        source_id: &str,
        target_id: &str,
        delta: u32,
        generation: u64,
    ) -> anyhow::Result<u32> {
        let key = edge_key(project_id, &kind, source_id, target_id);
        let now = now_ms();

        let wtx = self.db.begin_write()?;
        let new_weight;
        {
            let mut et = wtx.open_table(EDGES)?;
            let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
            let mut adj_in_t = wtx.open_table(ADJ_IN)?;

            let maybe_edge = {
                let existing = et.get(key.as_str())?;
                if let Some(v) = existing {
                    let bytes: &[u8] = v.value();
                    let e: Edge = serde_json::from_slice(bytes)?;
                    Some(e)
                } else {
                    None
                }
            };

            let final_edge = if let Some(mut e) = maybe_edge {
                e.weight = e.weight.saturating_add(delta);
                e.updated_at_ms = now;
                e.generation = generation;
                new_weight = e.weight;
                e
            } else {
                let e = Edge {
                    source_id: source_id.to_string(),
                    target_id: target_id.to_string(),
                    namespace: namespace.to_string(),
                    language: language.to_string(),
                    edge_kind: kind.clone(),
                    weight: delta,
                    generation,
                    metadata: None,
                    updated_at_ms: now,
                };
                new_weight = e.weight;
                e
            };

            let bytes = serde_json::to_vec(&final_edge)?;
            et.insert(key.as_str(), bytes.as_slice())?;

            // Update adjacency tables
            let out_key = adj_key(project_id, &kind, source_id);
            let mut out_list = read_adj_list(&adj_out_t, &out_key)?;
            upsert_adj_entry(&mut out_list, target_id, new_weight, now);
            adj_out_t.insert(out_key.as_str(), serde_json::to_vec(&out_list)?.as_slice())?;

            let in_key = adj_key(project_id, &kind, target_id);
            let mut in_list = read_adj_list(&adj_in_t, &in_key)?;
            upsert_adj_entry(&mut in_list, source_id, new_weight, now);
            adj_in_t.insert(in_key.as_str(), serde_json::to_vec(&in_list)?.as_slice())?;
        }
        wtx.commit()?;
        Ok(new_weight)
    }

    #[allow(clippy::too_many_arguments)]
    /// Increment an undirected edge by updating both directions.
    pub fn increment_undirected_edge(
        &self,
        project_id: &str,
        namespace: &str,
        language: &str,
        kind: EdgeKind,
        a: &str,
        b: &str,
        delta: u32,
        generation: u64,
    ) -> anyhow::Result<()> {
        if a == b {
            return Ok(());
        }
        self.increment_edge(
            project_id,
            namespace,
            language,
            kind.clone(),
            a,
            b,
            delta,
            generation,
        )?;
        self.increment_edge(
            project_id, namespace, language, kind, b, a, delta, generation,
        )?;
        Ok(())
    }

    /// Get weighted outgoing neighbors for `source_id`.
    /// O(degree) due to adjacency index.
    pub fn neighbors(
        &self,
        project_id: &str,
        kind: EdgeKind,
        source_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, u32)>> {
        let key = adj_key(project_id, &kind, source_id);
        let rtx = self.db.begin_read()?;
        let adj = rtx.open_table(ADJ_OUT)?;
        let list = read_adj_list_ro(&adj, &key)?;

        let mut out: Vec<(String, u32)> = list.into_iter().map(|e| (e.id, e.weight)).collect();
        out.sort_by(|a, b| b.1.cmp(&a.1));
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    pub fn list_edges_by_kind(
        &self,
        project_id: &str,
        kind: EdgeKind,
        limit: usize,
    ) -> anyhow::Result<Vec<Edge>> {
        let prefix = format!("{project_id}\0{}\0", kind.as_str());
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        let mut out = Vec::new();
        for r in et.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let e: Edge = serde_json::from_slice(v.value())?;
            out.push(e);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Get weighted incoming neighbors for `target_id`.
    /// O(degree) due to adjacency index.
    pub fn find_incoming_edges(
        &self,
        project_id: &str,
        kind: Option<EdgeKind>,
        target_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, u32)>> {
        let results = self.find_incoming_edges_with_kind(project_id, kind, target_id, limit)?;
        Ok(results.into_iter().map(|(id, _, w)| (id, w)).collect())
    }

    /// Get weighted incoming neighbors for `target_id` with their edge kinds.
    pub fn find_incoming_edges_with_kind(
        &self,
        project_id: &str,
        kind: Option<EdgeKind>,
        target_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, EdgeKind, u32)>> {
        let rtx = self.db.begin_read()?;
        let adj = rtx.open_table(ADJ_IN)?;

        let mut out: Vec<(String, EdgeKind, u32)> = Vec::new();

        if let Some(k) = kind {
            // Single kind: O(degree)
            let key = adj_key(project_id, &k, target_id);
            let list = read_adj_list_ro(&adj, &key)?;
            for e in list {
                out.push((e.id, k.clone(), e.weight));
            }
        } else {
            // All edge kinds
            for ek in &[
                EdgeKind::CoOccurrence,
                EdgeKind::TemporalCoupling,
                EdgeKind::Insight,
                EdgeKind::Dependency,
                EdgeKind::AntiPattern,
                EdgeKind::Contains,
                EdgeKind::Imports,
                EdgeKind::SqlCalls,
            ] {
                let key = adj_key(project_id, ek, target_id);
                let list = read_adj_list_ro(&adj, &key)?;
                for e in list {
                    out.push((e.id, ek.clone(), e.weight));
                }
            }
        }

        out.sort_by(|a, b| b.2.cmp(&a.2));
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    pub fn get_node(&self, project_id: &str, node_id: &str) -> anyhow::Result<Option<Node>> {
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let key = format!("{project_id}\0{node_id}");
        let Some(v) = nt.get(key.as_str())? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(v.value())?))
    }

    pub fn list_edges(
        &self,
        project_id: &str,
        kind: Option<EdgeKind>,
    ) -> anyhow::Result<Vec<Edge>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        let mut out = Vec::new();
        for r in et.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let e: Edge = serde_json::from_slice(v.value())?;
            if kind.as_ref().is_some_and(|fk| e.edge_kind != *fk) {
                continue;
            }
            out.push(e);
        }
        Ok(out)
    }

    /// List all node IDs in a project (optionally filtered by node_type).
    pub fn list_node_ids(
        &self,
        project_id: &str,
        node_type: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let mut out = Vec::new();
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            let key = k.value();
            if !key.starts_with(&prefix) {
                break;
            }
            let bytes: &[u8] = v.value();
            let n: Node = serde_json::from_slice(bytes)?;
            if node_type.is_some_and(|t| n.node_type != t) {
                continue;
            }
            out.push(n.node_id);
        }
        Ok(out)
    }

    /// Count total nodes for a project without deserializing them.
    pub fn count_nodes(&self, project_id: &str) -> anyhow::Result<usize> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let mut count = 0;
        for r in nt.range(prefix.as_str()..)? {
            let (k, _) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    pub fn query_nodes(
        &self,
        project_id: &str,
        node_type: Option<&str>,
        name_pattern: Option<&str>,
        file_path: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Node>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let mut out = Vec::new();

        let name_q = name_pattern.map(|s| s.to_lowercase());
        let path_q = file_path.map(|s| s.replace('\\', "/").to_lowercase());

        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = serde_json::from_slice(v.value())?;

            if node_type.is_some_and(|t| !t.is_empty() && n.node_type != t) {
                continue;
            }

            if name_q
                .as_ref()
                .is_some_and(|q| !q.is_empty() && !n.name.to_lowercase().contains(q))
            {
                continue;
            }

            if path_q
                .as_ref()
                .is_some_and(|q| !q.is_empty() && !n.file_path.as_str().to_lowercase().contains(q))
            {
                continue;
            }

            out.push(n);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn count_edges(&self, project_id: &str) -> anyhow::Result<usize> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        let mut count = 0;
        for r in et.range(prefix.as_str()..)? {
            let (k, _) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    pub fn get_centrality(&self, _project_id: &str, _node_id: &str) -> anyhow::Result<f32> {
        // TODO: materialize graph in memory and compute centrality (PageRank, etc.)
        Ok(0.0)
    }

    pub fn get_cached_centrality(
        &self,
        project_id: &str,
        generation: u64,
    ) -> anyhow::Result<Option<HashMap<String, f32>>> {
        let rtx = self.db.begin_read()?;
        let ct = rtx.open_table(CENTRALITY)?;
        let key = format!("{project_id}\0{generation}");
        let Some(v) = ct.get(key.as_str())? else {
            return Ok(None);
        };
        let map: HashMap<String, f32> = serde_json::from_slice(v.value())?;
        Ok(Some(map))
    }

    pub fn set_cached_centrality(
        &self,
        project_id: &str,
        generation: u64,
        metrics: &HashMap<String, f32>,
    ) -> anyhow::Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut ct = wtx.open_table(CENTRALITY)?;
            let key = format!("{project_id}\0{generation}");
            let val = serde_json::to_vec(metrics)?;
            ct.insert(key.as_str(), val.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn delete_project_data(&self, project_id: &str) -> anyhow::Result<()> {
        let prefix = format!("{project_id}\0");
        let wtx = self.db.begin_write()?;
        {
            // Clean up adjacency tables
            for tdef in [&ADJ_OUT, &ADJ_IN] {
                let mut t = wtx.open_table(*tdef)?;
                let mut keys = Vec::new();
                for r in t.range(prefix.as_str()..)? {
                    let (k, _) = r?;
                    if !k.value().starts_with(&prefix) {
                        break;
                    }
                    keys.push(k.value().to_string());
                }
                for k in keys {
                    t.remove(k.as_str())?;
                }
            }
        }
        {
            let mut nt = wtx.open_table(NODES)?;
            let mut keys = Vec::new();
            for r in nt.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                nt.remove(k.as_str())?;
            }
        }
        {
            let mut et = wtx.open_table(EDGES)?;
            let mut keys = Vec::new();
            for r in et.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                et.remove(k.as_str())?;
            }
        }
        {
            let mut mt = wtx.open_table(META)?;
            let mut keys = Vec::new();
            for r in mt.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                mt.remove(k.as_str())?;
            }
        }
        {
            let mut ct = wtx.open_table(CENTRALITY)?;
            let mut keys = Vec::new();
            for r in ct.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                ct.remove(k.as_str())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    /// Create an insight node plus edges linking it to sources.
    #[allow(clippy::too_many_arguments)]
    pub fn create_insight(
        &self,
        project_id: &str,
        insight_id: &str,
        title: &str,
        summary: &str,
        source_node_ids: &[String],
        evidence: Option<Vec<String>>,
        cluster_fingerprint: Option<String>,
        generation: u64,
    ) -> anyhow::Result<()> {
        let node = Node {
            node_id: insight_id.to_string(),
            node_type: "insight".into(),
            name: title.to_string(),
            namespace: engram_core::namespaces::NAMESPACE_INSIGHTS.into(),
            language: "text".into(),
            file_path: "".into(),
            start_line: 0,
            end_line: 0,
            generation,
            metadata: Some(serde_json::json!({
                "summary": summary,
                "source_nodes": source_node_ids,
                "evidence": evidence,
                "cluster_fingerprint": cluster_fingerprint,
                "created_at_ms": now_ms(),
            })),
        };
        self.upsert_nodes(project_id, &[node])?;

        let now = now_ms();
        let mut edges = Vec::with_capacity(source_node_ids.len() * 2);
        for sid in source_node_ids {
            edges.push(Edge {
                source_id: insight_id.to_string(),
                target_id: sid.clone(),
                namespace: engram_core::namespaces::NAMESPACE_INSIGHTS.into(),
                language: "text".into(),
                edge_kind: EdgeKind::Insight,
                weight: 1,
                generation,
                metadata: None,
                updated_at_ms: now,
            });
            edges.push(Edge {
                source_id: sid.clone(),
                target_id: insight_id.to_string(),
                namespace: engram_core::namespaces::NAMESPACE_INSIGHTS.into(),
                language: "text".into(),
                edge_kind: EdgeKind::Insight,
                weight: 1,
                generation,
                metadata: None,
                updated_at_ms: now,
            });
        }
        self.upsert_edges(project_id, &edges)?;
        Ok(())
    }

    pub fn cluster_has_insight(
        &self,
        project_id: &str,
        cluster_node_ids: &[String],
        limit_per_node: usize,
    ) -> anyhow::Result<bool> {
        for nid in cluster_node_ids {
            let neigh = self.neighbors(project_id, EdgeKind::Insight, nid, limit_per_node)?;
            if !neigh.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn fingerprint_has_insight(
        &self,
        project_id: &str,
        fingerprint: &str,
    ) -> anyhow::Result<bool> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = serde_json::from_slice(v.value())?;
            if n.node_type == "insight"
                && let Some(meta) = n.metadata
                && let Some(fp) = meta.get("cluster_fingerprint").and_then(|v| v.as_str())
                && fp == fingerprint
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn purge_old_generations(
        &self,
        project_id: &str,
        active_generation: u64,
    ) -> anyhow::Result<()> {
        let prefix = format!("{project_id}\0");
        let wtx = self.db.begin_write()?;
        {
            let mut nt = wtx.open_table(NODES)?;
            let mut keys_to_remove = Vec::new();
            for r in nt.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let n: Node = serde_json::from_slice(v.value())?;
                if let Ok(policy) = engram_core::get_policy(&n.namespace) {
                    match policy.retention {
                        engram_core::NamespaceRetention::KeepLatestOnly => {
                            if n.generation != active_generation {
                                keys_to_remove.push(k.value().to_string());
                            }
                        }
                        engram_core::NamespaceRetention::KeepLastGenerations(n_keep) => {
                            let min_keep = active_generation.saturating_sub(n_keep as u64 - 1);
                            if n.generation < min_keep {
                                keys_to_remove.push(k.value().to_string());
                            }
                        }
                        engram_core::NamespaceRetention::KeepForever => {}
                    }
                }
            }
            for k in keys_to_remove {
                nt.remove(k.as_str())?;
            }
        }
        {
            let mut et = wtx.open_table(EDGES)?;
            let mut keys_to_remove = Vec::new();
            for r in et.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let e: Edge = serde_json::from_slice(v.value())?;
                if let Ok(policy) = engram_core::get_policy(&e.namespace) {
                    match policy.retention {
                        engram_core::NamespaceRetention::KeepLatestOnly => {
                            if e.generation != active_generation {
                                keys_to_remove.push(k.value().to_string());
                            }
                        }
                        engram_core::NamespaceRetention::KeepLastGenerations(n_keep) => {
                            let min_keep = active_generation.saturating_sub(n_keep as u64 - 1);
                            if e.generation < min_keep {
                                keys_to_remove.push(k.value().to_string());
                            }
                        }
                        engram_core::NamespaceRetention::KeepForever => {}
                    }
                }
            }
            for k in keys_to_remove {
                et.remove(k.as_str())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    /// Find paths from a start node to any SQL nodes.
    ///
    /// Useful for tracing "Click -> Handler -> SQL".
    pub fn find_ui_paths(
        &self,
        project_id: &str,
        start_node_id: &str,
        max_hops: usize,
        max_paths: usize,
    ) -> anyhow::Result<Vec<Vec<Node>>> {
        use std::collections::{HashSet, VecDeque};

        let mut queue = VecDeque::new();
        let mut results = Vec::new();
        let mut visited = HashSet::new();

        // Each queue entry is (current_node_id, path_so_far)
        queue.push_back((start_node_id.to_string(), Vec::new()));

        while let Some((curr_id, mut path)) = queue.pop_front() {
            let Some(node) = self.get_node(project_id, &curr_id)? else {
                continue;
            };

            // Avoid cycles within a single path
            if path.iter().any(|n: &Node| n.node_id == curr_id) {
                continue;
            }

            path.push(node.clone());

            // If we hit a SQL node, we found a target path
            if node.node_id.starts_with("sql:")
                || node.node_type == "inline_sql"
                || node.node_type == "stored_proc"
            {
                results.push(path.clone());
                if results.len() >= max_paths {
                    break;
                }
                // Don't continue BFS from a SQL node (it's a leaf for our purposes)
                continue;
            }

            if path.len() > max_hops {
                continue;
            }

            // Global visited to avoid redundant work in BFS
            if !visited.insert(curr_id.clone()) && curr_id != start_node_id {
                continue;
            }

            let mut neighbors = Vec::new();
            // Trace through event_wiring (Dependency), calls (Dependency), containment (Contains), and SQL calls
            let kinds = vec![EdgeKind::Dependency, EdgeKind::Contains, EdgeKind::SqlCalls];
            for k in kinds {
                let out = self.neighbors(project_id, k, &curr_id, 100)?;
                neighbors.extend(out.into_iter().map(|(id, _)| id));
            }

            for mut next_id in neighbors {
                if next_id.starts_with("::") {
                    let name = &next_id[2..];
                    let short_name = name.split('.').next_back().unwrap_or(name);
                    if let Ok(candidates) =
                        self.query_nodes(project_id, None, Some(short_name), None, 5)
                        && !candidates.is_empty()
                    {
                        if name.contains('.') {
                            if let Some(best) = candidates.iter().find(|n| {
                                n.metadata
                                    .as_ref()
                                    .and_then(|m| m.get("fqn"))
                                    .and_then(|v| v.as_str())
                                    == Some(name)
                            }) {
                                next_id = best.node_id.clone();
                            } else {
                                next_id = candidates[0].node_id.clone();
                            }
                        } else {
                            next_id = candidates[0].node_id.clone();
                        }
                    }
                }

                // Only enqueue if we haven't visited this node OR it's a SQL target
                // (SQL targets are always enqueued so we capture the path)
                if !visited.contains(&next_id) || next_id.starts_with("sql:") {
                    queue.push_back((next_id, path.clone()));
                }
            }
        }

        Ok(results)
    }

    /// Multi-hop BFS traversal from a start node.
    pub fn traverse(
        &self,
        project_id: &str,
        start_node_id: &str,
        max_hops: usize,
        edge_kinds: Option<Vec<EdgeKind>>,
        direction: &str, // "in", "out", "both"
    ) -> anyhow::Result<Vec<(Node, usize)>> {
        use std::collections::{HashSet, VecDeque};

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut results = Vec::new();

        queue.push_back((start_node_id.to_string(), 0));
        visited.insert(start_node_id.to_string());

        while let Some((curr_id, dist)) = queue.pop_front() {
            if let Some(node) = self.get_node(project_id, &curr_id)? {
                results.push((node, dist));
            }

            if dist >= max_hops {
                continue;
            }

            let mut neighbors = Vec::new();

            // Incoming
            if direction == "in" || direction == "both" {
                if let Some(ref kinds) = edge_kinds {
                    for k in kinds {
                        let inc =
                            self.find_incoming_edges(project_id, Some(k.clone()), &curr_id, 100)?;
                        neighbors.extend(inc.into_iter().map(|(id, _)| id));
                    }
                } else {
                    let inc = self.find_incoming_edges(project_id, None, &curr_id, 100)?;
                    neighbors.extend(inc.into_iter().map(|(id, _)| id));
                }
            }

            // Outgoing
            if direction == "out" || direction == "both" {
                if let Some(ref kinds) = edge_kinds {
                    for k in kinds {
                        let out = self.neighbors(project_id, k.clone(), &curr_id, 100)?;
                        neighbors.extend(out.into_iter().map(|(id, _)| id));
                    }
                } else {
                    // If no kinds specified for outgoing, we might need a generic list_edges scan or default to some.
                    // For BFS traversal, we typically want to know what it is connected to.
                    // Let's iterate all known EdgeKinds if None.
                    let all_kinds = vec![
                        EdgeKind::Dependency,
                        EdgeKind::Contains,
                        EdgeKind::Imports,
                        EdgeKind::Insight,
                        EdgeKind::TemporalCoupling,
                        EdgeKind::CoOccurrence,
                    ];
                    for k in all_kinds {
                        let out = self.neighbors(project_id, k, &curr_id, 100)?;
                        neighbors.extend(out.into_iter().map(|(id, _)| id));
                    }
                }
            }

            for mut next_id in neighbors {
                if next_id.starts_with("::") {
                    // Try to resolve it using the same logic as resolve_symbol_edges (simplified)
                    let name = &next_id[2..];
                    let short_name = name.split('.').next_back().unwrap_or(name);

                    // We'll need name_to_targets here too if we want full robustness,
                    // but for a traversal, maybe we can just query the graph for nodes with this name.
                    // For performance in BFS, let's just use the query_nodes method (which is indexed).
                    if let Ok(candidates) =
                        self.query_nodes(project_id, None, Some(short_name), None, 5)
                        && !candidates.is_empty()
                    {
                        // Prefer one with matching FQN if name is FQN
                        if name.contains('.') {
                            if let Some(best) = candidates.iter().find(|n| {
                                n.metadata
                                    .as_ref()
                                    .and_then(|m| m.get("fqn"))
                                    .and_then(|v| v.as_str())
                                    == Some(name)
                            }) {
                                next_id = best.node_id.clone();
                            } else {
                                next_id = candidates[0].node_id.clone();
                            }
                        } else {
                            next_id = candidates[0].node_id.clone();
                        }
                    }
                }

                if !visited.contains(&next_id) {
                    visited.insert(next_id.clone());
                    queue.push_back((next_id, dist + 1));
                }
            }
        }

        Ok(results)
    }

    /// Post-processing step to link unresolved "::name" edges to real nodes.
    /// Prefers targets in the same file and same language.
    pub fn resolve_symbol_edges(&self, project_id: &str) -> anyhow::Result<usize> {
        let prefix = format!("{project_id}\0");

        // 1. Collect all potential targets (classes/functions)
        // Map: name -> Vec<(node_id, file_path, language, metadata)>
        let mut name_to_targets: TargetMap = HashMap::new();
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = serde_json::from_slice(v.value())?;
            // We favor functions/classes over chunks
            if n.node_type == "function" || n.node_type == "class" {
                // Index by short name
                name_to_targets.entry(n.name.clone()).or_default().push((
                    n.node_id.clone(),
                    n.file_path.clone(),
                    n.language.clone(),
                    n.metadata.clone(),
                ));
                // Index by FQN if present
                if let Some(fqn) = n
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                {
                    name_to_targets.entry(fqn.to_string()).or_default().push((
                        n.node_id.clone(),
                        n.file_path.clone(),
                        n.language.clone(),
                        n.metadata.clone(),
                    ));
                }
            }
        }
        drop(nt);
        drop(rtx);

        // 2. Scan edges for unresolved targets
        let mut resolved_count = 0;
        let wtx = self.db.begin_write()?;
        {
            let mut et = wtx.open_table(EDGES)?;
            let nt = wtx.open_table(NODES)?;
            let mut updates = Vec::new();
            for r in et.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let e: Edge = serde_json::from_slice(v.value())?;

                if e.target_id.starts_with("::") {
                    let name = &e.target_id[2..];

                    // If name is an FQN, the last segment is the short name
                    let short_name = name.split('.').next_back().unwrap_or(name);

                    if let Some(targets) = name_to_targets.get(short_name) {
                        // specialized: check if edge has FQN metadata
                        let edge_fqn = e
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("fqn"))
                            .and_then(|v| v.as_str());

                        // Also check if the 'name' itself looks like an FQN
                        let target_fqn = if name.contains('.') {
                            Some(name)
                        } else {
                            edge_fqn
                        };

                        let mut target_node_id = None;

                        if let Some(fqn) = target_fqn {
                            // Find target that matches this FQN exactly in its own metadata
                            target_node_id = targets
                                .iter()
                                .find(|(_, _, _, meta)| {
                                    meta.as_ref()
                                        .and_then(|m| m.get("fqn"))
                                        .and_then(|v| v.as_str())
                                        == Some(fqn)
                                })
                                .map(|t| t.0.clone());
                        }

                        if target_node_id.is_none() {
                            // Fall back to existing heuristics
                            let source_key = format!("{project_id}\0{}", e.source_id);
                            let source_node: Option<Node> = nt
                                .get(source_key.as_str())?
                                .and_then(|v| serde_json::from_slice(v.value()).ok());

                            target_node_id = if let Some(sn) = source_node {
                                // 1. Same file?
                                if let Some(t) =
                                    targets.iter().find(|(_, p, _, _)| *p == sn.file_path)
                                {
                                    Some(t.0.clone())
                                }
                                // 2. Same language?
                                else if let Some(t) =
                                    targets.iter().find(|(_, _, lang, _)| *lang == sn.language)
                                {
                                    Some(t.0.clone())
                                }
                                // 3. Just first one
                                else {
                                    targets.first().map(|t| t.0.clone())
                                }
                            } else {
                                targets.first().map(|t| t.0.clone())
                            };
                        }

                        if let Some(tid) = target_node_id {
                            let mut new_e = e.clone();
                            new_e.target_id = tid;
                            updates.push((k.value().to_string(), new_e));
                        }
                    }
                }
            }

            let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
            let mut adj_in_t = wtx.open_table(ADJ_IN)?;
            let now = now_ms();

            for (old_key, new_edge) in updates {
                // Remove old edge
                et.remove(old_key.as_str())?;

                // Remove stale adjacency entries for the old (unresolved) target
                // Parse old key: "project\0kind\0source\0old_target"
                let old_parts: Vec<&str> = old_key.splitn(4, '\0').collect();
                if old_parts.len() == 4 {
                    let old_target = old_parts[3];
                    let old_out_key = adj_key(project_id, &new_edge.edge_kind, &new_edge.source_id);
                    let mut out_list = read_adj_list(&adj_out_t, &old_out_key)?;
                    out_list.retain(|e| e.id != old_target);
                    upsert_adj_entry(&mut out_list, &new_edge.target_id, new_edge.weight, now);
                    adj_out_t.insert(
                        old_out_key.as_str(),
                        serde_json::to_vec(&out_list)?.as_slice(),
                    )?;

                    let old_in_key = adj_key(project_id, &new_edge.edge_kind, old_target);
                    let mut in_list = read_adj_list(&adj_in_t, &old_in_key)?;
                    in_list.retain(|e| e.id != new_edge.source_id);
                    if !in_list.is_empty() {
                        adj_in_t.insert(
                            old_in_key.as_str(),
                            serde_json::to_vec(&in_list)?.as_slice(),
                        )?;
                    }

                    let new_in_key = adj_key(project_id, &new_edge.edge_kind, &new_edge.target_id);
                    let mut new_in_list = read_adj_list(&adj_in_t, &new_in_key)?;
                    upsert_adj_entry(&mut new_in_list, &new_edge.source_id, new_edge.weight, now);
                    adj_in_t.insert(
                        new_in_key.as_str(),
                        serde_json::to_vec(&new_in_list)?.as_slice(),
                    )?;
                }

                // Insert new edge
                let new_key = edge_key(
                    project_id,
                    &new_edge.edge_kind,
                    &new_edge.source_id,
                    &new_edge.target_id,
                );
                let val = serde_json::to_vec(&new_edge)?;
                et.insert(new_key.as_str(), val.as_slice())?;
                resolved_count += 1;
            }
        }
        wtx.commit()?;

        Ok(resolved_count)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn edge_key(project_id: &str, kind: &EdgeKind, source_id: &str, target_id: &str) -> String {
    format!("{project_id}\0{}\0{source_id}\0{target_id}", kind.as_str())
}

fn adj_key(project_id: &str, kind: &EdgeKind, node_id: &str) -> String {
    format!("{project_id}\0{}\0{node_id}", kind.as_str())
}

fn read_adj_list<T: redb::ReadableTable<&'static str, &'static [u8]>>(
    table: &T,
    key: &str,
) -> anyhow::Result<Vec<AdjEntry>> {
    match table.get(key)? {
        Some(v) => Ok(serde_json::from_slice(v.value()).unwrap_or_default()),
        None => Ok(Vec::new()),
    }
}

fn read_adj_list_ro(
    table: &redb::ReadOnlyTable<&str, &[u8]>,
    key: &str,
) -> anyhow::Result<Vec<AdjEntry>> {
    match table.get(key)? {
        Some(v) => Ok(serde_json::from_slice(v.value()).unwrap_or_default()),
        None => Ok(Vec::new()),
    }
}

fn upsert_adj_entry(list: &mut Vec<AdjEntry>, id: &str, weight: u32, updated_at_ms: u64) {
    if let Some(e) = list.iter_mut().find(|e| e.id == id) {
        e.weight = weight;
        e.updated_at_ms = updated_at_ms;
    } else {
        list.push(AdjEntry {
            id: id.to_string(),
            weight,
            updated_at_ms,
        });
    }
}
