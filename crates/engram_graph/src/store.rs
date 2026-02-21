use crate::analysis::compute_pagerank;
use engram_core::RelPath;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Table definitions ───────────────────────────────────────────────────────
// v2 tables use bincode serialization for Node/Edge and composite keys for
// adjacency. Legacy v1 tables (JSON, single-key adj) are dropped on open().
//
// Key format examples:
//   nodes_v2:   "{project}\0{node_id}"          → bincode(Node)
//   edges_v2:   "{project}\0{kind}\0{src}\0{tgt}" → bincode(Edge)
//   adj_out_v2: ("{project}\0{kind}\0{src}", "{tgt}")  → [weight:u32 LE, ts:u64 LE]
//   adj_in_v2:  ("{project}\0{kind}\0{tgt}", "{src}")  → [weight:u32 LE, ts:u64 LE]
//   meta:       "{project}\0{key}"              → UTF-8 bytes (unchanged)
static NODES: TableDefinition<&str, &[u8]> = TableDefinition::new("nodes_v2");
static EDGES: TableDefinition<&str, &[u8]> = TableDefinition::new("edges_v2");
static ADJ_OUT: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("adj_out_v2");
static ADJ_IN: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("adj_in_v2");
static META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
static CENTRALITY: TableDefinition<&str, &[u8]> = TableDefinition::new("centrality");
static INSIGHT_FINGERPRINTS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("insight_fingerprints");

// ─── Adjacency value helpers (fixed 12-byte binary, no serde) ────────────────

/// Encode adjacency value as 12 bytes: weight (4 LE) + updated_at_ms (8 LE).
fn encode_adj_value(weight: u32, updated_at_ms: u64) -> [u8; 12] {
    let mut buf = [0u8; 12];
    buf[..4].copy_from_slice(&weight.to_le_bytes());
    buf[4..].copy_from_slice(&updated_at_ms.to_le_bytes());
    buf
}

/// Decode adjacency value from 12 bytes.
fn decode_adj_value(bytes: &[u8]) -> (u32, u64) {
    let weight = u32::from_le_bytes(bytes[..4].try_into().unwrap_or([0; 4]));
    let ts = u64::from_le_bytes(bytes[4..12].try_into().unwrap_or([0; 8]));
    (weight, ts)
}

// ─── EdgeKind ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    CoOccurrence,
    TemporalCoupling,
    Insight,
    Dependency,
    AntiPattern,
    Contains,
    Imports,
    SqlCalls,
    HasColumn,
    ForeignKey,
    QueriesTable,
    ReadsState,
    WritesState,
    DataBinding,
    RegistersControl,
    IncludesFile,
    UnresolvedStateRead,
    UnresolvedStateWrite,
    ExposesWebService,
    ExposesHttpHandler,
    ExposesWcfService,
    ContainsUi,
    UiLayoutNeighbor,
    ReadsColumn,
    RegistersModule,
    RegistersHandler,
    ManipulatesDom,
    TriggersPostback,
    ApiCall,
}

impl EdgeKind {
    pub const ALL: &'static [EdgeKind] = &[
        EdgeKind::CoOccurrence,
        EdgeKind::TemporalCoupling,
        EdgeKind::Insight,
        EdgeKind::Dependency,
        EdgeKind::AntiPattern,
        EdgeKind::Contains,
        EdgeKind::Imports,
        EdgeKind::SqlCalls,
        EdgeKind::HasColumn,
        EdgeKind::ForeignKey,
        EdgeKind::QueriesTable,
        EdgeKind::ReadsState,
        EdgeKind::WritesState,
        EdgeKind::DataBinding,
        EdgeKind::RegistersControl,
        EdgeKind::IncludesFile,
        EdgeKind::UnresolvedStateRead,
        EdgeKind::UnresolvedStateWrite,
        EdgeKind::ExposesWebService,
        EdgeKind::ExposesHttpHandler,
        EdgeKind::ExposesWcfService,
        EdgeKind::ContainsUi,
        EdgeKind::UiLayoutNeighbor,
        EdgeKind::ReadsColumn,
        EdgeKind::RegistersModule,
        EdgeKind::RegistersHandler,
        EdgeKind::ManipulatesDom,
        EdgeKind::TriggersPostback,
        EdgeKind::ApiCall,
    ];

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
            EdgeKind::HasColumn => "has_column",
            EdgeKind::ForeignKey => "foreign_key",
            EdgeKind::QueriesTable => "queries_table",
            EdgeKind::ReadsState => "reads_state",
            EdgeKind::WritesState => "writes_state",
            EdgeKind::DataBinding => "data_binding",
            EdgeKind::RegistersControl => "registers_control",
            EdgeKind::IncludesFile => "includes_file",
            EdgeKind::UnresolvedStateRead => "unresolved_state_read",
            EdgeKind::UnresolvedStateWrite => "unresolved_state_write",
            EdgeKind::ExposesWebService => "exposes_web_service",
            EdgeKind::ExposesHttpHandler => "exposes_http_handler",
            EdgeKind::ExposesWcfService => "exposes_wcf_service",
            EdgeKind::ContainsUi => "contains_ui",
            EdgeKind::UiLayoutNeighbor => "ui_layout_neighbor",
            EdgeKind::ReadsColumn => "reads_column",
            EdgeKind::RegistersModule => "registers_module",
            EdgeKind::RegistersHandler => "registers_handler",
            EdgeKind::ManipulatesDom => "manipulates_dom",
            EdgeKind::TriggersPostback => "triggers_postback",
            EdgeKind::ApiCall => "api_call",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "co_occurrence" => Some(EdgeKind::CoOccurrence),
            "temporal_coupling" => Some(EdgeKind::TemporalCoupling),
            "insight" => Some(EdgeKind::Insight),
            "dependency" => Some(EdgeKind::Dependency),
            "anti_pattern" => Some(EdgeKind::AntiPattern),
            "contains" => Some(EdgeKind::Contains),
            "imports" => Some(EdgeKind::Imports),
            "sql_calls" => Some(EdgeKind::SqlCalls),
            "has_column" => Some(EdgeKind::HasColumn),
            "foreign_key" => Some(EdgeKind::ForeignKey),
            "queries_table" => Some(EdgeKind::QueriesTable),
            "reads_state" => Some(EdgeKind::ReadsState),
            "writes_state" => Some(EdgeKind::WritesState),
            "data_binding" => Some(EdgeKind::DataBinding),
            "registers_control" => Some(EdgeKind::RegistersControl),
            "includes_file" => Some(EdgeKind::IncludesFile),
            "unresolved_state_read" => Some(EdgeKind::UnresolvedStateRead),
            "unresolved_state_write" => Some(EdgeKind::UnresolvedStateWrite),
            "exposes_web_service" => Some(EdgeKind::ExposesWebService),
            "exposes_http_handler" => Some(EdgeKind::ExposesHttpHandler),
            "exposes_wcf_service" => Some(EdgeKind::ExposesWcfService),
            "contains_ui" => Some(EdgeKind::ContainsUi),
            "ui_layout_neighbor" => Some(EdgeKind::UiLayoutNeighbor),
            "reads_column" => Some(EdgeKind::ReadsColumn),
            "registers_module" => Some(EdgeKind::RegistersModule),
            "registers_handler" => Some(EdgeKind::RegistersHandler),
            "manipulates_dom" => Some(EdgeKind::ManipulatesDom),
            "triggers_postback" => Some(EdgeKind::TriggersPostback),
            "api_call" => Some(EdgeKind::ApiCall),
            _ => None,
        }
    }
}

