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

/// Decode adjacency value from fixed 12-byte binary: `[weight: u32 LE][timestamp: u64 LE]`.
///
/// Returns an error on short/corrupted buffers instead of silently returning zeros.
fn decode_adj_value(bytes: &[u8]) -> anyhow::Result<(u32, u64)> {
    anyhow::ensure!(
        bytes.len() >= 12,
        "decode_adj_value: expected 12 bytes, got {} — possible DB corruption",
        bytes.len()
    );
    let weight = u32::from_le_bytes(bytes[..4].try_into()?);
    let ts = u64::from_le_bytes(bytes[4..12].try_into()?);
    Ok((weight, ts))
}

// ─── EdgeKind ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    CoOccurrence,
    TemporalCoupling,
    Insight,
    Calls,
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
    ParameterBinding,
    /// Links VB backend coordinate provider to frontend GIS map consumer.
    SpatialCall,
    /// Connects related state keys accessed by the same methods.
    StateAffinity,
    /// VB method injects JavaScript into the page via RegisterStartupScript et al.
    InjectsScript,
    /// Code-behind or Classic ASP calls a stored procedure (CommandType.StoredProcedure).
    CallsStoredProcedure,
    /// Stored procedure reads from a table (SELECT / JOIN).
    StoredProcReadsTable,
    /// Stored procedure writes to a table (INSERT / UPDATE / DELETE / MERGE).
    StoredProcWritesTable,
    /// An .aspx Content control fills a ContentPlaceHolder region in a .master page.
    FillsRegion,
    /// Runtime-observed control interaction/event not guaranteed by static analysis.
    ObservedRuntimeControl,
    /// Runtime-observed SQL execution (query/SP) captured from logs/profilers.
    ObservedRuntimeSql,
    /// Code reads a configuration/app setting (web.config appSettings key,
    /// My.Settings member, or a settings accessor helper).
    ReadsSetting,
    /// Class inherits from a base class (C# `: Base`, VB `Inherits Base`).
    /// Distinct from the webforms page→codebehind "inherits" raw kind,
    /// which maps to Contains.
    InheritsFrom,
    /// Class implements an interface (C# `: IFoo`, VB `Implements IFoo`).
    Implements,
}

impl EdgeKind {
    pub const ALL: &'static [EdgeKind] = &[
        EdgeKind::CoOccurrence,
        EdgeKind::TemporalCoupling,
        EdgeKind::Insight,
        EdgeKind::Calls,
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
        EdgeKind::ParameterBinding,
        EdgeKind::SpatialCall,
        EdgeKind::StateAffinity,
        EdgeKind::InjectsScript,
        EdgeKind::CallsStoredProcedure,
        EdgeKind::StoredProcReadsTable,
        EdgeKind::StoredProcWritesTable,
        EdgeKind::FillsRegion,
        EdgeKind::ObservedRuntimeControl,
        EdgeKind::ObservedRuntimeSql,
        EdgeKind::ReadsSetting,
        EdgeKind::InheritsFrom,
        EdgeKind::Implements,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::CoOccurrence => "co_occurrence",
            EdgeKind::TemporalCoupling => "temporal_coupling",
            EdgeKind::Insight => "insight",
            EdgeKind::Calls => "calls",
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
            EdgeKind::ParameterBinding => "parameter_binding",
            EdgeKind::SpatialCall => "spatial_call",
            EdgeKind::StateAffinity => "state_affinity",
            EdgeKind::InjectsScript => "injects_script",
            EdgeKind::CallsStoredProcedure => "calls_stored_procedure",
            EdgeKind::StoredProcReadsTable => "stored_proc_reads_table",
            EdgeKind::StoredProcWritesTable => "stored_proc_writes_table",
            EdgeKind::FillsRegion => "fills_region",
            EdgeKind::ObservedRuntimeControl => "observed_runtime_control",
            EdgeKind::ObservedRuntimeSql => "observed_runtime_sql",
            EdgeKind::ReadsSetting => "reads_setting",
            EdgeKind::InheritsFrom => "inherits_from",
            EdgeKind::Implements => "implements_interface",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "co_occurrence" => Some(EdgeKind::CoOccurrence),
            "temporal_coupling" => Some(EdgeKind::TemporalCoupling),
            "insight" => Some(EdgeKind::Insight),
            "calls" => Some(EdgeKind::Calls),
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
            "parameter_binding" => Some(EdgeKind::ParameterBinding),
            "spatial_call" => Some(EdgeKind::SpatialCall),
            "state_affinity" => Some(EdgeKind::StateAffinity),
            "injects_script" => Some(EdgeKind::InjectsScript),
            "calls_stored_procedure" => Some(EdgeKind::CallsStoredProcedure),
            "stored_proc_reads_table" => Some(EdgeKind::StoredProcReadsTable),
            "stored_proc_writes_table" => Some(EdgeKind::StoredProcWritesTable),
            "fills_region" => Some(EdgeKind::FillsRegion),
            "observed_runtime_control" => Some(EdgeKind::ObservedRuntimeControl),
            "observed_runtime_sql" => Some(EdgeKind::ObservedRuntimeSql),
            "reads_setting" => Some(EdgeKind::ReadsSetting),
            "inherits_from" => Some(EdgeKind::InheritsFrom),
            "implements_interface" => Some(EdgeKind::Implements),
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

mod optional_json_string {
    use serde::de::Error as DeError;
    use serde::ser::Error as SerError;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::Value as JsonValue;

