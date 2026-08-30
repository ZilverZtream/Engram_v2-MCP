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
    /// A conformance-test source file paired with its golden output sidecar
    /// (`foo.ml` → `foo.expected` / `foo.error`).
    TestOracle,
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
        EdgeKind::TestOracle,
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
            EdgeKind::TestOracle => "test_oracle",
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
            "test_oracle" => Some(EdgeKind::TestOracle),
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
                // Metadata-preserving upsert: many writers create nodes only
                // as edge endpoints (dreamer, explain-change) and carry
                // metadata: None. A last-writer-wins overwrite nulled the
                // ingest fingerprints (mtime/size/hash) on file nodes, so
                // change detection re-indexed the same ~1000 files on every
                // update_project forever. Absent metadata means "no opinion",
                // not "clear".
                let val = if n.metadata.is_none() {
                    let existing_meta = nt
                        .get(key.as_str())?
                        .and_then(|g| bincode::deserialize::<Node>(g.value()).ok())
                        .and_then(|old| old.metadata);
                    if let Some(meta) = existing_meta {
                        let mut merged = n.clone();
                        merged.metadata = Some(meta);
                        bincode::serialize(&merged)?
                    } else {
                        bincode::serialize(n)?
                    }
                } else {
                    bincode::serialize(n)?
                };
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
                    // Same metadata-preserving rule as upsert_nodes: absent
                    // metadata must not clear an existing record's metadata.
                    let val = if n.metadata.is_none() {
                        let existing_meta = nt
                            .get(key.as_str())?
                            .and_then(|g| bincode::deserialize::<Node>(g.value()).ok())
                            .and_then(|old| old.metadata);
                        if let Some(meta) = existing_meta {
                            let mut merged = n.clone();
                            merged.metadata = Some(meta);
                            bincode::serialize(&merged)?
                        } else {
                            bincode::serialize(n)?
                        }
                    } else {
                        bincode::serialize(n)?
                    };
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

    /// All STRUCTURAL edges touching `node_id` (either direction), with full
    /// metadata, in O(degree) instead of O(all edges).
    ///
    /// The previous consumers of this question ran `list_structural_edges`
    /// (a full multi-kind table scan deserializing every edge in the
    /// project) and filtered — the dominant cost of compute_blast_radius on
    /// large graphs, paid by check_edit_safety, the migration dossier, and
    /// up to 20× per pre_commit_review. Outgoing edges come from an EDGES
    /// prefix seek per kind (key layout `project\0kind\0source\0target`);
    /// incoming edges resolve via ADJ_IN then point-lookups into EDGES.
    /// TemporalCoupling/CoOccurrence are skipped to match
    /// `list_structural_edges` semantics.
    pub fn edges_touching(
        &self,
        project_id: &str,
        node_id: &str,
        per_direction_limit: usize,
    ) -> anyhow::Result<Vec<Edge>> {
        Ok(self
            .edges_touching_with_coverage(project_id, node_id, per_direction_limit)?
            .0)
    }

    /// Structural edges touching a node with a PRECISE cap contract:
    /// `per_direction_limit` bounds outgoing and incoming SEPARATELY, and the
    /// returned flag says whether EITHER direction was truncated. The old
    /// version's outgoing `break` only left the inner per-kind loop, so the
    /// total could exceed the limit across kinds and `len >= limit` was a
    /// meaningless truncation signal.
    pub fn edges_touching_with_coverage(
        &self,
        project_id: &str,
        node_id: &str,
        per_direction_limit: usize,
    ) -> anyhow::Result<(Vec<Edge>, bool)> {
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        let mut out: Vec<Edge> = Vec::new();
        let mut truncated = false;

        // Outgoing: prefix seek per structural kind, ONE shared budget.
        let mut out_count = 0usize;
        'outgoing: for kind in EdgeKind::ALL {
            if matches!(kind, EdgeKind::TemporalCoupling | EdgeKind::CoOccurrence) {
                continue;
            }
            let prefix = format!("{project_id}\0{}\0{node_id}\0", kind.as_str());
            for r in et.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                if out_count >= per_direction_limit {
                    truncated = true;
                    break 'outgoing;
                }
                out.push(bincode::deserialize(v.value())?);
                out_count += 1;
            }
        }

        // Incoming: per-kind prefix scans with historical kinds excluded
        // BEFORE the shared budget (the old all-kinds fetch was weight-ranked,
        // so heavy temporal edges consumed the cap and hid structural/causal
        // touching edges), and accepted-cap detection so exactly-at-limit is
        // complete.
        let adj = rtx.open_table(ADJ_IN)?;
        let mut in_count = 0usize;
        'incoming: for kind in EdgeKind::ALL {
            if matches!(kind, EdgeKind::TemporalCoupling | EdgeKind::CoOccurrence) {
                continue;
            }
            let prefix = adj_key(project_id, kind, node_id);
            for r in adj.range((prefix.as_str(), "")..)? {
                let (k, _v) = r?;
                let (pfx, source_id) = k.value();
                if pfx != prefix.as_str() {
                    break;
                }
                if in_count >= per_direction_limit {
                    truncated = true;
                    break 'incoming;
                }
                let key = edge_key(project_id, kind, source_id, node_id);
                if let Some(v) = et.get(key.as_str())? {
                    out.push(bincode::deserialize(v.value())?);
                    in_count += 1;
                }
            }
        }
        Ok((out, truncated))
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

    /// One-hop adjacency of a COMPONENT with the policy applied BEFORE the
    /// accepted-result caps — the substrate contract the impact tools need:
    /// "return the first N+1 ACCEPTED external neighbors after kind,
    /// direction, and component-boundary policy", which no post-hoc handler
    /// filtering can recover from a raw first-N fetch (>N higher-weight
    /// internal or non-causal edges would consume the budget and silently
    /// hide external causal ones). ONE read transaction for every (kind,
    /// endpoint) pair — replaces the per-symbol per-kind transaction storm.
    ///
    /// - `endpoints`: the component members whose edges we sweep (target node
    ///   plus, for a file, its contained symbols).
    /// - `component`: the full membership set; an edge whose OTHER end is
    ///   inside it is internal wiring — counted, never returned, and it never
    ///   consumes cap budget.
    /// - `cap_in` / `cap_out`: accepted-result cap per (kind, endpoint);
    ///   return 0 to skip a kind entirely. A (kind, endpoint) pair lands in
    ///   `truncated_*` only when an accepted edge BEYOND the cap exists —
    ///   exactly-at-cap is complete.
    ///
    /// Iteration is in key order (source id) per (kind, endpoint), so results
    /// are deterministic across runs; callers rank for display themselves.
    pub fn component_adjacency(
        &self,
        project_id: &str,
        endpoints: &[String],
        component: &std::collections::HashSet<String>,
        cap_in: &dyn Fn(&EdgeKind) -> usize,
        cap_out: &dyn Fn(&EdgeKind) -> usize,
    ) -> anyhow::Result<ComponentAdjacency> {
        // Contract enforcement, not caller trust: endpoints are deduplicated
        // (a duplicate endpoint would double-count every edge) and the
        // component is unioned with the endpoints (the boundary invariant
        // "endpoints are component members" holds by construction).
        let mut endpoints: Vec<String> = endpoints.to_vec();
        endpoints.sort();
        endpoints.dedup();
        let mut component_owned: std::collections::HashSet<&str> =
            component.iter().map(|s| s.as_str()).collect();
        for ep in &endpoints {
            component_owned.insert(ep.as_str());
        }
        let component = &component_owned;
        let rtx = self.db.begin_read()?;
        let adj_in = rtx.open_table(ADJ_IN)?;
        let adj_out = rtx.open_table(ADJ_OUT)?;
        let mut result = ComponentAdjacency::default();
        for kind in EdgeKind::ALL {
            let icap = cap_in(kind);
            let ocap = cap_out(kind);
            if icap == 0 && ocap == 0 {
                continue;
            }
            for ep in &endpoints {
                let prefix = adj_key(project_id, kind, ep);
                if icap > 0 {
                    let mut accepted = 0usize;
                    for r in adj_in.range((prefix.as_str(), "")..)? {
                        let (k, v) = r?;
                        let (pfx, source_id) = k.value();
                        if pfx != prefix.as_str() {
                            break;
                        }
                        if component.contains(source_id) {
                            result.internal_skipped += 1;
                            continue; // internal wiring never consumes cap
                        }
                        if accepted >= icap {
                            result.truncated_in.push((kind.clone(), ep.clone()));
                            break;
                        }
                        let (weight, _ts) = decode_adj_value(v.value())?;
                        result.incoming.push((
                            source_id.to_string(),
                            kind.clone(),
                            weight,
                            ep.clone(),
                        ));
                        accepted += 1;
                    }
                }
                if ocap > 0 {
                    let mut accepted = 0usize;
                    for r in adj_out.range((prefix.as_str(), "")..)? {
                        let (k, v) = r?;
                        let (pfx, target_id) = k.value();
                        if pfx != prefix.as_str() {
                            break;
                        }
                        if component.contains(target_id) {
                            // Counted once, on the INCOMING view (every
                            // internal edge has its target inside too), so
                            // `internal_skipped` is an exact edge count, not
                            // a per-endpoint view count.
                            continue;
                        }
                        if accepted >= ocap {
                            result.truncated_out.push((kind.clone(), ep.clone()));
                            break;
                        }
                        let (weight, _ts) = decode_adj_value(v.value())?;
                        result.outgoing.push((
                            target_id.to_string(),
                            kind.clone(),
                            weight,
                            ep.clone(),
                        ));
                        accepted += 1;
                    }
                }
            }
        }
        Ok(result)
    }

    /// Resolve a file's component members with EXACT `file_path` equality
    /// applied BEFORE the cap (the generic `query_nodes` substring pre-filter
    /// lets suffix-colliding paths consume the budget, and requesting exactly
    /// `cap` made exactly-`cap` results read as truncated). Returns the sorted
    /// member node ids and whether an accepted member beyond `cap` exists.
    pub fn file_component_members(
        &self,
        project_id: &str,
        file_rel_path: &str,
        file_node_id: &str,
        cap: usize,
    ) -> anyhow::Result<(Vec<String>, bool)> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let mut members: Vec<String> = Vec::new();
        let mut truncated = false;
        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let node: Node = bincode::deserialize(v.value())?;
            if node.node_id == file_node_id || node.file_path.as_str() != file_rel_path {
                continue;
            }
            if members.len() >= cap {
                truncated = true;
                break;
            }
            members.push(node.node_id);
        }
        members.sort();
        members.dedup();
        Ok((members, truncated))
    }

    /// Batch node lookup in ONE read transaction. Replaces N+1 `get_node`
    /// calls (each opening its own transaction) when rendering a dependent
    /// list. Missing ids map to `None` so callers can tell "dangling edge"
    /// from "found".
    pub fn get_nodes(
        &self,
        project_id: &str,
        node_ids: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, Option<Node>>> {
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;
        let mut out = std::collections::HashMap::with_capacity(node_ids.len());
        for id in node_ids {
            if out.contains_key(id) {
                continue;
            }
            let key = format!("{project_id}\0{id}");
            let node = match nt.get(key.as_str())? {
                Some(v) => Some(bincode::deserialize(v.value())?),
                None => None,
            };
            out.insert(id.clone(), node);
        }
        Ok(out)
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

    /// All edges EXCEPT the statistical history kinds (TemporalCoupling,
    /// CoOccurrence). After git-history indexing those can outnumber
    /// structural edges 10:1 (pilot corpus: 1.3M temporal vs 113k structural) —
    /// a full-table scan per tool call stops scaling. Key layout is
    /// `project\0kind\0source\0target`, so per-kind prefix scans skip the
    /// temporal ranges entirely.
    /// Stream every edge of one kind through a visitor without
    /// materializing the list — the pilot corpus has 1.3M TemporalCoupling edges and
    /// collecting them costs hundreds of MB for aggregation that needs a
    /// running map.
    /// Point lookup of one edge's resolution confidence (TODO-12).
    /// Returns None when the edge is missing or carries no confidence.
    pub fn get_edge_confidence(
        &self,
        project_id: &str,
        kind: &EdgeKind,
        source_id: &str,
        target_id: &str,
    ) -> anyhow::Result<Option<f32>> {
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        let key = edge_key(project_id, kind, source_id, target_id);
        let Some(v) = et.get(key.as_str())? else {
            return Ok(None);
        };
        let e: Edge = bincode::deserialize(v.value())?;
        Ok(e.metadata
            .as_ref()
            .and_then(|m| m.get("confidence"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f32>().ok()))
    }

    /// Delete every edge of one kind for a project (EDGES + both adjacency
    /// tables). 16d: a git-history walk that dies mid-run leaves temporal
    /// increments behind; the rerun re-walks from scratch and double-counts.
    /// A fresh walk (no watermark) clears prior statistical edges first so
    /// crash-reruns are idempotent.
    pub fn delete_edges_of_kind(&self, project_id: &str, kind: &EdgeKind) -> anyhow::Result<usize> {
        const BATCH: usize = 5_000;
        let edge_prefix = format!("{project_id}\0{}\0", kind.as_str());
        let mut removed = 0usize;
        loop {
            let keys: Vec<String> = {
                let rtx = self.db.begin_read()?;
                let et = rtx.open_table(EDGES)?;
                let mut v = Vec::with_capacity(BATCH);
                for r in et.range(edge_prefix.as_str()..)? {
                    let (k, _) = r?;
                    if !k.value().starts_with(&edge_prefix) {
                        break;
                    }
                    v.push(k.value().to_string());
                    if v.len() >= BATCH {
                        break;
                    }
                }
                v
            };
            if keys.is_empty() {
                break;
            }
            let wtx = self.db.begin_write()?;
            {
                let mut et = wtx.open_table(EDGES)?;
                for k in &keys {
                    et.remove(k.as_str())?;
                }
            }
            wtx.commit()?;
            removed += keys.len();
        }

        // Adjacency mirrors: tuple keys whose first element is
        // "{pid}\0{kind}\0{node}" — prefix-match on the kind segment.
        let adj_prefix = format!("{project_id}\0{}\0", kind.as_str());
        for which in 0..2 {
            loop {
                let keys: Vec<(String, String)> = {
                    let rtx = self.db.begin_read()?;
                    let at = if which == 0 {
                        rtx.open_table(ADJ_OUT)?
                    } else {
                        rtx.open_table(ADJ_IN)?
                    };
                    let mut v = Vec::with_capacity(BATCH);
                    for r in at.range((adj_prefix.as_str(), "")..)? {
                        let (k, _) = r?;
                        let (pfx, other) = k.value();
                        if !pfx.starts_with(&adj_prefix) {
                            break;
                        }
                        v.push((pfx.to_string(), other.to_string()));
                        if v.len() >= BATCH {
                            break;
                        }
                    }
                    v
                };
                if keys.is_empty() {
                    break;
                }
                let wtx = self.db.begin_write()?;
                {
                    let mut at = if which == 0 {
                        wtx.open_table(ADJ_OUT)?
                    } else {
                        wtx.open_table(ADJ_IN)?
                    };
                    for (a, b) in &keys {
                        at.remove(&(a.as_str(), b.as_str()))?;
                    }
                }
                wtx.commit()?;
            }
        }
        Ok(removed)
    }

    /// Batched variant of [`Self::get_edge_confidence`]: one read
    /// transaction for the whole set. Blast radius checks confidence for
    /// every incoming edge of a target — per-edge transactions made the
    /// danger-zone sweep in produce_claude_md crawl.
    pub fn get_edge_confidences(
        &self,
        project_id: &str,
        queries: &[(EdgeKind, String, String)],
    ) -> anyhow::Result<Vec<Option<f32>>> {
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        let mut out = Vec::with_capacity(queries.len());
        for (kind, source_id, target_id) in queries {
            let key = edge_key(project_id, kind, source_id, target_id);
            let conf = match et.get(key.as_str())? {
                Some(v) => {
                    let e: Edge = bincode::deserialize(v.value())?;
                    e.metadata
                        .as_ref()
                        .and_then(|m| m.get("confidence"))
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f32>().ok())
                }
                None => None,
            };
            out.push(conf);
        }
        Ok(out)
    }

    pub fn fold_edges_by_kind(
        &self,
        project_id: &str,
        kind: EdgeKind,
        mut visit: impl FnMut(&Edge),
    ) -> anyhow::Result<()> {
        let prefix = format!("{project_id}\0{}\0", kind.as_str());
        let rtx = self.db.begin_read()?;
        let et = rtx.open_table(EDGES)?;
        for r in et.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let e: Edge = bincode::deserialize(v.value())?;
            visit(&e);
        }
        Ok(())
    }

    pub fn list_structural_edges(&self, project_id: &str) -> anyhow::Result<Vec<Edge>> {
        let mut out = Vec::new();
        for kind in EdgeKind::ALL {
            if matches!(kind, EdgeKind::TemporalCoupling | EdgeKind::CoOccurrence) {
                continue;
            }
            out.extend(self.list_edges_by_kind(project_id, kind.clone(), usize::MAX)?);
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

    /// Symbol-name lookup for the graph-reference tools.
    ///
    /// Unlike [`Self::query_nodes`], whose `name_pattern` is a SUBSTRING
    /// filter, this applies the exact/suffix match rule DURING the scan, so
    /// `limit` bounds the number of MATCHING nodes rather than the number of
    /// candidates inspected.
    ///
    /// The distinction is not cosmetic. `find_symbol_references("Worker")`
    /// reported "No graph symbol found" against a live 26k-function index
    /// while `resolve_id` confirmed the node existed and recommended that
    /// very call: 50 substring neighbours (`FuzzWorkerProtocol`, …) filled
    /// the prefetch window before the scan reached the exact node, and the
    /// caller's post-hoc filter then had nothing to keep. Raising the cap
    /// only moves the threshold — the cap must apply after matching.
    ///
    /// Match ladder (identical to the one callers applied post-hoc): exact
    /// name, `.`-qualified suffix, `::`-qualified suffix, or a `:`-suffixed
    /// `node_id`. `file_scope_prefix`, when set, keeps only nodes whose file
    /// path starts with it; empty paths are never filtered out.
    pub fn query_nodes_by_symbol_name(
        &self,
        project_id: &str,
        symbol_name: &str,
        file_scope_prefix: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Node>> {
        let mut out = Vec::new();
        let needle = symbol_name.to_lowercase();
        if needle.is_empty() || limit == 0 {
            return Ok(out);
        }
        let dot_suffix = format!(".{needle}");
        let colon_suffix = format!("::{needle}");
        let id_suffix = format!(":{needle}");

        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let nt = rtx.open_table(NODES)?;

        for r in nt.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let n: Node = bincode::deserialize(v.value())?;

            let name_lower = n.name.to_lowercase();
            let matches = name_lower == needle
                || name_lower.ends_with(&dot_suffix)
                || name_lower.ends_with(&colon_suffix)
                || n.node_id.to_lowercase().ends_with(&id_suffix);
            if !matches {
                continue;
            }

            if let Some(scope) = file_scope_prefix {
                let fp = n.file_path.as_str();
                if !fp.is_empty() && !fp.starts_with(scope) {
                    continue;
                }
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

        // A bare project-relative path is a legitimate identifier for a file
        // node — callers pass one to ast_dependency_graph / trace_data_flow —
        // but file nodes carry the BASENAME in `name` and the full path only
        // in `node_id`/`file_path`, so step 2's exact-name match never fires
        // for one. Resolve it against `file:{path}` here.
        //
        // Without this the input reached step 4, whose `split('.').next_back()`
        // yields the EXTENSION for a path ("ml"), matching every file of that
        // type: an exact, existing `.ml` path was reported as "ambiguous
        // (44 matches)" against unrelated bench kernels.
        let looks_like_path = input.contains('/') || input.contains('\\');
        if looks_like_path {
            let normalized = input.replace('\\', "/");
            if let Some(node) = self.get_node(project_id, &format!("file:{normalized}"))? {
                return Ok(ResolveResult::Unique(node));
            }
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

        // Step 4: short-name fallback, for DOTTED SYMBOL NAMES only
        // (`Namespace.Class.Method` -> `Method`).
        //
        // Deliberately skipped for path-shaped input: there the terminal `.`
        // segment is the file extension, so this would "resolve" every file
        // sharing it. A caller who mistypes a path must get NotFound, not an
        // ambiguity list of 40 unrelated same-extension files.
        let short = input.split('.').next_back().unwrap_or(input);
        //
        // The match rule applies DURING the scan (`query_nodes_by_symbol_name`,
        // the same fix `find_symbol_references` needed): a 50-node SUBSTRING
        // window let a big project's real candidates fall outside it — golden
        // `ox_impact_4` ("GetByID in the projekt DAL") never saw
        // `_grunddata.projekt.GetByID` in the ambiguity list, only four
        // `_ata.*` names, so the question's qualifier had nothing to narrow.
        if !looks_like_path
            && let Ok(nodes) = self.query_nodes_by_symbol_name(project_id, short, None, 500)
        {
            let suffix = format!(".{short}");
            let exact_short: Vec<Node> = nodes
                .into_iter()
                .filter(|n| node_type.is_none_or(|t| n.node_type == t))
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

        let computed = self.get_or_compute_centrality(project_id, generation)?;
        Ok(computed.get(node_id).copied().unwrap_or(0.0))
    }

    /// Cached PageRank for a whole project generation: cache hit returns the
    /// stored map; miss computes, persists (best-effort), and returns it.
    /// TODO-41: callers (blast radius, reranking) previously recomputed
    /// PageRank on every request and the cache was never written.
    pub fn get_or_compute_centrality(
        &self,
        project_id: &str,
        generation: u64,
    ) -> anyhow::Result<HashMap<String, f32>> {
        if let Some(cached) = self.get_cached_centrality(project_id, generation)? {
            return Ok(cached);
        }
        let computed = compute_pagerank(self, project_id, generation)?;
        if let Err(e) = self.set_cached_centrality(project_id, generation, &computed.pagerank) {
            tracing::warn!(
                "centrality cache write failed for {project_id} gen {generation}: {e:#}"
            );
        }
        Ok(computed.pagerank)
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

    /// Global purge with the namespace retention policy read as "anything
    /// not at `active_generation` is stale" (KeepLatestOnly). Correct ONLY
    /// when every file was just re-indexed at `active_generation` (a full
    /// index). After INCREMENTAL updates use [`Self::purge_generations_below`].
    pub fn purge_old_generations(
        &self,
        project_id: &str,
        active_generation: u64,
    ) -> anyhow::Result<()> {
        self.purge_where(project_id, &|namespace: &str, generation: u64| {
            match engram_core::get_policy(namespace) {
                Ok(policy) => match policy.retention {
                    engram_core::NamespaceRetention::KeepLatestOnly => {
                        generation != active_generation
                    }
                    engram_core::NamespaceRetention::KeepLastGenerations(n_keep) => {
                        let min_keep = active_generation.saturating_sub(n_keep as u64 - 1);
                        generation < min_keep
                    }
                    engram_core::NamespaceRetention::KeepForever => false,
                },
                Err(_) => false,
            }
        })
        .map(|_| ())
    }

    /// Global purge baselined on the LAST FULL INDEX generation: only nodes
    /// and edges OLDER than `baseline` are stale. Incremental updates write
    /// at generations ABOVE the last full index (the graph has no
    /// copy-forward), so a `!= baseline` reading — what
    /// [`Self::purge_old_generations`] does — deleted every incrementally
    /// re-indexed file's nodes at each GC tick. Returns (nodes, edges)
    /// removed.
    pub fn purge_generations_below(
        &self,
        project_id: &str,
        baseline: u64,
    ) -> anyhow::Result<(usize, usize)> {
        self.purge_where(project_id, &|namespace: &str, generation: u64| {
            match engram_core::get_policy(namespace) {
                Ok(policy) => match policy.retention {
                    engram_core::NamespaceRetention::KeepLatestOnly => generation < baseline,
                    engram_core::NamespaceRetention::KeepLastGenerations(n_keep) => {
                        let min_keep = baseline.saturating_sub(n_keep as u64 - 1);
                        generation < min_keep
                    }
                    engram_core::NamespaceRetention::KeepForever => false,
                },
                Err(_) => false,
            }
        })
    }

    /// Shared purge core: remove every node and edge of `project_id` for
    /// which `is_stale(namespace, generation)` holds, in batches, keeping
    /// the adjacency tables consistent. Returns (nodes, edges) removed.
    fn purge_where(
        &self,
        project_id: &str,
        is_stale: &dyn Fn(&str, u64) -> bool,
    ) -> anyhow::Result<(usize, usize)> {
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
                if is_stale(&n.namespace, n.generation) {
                    keys.push(k.value().to_string());
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
                if is_stale(&e.namespace, e.generation) {
                    keys.push(k.value().to_string());
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
        Ok((node_keys_to_remove.len(), edge_keys_to_remove.len()))
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

    /// TODO-12: record how a placeholder edge was rewired so consumers can
    /// weigh it. Merges into existing metadata, preserving extractor keys.
    fn stamp_resolution(edge: &mut Edge, method: &str, confidence: f32) {
        let mut obj = match edge.metadata.take() {
            Some(serde_json::Value::Object(o)) => o,
            Some(other) => {
                // Non-object metadata is unexpected; preserve it under a key.
                let mut o = serde_json::Map::new();
                o.insert("legacy_metadata".into(), other);
                o
            }
            None => serde_json::Map::new(),
        };
        obj.insert(
            "resolution".into(),
            serde_json::Value::String(method.to_string()),
        );
        obj.insert(
            "confidence".into(),
            serde_json::Value::String(format!("{confidence:.2}")),
        );
        edge.metadata = Some(serde_json::Value::Object(obj));
    }

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
        // node_id → name (for suffix-qualified matching)
        let mut node_names: HashMap<String, String> = HashMap::new();
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
                node_names.insert(node.node_id.clone(), node.name.clone());

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
            // Per-kind prefix scans, skipping statistical history kinds:
            // TemporalCoupling/CoOccurrence endpoints are always concrete
            // file ids, never `::` placeholders, and after git-history
            // indexing they dominate the table (pilot corpus: 1.3M vs 113k).
            for kind in EdgeKind::ALL {
                if matches!(kind, EdgeKind::TemporalCoupling | EdgeKind::CoOccurrence) {
                    continue;
                }
                let kind_prefix = format!("{project_id}\0{}\0", kind.as_str());
                for r in et.range(kind_prefix.as_str()..)? {
                    let (k, v) = r?;
                    if !k.value().starts_with(&kind_prefix) {
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
        }

        let phase2_elapsed = phase2_start.elapsed();
        tracing::info!(
            "resolve_symbol_edges: collected unresolved edges project_id={} count={} elapsed_ms={}",
            project_id,
            unresolved.len(),
            phase2_elapsed.as_millis()
        );

        if unresolved.is_empty() {
            // Item 8: the route pass runs even when nothing is left to bind by name.
            return self.resolve_route_edges(project_id);
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
            // Defensive: Roslyn-emitted call targets can be signature-shaped
            // ("Ns.Cls.Save(Integer, String)") while node names are bare —
            // strip the parameter list before deriving name/terminal keys.
            let name = name.split('(').next().unwrap_or(name).trim_end();
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
                Self::stamp_resolution(&mut new_e, "post_exact_name", 0.85);
                updates.push((entry.old_key.clone(), new_e));
                continue;
            }

            // Step 2: metadata.fqn match
            if let Some(id) = by_metadata_fqn.get(name) {
                let mut new_e = entry.edge.clone();
                new_e.target_id = id.clone();
                Self::stamp_resolution(&mut new_e, "post_node_fqn", 0.9);
                updates.push((entry.old_key.clone(), new_e));
                continue;
            }

            // Step 2b: the EDGE's own metadata.fqn. WebForms event_wiring
            // edges carry the handler's full FQN there while the placeholder
            // holds only the short method name — and the same-file tiebreak
            // can't help because the control lives in the .aspx page while
            // the handler lives in the code-behind file.
            if let Some(edge_fqn) = entry
                .edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("fqn"))
                .and_then(|v| v.as_str())
                && edge_fqn != name
            {
                let resolved = match by_name.get(edge_fqn) {
                    Some(SymbolMatch::Unique(id)) => Some(id.clone()),
                    Some(SymbolMatch::Ambiguous(ids)) => resolve_ambiguous(ids, source_file),
                    None => by_metadata_fqn.get(edge_fqn).cloned(),
                };
                if let Some(target_id) = resolved {
                    let mut new_e = entry.edge.clone();
                    new_e.target_id = target_id;
                    Self::stamp_resolution(&mut new_e, "post_edge_fqn", 0.9);
                    updates.push((entry.old_key.clone(), new_e));
                    continue;
                }
            }

            // Step 2c: suffix-qualified match — the extracted name lacks a
            // namespace-alias prefix the node name carries
            // (_us.UserAccessObject.x vs UserAccessObject.x). Matching the
            // FULL dotted name as a .-anchored suffix keeps every qualified
            // segment and is far more trustworthy than the bare terminal
            // fallback below. Dual-spelling duplicates (same node NAME under
            // two file-path spellings) count as one symbol.
            let short = name.rsplit('.').next().unwrap_or(name);
            if name.contains('.')
                && let Some(candidates) = by_terminal.get(short)
            {
                let dot_name = format!(".{name}");
                let suffixed: Vec<&String> = candidates
                    .iter()
                    .filter(|cid| node_names.get(*cid).is_some_and(|n| n.ends_with(&dot_name)))
                    .collect();
                let all_same_name = suffixed.len() > 1
                    && suffixed
                        .windows(2)
                        .all(|w| node_names.get(w[0]) == node_names.get(w[1]));
                let picked = if suffixed.len() == 1 {
                    Some((suffixed[0].clone(), 0.8f32, "post_suffix_qualified"))
                } else if !suffixed.is_empty() {
                    let owned: Vec<String> = suffixed.iter().map(|s| (*s).clone()).collect();
                    resolve_ambiguous(&owned, source_file)
                        .map(|id| (id, 0.7, "post_suffix_samefile"))
                        .or_else(|| {
                            all_same_name
                                .then(|| (owned[0].clone(), 0.7, "post_suffix_dupspelling"))
                        })
                } else {
                    None
                };
                if let Some((target_id, conf, method)) = picked {
                    let mut new_e = entry.edge.clone();
                    new_e.target_id = target_id;
                    Self::stamp_resolution(&mut new_e, method, conf);
                    updates.push((entry.old_key.clone(), new_e));
                    continue;
                }
            }

            // Step 3: terminal segment fallback
            if let Some(candidates) = by_terminal.get(short) {
                if candidates.len() == 1 {
                    let mut new_e = entry.edge.clone();
                    new_e.target_id = candidates[0].clone();
                    Self::stamp_resolution(&mut new_e, "post_terminal_unique", 0.45);
                    updates.push((entry.old_key.clone(), new_e));
                } else if let Some(id) = resolve_ambiguous(candidates, source_file) {
                    let mut new_e = entry.edge.clone();
                    new_e.target_id = id;
                    Self::stamp_resolution(&mut new_e, "post_terminal_samefile", 0.6);
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

        let routed = self.resolve_route_edges(project_id)?;
        Ok(updates.len() + routed)
    }

    /// External audit round 2, item 8 — TS→API route resolution, the
    /// ImpactEngine one-hop slice.
    ///
    /// An `ApiCall` edge from a script names its server side only indirectly:
    /// a web endpoint with the method in metadata (`/api.asmx/getimg` →
    /// `ajax_target_method = getimg`), or a bare function-name literal that a
    /// broker dispatches (`api.ajax('athDeleteByID')` → `Case "athDeleteByID"`
    /// → `s = DeleteChangeRequest(qry)`, whose Calls edge carries
    /// `dispatch_key`). This pass adds the edge the callee/impact arms need —
    /// from the ENCLOSING client function (the callee arm walks function
    /// nodes; the file stays the fallback) to the serving method — stamped
    /// `route_dispatch` (broker arm), `route_method` (`<class>.<method>` of
    /// the endpoint's class), `route_unique` (the one method of that name) or
    /// `route_enclosing` (target already bound; only the source is lifted).
    /// Ambiguous targets are skipped: a wrong route is worse than none.
    /// Idempotent — edges are keyed by (kind, source, target).
    pub fn resolve_route_edges(&self, project_id: &str) -> anyhow::Result<usize> {
        let prefix = format!("{project_id}\0");
        let mut node_name: HashMap<String, String> = HashMap::new();
        let mut node_path: HashMap<String, String> = HashMap::new();
        let mut fn_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut fns_by_name: HashMap<String, Vec<String>> = HashMap::new();
        let mut fns_by_terminal: HashMap<String, Vec<String>> = HashMap::new();
        let mut fns_by_file: HashMap<String, Vec<(u32, u32, String)>> = HashMap::new();
        let mut dispatch: HashMap<String, Vec<String>> = HashMap::new();
        let mut exposes: HashMap<String, String> = HashMap::new();
        let mut api_calls: Vec<Edge> = Vec::new();
        {
            let rtx = self.db.begin_read()?;
            let nt = rtx.open_table(NODES)?;
            for r in nt.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let node: Node = bincode::deserialize(v.value())?;
                let path = node.file_path.as_str().replace('\\', "/");
                node_name.insert(node.node_id.clone(), node.name.clone());
                node_path.insert(node.node_id.clone(), path.clone());
                if !matches!(
                    node.node_type.as_str(),
                    "function" | "method" | "sub" | "procedure"
                ) {
                    continue;
                }
                fn_ids.insert(node.node_id.clone());
                let lower = node.name.to_lowercase();
                if let Some(t) = lower.rsplit('.').next() {
                    if t != lower {
                        fns_by_terminal
                            .entry(t.to_string())
                            .or_default()
                            .push(node.node_id.clone());
                    }
                }
                fns_by_name
                    .entry(lower)
                    .or_default()
                    .push(node.node_id.clone());
                if node.start_line > 0 {
                    fns_by_file.entry(path).or_default().push((
                        node.start_line,
                        node.end_line.max(node.start_line),
                        node.node_id.clone(),
                    ));
                }
            }
            let et = rtx.open_table(EDGES)?;
            for kind in EdgeKind::ALL {
                let is_exposes = kind.as_str().starts_with("exposes_");
                if !(is_exposes || matches!(kind, EdgeKind::Calls | EdgeKind::ApiCall)) {
                    continue;
                }
                let kind_prefix = format!("{project_id}\0{}\0", kind.as_str());
                for r in et.range(kind_prefix.as_str()..)? {
                    let (k, v) = r?;
                    if !k.value().starts_with(&kind_prefix) {
                        break;
                    }
                    let e: Edge = bincode::deserialize(v.value())?;
                    let meta_str = |key: &str| -> Option<String> {
                        e.metadata
                            .as_ref()
                            .and_then(|m| m.get(key))
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                    };
                    if is_exposes {
                        let class = match e.target_id.strip_prefix("::") {
                            Some(placeholder) => placeholder.to_string(),
                            None => node_name.get(&e.target_id).cloned().unwrap_or_default(),
                        };
                        let simple = class.rsplit('.').next().unwrap_or("").to_lowercase();
                        if !simple.is_empty() {
                            exposes.insert(e.source_id.clone(), simple);
                        }
                    } else if matches!(kind, EdgeKind::Calls) {
                        if let Some(key) = meta_str("dispatch_key") {
                            // Live r43: `DeleteChangeRequest` names a page
                            // property, a helper and the implementation, so
                            // by-name binding left the arm's call a placeholder.
                            // A VB unqualified call inside `Partial Class api`
                            // binds to a member of `api` first; properties never
                            // take a call.
                            let bound = if fn_ids.contains(&e.target_id) {
                                Some(e.target_id.clone())
                            } else {
                                e.target_id.strip_prefix("::").and_then(|name| {
                                    let name_l =
                                        name.rsplit('.').next().unwrap_or(name).to_lowercase();
                                    let mut cands: Vec<String> =
                                        fns_by_name.get(&name_l).cloned().unwrap_or_default();
                                    for id in fns_by_terminal.get(&name_l).into_iter().flatten() {
                                        if !cands.contains(id) {
                                            cands.push(id.clone());
                                        }
                                    }
                                    if cands.len() > 1 {
                                        if let Some(prefix) = node_name
                                            .get(&e.source_id)
                                            .and_then(|n| n.rsplit_once('.'))
                                            .map(|(c, _)| format!("{}.", c.to_lowercase()))
                                        {
                                            let same: Vec<String> = cands
                                                .iter()
                                                .filter(|id| {
                                                    node_name.get(*id).is_some_and(|n| {
                                                        n.to_lowercase().starts_with(&prefix)
                                                    })
                                                })
                                                .cloned()
                                                .collect();
                                            if same.len() == 1 {
                                                cands = same;
                                            }
                                        }
                                    }
                                    (cands.len() == 1).then(|| cands.remove(0))
                                })
                            };
                            if let Some(id) = bound {
                                dispatch.entry(key.to_lowercase()).or_default().push(id);
                            }
                        }
                    } else if meta_str("ajax_target_method").is_some() {
                        api_calls.push(e);
                    }
                }
            }
        }

        let unique = |ids: Option<&Vec<String>>| -> Option<String> {
            match ids {
                Some(v) if v.len() == 1 => Some(v[0].clone()),
                _ => None,
            }
        };
        let now = now_ms();
        let mut new_edges: Vec<Edge> = Vec::new();
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let (mut ambiguous, mut unbound) = (0usize, 0usize);
        for e in &api_calls {
            let meta = |key: &str| -> Option<String> {
                e.metadata
                    .as_ref()
                    .and_then(|m| m.get(key))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            let Some(method) = meta("ajax_target_method") else {
                continue;
            };
            let method_l = method.to_lowercase();
            let name_route = meta("ajax_transport").as_deref() == Some("api_name");
            // route_dispatch precedence: for a name route the broker's arm is
            // the authority — a symbol that merely shares the API name
            // (bound by name before this pass) does not serve the call.
            let arm = if name_route {
                dispatch.get(&method_l)
            } else {
                None
            };
            let (target, how, conf) = if let Some(ids) = arm {
                if ids.len() != 1 {
                    ambiguous += 1;
                    continue;
                }
                (ids[0].clone(), "route_dispatch", 0.85f32)
            } else if fn_ids.contains(&e.target_id) {
                (e.target_id.clone(), "route_enclosing", 0.9f32)
            } else if name_route {
                if let Some(id) = unique(fns_by_name.get(&method_l))
                    .or_else(|| unique(fns_by_terminal.get(&method_l)))
                {
                    (id, "route_unique", 0.6)
                } else {
                    if fns_by_terminal.get(&method_l).is_some_and(|v| v.len() > 1) {
                        ambiguous += 1;
                    } else {
                        unbound += 1;
                    }
                    continue;
                }
            } else {
                // web-method route: the endpoint's class, then a unique name
                let class = exposes.get(&e.target_id).cloned().or_else(|| {
                    node_name
                        .get(&e.target_id)
                        .and_then(|n| n.rsplit('/').next())
                        .and_then(|n| n.split('.').next())
                        .map(|s| s.to_lowercase())
                        .filter(|s| !s.is_empty())
                });
                let by_class = class
                    .as_ref()
                    .and_then(|c| unique(fns_by_name.get(&format!("{c}.{method_l}"))));
                if let Some(id) = by_class {
                    (id, "route_method", 0.8)
                } else if let Some(id) = unique(fns_by_terminal.get(&method_l)) {
                    (id, "route_unique", 0.6)
                } else {
                    if fns_by_terminal.get(&method_l).is_some_and(|v| v.len() > 1) {
                        ambiguous += 1;
                    } else {
                        unbound += 1;
                    }
                    continue;
                }
            };

            // lift the source to the enclosing client function when known
            let src_line: Option<u32> = meta("src_line").and_then(|s| s.parse().ok());
            let source = if fn_ids.contains(&e.source_id) {
                e.source_id.clone()
            } else {
                let path = node_path.get(&e.source_id).cloned().or_else(|| {
                    e.source_id
                        .strip_prefix("file:")
                        .map(|p| p.replace('\\', "/"))
                });
                match (path, src_line) {
                    (Some(p), Some(line)) => fns_by_file
                        .get(&p)
                        .and_then(|fns| {
                            fns.iter()
                                .filter(|(s, en, _)| *s <= line && line <= *en)
                                .min_by_key(|(s, en, _)| en - s)
                                .map(|(_, _, id)| id.clone())
                        })
                        .unwrap_or_else(|| e.source_id.clone()),
                    _ => e.source_id.clone(),
                }
            };
            if (source == e.source_id && target == e.target_id)
                || source == target
                || !seen.insert((source.clone(), target.clone()))
            {
                continue;
            }
            let mut edge = Edge {
                source_id: source,
                target_id: target,
                namespace: e.namespace.clone(),
                language: e.language.clone(),
                edge_kind: EdgeKind::ApiCall,
                weight: e.weight.max(1),
                generation: e.generation,
                metadata: e.metadata.clone(),
                updated_at_ms: now,
            };
            if let Some(serde_json::Value::Object(o)) = edge.metadata.as_mut() {
                o.insert(
                    "route_via".into(),
                    serde_json::Value::String(e.target_id.clone()),
                );
            }
            Self::stamp_resolution(&mut edge, how, conf);
            new_edges.push(edge);
        }
        if !new_edges.is_empty() {
            self.upsert_edges(project_id, &new_edges)?;
        }
        tracing::info!(
            "resolve_route_edges: project_id={} api_calls={} added={} ambiguous={} unbound={}",
            project_id,
            api_calls.len(),
            new_edges.len(),
            ambiguous,
            unbound
        );
        Ok(new_edges.len())
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

/// Result of a component-scoped one-hop sweep (`component_adjacency`).
/// Tuples are (other_node_id, kind, weight, endpoint_reached) — the endpoint
/// is which component member the edge touches, preserving the "via" path.
#[derive(Debug, Clone, Default)]
pub struct ComponentAdjacency {
    pub incoming: Vec<(String, EdgeKind, u32, String)>,
    pub outgoing: Vec<(String, EdgeKind, u32, String)>,
    /// (kind, endpoint) pairs where an ACCEPTED edge beyond the cap exists.
    pub truncated_in: Vec<(EdgeKind, String)>,
    pub truncated_out: Vec<(EdgeKind, String)>,
    /// OBSERVED edges skipped because the other end was inside the component.
    /// Exact only when no sweep was truncated: a sweep stops at its accepted-
    /// external cap, so internal edges ordered after that point are unvisited.
    pub internal_skipped: usize,
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

    /// Build `count` decoy nodes whose names CONTAIN `needle` without equalling
    /// it, with node_ids that sort BEFORE `exact_node_id` so a range scan meets
    /// them first — the real shape in MiniLangCompiler, where `Worker` is
    /// preceded by `FuzzWorkerProtocol` and friends.
    fn crowding_nodes(count: usize, needle: &str, exact_file: &str) -> Vec<Node> {
        let mut nodes: Vec<Node> = (0..count)
            .map(|i| {
                test_node(
                    &format!("sym:function:a{i:03}.vb:Fuzz{needle}Protocol{i}:1"),
                    "function",
                    &format!("Fuzz{needle}Protocol{i}"),
                    &format!("a{i:03}.vb"),
                )
            })
            .collect();
        nodes.push(test_node(
            &format!("sym:function:z{exact_file}:{needle}:6"),
            "function",
            needle,
            exact_file,
        ));
        nodes
    }

    /// Regression: `find_symbol_references("Worker")` reported "No graph symbol
    /// found" against a live index while `resolve_id` confirmed the node
    /// existed AND recommended that exact call.
    ///
    /// Cause: the handler asked `query_nodes` for up to N *substring* matches
    /// and then filtered them for exact/suffix equality. The cap applied
    /// BEFORE the filter, so N nodes merely containing "worker" consumed the
    /// whole window and the exact node was never returned. Raising N only
    /// moves the threshold; the cap has to apply to MATCHING nodes.
    #[test]
    fn exact_symbol_name_survives_substring_crowding() {
        let store = test_store();
        let pid = "p1";
        let nodes = crowding_nodes(60, "Worker", "z.ml");
        store.upsert_nodes(pid, &nodes).expect("seed nodes");

        // Old path: substring query capped at 50, then exact-filtered.
        let prefetched = store
            .query_nodes(pid, None, Some("Worker"), None, 50)
            .expect("query_nodes");
        let exact_after_cap = prefetched.iter().filter(|n| n.name == "Worker").count();

        // New path: the cap applies after matching.
        let matched = store
            .query_nodes_by_symbol_name(pid, "Worker", None, 50)
            .expect("query_nodes_by_symbol_name");

        assert!(
            matched.iter().any(|n| n.name == "Worker"),
            "exact 'Worker' must be found regardless of how many substring \
             neighbours precede it; got {:?}",
            matched.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
        assert_eq!(
            exact_after_cap, 0,
            "precondition: this fixture must actually crowd the old path out, \
             otherwise the test proves nothing"
        );
    }

    /// The suffix forms the handler accepts (`Ns.Worker`, `Ns::Worker`) must
    /// resolve too — MiniLang qualifies module-level functions, so a bare
    /// name has to reach a qualified node.
    #[test]
    fn symbol_name_lookup_matches_qualified_suffixes() {
        let store = test_store();
        let pid = "p1";
        let nodes = vec![
            test_node(
                "sym:function:m.ml:Mod.Launch:6",
                "function",
                "Mod.Launch",
                "m.ml",
            ),
            test_node(
                "sym:function:n.vb:Ns::Launch:9",
                "function",
                "Ns::Launch",
                "n.vb",
            ),
            test_node(
                "sym:function:o.ml:Unrelated:1",
                "function",
                "Unrelated",
                "o.ml",
            ),
        ];
        store.upsert_nodes(pid, &nodes).expect("seed nodes");

        let matched = store
            .query_nodes_by_symbol_name(pid, "Launch", None, 50)
            .expect("query");
        let names: Vec<&str> = matched.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"Mod.Launch"), "dot-qualified: {names:?}");
        assert!(names.contains(&"Ns::Launch"), "colon-qualified: {names:?}");
        assert!(
            !names.contains(&"Unrelated"),
            "must not over-match: {names:?}"
        );
    }

    /// A substring neighbour must NOT be returned — the old handler filter
    /// rejected these, and pushing the filter down must not loosen it.
    #[test]
    fn symbol_name_lookup_rejects_mere_substring_neighbours() {
        let store = test_store();
        let pid = "p1";
        let nodes = crowding_nodes(3, "Worker", "z.ml");
        store.upsert_nodes(pid, &nodes).expect("seed nodes");

        let matched = store
            .query_nodes_by_symbol_name(pid, "Worker", None, 50)
            .expect("query");
        assert_eq!(
            matched.len(),
            1,
            "only the exact node qualifies; got {:?}",
            matched.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
        assert_eq!(matched[0].name, "Worker");
    }

    /// Mirrors how ingest actually builds file nodes (see `dreamer.rs`):
    /// `node_id` carries the full path, but `name` is the BASENAME. Using
    /// the full path as `name` here would make the exact-path test pass at
    /// step 2 and hide the very bug these tests exist for.
    fn file_node(path: &str) -> Node {
        let base = path.rsplit('/').next().unwrap_or(path);
        let mut n = test_node(&format!("file:{path}"), "file", base, path);
        n.language = "minilang".to_string();
        n
    }

    /// Regression: `ast_dependency_graph(entry="tests/…/x.ml")` — an exact,
    /// existing project-relative path — failed with "Symbol '…' is ambiguous
    /// (44 matches)" listing entirely unrelated files (`bench/kernels/
    /// collatz/kernel.ml`, …).
    ///
    /// Cause: a bare path matches no DIRECT_PREFIXES, so resolution fell to
    /// the short-name fallback, whose `input.split('.').next_back()` yields
    /// the EXTENSION for a path (`ml`). Every node whose name ends in `.ml`
    /// then "matched". The fallback exists for dotted symbol names
    /// (`Namespace.Class.Method` -> `Method`) and is meaningless for paths.
    #[test]
    fn file_path_input_resolves_to_the_file_node() {
        let store = test_store();
        let pid = "p1";
        let mut nodes: Vec<Node> = (0..40)
            .map(|i| file_node(&format!("bench/kernels/k{i:02}/kernel.ml")))
            .collect();
        nodes.push(file_node("tests/conformance/fibers/target.ml"));
        store.upsert_nodes(pid, &nodes).expect("seed");

        let got = store
            .resolve_symbol(pid, "tests/conformance/fibers/target.ml", None, None)
            .expect("resolve");
        match got {
            ResolveResult::Unique(n) => {
                assert_eq!(n.node_id, "file:tests/conformance/fibers/target.ml")
            }
            ResolveResult::Ambiguous(v) => panic!(
                "exact path must resolve uniquely, not to {} candidates: {:?}",
                v.len(),
                v.iter().map(|n| &n.node_id).collect::<Vec<_>>()
            ),
            ResolveResult::NotFound => panic!("exact path must resolve"),
        }
    }

    /// A path that does NOT exist must be NotFound — never "ambiguous with
    /// every file sharing its extension", which is what made the original
    /// error message useless.
    #[test]
    fn missing_file_path_does_not_degrade_to_extension_matching() {
        let store = test_store();
        let pid = "p1";
        let nodes: Vec<Node> = (0..40)
            .map(|i| file_node(&format!("bench/kernels/k{i:02}/kernel.ml")))
            .collect();
        store.upsert_nodes(pid, &nodes).expect("seed");

        let got = store
            .resolve_symbol(pid, "no/such/file.ml", None, None)
            .expect("resolve");
        match got {
            ResolveResult::NotFound => {}
            ResolveResult::Unique(n) => panic!("must not resolve to {}", n.node_id),
            ResolveResult::Ambiguous(v) => panic!(
                "a missing path must be NotFound, not ambiguous across {} \
                 same-extension files",
                v.len()
            ),
        }
    }

    /// The dotted short-name fallback must still work — it is the reason
    /// step 4 exists, and narrowing it to non-paths must not remove it.
    #[test]
    fn dotted_symbol_short_name_fallback_still_resolves() {
        let store = test_store();
        let pid = "p1";
        let nodes = vec![
            test_node(
                "sym:function:m.ml:Orchestrator.Worker:12",
                "function",
                "Orchestrator.Worker",
                "m.ml",
            ),
            test_node(
                "sym:function:m.ml:Unrelated:30",
                "function",
                "Unrelated",
                "m.ml",
            ),
        ];
        store.upsert_nodes(pid, &nodes).expect("seed");

        match store
            .resolve_symbol(pid, "Some.Other.Worker", None, None)
            .expect("resolve")
        {
            ResolveResult::Unique(n) => assert_eq!(n.name, "Orchestrator.Worker"),
            other => panic!("dotted fallback must still resolve, got {other:?}"),
        }
    }

    #[test]
    fn edges_touching_matches_full_scan_filter() {
        // Equivalence guard for the O(degree) rewrite: edges_touching must
        // return exactly the edges the old code found by full-scanning
        // list_structural_edges and filtering on source|target == node.
        let store = test_store();
        let pid = "p1";
        let mk_edge =
            |src: &str, tgt: &str, kind: EdgeKind, meta: Option<serde_json::Value>| Edge {
                source_id: src.to_string(),
                target_id: tgt.to_string(),
                namespace: "memory".into(),
                language: "vb".into(),
                edge_kind: kind,
                weight: 1,
                generation: 1,
                metadata: meta,
                updated_at_ms: 0,
            };
        let edges = vec![
            mk_edge(
                "a",
                "x",
                EdgeKind::Dependency,
                Some(serde_json::json!({"dynamic_control": true})),
            ),
            mk_edge("b", "x", EdgeKind::ReadsState, None),
            mk_edge(
                "x",
                "c",
                EdgeKind::SqlCalls,
                Some(serde_json::json!({"table_inference_confidence": 0.5})),
            ),
            mk_edge("x", "d", EdgeKind::Contains, None),
            // Unrelated edge — must NOT appear.
            mk_edge("a", "b", EdgeKind::Dependency, None),
            // Temporal — excluded by both implementations.
            mk_edge("e", "x", EdgeKind::TemporalCoupling, None),
        ];
        store.upsert_edges(pid, &edges).expect("upsert");

        let mut fast: Vec<(String, String, String)> = store
            .edges_touching(pid, "x", 1000)
            .expect("edges_touching")
            .into_iter()
            .map(|e| (e.edge_kind.as_str().to_string(), e.source_id, e.target_id))
            .collect();
        fast.sort();
        let mut slow: Vec<(String, String, String)> = store
            .list_structural_edges(pid)
            .expect("full scan")
            .into_iter()
            .filter(|e| e.source_id == "x" || e.target_id == "x")
            .map(|e| (e.edge_kind.as_str().to_string(), e.source_id, e.target_id))
            .collect();
        slow.sort();
        assert_eq!(fast, slow, "O(degree) path must equal full-scan filter");
        assert_eq!(fast.len(), 4, "{fast:?}");

        // Metadata must survive the point-lookup path (incoming edge).
        let with_meta = store.edges_touching(pid, "x", 1000).unwrap();
        assert!(
            with_meta.iter().any(|e| e
                .metadata
                .as_ref()
                .is_some_and(|m| m.get("dynamic_control").is_some())),
            "incoming edge metadata must be preserved"
        );
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
                | EdgeKind::Implements
                | EdgeKind::TestOracle => all_set.contains(&ek),
            }
        };

        for ek in EdgeKind::ALL {
            assert!(
                check_variant(ek.clone()),
                "EdgeKind::{:?} is in ALL but not in the exhaustive match — update the test",
                ek
            );
        }

        let variant_count = 44;
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

    /// Regression guard for the pilot-corpus shape where a GridView's
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
            "BFS must terminate at a db_column endpoint — this is the pilot-corpus shape"
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

    #[test]
    fn test_oracle_edge_kind_round_trips() {
        assert_eq!(EdgeKind::TestOracle.as_str(), "test_oracle");
        assert_eq!(EdgeKind::parse("test_oracle"), Some(EdgeKind::TestOracle));
        assert!(
            EdgeKind::ALL.contains(&EdgeKind::TestOracle),
            "TestOracle must be in ALL or count-by-kind reporting silently omits it"
        );
    }

    // ── component_adjacency / file_component_members contract suite ─────────
    // The substrate contract: "first N+1 ACCEPTED external neighbors after
    // kind, direction, and component policy" — consumer tests do not replace
    // these (auditor: missing foundational tests).

    fn ca_edge(src: &str, dst: &str, kind: EdgeKind, weight: u32) -> Edge {
        Edge {
            source_id: src.to_string(),
            target_id: dst.to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            edge_kind: kind,
            weight,
            generation: 1,
            metadata: None,
            updated_at_ms: 1,
        }
    }

    fn ca_setup(edges: &[Edge]) -> GraphStore {
        let store = test_store();
        let mut ids: Vec<String> = Vec::new();
        for e in edges {
            ids.push(e.source_id.clone());
            ids.push(e.target_id.clone());
        }
        ids.sort();
        ids.dedup();
        let nodes: Vec<Node> = ids
            .iter()
            .map(|id| test_node(id, "function", id, "f.vb"))
            .collect();
        store.upsert_nodes("p", &nodes).expect("nodes");
        store.upsert_edges("p", edges).expect("edges");
        store
    }

    fn comp(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ca_incoming_and_outgoing_directions() {
        let t = "sym:t";
        let store = ca_setup(&[
            ca_edge("sym:a", t, EdgeKind::Calls, 1),
            ca_edge(t, "sym:b", EdgeKind::Calls, 1),
        ]);
        let r = store
            .component_adjacency("p", &[t.to_string()], &comp(&[t]), &|_| 10, &|_| 10)
            .unwrap();
        assert_eq!(r.incoming.len(), 1);
        assert_eq!(r.incoming[0].0, "sym:a");
        assert_eq!(r.outgoing.len(), 1);
        assert_eq!(r.outgoing[0].0, "sym:b");
    }

    #[test]
    fn ca_exact_cap_is_complete_cap_plus_one_truncates() {
        let t = "sym:t";
        let mut edges: Vec<Edge> = (0..3)
            .map(|i| ca_edge(&format!("sym:c{i}"), t, EdgeKind::Calls, 1))
            .collect();
        let store = ca_setup(&edges);
        let r = store
            .component_adjacency("p", &[t.to_string()], &comp(&[t]), &|_| 3, &|_| 0)
            .unwrap();
        assert_eq!(r.incoming.len(), 3);
        assert!(r.truncated_in.is_empty(), "exactly-at-cap must be complete");
        edges.push(ca_edge("sym:c3", t, EdgeKind::Calls, 1));
        let store = ca_setup(&edges);
        let r = store
            .component_adjacency("p", &[t.to_string()], &comp(&[t]), &|_| 3, &|_| 0)
            .unwrap();
        assert_eq!(r.incoming.len(), 3);
        assert_eq!(r.truncated_in.len(), 1, "cap+1 accepted must truncate");
    }

    #[test]
    fn ca_internal_before_cap() {
        // 5 internal sources sorting BEFORE 2 external ones, cap 2: internals
        // must not consume the budget; both externals returned, complete.
        let t = "sym:t";
        let mut edges: Vec<Edge> = (0..5)
            .map(|i| ca_edge(&format!("sym:a_int{i}"), t, EdgeKind::Calls, 9999))
            .collect();
        edges.push(ca_edge("sym:z_ext0", t, EdgeKind::Calls, 1));
        edges.push(ca_edge("sym:z_ext1", t, EdgeKind::Calls, 1));
        let store = ca_setup(&edges);
        let component = comp(&[
            "sym:t",
            "sym:a_int0",
            "sym:a_int1",
            "sym:a_int2",
            "sym:a_int3",
            "sym:a_int4",
        ]);
        let r = store
            .component_adjacency("p", &[t.to_string()], &component, &|_| 2, &|_| 0)
            .unwrap();
        let srcs: Vec<&str> = r.incoming.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(srcs, vec!["sym:z_ext0", "sym:z_ext1"]);
        assert!(
            r.truncated_in.is_empty(),
            "internal edges must not cause truncation"
        );
        assert_eq!(r.internal_skipped, 5);
    }

    #[test]
    fn ca_internal_after_external_truncation_is_observed_not_exact() {
        // Externals sort BEFORE the internal; cap 1 stops the sweep at the
        // second accepted external, so the internal after it is unvisited.
        // Contract: internal_skipped is OBSERVED, exact only when complete.
        let t = "sym:t";
        let edges = [
            ca_edge("sym:a_ext0", t, EdgeKind::Calls, 1),
            ca_edge("sym:b_ext1", t, EdgeKind::Calls, 1),
            ca_edge("sym:z_int0", t, EdgeKind::Calls, 1),
        ];
        let store = ca_setup(&edges);
        let component = comp(&["sym:t", "sym:z_int0"]);
        let r = store
            .component_adjacency("p", &[t.to_string()], &component, &|_| 1, &|_| 0)
            .unwrap();
        assert_eq!(r.incoming.len(), 1);
        assert_eq!(r.truncated_in.len(), 1);
        assert_eq!(
            r.internal_skipped, 0,
            "documented observed-count semantics: unvisited internals are not counted"
        );
    }

    #[test]
    fn ca_multiple_endpoints_and_kinds_with_via() {
        let store = ca_setup(&[
            ca_edge("sym:x", "sym:e1", EdgeKind::Calls, 1),
            ca_edge("sym:y", "sym:e2", EdgeKind::Imports, 1),
        ]);
        let eps = vec!["sym:e1".to_string(), "sym:e2".to_string()];
        let r = store
            .component_adjacency("p", &eps, &comp(&["sym:e1", "sym:e2"]), &|_| 10, &|_| 0)
            .unwrap();
        assert_eq!(r.incoming.len(), 2);
        let via_x = r.incoming.iter().find(|e| e.0 == "sym:x").unwrap();
        assert_eq!(via_x.3, "sym:e1", "endpoint reached must be preserved");
        let via_y = r.incoming.iter().find(|e| e.0 == "sym:y").unwrap();
        assert_eq!(via_y.3, "sym:e2");
        assert_eq!(via_y.1, EdgeKind::Imports);
    }

    #[test]
    fn ca_self_edge_is_internal() {
        let t = "sym:t";
        let store = ca_setup(&[ca_edge(t, t, EdgeKind::Calls, 1)]);
        let r = store
            .component_adjacency("p", &[t.to_string()], &comp(&[t]), &|_| 10, &|_| 10)
            .unwrap();
        assert!(r.incoming.is_empty());
        assert!(r.outgoing.is_empty());
        assert!(r.internal_skipped >= 1, "self-edge is internal wiring");
    }

    #[test]
    fn ca_duplicate_endpoints_do_not_double_count() {
        let t = "sym:t";
        let store = ca_setup(&[ca_edge("sym:a", t, EdgeKind::Calls, 1)]);
        let eps = vec![t.to_string(), t.to_string(), t.to_string()];
        let r = store
            .component_adjacency("p", &eps, &comp(&[t]), &|_| 10, &|_| 0)
            .unwrap();
        assert_eq!(
            r.incoming.len(),
            1,
            "duplicate endpoints must be deduplicated"
        );
    }

    #[test]
    fn ca_endpoint_outside_component_is_unioned_in() {
        // Contract enforcement: an endpoint missing from the component set is
        // treated as a member (its self-referential edges are internal).
        let t = "sym:t";
        let store = ca_setup(&[ca_edge(t, t, EdgeKind::Calls, 1)]);
        let r = store
            .component_adjacency(
                "p",
                &[t.to_string()],
                &std::collections::HashSet::new(),
                &|_| 10,
                &|_| 10,
            )
            .unwrap();
        assert!(
            r.incoming.is_empty(),
            "endpoint is unioned into the component"
        );
        assert!(r.internal_skipped >= 1);
    }

    #[test]
    fn ca_zero_cap_kind_is_skipped() {
        let t = "sym:t";
        let store = ca_setup(&[
            ca_edge("sym:a", t, EdgeKind::Calls, 1),
            ca_edge("file:h.sql", t, EdgeKind::TemporalCoupling, 1),
        ]);
        let r = store
            .component_adjacency(
                "p",
                &[t.to_string()],
                &comp(&[t]),
                &|k| if *k == EdgeKind::Calls { 10 } else { 0 },
                &|_| 0,
            )
            .unwrap();
        assert_eq!(r.incoming.len(), 1);
        assert_eq!(r.incoming[0].1, EdgeKind::Calls);
        assert!(
            r.truncated_in.is_empty(),
            "zero-cap kinds are skipped, not truncated"
        );
    }

    #[test]
    fn ca_stable_ordering_across_calls() {
        let t = "sym:t";
        let edges: Vec<Edge> = (0..20)
            .map(|i| ca_edge(&format!("sym:c{i:02}"), t, EdgeKind::Calls, (i % 5) as u32))
            .collect();
        let store = ca_setup(&edges);
        let a = store
            .component_adjacency("p", &[t.to_string()], &comp(&[t]), &|_| 50, &|_| 0)
            .unwrap();
        let b = store
            .component_adjacency("p", &[t.to_string()], &comp(&[t]), &|_| 50, &|_| 0)
            .unwrap();
        assert_eq!(
            a.incoming, b.incoming,
            "identical calls must return identical order"
        );
    }

    #[test]
    fn fcm_exact_path_beats_suffix_collision() {
        let store = test_store();
        let nodes = vec![
            test_node("file:a/b.vb", "file", "b.vb", "a/b.vb"),
            test_node("sym:a/b.vb:M", "function", "M", "a/b.vb"),
            test_node("file:xa/b.vb", "file", "b.vb", "xa/b.vb"),
            test_node("sym:xa/b.vb:N", "function", "N", "xa/b.vb"),
        ];
        store.upsert_nodes("p", &nodes).unwrap();
        let (members, trunc) = store
            .file_component_members("p", "a/b.vb", "file:a/b.vb", 100)
            .unwrap();
        assert_eq!(members, vec!["sym:a/b.vb:M".to_string()]);
        assert!(!trunc);
    }

    #[test]
    fn fcm_exact_cap_complete_cap_plus_one_truncates() {
        let store = test_store();
        let mut nodes = vec![test_node("file:f.vb", "file", "f.vb", "f.vb")];
        for i in 0..3 {
            nodes.push(test_node(
                &format!("sym:f.vb:M{i}"),
                "function",
                "M",
                "f.vb",
            ));
        }
        store.upsert_nodes("p", &nodes).unwrap();
        let (members, trunc) = store
            .file_component_members("p", "f.vb", "file:f.vb", 3)
            .unwrap();
        assert_eq!(members.len(), 3);
        assert!(!trunc, "exactly-at-cap must be complete");
        let (members, trunc) = store
            .file_component_members("p", "f.vb", "file:f.vb", 2)
            .unwrap();
        assert_eq!(members.len(), 2);
        assert!(trunc, "cap+1 accepted member must truncate");
    }
}

#[cfg(test)]
mod purge_baseline_tests {
    //! Incremental updates write nodes at generations ABOVE the last full
    //! index; a global purge must only remove what is OLDER than that
    //! baseline. (`purge_old_generations` treats `!= baseline` as stale,
    //! which deleted every incrementally re-indexed file hourly.)
    use super::*;
    use engram_core::RelPath;

    fn store() -> GraphStore {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("g.redb");
        let s = GraphStore::open(&path).expect("open");
        std::mem::forget(tmp);
        s
    }

    fn node(id: &str, generation: u64) -> Node {
        Node {
            node_id: id.into(),
            node_type: "function".into(),
            name: id.into(),
            namespace: "memory".into(),
            language: "vbnet".into(),
            file_path: RelPath::new("a.vb"),
            start_line: 1,
            end_line: 2,
            generation,
            metadata: None,
        }
    }

    fn edge(src: &str, dst: &str, generation: u64) -> Edge {
        Edge {
            source_id: src.into(),
            target_id: dst.into(),
            namespace: "memory".into(),
            language: "vbnet".into(),
            edge_kind: EdgeKind::Calls,
            weight: 1,
            generation,
            metadata: None,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn purge_below_baseline_keeps_generations_at_or_above_it() {
        let s = store();
        s.upsert_nodes("p", &[node("full", 1), node("incr", 5)])
            .unwrap();
        s.upsert_edges("p", &[edge("incr", "full", 5)]).unwrap();

        let (n, e) = s.purge_generations_below("p", 1).unwrap();
        assert_eq!((n, e), (0, 0), "nothing is older than the full index");
        assert!(s.get_node("p", "full").unwrap().is_some());
        assert!(
            s.get_node("p", "incr").unwrap().is_some(),
            "incremental node deleted"
        );
        assert_eq!(
            s.find_incoming_edges_with_kind("p", Some(EdgeKind::Calls), "full", 10)
                .unwrap()
                .len(),
            1,
            "incremental edge deleted"
        );
    }

    #[test]
    fn purge_below_baseline_removes_only_older_generations() {
        let s = store();
        s.upsert_nodes("p", &[node("full", 1), node("incr", 5)])
            .unwrap();
        s.upsert_edges("p", &[edge("full", "incr", 1), edge("incr", "full", 5)])
            .unwrap();

        // A later FULL reindex landed at gen 5: only gen-1 leftovers are stale.
        let (n, e) = s.purge_generations_below("p", 5).unwrap();
        assert_eq!((n, e), (1, 1));
        assert!(s.get_node("p", "full").unwrap().is_none());
        assert!(s.get_node("p", "incr").unwrap().is_some());
        assert!(
            s.find_incoming_edges_with_kind("p", Some(EdgeKind::Calls), "incr", 10)
                .unwrap()
                .is_empty(),
            "the gen-1 edge must go with its generation"
        );
    }
}