impl FromStr for EdgeKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or(())
    }
}

// ─── Node / Edge ─────────────────────────────────────────────────────────────
// NOTE: `skip_serializing_if` is intentionally absent — bincode requires all
// fields to be serialized in a fixed positional layout. The `#[serde(default)]`
// on `metadata` is retained for backward-compatible JSON deserialization in API
// responses (where Node may also be serialized via serde_json).

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
    #[serde(default)]
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
    #[serde(default)]
    pub metadata: Option<JsonValue>,
    pub updated_at_ms: u64,
}

// ─── GraphStore ──────────────────────────────────────────────────────────────

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

        // Drop legacy v1 tables (JSON-serialized, single-key adjacency).
        // Data loss is acceptable — the graph is a derived index rebuilt on
        // the next project update/indexing run.
        {
            let wtx = db.begin_write()?;
            let mut dropped = Vec::new();
            for name in ["nodes", "edges", "adj_out", "adj_in"] {
                match wtx.delete_table(TableDefinition::<&str, &[u8]>::new(name)) {
                    Ok(true) => dropped.push(name),
                    _ => {}
                }
            }
            wtx.commit()?;
            if !dropped.is_empty() {
                tracing::info!(
                    "Migrated graph store to v2 format — dropped legacy tables: {:?}. \
                     Graph data will be rebuilt on next indexing run.",
                    dropped
                );
            }
        }

        // Ensure v2 tables exist.
        let wtx = db.begin_write()?;
        {
            let _ = wtx.open_table(NODES)?;
            let _ = wtx.open_table(EDGES)?;
            let _ = wtx.open_table(ADJ_OUT)?;
            let _ = wtx.open_table(ADJ_IN)?;
            let _ = wtx.open_table(META)?;
            let _ = wtx.open_table(CENTRALITY)?;
            let _ = wtx.open_table(INSIGHT_FINGERPRINTS)?;
        }
        wtx.commit()?;

        Ok(Self { db: Arc::new(db) })
    }

    // ── Upsert ───────────────────────────────────────────────────────────────

    pub fn upsert_nodes(&self, project_id: &str, nodes: &[Node]) -> anyhow::Result<()> {
        if nodes.is_empty() {
            return Ok(());
        }
        validate_key_component("project_id", project_id)?;
        let wtx = self.db.begin_write()?;
        {
            let mut nt = wtx.open_table(NODES)?;
            for n in nodes {
                validate_key_component("node_id", &n.node_id)?;
                let key = format!("{project_id}\0{}", n.node_id);
                let val = bincode::serialize(n)?;
                nt.insert(key.as_str(), val.as_slice())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn upsert_nodes_and_edges(
        &self,
        project_id: &str,
        nodes: &[Node],
        edges: &[Edge],
    ) -> anyhow::Result<()> {
        if nodes.is_empty() && edges.is_empty() {
            return Ok(());
        }
        validate_key_component("project_id", project_id)?;
        for n in nodes {
            validate_key_component("node_id", &n.node_id)?;
        }
        for e in edges {
            validate_key_component("source_id", &e.source_id)?;
            validate_key_component("target_id", &e.target_id)?;
        }

        let wtx = self.db.begin_write()?;
        {
            if !nodes.is_empty() {
                let mut nt = wtx.open_table(NODES)?;
                for n in nodes {
                    let key = format!("{project_id}\0{}", n.node_id);
                    let val = bincode::serialize(n)?;
                    nt.insert(key.as_str(), val.as_slice())?;
                }
            }

            if !edges.is_empty() {
                let mut et = wtx.open_table(EDGES)?;
                let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
                let mut adj_in_t = wtx.open_table(ADJ_IN)?;

                for e in edges {
                    let ekey = edge_key(project_id, &e.edge_kind, &e.source_id, &e.target_id);
                    let val = bincode::serialize(e)?;
                    et.insert(ekey.as_str(), val.as_slice())?;

                    let adj_val = encode_adj_value(e.weight, e.updated_at_ms);
                    let out_prefix = adj_key(project_id, &e.edge_kind, &e.source_id);
                    adj_out_t.insert(
                        (out_prefix.as_str(), e.target_id.as_str()),
                        adj_val.as_slice(),
                    )?;

                    let in_prefix = adj_key(project_id, &e.edge_kind, &e.target_id);
                    adj_in_t.insert(
                        (in_prefix.as_str(), e.source_id.as_str()),
                        adj_val.as_slice(),
                    )?;
                }
            }
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn upsert_edges(&self, project_id: &str, edges: &[Edge]) -> anyhow::Result<()> {
        if edges.is_empty() {
            return Ok(());
        }
        validate_key_component("project_id", project_id)?;
        for e in edges {
            validate_key_component("source_id", &e.source_id)?;
            validate_key_component("target_id", &e.target_id)?;
        }
        let wtx = self.db.begin_write()?;
        {
            let mut et = wtx.open_table(EDGES)?;
            let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
            let mut adj_in_t = wtx.open_table(ADJ_IN)?;

            for e in edges {
                let ekey = edge_key(project_id, &e.edge_kind, &e.source_id, &e.target_id);
                let val = bincode::serialize(e)?;
                et.insert(ekey.as_str(), val.as_slice())?;

                let adj_val = encode_adj_value(e.weight, e.updated_at_ms);
                let out_prefix = adj_key(project_id, &e.edge_kind, &e.source_id);
                adj_out_t.insert(
                    (out_prefix.as_str(), e.target_id.as_str()),
                    adj_val.as_slice(),
                )?;

                let in_prefix = adj_key(project_id, &e.edge_kind, &e.target_id);
                adj_in_t.insert(
                    (in_prefix.as_str(), e.source_id.as_str()),
                    adj_val.as_slice(),
                )?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    // ── Meta ─────────────────────────────────────────────────────────────────

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

    // ── Edge increment ───────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
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
                    bincode::deserialize::<Edge>(v.value()).ok()
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
                new_weight = delta;
                Edge {
                    source_id: source_id.to_string(),
                    target_id: target_id.to_string(),
                    namespace: namespace.to_string(),
                    language: language.to_string(),
                    edge_kind: kind.clone(),
                    weight: delta,
                    generation,
                    metadata: None,
                    updated_at_ms: now,
                }
            };

            let bytes = bincode::serialize(&final_edge)?;
            et.insert(key.as_str(), bytes.as_slice())?;

            // O(1) point-insert adjacency entries (no list deserialization).
            let adj_val = encode_adj_value(new_weight, now);
            let out_prefix = adj_key(project_id, &kind, source_id);
            adj_out_t.insert((out_prefix.as_str(), target_id), adj_val.as_slice())?;

            let in_prefix = adj_key(project_id, &kind, target_id);
            adj_in_t.insert((in_prefix.as_str(), source_id), adj_val.as_slice())?;
        }
        wtx.commit()?;
        Ok(new_weight)
    }

    #[allow(clippy::too_many_arguments)]
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
        self.batch_increment_undirected_edges(
            project_id,
            namespace,
            language,
            generation,
            &[(kind, a.to_string(), b.to_string(), delta)],
        )
    }

    /// Batch-increment multiple edges in a single write transaction.
    ///
    /// With composite-key adjacency tables each adj update is an O(1) point
    /// insert — no more read-modify-write of JSON arrays, no adjacency caches.
    #[allow(clippy::too_many_arguments)]
    pub fn batch_increment_edges(
        &self,
        project_id: &str,
        namespace: &str,
        language: &str,
        generation: u64,
        increments: &[(EdgeKind, String, String, u32)],
    ) -> anyhow::Result<()> {
        if increments.is_empty() {
            return Ok(());
        }
        validate_key_component("project_id", project_id)?;
        for (_, src, tgt, _) in increments {
            validate_key_component("source_id", src)?;
            validate_key_component("target_id", tgt)?;
        }
        let now = now_ms();
        let wtx = self.db.begin_write()?;
        {
            let mut et = wtx.open_table(EDGES)?;
            let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
            let mut adj_in_t = wtx.open_table(ADJ_IN)?;

            for (kind, source_id, target_id, delta) in increments {
                let key = edge_key(project_id, kind, source_id, target_id);

                let maybe_edge = {
                    let existing = et.get(key.as_str())?;
                    if let Some(v) = existing {
                        bincode::deserialize::<Edge>(v.value()).ok()
                    } else {
                        None
                    }
                };

                let final_edge = if let Some(mut e) = maybe_edge {
                    e.weight = e.weight.saturating_add(*delta);
                    e.updated_at_ms = now;
                    e.generation = generation;
                    e
                } else {
                    Edge {
                        source_id: source_id.clone(),
                        target_id: target_id.clone(),
                        namespace: namespace.to_string(),
                        language: language.to_string(),
                        edge_kind: kind.clone(),
                        weight: *delta,
                        generation,
                        metadata: None,
                        updated_at_ms: now,
                    }
                };

                let new_weight = final_edge.weight;
                let bytes = bincode::serialize(&final_edge)?;
                et.insert(key.as_str(), bytes.as_slice())?;

                let adj_val = encode_adj_value(new_weight, now);
                let out_prefix = adj_key(project_id, kind, source_id);
                adj_out_t.insert(
                    (out_prefix.as_str(), target_id.as_str()),
                    adj_val.as_slice(),
                )?;

                let in_prefix = adj_key(project_id, kind, target_id);
                adj_in_t.insert((in_prefix.as_str(), source_id.as_str()), adj_val.as_slice())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn batch_increment_undirected_edges(
        &self,
        project_id: &str,
        namespace: &str,
        language: &str,
        generation: u64,
        pairs: &[(EdgeKind, String, String, u32)],
    ) -> anyhow::Result<()> {
        let mut directed: Vec<(EdgeKind, String, String, u32)> =
            Vec::with_capacity(pairs.len() * 2);
        for (kind, a, b, delta) in pairs {
            if a == b {
                continue;
            }
            directed.push((kind.clone(), a.clone(), b.clone(), *delta));
            directed.push((kind.clone(), b.clone(), a.clone(), *delta));
        }
        self.batch_increment_edges(project_id, namespace, language, generation, &directed)
    }

    // ── Neighbor queries (composite-key range scan) ──────────────────────────

    /// Get weighted outgoing neighbors for `source_id`.
    /// O(degree) via composite-key range scan — no JSON deserialization.
    pub fn neighbors(
        &self,
        project_id: &str,
        kind: EdgeKind,
        source_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, u32)>> {
        let prefix = adj_key(project_id, &kind, source_id);
        let rtx = self.db.begin_read()?;
        let adj = rtx.open_table(ADJ_OUT)?;

        let mut out: Vec<(String, u32)> = Vec::new();
        for result in adj.range((prefix.as_str(), "")..)? {
            let (key_guard, val_guard) = result?;
            let (pfx, neighbor_id) = key_guard.value();
            if pfx != prefix.as_str() {
                break;
            }
            let (weight, _ts) = decode_adj_value(val_guard.value());
            out.push((neighbor_id.to_string(), weight));
        }

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
            let e: Edge = bincode::deserialize(v.value())?;
            out.push(e);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Get weighted incoming neighbors for `target_id`.
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

        let kinds: &[EdgeKind] = if let Some(ref k) = kind {
            std::slice::from_ref(k)
        } else {
            EdgeKind::ALL
        };

        for ek in kinds {
            let prefix = adj_key(project_id, ek, target_id);
            for result in adj.range((prefix.as_str(), "")..)? {
                let (key_guard, val_guard) = result?;
                let (pfx, source_id) = key_guard.value();
                if pfx != prefix.as_str() {
                    break;
                }
                let (weight, _ts) = decode_adj_value(val_guard.value());
                out.push((source_id.to_string(), ek.clone(), weight));
            }
        }

        out.sort_by(|a, b| b.2.cmp(&a.2));
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    // ── Node / Edge reads ────────────────────────────────────────────────────

    pub fn get_node(&self, project_id: &str, node_id: &str) -> anyhow::Result<Option<Node>> {
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let key = format!("{project_id}\0{node_id}");
        let Some(v) = nt.get(key.as_str())? else {
            return Ok(None);
        };
        Ok(Some(bincode::deserialize(v.value())?))
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
            let e: Edge = bincode::deserialize(v.value())?;
            if kind.as_ref().is_some_and(|fk| e.edge_kind != *fk) {
                continue;
            }
            out.push(e);
        }
        Ok(out)
    }

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
            let n: Node = bincode::deserialize(v.value())?;
            if node_type.is_some_and(|t| n.node_type != t) {
                continue;
            }
            out.push(n.node_id);
        }
        Ok(out)
    }

    pub fn list_file_node_metadata(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<(RelPath, Option<serde_json::Value>)>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let mut out = Vec::new();
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = bincode::deserialize(v.value())?;
            if n.node_type != "file" {
                continue;
            }
            out.push((n.file_path, n.metadata));
        }
        Ok(out)
    }

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

    /// Count nodes grouped by `node_type` (class, function, file, db_table, etc.).
    pub fn count_nodes_by_type(&self, project_id: &str) -> anyhow::Result<HashMap<String, usize>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let mut counts = HashMap::new();
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            if let Ok(node) = bincode::deserialize::<Node>(v.value()) {
                *counts.entry(node.node_type).or_insert(0) += 1;
            }
        }
        Ok(counts)
    }

    /// Count edges grouped by `EdgeKind`.
    pub fn count_edges_by_kind(&self, project_id: &str) -> anyhow::Result<HashMap<String, usize>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        let mut counts = HashMap::new();
        for r in et.range(prefix.as_str()..)? {
            let (k, _) = r?;
            let key = k.value();
            if !key.starts_with(&prefix) {
                break;
            }
            // Edge key: "{project}\0{kind}\0{src}\0{tgt}"
            if let Some(kind_str) = key.strip_prefix(&prefix).and_then(|s| s.split('\0').next()) {
                *counts.entry(kind_str.to_string()).or_insert(0) += 1;
            }
        }
        Ok(counts)
    }

    // ── Query (allocation-free case-insensitive matching) ────────────────────

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

        // Pre-lowercase the queries once; the per-node matching uses
        // `contains_case_insensitive` which avoids allocating on every row.
        let name_q = name_pattern.map(|s| s.to_lowercase());
        let path_q = file_path.map(|s| s.replace('\\', "/").to_lowercase());

        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = bincode::deserialize(v.value())?;

            if node_type.is_some_and(|t| !t.is_empty() && n.node_type != t) {
                continue;
            }

            if name_q
                .as_ref()
                .is_some_and(|q| !q.is_empty() && !contains_case_insensitive(&n.name, q))
            {
                continue;
            }

            if path_q.as_ref().is_some_and(|q| {
                !q.is_empty() && !contains_case_insensitive_path(n.file_path.as_str(), q)
            }) {
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

    // ── Centrality ───────────────────────────────────────────────────────────

    pub fn get_centrality(&self, project_id: &str, node_id: &str) -> anyhow::Result<f32> {
        let generation = self
            .get_meta(project_id, "active_generation")?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if let Some(cached) = self.get_cached_centrality(project_id, generation)?
            && let Some(score) = cached.get(node_id)
        {
            return Ok(*score);
        }

        let computed = compute_pagerank(self, project_id, generation)?;
        Ok(computed.pagerank.get(node_id).copied().unwrap_or(0.0))
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

    // ── Delete project ───────────────────────────────────────────────────────

    pub fn delete_project_data(&self, project_id: &str) -> anyhow::Result<()> {
        let prefix = format!("{project_id}\0");
        const BATCH_SIZE: usize = 2000;

        // Helper: collect one batch of keys from a single-key table.
        fn collect_batch(
            db: &Database,
            tdef: TableDefinition<&str, &[u8]>,
            prefix: &str,
            batch_size: usize,
        ) -> anyhow::Result<Vec<String>> {
            let rtx = db.begin_read()?;
            let t = rtx.open_table(tdef)?;
            let mut batch = Vec::with_capacity(batch_size);
            for r in t.range(prefix..)? {
                let (k, _) = r?;
                if !k.value().starts_with(prefix) {
                    break;
                }
                batch.push(k.value().to_string());
                if batch.len() >= batch_size {
                    break;
                }
            }
            Ok(batch)
        }

        // Helper: collect one batch of composite keys from an adj table.
        fn collect_adj_batch(
            db: &Database,
            tdef: TableDefinition<(&str, &str), &[u8]>,
            prefix: &str,
            batch_size: usize,
        ) -> anyhow::Result<Vec<(String, String)>> {
            let rtx = db.begin_read()?;
            let t = rtx.open_table(tdef)?;
            let mut batch = Vec::with_capacity(batch_size);
            for r in t.range((prefix, "")..)? {
                let (k, _) = r?;
                let (first, second) = k.value();
                if !first.starts_with(prefix) {
                    break;
                }
                batch.push((first.to_string(), second.to_string()));
                if batch.len() >= batch_size {
                    break;
                }
            }
            Ok(batch)
        }

        // Drain single-key tables
        let tables = [NODES, EDGES, META, CENTRALITY, INSIGHT_FINGERPRINTS];
        for tdef in tables {
            loop {
                let batch = collect_batch(&self.db, tdef, &prefix, BATCH_SIZE)?;
                if batch.is_empty() {
                    break;
                }
                let wtx = self.db.begin_write()?;
                {
                    let mut t = wtx.open_table(tdef)?;
                    for k in &batch {
                        t.remove(k.as_str())?;
                    }
                }
                wtx.commit()?;
            }
        }

        // Drain composite-key adjacency tables
        let adj_tables = [ADJ_OUT, ADJ_IN];
        for tdef in adj_tables {
            loop {
                let batch = collect_adj_batch(&self.db, tdef, &prefix, BATCH_SIZE)?;
                if batch.is_empty() {
                    break;
                }
                let wtx = self.db.begin_write()?;
                {
                    let mut t = wtx.open_table(tdef)?;
                    for (k1, k2) in &batch {
                        t.remove((k1.as_str(), k2.as_str()))?;
                    }
                }
                wtx.commit()?;
            }
        }

        Ok(())
    }

    // ── Insights ─────────────────────────────────────────────────────────────

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
        if let Some(fp) = cluster_fingerprint.filter(|fp| !fp.is_empty()) {
            self.set_insight_fingerprint(project_id, &fp)?;
        }
        Ok(())
    }

    fn set_insight_fingerprint(&self, project_id: &str, fingerprint: &str) -> anyhow::Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(INSIGHT_FINGERPRINTS)?;
            let k = format!("{project_id}\0{fingerprint}");
            t.insert(k.as_str(), b"1" as &[u8])?;
        }
        wtx.commit()?;
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
        let key = format!("{project_id}\0{fingerprint}");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(INSIGHT_FINGERPRINTS)?;
        if t.get(key.as_str())?.is_some() {
            return Ok(true);
        }
        drop(t);
        drop(rtx);

        // Backward-compat fallback for pre-index-table data; hydrate on hit.
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = bincode::deserialize(v.value())?;
            if n.node_type == "insight"
                && let Some(meta) = n.metadata
                && let Some(fp) = meta.get("cluster_fingerprint").and_then(|v| v.as_str())
                && fp == fingerprint
            {
                self.set_insight_fingerprint(project_id, fingerprint)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── Purge (composite-key point deletes — no list manipulation) ───────────

    pub fn purge_old_generations(
        &self,
        project_id: &str,
        active_generation: u64,
    ) -> anyhow::Result<()> {
        let prefix = format!("{project_id}\0");
        const BATCH_SIZE: usize = 1000;

        // --- Phase 1: Collect stale node keys (read-only) ---
        let node_keys_to_remove = {
            let rtx = self.db.begin_read()?;
            let nt = rtx.open_table(NODES)?;
            let mut keys = Vec::new();
            for r in nt.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let n: Node = bincode::deserialize(v.value())?;
                if let Ok(policy) = engram_core::get_policy(&n.namespace) {
                    let stale = match policy.retention {
                        engram_core::NamespaceRetention::KeepLatestOnly => {
                            n.generation != active_generation
                        }
                        engram_core::NamespaceRetention::KeepLastGenerations(n_keep) => {
                            let min_keep = active_generation.saturating_sub(n_keep as u64 - 1);
                            n.generation < min_keep
                        }
                        engram_core::NamespaceRetention::KeepForever => false,
                    };
                    if stale {
                        keys.push(k.value().to_string());
                    }
                }
            }
            keys
        };

        for chunk in node_keys_to_remove.chunks(BATCH_SIZE) {
            let wtx = self.db.begin_write()?;
            {
                let mut nt = wtx.open_table(NODES)?;
                for k in chunk {
                    nt.remove(k.as_str())?;
                }
            }
            wtx.commit()?;
        }

        // --- Phase 2: Collect stale edge keys (read-only) ---
        let edge_keys_to_remove = {
            let rtx = self.db.begin_read()?;
            let et = rtx.open_table(EDGES)?;
            let mut keys = Vec::new();
            for r in et.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let e: Edge = bincode::deserialize(v.value())?;
                if let Ok(policy) = engram_core::get_policy(&e.namespace) {
                    let stale = match policy.retention {
                        engram_core::NamespaceRetention::KeepLatestOnly => {
                            e.generation != active_generation
                        }
                        engram_core::NamespaceRetention::KeepLastGenerations(n_keep) => {
                            let min_keep = active_generation.saturating_sub(n_keep as u64 - 1);
                            e.generation < min_keep
                        }
                        engram_core::NamespaceRetention::KeepForever => false,
                    };
                    if stale {
                        keys.push(k.value().to_string());
                    }
                }
            }
            keys
        };

        // Remove stale edges + adjacency entries in batches.
        // With composite-key adjacency, each removal is an O(1) point delete.
        for chunk in edge_keys_to_remove.chunks(BATCH_SIZE) {
            let wtx = self.db.begin_write()?;
            {
                let mut et = wtx.open_table(EDGES)?;
                let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
                let mut adj_in_t = wtx.open_table(ADJ_IN)?;

                for k in chunk {
                    // Parse edge key: "project\0kind\0source\0target"
                    let parts: Vec<&str> = k.splitn(4, '\0').collect();
                    if parts.len() == 4 {
                        let kind_str = parts[1];
                        let source_id = parts[2];
                        let target_id = parts[3];

                        if let Some(ek) = EdgeKind::parse(kind_str) {
                            let out_prefix = adj_key(project_id, &ek, source_id);
                            adj_out_t.remove((out_prefix.as_str(), target_id))?;

                            let in_prefix = adj_key(project_id, &ek, target_id);
                            adj_in_t.remove((in_prefix.as_str(), source_id))?;
                        }
                    }
                    et.remove(k.as_str())?;
                }
            }
            wtx.commit()?;
        }
        Ok(())
    }

    // ── BFS: find_ui_paths (parent-map, no Vec cloning) ──────────────────────

    /// Find paths from a start node to any SQL nodes.
    ///
    /// Uses a parent-map BFS that stores `(node_id, parent_index)` entries
    /// instead of cloning the entire path at every branch point. Paths are
    /// reconstructed only for the winning terminals, eliminating exponential
    /// memory growth on branchy graphs.
    pub fn find_ui_paths(
        &self,
        project_id: &str,
        start_node_id: &str,
        max_hops: usize,
        max_paths: usize,
    ) -> anyhow::Result<Vec<Vec<Node>>> {
        use std::collections::{HashMap, VecDeque};

        let max_bfs_queue_ops: usize = (max_hops * max_paths * 100).clamp(5000, 50_000);
        const MAX_BRANCHING: usize = 30;
        let mut bfs_ops: usize = 0;

        // Each entry is a (node_id, parent_idx) pair. Paths are reconstructed
        // by walking the parent chain, not by carrying full Vec<String> clones.
        let mut entries: Vec<BfsEntryOuter> = Vec::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        let mut id_paths: Vec<Vec<String>> = Vec::new();
        let mut best_depth: HashMap<String, usize> = HashMap::new();

        entries.push(BfsEntryOuter {
            node_id: start_node_id.to_string(),
            parent_idx: None,
        });
        queue.push_back(0);

        while let Some(entry_idx) = queue.pop_front() {
            bfs_ops += 1;
            if bfs_ops > max_bfs_queue_ops {
                break;
            }

            let curr_id = entries[entry_idx].node_id.clone();

            let Some(node) = self.get_node(project_id, &curr_id)? else {
                continue;
            };

            // Per-path cycle detection: check if curr_id appears in ancestors.
            if bfs_path_contains(&entries, entry_idx, &curr_id) {
                continue;
            }

            // If we hit a SQL node, reconstruct and record the path.
            if node.node_id.starts_with("sql:")
                || node.node_type == "inline_sql"
                || node.node_type == "stored_proc"
            {
                id_paths.push(bfs_reconstruct_path(&entries, entry_idx));
                if id_paths.len() >= max_paths {
                    break;
                }
                continue;
            }

            let depth = bfs_depth(&entries, entry_idx);
            if depth > max_hops {
                continue;
            }

            if let Some(&prev_depth) = best_depth.get(&curr_id) {
                if depth > prev_depth && curr_id != start_node_id {
                    continue;
                }
            }
            best_depth.insert(curr_id.clone(), depth);

            let mut neighbors = Vec::new();
            let kinds = [EdgeKind::Dependency, EdgeKind::Contains, EdgeKind::SqlCalls];
            for k in kinds {
                let out = self.neighbors(project_id, k, &curr_id, 100)?;
                neighbors.extend(out.into_iter().map(|(id, _)| id));
            }

            neighbors.sort();
            neighbors.dedup();
            neighbors.truncate(MAX_BRANCHING);

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
                            } else if candidates.len() == 1 {
                                next_id = candidates[0].node_id.clone();
                            }
                        } else if candidates.len() == 1 {
                            next_id = candidates[0].node_id.clone();
                        }
                    }
                }

                // Allow SQL nodes even if in path (they are terminals).
                let in_path = bfs_path_contains(&entries, entry_idx, &next_id)
                    || entries[entry_idx].node_id == next_id;
                if !in_path || next_id.starts_with("sql:") {
                    let new_idx = entries.len();
                    entries.push(BfsEntryOuter {
                        node_id: next_id,
                        parent_idx: Some(entry_idx),
                    });
                    queue.push_back(new_idx);
                }
            }
        }

        // Materialize Node objects only for the winning paths.
        let mut results = Vec::with_capacity(id_paths.len());
        for id_path in id_paths {
            let mut node_path = Vec::with_capacity(id_path.len());
            for nid in &id_path {
                if let Some(n) = self.get_node(project_id, nid)? {
                    node_path.push(n);
                }
            }
            results.push(node_path);
        }

        Ok(results)
    }

    // ── Traverse ─────────────────────────────────────────────────────────────

    pub fn traverse(
        &self,
        project_id: &str,
        start_node_id: &str,
        max_hops: usize,
        edge_kinds: Option<Vec<EdgeKind>>,
        direction: &str,
    ) -> anyhow::Result<Vec<(Node, usize)>> {
        use std::collections::{HashSet, VecDeque};

        const MAX_BFS_RESULTS: usize = 500;
        const MAX_BRANCHING_FACTOR: usize = 50;

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut results = Vec::new();

        queue.push_back((start_node_id.to_string(), 0));
        visited.insert(start_node_id.to_string());

        while let Some((curr_id, dist)) = queue.pop_front() {
            if results.len() >= MAX_BFS_RESULTS {
                break;
            }

            if let Some(node) = self.get_node(project_id, &curr_id)? {
                results.push((node, dist));
            }

            if dist >= max_hops {
                continue;
            }

            let mut neighbors = Vec::new();

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

            if direction == "out" || direction == "both" {
                if let Some(ref kinds) = edge_kinds {
                    for k in kinds {
                        let out = self.neighbors(project_id, k.clone(), &curr_id, 100)?;
                        neighbors.extend(out.into_iter().map(|(id, _)| id));
                    }
                } else {
                    for k in EdgeKind::ALL.iter().cloned() {
                        let out = self.neighbors(project_id, k, &curr_id, 100)?;
                        neighbors.extend(out.into_iter().map(|(id, _)| id));
                    }
                }
            }

            neighbors.sort();
            neighbors.dedup();
            neighbors.truncate(MAX_BRANCHING_FACTOR);

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

                if !visited.contains(&next_id) {
                    visited.insert(next_id.clone());
                    queue.push_back((next_id, dist + 1));
                }
            }
        }

        Ok(results)
    }

    // ── Resolve symbol edges ─────────────────────────────────────────────────

    pub fn resolve_symbol_edges(&self, project_id: &str) -> anyhow::Result<usize> {
        let prefix = format!("{project_id}\0");

        // 1. Collect all potential targets (classes/functions)
        let mut name_to_targets: TargetMap = HashMap::new();
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = bincode::deserialize(v.value())?;
            if n.node_type == "function" || n.node_type == "class" {
                name_to_targets.entry(n.name.clone()).or_default().push((
                    n.node_id.clone(),
                    n.file_path.clone(),
                    n.language.clone(),
                    n.metadata.clone(),
                ));
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
                let e: Edge = bincode::deserialize(v.value())?;

                if e.target_id.starts_with("::") {
                    let name = &e.target_id[2..];
                    let short_name = name.split('.').next_back().unwrap_or(name);

                    if let Some(targets) = name_to_targets.get(short_name) {
                        let edge_fqn = e
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("fqn"))
                            .and_then(|v| v.as_str());

                        let target_fqn = if name.contains('.') {
                            Some(name)
                        } else {
                            edge_fqn
                        };

                        let mut target_node_id = None;

                        if let Some(fqn) = target_fqn {
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
                            let source_key = format!("{project_id}\0{}", e.source_id);
                            let source_node: Option<Node> = nt
                                .get(source_key.as_str())?
                                .and_then(|v| bincode::deserialize(v.value()).ok());

                            target_node_id = if let Some(sn) = source_node {
                                if let Some(t) =
                                    targets.iter().find(|(_, p, _, _)| *p == sn.file_path)
                                {
                                    Some(t.0.clone())
                                } else if let Some(t) =
                                    targets.iter().find(|(_, _, lang, _)| *lang == sn.language)
                                {
                                    Some(t.0.clone())
                                } else if targets.len() == 1 {
                                    Some(targets[0].0.clone())
                                } else {
                                    None
                                }
                            } else if targets.len() == 1 {
                                Some(targets[0].0.clone())
                            } else {
                                None
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
                et.remove(old_key.as_str())?;

                // Parse old key: "project\0kind\0source\0old_target"
                let old_parts: Vec<&str> = old_key.splitn(4, '\0').collect();
                if old_parts.len() == 4 {
                    let old_target = old_parts[3];

                    // Remove old adjacency entries (O(1) point deletes).
                    let out_prefix = adj_key(project_id, &new_edge.edge_kind, &new_edge.source_id);
                    adj_out_t.remove((out_prefix.as_str(), old_target))?;

                    let old_in_prefix = adj_key(project_id, &new_edge.edge_kind, old_target);
                    adj_in_t.remove((old_in_prefix.as_str(), new_edge.source_id.as_str()))?;

                    // Insert new adjacency entries.
                    let adj_val = encode_adj_value(new_edge.weight, now);
                    adj_out_t.insert(
                        (out_prefix.as_str(), new_edge.target_id.as_str()),
                        adj_val.as_slice(),
                    )?;

                    let new_in_prefix =
                        adj_key(project_id, &new_edge.edge_kind, &new_edge.target_id);
                    adj_in_t.insert(
                        (new_in_prefix.as_str(), new_edge.source_id.as_str()),
                        adj_val.as_slice(),
                    )?;
                }

                let new_key = edge_key(
                    project_id,
                    &new_edge.edge_kind,
                    &new_edge.source_id,
                    &new_edge.target_id,
                );
                let val = bincode::serialize(&new_edge)?;
                et.insert(new_key.as_str(), val.as_slice())?;
                resolved_count += 1;
            }
        }
        wtx.commit()?;

        Ok(resolved_count)
    }
}

// ─── Free functions ──────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Validate that a key component does not contain the NUL byte separator.
fn validate_key_component(name: &str, value: &str) -> anyhow::Result<()> {
    if value.contains('\0') {
        anyhow::bail!(
            "Graph store key component '{name}' contains NUL byte — this would \
             corrupt composite keys. Value (truncated): {:?}",
            &value[..value.len().min(80)]
        );
    }
    Ok(())
}

fn edge_key(project_id: &str, kind: &EdgeKind, source_id: &str, target_id: &str) -> String {
    format!("{project_id}\0{}\0{source_id}\0{target_id}", kind.as_str())
}

fn adj_key(project_id: &str, kind: &EdgeKind, node_id: &str) -> String {
    format!("{project_id}\0{}\0{node_id}", kind.as_str())
}

/// Case-insensitive ASCII substring check without heap allocation.
fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_bytes = needle.as_bytes();
    let haystack_bytes = haystack.as_bytes();
    if needle_bytes.len() > haystack_bytes.len() {
        return false;
    }
    haystack_bytes
        .windows(needle_bytes.len())
        .any(|window| window.eq_ignore_ascii_case(needle_bytes))
}