    pub fn serialize<S>(value: &Option<JsonValue>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(v) => {
                let s = serde_json::to_string(v).map_err(S::Error::custom)?;
                serializer.serialize_some(&s)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<JsonValue>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Option<String> = Option::deserialize(deserializer)?;
        match s {
            Some(str_val) => {
                let v = serde_json::from_str(&str_val).map_err(D::Error::custom)?;
                Ok(Some(v))
            }
            None => Ok(None),
        }
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
    #[serde(default, with = "optional_json_string")]
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
    #[serde(default, with = "optional_json_string")]
    pub metadata: Option<JsonValue>,
    pub updated_at_ms: u64,
}

// ─── GraphStore ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GraphStore {
    db: Arc<Database>,
}

/// Resolution result from [`GraphStore::resolve_symbol`].
#[derive(Debug, Clone)]
pub enum ResolveResult {
    /// Exactly one node found; safe to use.
    Unique(Node),
    /// Multiple candidates found; caller must disambiguate.
    Ambiguous(Vec<Node>),
    /// No candidates found.
    NotFound,
}

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
                if let Ok(true) = wtx.delete_table(TableDefinition::<&str, &[u8]>::new(name)) {
                    dropped.push(name)
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
        validate_key_component("project_id", project_id)?;
        validate_key_component("key", key)?;
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
            let (weight, _ts) = decode_adj_value(val_guard.value())?;
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
                let (weight, _ts) = decode_adj_value(val_guard.value())?;
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

    /// Resolve a user-supplied symbol identifier to a node using a fallback ladder:
    /// 1) direct node_id lookup for known prefixed IDs
    /// 2) exact `Node.name` match
    /// 3) exact `metadata.fqn` match (legacy)
    /// 4) bare-name fallback using terminal `.` segment
    /// 5) not found
    ///
    /// `node_type` narrows candidate searches in steps 2-4.
    /// `prefer_file_path` is used as a tiebreaker when multiple candidates exist.
    pub fn resolve_symbol(
        &self,
        project_id: &str,
        input: &str,
        node_type: Option<&str>,
        prefer_file_path: Option<&str>,
    ) -> anyhow::Result<ResolveResult> {
        const DIRECT_PREFIXES: [&str; 11] = [
            "sym:",
            "file:",
            "page:",
            "control:",
            "table:",
            "column:",
            "state:",
            "gis_config:",
            "binding_field:",
            "ui_container:",
            "sql:",
        ];

        // Step 1: direct node_id lookup.
        if DIRECT_PREFIXES.iter().any(|p| input.starts_with(p))
            && let Some(node) = self.get_node(project_id, input)?
        {
            return Ok(ResolveResult::Unique(node));
        }

        // Step 2: exact canonical name match (`Node.name`).
        if let Ok(nodes) = self.query_nodes(project_id, node_type, Some(input), None, 100) {
            let exact_name: Vec<Node> = nodes.into_iter().filter(|n| n.name == input).collect();
            if exact_name.len() == 1 {
                return Ok(ResolveResult::Unique(exact_name[0].clone()));
            }
            if exact_name.len() > 1 {
                if let Some(prefer) = prefer_file_path
                    && let Some(node) = exact_name.iter().find(|n| n.file_path.as_str() == prefer)
                {
                    return Ok(ResolveResult::Unique(node.clone()));
                }
                return Ok(ResolveResult::Ambiguous(exact_name));
            }
        }

        // Step 3: legacy metadata.fqn exact match.
        if let Ok(nodes) = self.query_nodes(project_id, node_type, None, None, 5000) {
            let fqn_match: Vec<Node> = nodes
                .into_iter()
                .filter(|n| {
                    n.metadata
                        .as_ref()
                        .and_then(|m| m.get("fqn"))
                        .and_then(|v| v.as_str())
                        == Some(input)
                })
                .collect();
            if fqn_match.len() == 1 {
                return Ok(ResolveResult::Unique(fqn_match[0].clone()));
            }
            if fqn_match.len() > 1 {
                if let Some(prefer) = prefer_file_path
                    && let Some(node) = fqn_match.iter().find(|n| n.file_path.as_str() == prefer)
                {
                    return Ok(ResolveResult::Unique(node.clone()));
                }
                return Ok(ResolveResult::Ambiguous(fqn_match));
            }
        }

        // Step 4: short-name fallback.
        let short = input.split('.').next_back().unwrap_or(input);
        if let Ok(nodes) = self.query_nodes(project_id, node_type, Some(short), None, 50) {
            let suffix = format!(".{short}");
            let exact_short: Vec<Node> = nodes
                .into_iter()
                .filter(|n| n.name == short || n.name.ends_with(&suffix))
                .collect();
            if exact_short.len() == 1 {
                return Ok(ResolveResult::Unique(exact_short[0].clone()));
            }
            if exact_short.len() > 1 {
                if let Some(prefer) = prefer_file_path
                    && let Some(node) = exact_short.iter().find(|n| n.file_path.as_str() == prefer)
                {
                    return Ok(ResolveResult::Unique(node.clone()));
                }
                return Ok(ResolveResult::Ambiguous(exact_short));
            }
        }

        Ok(ResolveResult::NotFound)
    }

    fn try_resolve_unresolved_target(&self, project_id: &str, target_id: &str) -> Option<String> {
        if !target_id.starts_with("::") {
            return None;
        }
        let name = &target_id[2..];
        match self.resolve_symbol(project_id, name, None, None).ok()? {
            ResolveResult::Unique(node) => Some(node.node_id),
            ResolveResult::Ambiguous(_) | ResolveResult::NotFound => None,
        }
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

    /// Remove stale-generation nodes (and edges touching them) for the given
    /// file paths ONLY. Safe after an INCREMENTAL update: only files that
    /// were re-extracted this generation are eligible, so unchanged files —
    /// whose nodes legitimately keep older generations until a full reindex —
    /// are never touched. (The global `purge_old_generations` is only safe
    /// when every file was re-indexed at `active_generation`.)
    pub fn purge_stale_nodes_for_paths(
        &self,
        project_id: &str,
        paths: &std::collections::HashSet<String>,
        active_generation: u64,
    ) -> anyhow::Result<(usize, usize)> {
        if paths.is_empty() {
            return Ok((0, 0));
        }
        let prefix = format!("{project_id}\0");
        const BATCH_SIZE: usize = 1000;

        // Phase 1: stale nodes belonging to the re-indexed files. Also build
        // a successor map (file, name, type) → current-generation node_id so
        // cross-file edges into a moved symbol can be REMAPPED instead of
        // severed (unchanged files are not re-extracted, so their edges
        // would otherwise dangle when a callee's declaration line shifts).
        let mut removed_node_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut removed_identity: std::collections::HashMap<String, (String, String, String)> =
            std::collections::HashMap::new();
        let mut successors: std::collections::HashMap<(String, String, String), String> =
            std::collections::HashMap::new();
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
                if !paths.contains(n.file_path.as_str()) {
                    continue;
                }
                let identity = (
                    n.file_path.as_str().to_string(),
                    n.name.clone(),
                    n.node_type.clone(),
                );
                if n.generation == active_generation {
                    successors.insert(identity, n.node_id.clone());
                    continue;
                }
                let stale = match engram_core::get_policy(&n.namespace).map(|p| p.retention) {
                    Ok(engram_core::NamespaceRetention::KeepLatestOnly) => true,
                    Ok(engram_core::NamespaceRetention::KeepLastGenerations(n_keep)) => {
                        n.generation < active_generation.saturating_sub(n_keep as u64 - 1)
                    }
                    _ => false,
                };
                if stale {
                    removed_identity.insert(n.node_id.clone(), identity);
                    removed_node_ids.insert(n.node_id);
                    keys.push(k.value().to_string());
                }
            }
            keys
        };
        let nodes_removed = node_keys_to_remove.len();
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

        // Phase 2: stale-generation edges touching a removed node. Each is
        // either REMAPPED (every removed endpoint has a same-identity
        // successor in the new generation — typical for cross-file edges
        // into a symbol whose line shifted) or dropped (the symbol is gone).
        // Current-generation edges always reference current-generation node
        // ids, so generation-filtering keeps live wiring intact.
        let (edge_keys_to_remove, remapped_edges) = {
            let rtx = self.db.begin_read()?;
            let et = rtx.open_table(EDGES)?;
            let mut keys = Vec::new();
            let mut remapped: Vec<Edge> = Vec::new();
            for r in et.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let e: Edge = bincode::deserialize(v.value())?;
                if e.generation == active_generation
                    || (!removed_node_ids.contains(&e.source_id)
                        && !removed_node_ids.contains(&e.target_id))
                {
                    continue;
                }
                keys.push(k.value().to_string());

                let map_endpoint = |id: &str| -> Option<String> {
                    if !removed_node_ids.contains(id) {
                        return Some(id.to_string());
                    }
                    removed_identity
                        .get(id)
                        .and_then(|identity| successors.get(identity))
                        .cloned()
                };
                if let (Some(src), Some(tgt)) =
                    (map_endpoint(&e.source_id), map_endpoint(&e.target_id))
                {
                    let mut ne = e.clone();
                    ne.source_id = src;
                    ne.target_id = tgt;
                    ne.generation = active_generation;
                    remapped.push(ne);
                }
            }
            (keys, remapped)
        };
        let edges_removed = edge_keys_to_remove.len();
        for chunk in edge_keys_to_remove.chunks(BATCH_SIZE) {
            let wtx = self.db.begin_write()?;
            {
                let mut et = wtx.open_table(EDGES)?;
                let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
                let mut adj_in_t = wtx.open_table(ADJ_IN)?;
                for k in chunk {
                    let parts: Vec<&str> = k.splitn(4, '\0').collect();
                    if parts.len() == 4
                        && let Some(ek) = EdgeKind::parse(parts[1])
                    {
                        let out_prefix = adj_key(project_id, &ek, parts[2]);
                        adj_out_t.remove((out_prefix.as_str(), parts[3]))?;
                        let in_prefix = adj_key(project_id, &ek, parts[3]);
                        adj_in_t.remove((in_prefix.as_str(), parts[2]))?;
                    }
                    et.remove(k.as_str())?;
                }
            }
            wtx.commit()?;
        }