/// Case-insensitive ASCII substring check with backslash → forward-slash
/// normalization, without heap allocation.
fn contains_case_insensitive_path(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_bytes = needle.as_bytes();
    let haystack_bytes = haystack.as_bytes();
    if needle_bytes.len() > haystack_bytes.len() {
        return false;
    }
    haystack_bytes.windows(needle_bytes.len()).any(|window| {
        window.iter().zip(needle_bytes.iter()).all(|(&h, &n)| {
            let h_norm = if h == b'\\' { b'/' } else { h };
            h_norm.eq_ignore_ascii_case(&n)
        })
    })
}

// ─── BFS helpers for find_ui_paths ───────────────────────────────────────────

struct BfsEntryOuter {
    node_id: String,
    parent_idx: Option<usize>,
}

/// Walk up the parent chain to compute depth (distance from root).
fn bfs_depth(entries: &[impl AsBfsEntry], idx: usize) -> usize {
    let mut d = 0;
    let mut current = entries[idx].parent();
    while let Some(pidx) = current {
        d += 1;
        current = entries[pidx].parent();
    }
    d
}

/// Check if `target_id` appears anywhere in the ancestor chain (excluding self).
fn bfs_path_contains(entries: &[impl AsBfsEntry], idx: usize, target: &str) -> bool {
    let mut current = entries[idx].parent();
    while let Some(i) = current {
        if entries[i].node_id_ref() == target {
            return true;
        }
        current = entries[i].parent();
    }
    false
}

/// Reconstruct the full path from root to `idx` by walking the parent chain.
fn bfs_reconstruct_path(entries: &[impl AsBfsEntry], idx: usize) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = Some(idx);
    while let Some(i) = current {
        path.push(entries[i].node_id_ref().to_string());
        current = entries[i].parent();
    }
    path.reverse();
    path
}

trait AsBfsEntry {
    fn node_id_ref(&self) -> &str;
    fn parent(&self) -> Option<usize>;
}

impl AsBfsEntry for BfsEntryOuter {
    fn node_id_ref(&self) -> &str {
        &self.node_id
    }
    fn parent(&self) -> Option<usize> {
        self.parent_idx
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_kind_all_is_exhaustive() {
        let all_set: std::collections::HashSet<&EdgeKind> = EdgeKind::ALL.iter().collect();

        let check_variant = |ek: EdgeKind| -> bool {
            match ek {
                EdgeKind::CoOccurrence
                | EdgeKind::TemporalCoupling
                | EdgeKind::Insight
                | EdgeKind::Dependency
                | EdgeKind::AntiPattern
                | EdgeKind::Contains
                | EdgeKind::Imports
                | EdgeKind::SqlCalls
                | EdgeKind::HasColumn
                | EdgeKind::ForeignKey
                | EdgeKind::QueriesTable
                | EdgeKind::ReadsState
                | EdgeKind::WritesState
                | EdgeKind::DataBinding
                | EdgeKind::RegistersControl
                | EdgeKind::IncludesFile
                | EdgeKind::UnresolvedStateRead
                | EdgeKind::UnresolvedStateWrite
                | EdgeKind::ExposesWebService
                | EdgeKind::ExposesHttpHandler
                | EdgeKind::ExposesWcfService
                | EdgeKind::ContainsUi
                | EdgeKind::UiLayoutNeighbor
                | EdgeKind::ReadsColumn
                | EdgeKind::RegistersModule
                | EdgeKind::RegistersHandler
                | EdgeKind::ManipulatesDom
                | EdgeKind::TriggersPostback
                | EdgeKind::ApiCall => all_set.contains(&ek),
            }
        };

        for ek in EdgeKind::ALL {
            assert!(
                check_variant(ek.clone()),
                "EdgeKind::{:?} is in ALL but not in the exhaustive match — update the test",
                ek
            );
        }

        let variant_count = 29;
        assert_eq!(
            EdgeKind::ALL.len(),
            variant_count,
            "EdgeKind::ALL length mismatch — a variant was added to the enum \
             but not to EdgeKind::ALL or this test"
        );
    }

    #[test]
    fn edge_kind_as_str_parse_roundtrip() {
        for ek in EdgeKind::ALL {
            let s = ek.as_str();
            let parsed = EdgeKind::parse(s);
            assert_eq!(
                parsed.as_ref(),
                Some(ek),
                "as_str/parse roundtrip failed for {:?} -> {:?}",
                ek,
                s
            );
        }
    }

    #[test]
    fn validate_key_component_rejects_nul() {
        assert!(validate_key_component("test", "good_value").is_ok());
        assert!(validate_key_component("test", "bad\0value").is_err());
        assert!(validate_key_component("test", "\0").is_err());
    }

    #[test]
    fn case_insensitive_search() {
        assert!(contains_case_insensitive("FooBar", "oob"));
        assert!(contains_case_insensitive("FooBar", "OOB"));
        assert!(contains_case_insensitive("FooBar", "foobar"));
        assert!(!contains_case_insensitive("FooBar", "baz"));
        assert!(contains_case_insensitive("anything", ""));
    }

    #[test]
    fn case_insensitive_path_normalizes_backslash() {
        assert!(contains_case_insensitive_path(
            "src\\main\\App.cs",
            "src/main"
        ));
        assert!(contains_case_insensitive_path(
            "src/main/App.cs",
            "src/main"
        ));
        assert!(contains_case_insensitive_path(
            "SRC\\Main\\APP.CS",
            "src/main/app.cs"
        ));
    }

    #[test]
    fn adj_value_roundtrip() {
        let encoded = encode_adj_value(42, 1234567890123);
        let (w, ts) = decode_adj_value(&encoded);
        assert_eq!(w, 42);
        assert_eq!(ts, 1234567890123);
    }
}