        // Re-insert the remapped edges (after deletions, so a remap whose key
        // collides with a deleted key survives).
        for chunk in remapped_edges.chunks(BATCH_SIZE) {
            let wtx = self.db.begin_write()?;
            {
                let mut et = wtx.open_table(EDGES)?;
                let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
                let mut adj_in_t = wtx.open_table(ADJ_IN)?;
                for e in chunk {
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
        }

        Ok((nodes_removed, edges_removed))
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

            // If we hit a SQL / database terminal node, reconstruct and
            // record the path. Besides `sql:` (inline SQL text) and the
            // legacy `inline_sql` / `stored_proc` node types, we also
            // terminate at:
            //   - `db_table` / `db_column` node types (emitted by the
            //     WebForms + DDL extractors when a control binds to a
            //     specific database table or column), and
            //   - `table:…` / `column:…` node-id prefixes (the
            //     canonical ID format for those same nodes — see
            //     `engram_core::ids::NodeId::table` / `::column`).
            //
            // Pages with `DataSourceID` binding on a GridView often
            // reach a `binding_field → ReadsColumn → column:…` chain
            // that is a perfectly valid evidence terminus for blast
            // radius and migration dossiers; without these endpoints
            // the BFS ran past them and exhausted `max_hops` without
            // recording a path.
            if node.node_id.starts_with("sql:")
                || node.node_id.starts_with("table:")
                || node.node_id.starts_with("column:")
                || node.node_type == "inline_sql"
                || node.node_type == "stored_proc"
                || node.node_type == "db_table"
                || node.node_type == "db_column"
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

            if let Some(&prev_depth) = best_depth.get(&curr_id)
                && depth > prev_depth
                && curr_id != start_node_id
            {
                continue;
            }
            best_depth.insert(curr_id.clone(), depth);

            let mut neighbors = Vec::new();
            // UI → SQL traversal edge set.
            //
            // `Dependency` covers generic symbol↔symbol links (and also
            // absorbs unknown string-kinds from extractors via the
            // ingest-side fallback, e.g. `event_wiring` → Dependency).
            //
            // `Contains` covers page → control, class → method.
            //
            // `DataBinding` is what ties a GridView (or any consumer) to
            // its `DataSourceID` control and what ties a data-binding
            // `<%# Eval("col") %>` to a `binding_field:col`. Without
            // this kind the trace cannot cross the declarative-binding
            // boundary from a grid to its data source.
            //
            // `Calls`, `QueriesTable`, `SqlCalls`, `StoredProcReadsTable`,
            // `StoredProcWritesTable`, and `CallsStoredProcedure` fill
            // out the method-to-SQL path: a handler `Calls` a helper
            // which `QueriesTable` / `SqlCalls` / `CallsStoredProcedure`
            // reaches the terminal SQL or stored-proc node. `ReadsColumn`
            // is included so column-level endpoints aren't stranded.
            let kinds = [
                EdgeKind::Dependency,
                EdgeKind::Contains,
                EdgeKind::DataBinding,
                EdgeKind::Calls,
                EdgeKind::SqlCalls,
                EdgeKind::QueriesTable,
                EdgeKind::CallsStoredProcedure,
                EdgeKind::StoredProcReadsTable,
                EdgeKind::StoredProcWritesTable,
                EdgeKind::ReadsColumn,
                // `HasColumn` lets the BFS cross from a column to its
                // owning table, useful when a path terminates at a
                // column but the consumer wants the surrounding table
                // as context (e.g. migration dossiers listing every
                // table a page reads).
                EdgeKind::HasColumn,
            ];
            for k in kinds {
                let out = self.neighbors(project_id, k, &curr_id, 100)?;
                neighbors.extend(out.into_iter().map(|(id, _)| id));
            }

            neighbors.sort();
            neighbors.dedup();
            neighbors.truncate(MAX_BRANCHING);

            for mut next_id in neighbors {
                if let Some(resolved_id) = self.try_resolve_unresolved_target(project_id, &next_id)
                {
                    next_id = resolved_id;
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
                if let Some(resolved_id) = self.try_resolve_unresolved_target(project_id, &next_id)
                {
                    next_id = resolved_id;
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

        // ── Phase 1: Build symbol map (single NODES scan) ────────────
        let phase1_start = std::time::Instant::now();

        enum SymbolMatch {
            Unique(String),
            Ambiguous(Vec<String>),
        }

        // node_id → file_path (for tiebreaker lookups)
        let mut node_file_paths: HashMap<String, String> = HashMap::new();
        // Exact name match
        let mut by_name: HashMap<String, SymbolMatch> = HashMap::new();
        // Terminal segment match (last dot-segment of name)
        let mut by_terminal: HashMap<String, Vec<String>> = HashMap::new();
        // Legacy metadata.fqn match
        let mut by_metadata_fqn: HashMap<String, String> = HashMap::new();

        let mut node_count = 0usize;
        {
            let rtx = self.db.begin_read()?;
            let nt = rtx.open_table(NODES)?;
            for r in nt.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let node: Node = bincode::deserialize(v.value())?;
                node_count += 1;

                node_file_paths.insert(node.node_id.clone(), node.file_path.as_str().to_string());

                // by_name: exact Node.name → node_id
                match by_name.get_mut(&node.name) {
                    Some(SymbolMatch::Unique(first)) => {
                        let first_id = first.clone();
                        by_name.insert(
                            node.name.clone(),
                            SymbolMatch::Ambiguous(vec![first_id, node.node_id.clone()]),
                        );
                    }
                    Some(SymbolMatch::Ambiguous(ids)) => {
                        ids.push(node.node_id.clone());
                    }
                    None => {
                        by_name
                            .insert(node.name.clone(), SymbolMatch::Unique(node.node_id.clone()));
                    }
                }

                // by_terminal: last dot-segment of name
                if let Some(terminal) = node.name.rsplit('.').next() {
                    if !terminal.is_empty() && terminal != node.name {
                        by_terminal
                            .entry(terminal.to_string())
                            .or_default()
                            .push(node.node_id.clone());
                    }
                }

                // by_metadata_fqn: metadata.fqn → node_id (first wins)
                if let Some(fqn) = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                {
                    by_metadata_fqn
                        .entry(fqn.to_string())
                        .or_insert_with(|| node.node_id.clone());
                }
            }
        }

        let phase1_elapsed = phase1_start.elapsed();
        tracing::info!(
            "resolve_symbol_edges: built symbol map project_id={} nodes={} by_name={} by_terminal={} by_metadata_fqn={} elapsed_ms={}",
            project_id,
            node_count,
            by_name.len(),
            by_terminal.len(),
            by_metadata_fqn.len(),
            phase1_elapsed.as_millis()
        );

        // ── Phase 2: Collect unresolved edges (single EDGES scan) ────
        let phase2_start = std::time::Instant::now();

        struct UnresolvedEdge {
            old_key: String,
            edge: Edge,
        }

        let mut unresolved: Vec<UnresolvedEdge> = Vec::new();
        {
            let rtx = self.db.begin_read()?;
            let et = rtx.open_table(EDGES)?;
            for r in et.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let e: Edge = bincode::deserialize(v.value())?;
                if e.target_id.starts_with("::") {
                    unresolved.push(UnresolvedEdge {
                        old_key: k.value().to_string(),
                        edge: e,
                    });
                }
            }
        }

        let phase2_elapsed = phase2_start.elapsed();
        tracing::info!(
            "resolve_symbol_edges: collected unresolved edges project_id={} count={} elapsed_ms={}",
            project_id,
            unresolved.len(),
            phase2_elapsed.as_millis()
        );

        if unresolved.is_empty() {
            return Ok(0);
        }

        // ── Phase 3: Resolve via HashMap lookups ─────────────────────
        let phase3_start = std::time::Instant::now();

        // Tiebreaker: when multiple candidates match, prefer the one
        // in the same file as the source node.
        let resolve_ambiguous =
            |candidates: &[String], source_file: Option<&String>| -> Option<String> {
                if let Some(sf) = source_file {
                    for cid in candidates {
                        if let Some(fp) = node_file_paths.get(cid) {
                            if fp == sf {
                                return Some(cid.clone());
                            }
                        }
                    }
                }
                None
            };

        let mut updates: Vec<(String, Edge)> = Vec::new();

        for entry in &unresolved {
            let name = &entry.edge.target_id[2..]; // strip "::"
            let source_file = node_file_paths.get(&entry.edge.source_id);

            // Step 1: exact name match
            let resolved = match by_name.get(name) {
                Some(SymbolMatch::Unique(id)) => Some(id.clone()),
                Some(SymbolMatch::Ambiguous(ids)) => resolve_ambiguous(ids, source_file),
                None => None,
            };

            if let Some(target_id) = resolved {
                let mut new_e = entry.edge.clone();
                new_e.target_id = target_id;
                updates.push((entry.old_key.clone(), new_e));
                continue;
            }

            // Step 2: metadata.fqn match
            if let Some(id) = by_metadata_fqn.get(name) {
                let mut new_e = entry.edge.clone();
                new_e.target_id = id.clone();
                updates.push((entry.old_key.clone(), new_e));
                continue;
            }

            // Step 3: terminal segment fallback
            let short = name.rsplit('.').next().unwrap_or(name);
            if let Some(candidates) = by_terminal.get(short) {
                if candidates.len() == 1 {
                    let mut new_e = entry.edge.clone();
                    new_e.target_id = candidates[0].clone();
                    updates.push((entry.old_key.clone(), new_e));
                } else if let Some(id) = resolve_ambiguous(candidates, source_file) {
                    let mut new_e = entry.edge.clone();
                    new_e.target_id = id;
                    updates.push((entry.old_key.clone(), new_e));
                }
            }
        }

        let phase3_elapsed = phase3_start.elapsed();
        tracing::info!(
            "resolve_symbol_edges: resolved project_id={} resolved={} unresolved={} elapsed_ms={}",
            project_id,
            updates.len(),
            unresolved.len() - updates.len(),
            phase3_elapsed.as_millis()
        );

        // ── Phase 4: Apply updates (single write transaction) ────────
        let phase4_start = std::time::Instant::now();

        let wtx = self.db.begin_write()?;
        {
            let mut et = wtx.open_table(EDGES)?;
            let mut adj_out_t = wtx.open_table(ADJ_OUT)?;
            let mut adj_in_t = wtx.open_table(ADJ_IN)?;
            let now = now_ms();

            for (old_key, new_edge) in &updates {
                et.remove(old_key.as_str())?;

                // Parse old key: "project\0kind\0source\0old_target"
                let old_parts: Vec<&str> = old_key.splitn(4, '\0').collect();
                if old_parts.len() == 4 {
                    let old_target = old_parts[3];

                    let out_prefix = adj_key(project_id, &new_edge.edge_kind, &new_edge.source_id);
                    adj_out_t.remove((out_prefix.as_str(), old_target))?;

                    let old_in_prefix = adj_key(project_id, &new_edge.edge_kind, old_target);
                    adj_in_t.remove((old_in_prefix.as_str(), new_edge.source_id.as_str()))?;

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
                let val = bincode::serialize(new_edge)?;
                et.insert(new_key.as_str(), val.as_slice())?;
            }
        }
        wtx.commit()?;

        let phase4_elapsed = phase4_start.elapsed();
        tracing::info!(
            "resolve_symbol_edges: applied updates project_id={} count={} elapsed_ms={}",
            project_id,
            updates.len(),
            phase4_elapsed.as_millis()
        );

        Ok(updates.len())
    }
}

// ─── Free functions ──────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Validate that a key component does not contain separator characters (NUL or newline).
///
/// Delegates to the shared `engram_core::validate_key_component` to stay consistent
/// with the doc store validation rules.
fn validate_key_component(name: &str, value: &str) -> anyhow::Result<()> {
    engram_core::validate_key_component(name, value).map_err(|e| anyhow::anyhow!("{e}"))
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_store() -> GraphStore {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut path: PathBuf = std::env::temp_dir();
        path.push(format!("engram_graph_store_test_{unique}.redb"));
        let _ = std::fs::remove_file(&path);
        GraphStore::open(&path).expect("open test graph store")
    }

    fn test_node(node_id: &str, node_type: &str, name: &str, file_path: &str) -> Node {
        Node {
            node_id: node_id.to_string(),
            node_type: node_type.to_string(),
            name: name.to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            file_path: RelPath::from(file_path),
            start_line: 1,
            end_line: 1,
            generation: 1,
            metadata: None,
        }
    }

    #[test]
    fn edge_kind_all_is_exhaustive() {
        let all_set: std::collections::HashSet<&EdgeKind> = EdgeKind::ALL.iter().collect();

        let check_variant = |ek: EdgeKind| -> bool {
            match ek {
                EdgeKind::CoOccurrence
                | EdgeKind::TemporalCoupling
                | EdgeKind::Insight
                | EdgeKind::Calls
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
                | EdgeKind::ApiCall
                | EdgeKind::ParameterBinding
                | EdgeKind::SpatialCall
                | EdgeKind::StateAffinity
                | EdgeKind::InjectsScript
                | EdgeKind::CallsStoredProcedure
                | EdgeKind::StoredProcReadsTable
                | EdgeKind::StoredProcWritesTable
                | EdgeKind::FillsRegion
                | EdgeKind::ObservedRuntimeControl
                | EdgeKind::ObservedRuntimeSql
                | EdgeKind::ReadsSetting
                | EdgeKind::InheritsFrom
                | EdgeKind::Implements => all_set.contains(&ek),
            }
        };

        for ek in EdgeKind::ALL {
            assert!(
                check_variant(ek.clone()),
                "EdgeKind::{:?} is in ALL but not in the exhaustive match — update the test",
                ek
            );
        }

        let variant_count = 40;
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
        let (w, ts) = decode_adj_value(&encoded).unwrap();
        assert_eq!(w, 42);
        assert_eq!(ts, 1234567890123);
    }

    #[test]
    fn decode_adj_value_rejects_short_buffer() {
        assert!(decode_adj_value(&[]).is_err());
        assert!(decode_adj_value(&[0; 4]).is_err());
        assert!(decode_adj_value(&[0; 11]).is_err());
        // Exactly 12 bytes should succeed
        assert!(decode_adj_value(&[0; 12]).is_ok());
    }

    #[test]
    fn resolve_symbol_direct_id_hit() {
        let store = test_store();
        let project = "p1";
        let node = test_node(
            "sym:function:src/a.vb:Ns.A:10",
            "function",
            "Ns.A",
            "src/a.vb",
        );
        store
            .upsert_nodes(project, std::slice::from_ref(&node))
            .unwrap();

        let resolved = store
            .resolve_symbol(project, &node.node_id, Some("function"), None)
            .unwrap();
        match resolved {
            ResolveResult::Unique(found) => assert_eq!(found.node_id, node.node_id),
            _ => panic!("expected unique resolution"),
        }
    }

    #[test]
    fn resolve_symbol_exact_name_match() {
        let store = test_store();
        let project = "p1";
        let node = test_node(
            "sym:function:src/a.vb:Ns.A:10",
            "function",
            "Ns.A",
            "src/a.vb",
        );
        store
            .upsert_nodes(project, std::slice::from_ref(&node))
            .unwrap();

        let resolved = store
            .resolve_symbol(project, "Ns.A", Some("function"), None)
            .unwrap();
        match resolved {
            ResolveResult::Unique(found) => assert_eq!(found.node_id, node.node_id),
            _ => panic!("expected unique resolution"),
        }
    }

    #[test]
    fn resolve_symbol_metadata_fqn_fallback() {
        let store = test_store();
        let project = "p1";
        let mut node = test_node(
            "sym:function:src/a.vb:CanonicalName:10",
            "function",
            "CanonicalName",
            "src/a.vb",
        );
        node.metadata = Some(serde_json::json!({ "fqn": "Ns.LegacyName" }));
        store
            .upsert_nodes(project, std::slice::from_ref(&node))
            .unwrap();

        let resolved = store
            .resolve_symbol(project, "Ns.LegacyName", Some("function"), None)
            .unwrap();
        match resolved {
            ResolveResult::Unique(found) => assert_eq!(found.node_id, node.node_id),
            _ => panic!("expected unique resolution"),
        }
    }

    #[test]
    fn resolve_symbol_bare_name_single_match() {
        let store = test_store();
        let project = "p1";
        let node = test_node(
            "sym:function:src/a.vb:sharedfunc.SafeRedirect:10",
            "function",
            "sharedfunc.SafeRedirect",
            "src/a.vb",
        );
        store
            .upsert_nodes(project, std::slice::from_ref(&node))
            .unwrap();

        let resolved = store
            .resolve_symbol(project, "SafeRedirect", Some("function"), None)
            .unwrap();
        match resolved {
            ResolveResult::Unique(found) => assert_eq!(found.node_id, node.node_id),
            _ => panic!("expected unique resolution"),
        }
    }

    #[test]
    fn resolve_symbol_bare_name_ambiguous_with_and_without_tiebreaker() {
        let store = test_store();
        let project = "p1";
        let left = test_node(
            "sym:function:src/a.vb:Ns.Left.SafeRedirect:10",
            "function",
            "Ns.Left.SafeRedirect",
            "src/a.vb",
        );
        let right = test_node(
            "sym:function:src/b.vb:Ns.Right.SafeRedirect:10",
            "function",
            "Ns.Right.SafeRedirect",
            "src/b.vb",
        );
        store
            .upsert_nodes(project, &[left.clone(), right.clone()])
            .unwrap();

        let amb = store
            .resolve_symbol(project, "SafeRedirect", Some("function"), None)
            .unwrap();
        match amb {
            ResolveResult::Ambiguous(nodes) => assert_eq!(nodes.len(), 2),
            _ => panic!("expected ambiguous resolution"),
        }

        let resolved = store
            .resolve_symbol(project, "SafeRedirect", Some("function"), Some("src/b.vb"))
            .unwrap();
        match resolved {
            ResolveResult::Unique(found) => assert_eq!(found.node_id, right.node_id),
            _ => panic!("expected unique resolution with tiebreaker"),
        }
    }

    #[test]
    fn resolve_symbol_not_found() {
        let store = test_store();
        let project = "p1";
        let resolved = store
            .resolve_symbol(project, "Missing.Symbol", Some("function"), None)
            .unwrap();
        assert!(matches!(resolved, ResolveResult::NotFound));
    }

    /// Regression guard for `trace_ui_event` on a page with declarative
    /// data binding: a `GridView` → `LinqDataSource` (DataBinding) →
    /// handler function (Dependency / event_wiring) → SQL must be
    /// reachable via `find_ui_paths`. Before the traversal edge set was
    /// expanded, the BFS only followed `[Dependency, Contains, SqlCalls]`
    /// and could not cross the `DataBinding` hop, so the grid returned
    /// no paths despite the graph having every intermediate edge.
    #[test]
    fn find_ui_paths_traverses_data_binding_and_calls_to_sql() {
        let store = test_store();
        let project = "p_ui";

        // Node chain: control:gv → control:ds → function:handler → sql:query
        let nodes = [
            test_node("control:p.aspx:gv", "control", "gv", "p.aspx"),
            test_node("control:p.aspx:ds", "control", "ds", "p.aspx"),
            test_node(
                "sym:function:p.aspx.vb:handler:10",
                "function",
                "handler",
                "p.aspx.vb",
            ),
            test_node("sql:select:q", "inline_sql", "q", "p.aspx.vb"),
        ];
        store
            .upsert_nodes(project, &nodes)
            .expect("upsert test nodes");

        fn edge(src: &str, tgt: &str, kind: EdgeKind) -> Edge {
            Edge {
                source_id: src.into(),
                target_id: tgt.into(),
                namespace: "memory".into(),
                language: "vb".into(),
                edge_kind: kind,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }
        }
        store
            .upsert_edges(
                project,
                &[
                    // GridView → LinqDataSource via DataSourceID.
                    edge(
                        "control:p.aspx:gv",
                        "control:p.aspx:ds",
                        EdgeKind::DataBinding,
                    ),
                    // LinqDataSource → OnSelecting handler (extractor emits
                    // the string kind `event_wiring` which ingest maps to
                    // `Dependency` — the fallback arm).
                    edge(
                        "control:p.aspx:ds",
                        "sym:function:p.aspx.vb:handler:10",
                        EdgeKind::Dependency,
                    ),
                    // Handler queries a SQL endpoint.
                    edge(
                        "sym:function:p.aspx.vb:handler:10",
                        "sql:select:q",
                        EdgeKind::SqlCalls,
                    ),
                ],
            )
            .expect("upsert test edges");

        let paths = store
            .find_ui_paths(project, "control:p.aspx:gv", 6, 5)
            .expect("find_ui_paths must succeed");
        assert!(
            !paths.is_empty(),
            "trace_ui_event must find at least one path through \
             DataBinding + Dependency + SqlCalls"
        );
        let first = &paths[0];
        let ids: Vec<&str> = first.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(ids.first().copied(), Some("control:p.aspx:gv"));
        assert!(
            ids.iter().any(|id| id.starts_with("sql:")),
            "path must terminate at a SQL node"
        );
        assert!(
            ids.iter().any(|id| id == &"control:p.aspx:ds"),
            "path must cross the LinqDataSource hop"
        );
    }

    /// Regression guard for the OciusX shape where a GridView's
    /// `DataSourceID` binding ends at a database column via
    /// `binding_field → ReadsColumn → column:…`. The BFS used to
    /// terminate only at `sql:` / `inline_sql` / `stored_proc` nodes
    /// and walked right past `db_column` endpoints, exhausting the
    /// hop budget and returning zero paths on real pages that
    /// actually had a complete, correct evidence chain.
    #[test]
    fn find_ui_paths_terminates_at_db_column() {
        let store = test_store();
        let project = "p_col";

        let nodes = [
            test_node("control:p.aspx:gv", "control", "gv", "p.aspx"),
            test_node(
                "binding_field:p.aspx:colA",
                "binding_field",
                "colA",
                "p.aspx",
            ),
            test_node("column:myTable:colA", "db_column", "colA", "myTable"),
        ];
        store.upsert_nodes(project, &nodes).expect("upsert nodes");

        fn edge(src: &str, tgt: &str, kind: EdgeKind) -> Edge {
            Edge {
                source_id: src.into(),
                target_id: tgt.into(),
                namespace: "memory".into(),
                language: "vb".into(),
                edge_kind: kind,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }
        }
        store
            .upsert_edges(
                project,
                &[
                    edge(
                        "control:p.aspx:gv",
                        "binding_field:p.aspx:colA",
                        EdgeKind::DataBinding,
                    ),
                    edge(
                        "binding_field:p.aspx:colA",
                        "column:myTable:colA",
                        EdgeKind::ReadsColumn,
                    ),
                ],
            )
            .expect("upsert edges");

        let paths = store
            .find_ui_paths(project, "control:p.aspx:gv", 6, 5)
            .expect("find_ui_paths");
        assert!(
            !paths.is_empty(),
            "BFS must terminate at a db_column endpoint — this is the OciusX shape"
        );
        let first = &paths[0];
        assert!(
            first.iter().any(|n| n.node_type == "db_column"),
            "path must contain the db_column terminal node"
        );
        assert!(
            first.iter().any(|n| n.node_id.starts_with("column:")),
            "path must contain a column:... node id"
        );
    }

    /// Same shape for `db_table` terminals — e.g. a direct
    /// `QueriesTable` edge from a handler to a table node.
    #[test]
    fn find_ui_paths_terminates_at_db_table() {
        let store = test_store();
        let project = "p_tbl";

        let nodes = [
            test_node(
                "sym:function:p.aspx.vb:handler:10",
                "function",
                "handler",
                "p.aspx.vb",
            ),
            test_node("table:orders", "db_table", "orders", "schema"),
        ];
        store.upsert_nodes(project, &nodes).expect("upsert nodes");

        fn edge(src: &str, tgt: &str, kind: EdgeKind) -> Edge {
            Edge {
                source_id: src.into(),
                target_id: tgt.into(),
                namespace: "memory".into(),
                language: "vb".into(),
                edge_kind: kind,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }
        }
        store
            .upsert_edges(
                project,
                &[edge(
                    "sym:function:p.aspx.vb:handler:10",
                    "table:orders",
                    EdgeKind::QueriesTable,
                )],
            )
            .expect("upsert edges");

        let paths = store
            .find_ui_paths(project, "sym:function:p.aspx.vb:handler:10", 4, 5)
            .expect("find_ui_paths");
        assert!(!paths.is_empty());
        assert!(paths[0].iter().any(|n| n.node_type == "db_table"));
    }
}
