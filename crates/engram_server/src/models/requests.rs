use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Validated enum types (fail-closed at JSON deserialization) ────────────────
//
// These replace String fields that previously accepted arbitrary values and
// silently coerced unknown inputs to a default.  With enum types, serde rejects
// unknown values at the request boundary — before any handler code runs —
// producing a clear deserialization error that names the bad value.

/// Full-text search mode. Unknown values are rejected at the JSON boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FtsMode {
    /// Exact phrase match (default).
    #[default]
    Strict,
    /// Any token match.
    Loose,
    /// Regular-expression match.
    Regex,
}

impl FtsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Loose => "loose",
            Self::Regex => "regex",
        }
    }
}

/// Target modern framework for scaffold/dossier generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TargetStack {
    #[default]
    Blazor,
    React,
    Angular,
}

impl TargetStack {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blazor => "blazor",
            Self::React => "react",
            Self::Angular => "angular",
        }
    }
}

/// Graph traversal direction.  `"incoming"` and `"outgoing"` are accepted as
/// aliases for `"in"` and `"out"` respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Follow edges pointing at the node (default).
    #[default]
    #[serde(alias = "incoming")]
    In,
    /// Follow edges originating from the node.
    #[serde(alias = "outgoing")]
    Out,
    /// Follow edges in both directions.
    Both,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Both => "both",
        }
    }
}

/// Test framework for characterization test generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TestFramework {
    #[default]
    NUnit,
    XUnit,
    MSTest,
}

impl TestFramework {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NUnit => "nunit",
            Self::XUnit => "xunit",
            Self::MSTest => "mstest",
        }
    }
}

/// Minimum severity threshold for hazard/sync-hazard filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MinSeverity {
    #[default]
    Medium,
    High,
    Critical,
}

impl MinSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// ENG-AUD-2026-EXH-P1-0001: Project type — replaces the stringly-typed
/// `project_type: String` field.  Unknown values are rejected at the JSON
/// deserialization boundary before any handler code runs.
///
/// Aliases accept the existing case-insensitive variant spellings so existing
/// MCP clients continue to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectType {
    /// ASP.NET WebForms — C# backend.
    /// Aliases: `dotnetwebformscs`, `webforms_cs`, `webformscs`
    #[serde(
        alias = "dotnetwebformscs",
        alias = "webforms_cs",
        alias = "webformscs",
        alias = "aspnet_webforms_cs",
        alias = "aspnet_webformscs"
    )]
    DotnetWebformsCs,
    /// ASP.NET WebForms — VB.NET backend.
    /// Aliases: `dotnetwebformsvb`, `webforms_vb`, `webformsvb`
    #[serde(
        alias = "dotnetwebformsvb",
        alias = "webforms_vb",
        alias = "webformsvb",
        alias = "aspnet_webforms_vb",
        alias = "aspnet_webformsvb"
    )]
    DotnetWebformsVb,
    /// General-purpose project: indexes all common source extensions.
    /// Use this when the project type does not match a known specialisation.
    #[serde(alias = "general_purpose", alias = "other")]
    General,
    /// Rust projects.
    #[serde(alias = "rustlang", alias = "cargo")]
    Rust,
    /// C# projects (non-WebForms focused indexing profile).
    #[serde(alias = "c#", alias = "cs", alias = "c_sharp", alias = "dotnet_csharp")]
    CSharp,
    /// C++ projects.
    #[serde(alias = "c++", alias = "cxx")]
    Cpp,
    /// C projects.
    #[serde(alias = "ansi_c")]
    C,
}

impl ProjectType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DotnetWebformsCs => "dotnet_webforms_cs",
            Self::DotnetWebformsVb => "dotnet_webforms_vb",
            Self::General => "general",
            Self::Rust => "rust",
            Self::CSharp => "csharp",
            Self::Cpp => "cpp",
            Self::C => "c",
        }
    }

    /// Parse project type values loaded from persisted registry records.
    ///
    /// Accepts historical aliases used before typed `ProjectType` validation
    /// was introduced so older records can still be opened and normalized.
    pub fn from_registry_str(raw: &str) -> Option<Self> {
        let v = raw.trim();
        if v.is_empty() {
            return None;
        }

        if [
            "dotnetwebformscs",
            "dotnet_webforms_cs",
            "webforms_cs",
            "webformscs",
            "aspnet_webforms_cs",
            "aspnet_webformscs",
        ]
        .iter()
        .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::DotnetWebformsCs)
        } else if [
            "dotnetwebformsvb",
            "dotnet_webforms_vb",
            "webforms_vb",
            "webformsvb",
            "aspnet_webforms_vb",
            "aspnet_webformsvb",
        ]
        .iter()
        .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::DotnetWebformsVb)
        } else if ["general", "general_purpose", "other"]
            .iter()
            .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::General)
        } else if ["rust", "rustlang", "cargo"]
            .iter()
            .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::Rust)
        } else if ["csharp", "c#", "cs", "c_sharp", "dotnet_csharp"]
            .iter()
            .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::CSharp)
        } else if ["cpp", "c++", "cxx"]
            .iter()
            .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::Cpp)
        } else if ["c", "ansi_c"].iter().any(|x| v.eq_ignore_ascii_case(x)) {
            Some(Self::C)
        } else {
            None
        }
    }
}

// -------------------- Default value functions --------------------

pub fn default_true() -> bool {
    true
}
pub fn default_top_k() -> usize {
    10
}
pub fn default_max_commits() -> usize {
    10_000
}
pub fn default_namespace_memory() -> String {
    "memory".to_string()
}
pub fn default_max_content_chars() -> usize {
    1200
}
pub fn default_priority() -> i32 {
    5
}
pub fn default_direction_in() -> Direction {
    Direction::In
}
pub fn default_direction_both() -> Direction {
    Direction::Both
}
pub fn default_limit_5() -> usize {
    5
}
pub fn default_limit_50() -> usize {
    50
}
pub fn default_limit_100() -> usize {
    100
}
pub fn default_symbol_boost() -> f32 {
    0.03
}
pub fn default_max_hops() -> usize {
    2
}
pub fn default_min_freq() -> usize {
    5
}
pub fn default_max_pairs() -> usize {
    10
}
pub fn default_max_depth_3() -> u8 {
    3
}
pub fn default_max_depth_10() -> u8 {
    10
}
pub fn default_diff_limit() -> usize {
    10
}
pub fn default_max_clusters() -> usize {
    8
}
pub fn default_min_freq_3() -> u32 {
    3
}
pub fn default_boundary_timeout_secs() -> u64 {
    120
}
pub fn default_graph_hop_depth_1() -> usize {
    1
}
pub fn default_content_preview_chars() -> usize {
    400
}
pub fn default_content_preview_800() -> usize {
    800
}
pub fn default_limit_200() -> usize {
    200
}
pub fn default_repair_scope() -> String {
    "full".to_string()
}
pub fn default_repair_max_commits() -> usize {
    500
}
pub fn default_direction_outgoing() -> Direction {
    Direction::Out
}
pub fn default_antipattern_action() -> String {
    "stats".to_string()
}
pub fn default_dream_max_clusters() -> usize {
    5
}
pub fn default_dream_min_edge_weight() -> u32 {
    2
}
pub fn default_dream_min_cluster_size() -> usize {
    3
}
pub fn default_dream_timeout_secs() -> u64 {
    60
}

pub const MAX_SEARCH_RESULTS: usize = 200;
pub const MAX_CONTENT_CHARS_PER_RESULT: usize = 20_000;
pub const MAX_GRAPH_HOPS: usize = 8;
pub const MAX_GIT_COMMITS: usize = 10_000;
pub const MAX_TEMPORAL_RESULTS: usize = 200;
pub const MAX_TEMPORAL_MIN_FREQUENCY: usize = 1_000;
pub const MAX_DREAM_PAIRS: usize = 500;
pub const MAX_DIFF_LIMIT: usize = 200;
pub const MAX_IMMUNE_TOP_K: usize = 200;
pub const MAX_SYMBOL_REFS: usize = 500;
pub const MAX_AST_DEPTH: usize = 12;
pub const MAX_ANTIPATTERN_RESULTS: usize = 200;
pub const MAX_TRACE_PATHS: usize = 100;
/// Maximum byte length of a SQL fragment passed to `validate_sql_fragment`.
/// Prevents memory exhaustion from unbounded `to_lowercase()` + regex passes.
pub const MAX_SQL_LENGTH: usize = 1_000_000;

// -------------------- Project lifecycle --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexProjectRequest {
    pub directory: String,
    pub project_name: String,
    pub project_type: ProjectType,
    #[serde(default = "default_true")]
    pub wait: bool,
    #[serde(default = "default_true")]
    pub dedupe_by_directory: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateProjectRequest {
    pub project_id: String,
    #[serde(default = "default_true")]
    pub wait: bool,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
    #[serde(default)]
    pub index_antipatterns: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectIdRequest {
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchProjectRequest {
    pub project_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

// -------------------- Repair --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepairProjectRequest {
    pub project_id: String,
    /// Repair scope: "full" (default), "graph_only", "tantivy_only", "vector_only".
    #[serde(default = "default_repair_scope")]
    pub scope: String,
    /// Wipe all data and re-index from scratch. Default: false.
    #[serde(default)]
    pub wipe_and_reindex: bool,
    /// Max git commits to replay during re-index (for history). Default: 500.
    #[serde(default = "default_repair_max_commits")]
    pub max_commits: usize,
    /// Index antipatterns during repair. Default: true.
    #[serde(default = "default_true")]
    pub index_antipatterns: bool,
}

// -------------------- Search --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchMemoryRequest {
    pub query: String,
    pub project_id: String,
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    #[serde(default = "default_top_k")]
    pub max_results: usize,
    #[serde(default = "default_true")]
    pub use_mmr: bool,
    /// Full-text search mode. Default: "strict".
    #[serde(default)]
    pub fts_mode: FtsMode,
    #[serde(default = "default_true")]
    pub include_content: bool,
    #[serde(default = "default_max_content_chars")]
    pub max_content_chars_per_result: usize,
    #[serde(default)]
    pub include_path_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_path_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub language_filters: Option<Vec<String>>,
    #[serde(default)]
    pub metadata_filter: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetChunkRequest {
    pub project_id: String,
    /// Per-instance document identity (required).
    pub doc_id: String,
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    #[serde(default)]
    pub inject_rules: bool,
    /// Logical slice type to filter content before returning.
    /// Values: "all" (default), "event_handlers", "ui_methods", "data_methods",
    /// "sql_queries", "state_access".
    #[serde(default)]
    pub logical_slice: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GraphSearchRequest {
    pub project_id: String,
    pub query: String,
    #[serde(default = "default_top_k")]
    pub max_results: usize,
    #[serde(default = "default_symbol_boost")]
    pub symbol_boost: f32,
    /// Namespace to search ("memory", "history", "antipattern"). Default: "memory".
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    /// Full-text search mode: "strict" (exact phrase), "loose" (any token), "regex". Default: "strict".
    #[serde(default)]
    pub fts_mode: FtsMode,
    /// Enable MMR reranking for diversity. Default: false.
    #[serde(default)]
    pub use_mmr: bool,
    /// Graph neighbor expansion depth (1-4). Default: 1.
    #[serde(default = "default_graph_hop_depth_1")]
    pub hop_depth: usize,
    /// Include content snippet preview in results. Default: false.
    #[serde(default)]
    pub include_content: bool,
    /// Max characters per content preview. Default: 400.
    #[serde(default = "default_content_preview_chars")]
    pub max_content_chars: usize,
    /// Filter by edge kinds for graph expansion (e.g. ["dependency", "imports"]). Default: all expansion kinds.
    #[serde(default)]
    pub expansion_edge_kinds: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindSymbolReferencesRequest {
    pub symbol_name: String,
    pub project_id: String,
    /// Maximum number of incoming references to return per symbol. Default: 200.
    #[serde(default = "default_limit_200")]
    pub max_incoming: usize,
    /// Maximum number of outgoing dependencies to return per edge kind. Default: 50.
    #[serde(default = "default_limit_50")]
    pub max_outgoing_per_kind: usize,
    /// Filter by specific edge kinds (e.g. ["dependency", "imports"]). Default: all kinds.
    #[serde(default)]
    pub edge_kind_filter: Option<Vec<String>>,
    /// Filter references to files under this path prefix.
    #[serde(default)]
    pub file_scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeErrorStackRequest {
    pub traceback: String,
    pub project_id: String,
}

// -------------------- Memory bank --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateMemoryBankRequest {
    pub project_id: String,
    #[serde(default)]
    pub section_id: Option<String>,
    pub section: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemorySectionRequest {
    pub project_id: String,
    pub section: String,
}

// -------------------- Repo rules --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddRepoRuleRequest {
    pub project_id: String,
    pub file_pattern: String,
    pub rule_text: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default)]
    pub rule_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteRepoRuleRequest {
    pub project_id: String,
    pub rule_id: String,
}

// -------------------- Graph --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryGraphNodesRequest {
    pub project_id: String,
    #[serde(default)]
    pub node_type: String,
    #[serde(default)]
    pub name_pattern: String,
    #[serde(default)]
    pub file_path: String,
    #[serde(default = "default_limit_100")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindReferencesRequest {
    pub project_id: String,
    pub node_id: String,
    #[serde(default)]
    pub edge_kind: Option<String>,
    #[serde(default = "default_direction_in")]
    pub direction: Direction,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraverseGraphRequest {
    pub project_id: String,
    pub node_id: String,
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
    #[serde(default)]
    pub edge_kinds: Option<Vec<String>>,
    #[serde(default = "default_direction_both")]
    pub direction: Direction,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImpactAnalysisRequest {
    pub project_id: String,
    pub file_path: Option<String>,
    pub symbol_fqn: Option<String>,
    #[serde(default = "default_limit_50")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetTableSchemaRequest {
    pub project_id: String,
    pub table_name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceStateUsageRequest {
    pub project_id: String,
    pub state_type: String,
    pub state_key: String,
    #[serde(default = "default_top_k")]
    pub limit: usize,
}

impl TraceStateUsageRequest {
    /// MCP1: clamp limit to MAX_SEARCH_RESULTS to prevent resource amplification.
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_SEARCH_RESULTS)
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceUiActionRequest {
    pub project_id: String,
    pub query: String,
    #[serde(default = "default_max_depth_3")]
    pub max_depth: u8,
    #[serde(default = "default_limit_5")]
    pub max_paths: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceUiEventRequest {
    pub project_id: String,
    pub page_path: String,
    pub control_id: Option<String>,
    pub handler_fqn: Option<String>,
    #[serde(default = "default_max_depth_10")]
    pub max_hops: u8,
    #[serde(default = "default_limit_5")]
    pub max_paths: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportCapturePackRequest {
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetUiBlueprintRequest {
    pub project_id: String,
    /// Project-relative path to the .aspx, .ascx, .Designer.vb, or .Designer.cs file.
    pub file_path: String,
}

// -------------------- Git --------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GitHistoryMode {
    /// Walk newer commits at HEAD that postdate the last_oid.
    Forward,
    /// Walk older commits from oldest_indexed_oid backwards through history.
    Backfill,
    /// Run Forward then Backfill until max_commits is exhausted.
    #[default]
    Both,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IndexGitHistoryRequest {
    pub project_id: String,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
    #[serde(default)]
    pub index_antipatterns: bool,
    #[serde(default)]
    pub mode: Option<GitHistoryMode>,
    #[serde(default = "default_true")]
    pub wait: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchHistoryRequest {
    pub query: String,
    pub project_id: String,
    #[serde(default)]
    pub file_filter: Option<String>,
    #[serde(default)]
    pub exclude_paths: Option<Vec<String>>,
    #[serde(default)]
    pub author_filter: Option<String>,
    #[serde(default)]
    pub date_after: Option<u64>,
    #[serde(default)]
    pub date_before: Option<u64>,
    #[serde(default = "default_limit_5")]
    pub limit: usize,
    /// Full-text search mode: "strict", "loose", "regex". Default: "strict".
    #[serde(default)]
    pub fts_mode: FtsMode,
    /// Enable MMR reranking for diversity. Default: false.
    #[serde(default)]
    pub use_mmr: bool,
    /// Max characters per content preview (0 = no content). Default: 800.
    #[serde(default = "default_content_preview_800")]
    pub max_content_chars: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeTemporalCouplingsRequest {
    pub project_id: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default = "default_min_freq")]
    pub min_frequency: usize,
    #[serde(default = "default_limit_50")]
    pub limit: usize,
    #[serde(default = "default_true")]
    pub inject_edges: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeRevertsRequest {
    pub project_id: String,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestZipHistoryRequest {
    pub project_id: String,
    pub directory: String,
    #[serde(default = "default_true")]
    pub wait: bool,
}

// -------------------- Cognitive --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DreamProjectRequest {
    pub project_id: String,
    /// If true, block until the dream cycle completes and return insight count.
    #[serde(default)]
    pub wait: bool,
    /// Maximum co-occurrence clusters to process (default 5, max 500).
    /// Backward-compatible alias: `max_pairs` is also accepted.
    #[serde(default = "default_dream_max_clusters", alias = "max_pairs")]
    pub max_clusters: usize,
    /// Minimum edge weight for co-occurrence clustering (default 2, max 100).
    #[serde(default = "default_dream_min_edge_weight")]
    pub min_edge_weight: u32,
    /// Minimum nodes in a cluster for insight generation (default 3, max 50).
    #[serde(default = "default_dream_min_cluster_size")]
    pub min_cluster_size: usize,
    /// Timeout in seconds for the dream cycle when wait=true (default 60, max 300).
    #[serde(default = "default_dream_timeout_secs")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeFileCodingStyleRequest {
    pub project_id: String,
    pub file_path: String,
    #[serde(default = "default_diff_limit")]
    pub diff_limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestMigrationBoundariesRequest {
    pub project_id: String,
    /// Minimum co-change frequency to include a coupling edge (default 3).
    #[serde(default = "default_min_freq_3")]
    pub min_frequency: u32,
    /// Maximum number of bounded-context clusters to return (default 8).
    #[serde(default = "default_max_clusters")]
    pub max_clusters: usize,
    /// Return machine-readable JSON output instead of human-readable text (default false).
    #[serde(default)]
    pub output_json: bool,
    /// Include cross-cluster dependency analysis showing shared state/tables (default true).
    #[serde(default = "default_true")]
    pub include_cross_cluster_deps: bool,
    /// Timeout in seconds for LLM boundary suggestion (default 120, max 300).
    #[serde(default = "default_boundary_timeout_secs")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImmuneCheckRequest {
    pub project_id: String,
    /// Code snippet to check against the anti-pattern index.
    pub code: String,
    /// Optional target file path the snippet would be applied to. When
    /// supplied, the check cross-references active `immune_*` repo rules
    /// whose `file_pattern` matches this path and escalates the verdict
    /// accordingly — a snippet that touches a previously-reverted file
    /// AND contains destructive patterns is never CLEAN regardless of
    /// raw similarity score.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Number of results to consider (default 10, max 200).
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Use hybrid search (FTS + vector) instead of FTS-only (default true).
    #[serde(default = "default_true")]
    pub use_vector: bool,
    /// Include matched anti-pattern content in output (default false).
    #[serde(default)]
    pub include_content: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AntiPatternGuardRequest {
    pub project_id: String,
    /// Code snippet to check for anti-pattern matches.
    pub code: String,
    /// Maximum anti-pattern matches to return (default 5, max 200).
    #[serde(default = "default_limit_5")]
    pub limit: usize,
    /// Use hybrid search (FTS + vector) instead of FTS-only (default true).
    #[serde(default = "default_true")]
    pub use_vector: bool,
    /// Include matched anti-pattern content in output (default true).
    #[serde(default = "default_true")]
    pub include_content: bool,
}

// -------------------- Migration slicer --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateMigrationBlueprintRequest {
    pub project_id: String,
    /// The node ID of the entry point (e.g. a file node like "file:Map.aspx",
    /// or a symbol like "sym:class:MapPage"). Partial matches are attempted
    /// if an exact node is not found.
    pub entry_node: String,
    /// Maximum BFS depth from the entry node (default 3, max 8).
    #[serde(default = "default_max_depth_3")]
    pub max_depth: u8,
    /// Return JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
    /// Filter BFS to specific edge kinds (e.g. ["dependency", "sql_calls"]). Default: all.
    #[serde(default)]
    pub include_edge_kinds: Option<Vec<String>>,
    /// Skip dead code nodes during BFS. Default: true.
    #[serde(default = "default_true")]
    pub exclude_dead_code: bool,
}

// -------------------- AST Dependency Graph --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AstDependencyGraphRequest {
    pub project_id: String,
    /// File path (project-relative) or node ID to root the dependency tree from.
    pub entry: String,
    /// Maximum depth of dependency traversal. Default: 3, max: 12.
    #[serde(default = "default_max_depth_3")]
    pub max_depth: u8,
    /// Direction: "outgoing"/"out" (what this depends on), "incoming"/"in" (what depends on this), "both". Default: "out".
    #[serde(default = "default_direction_outgoing")]
    pub direction: Direction,
    /// Only include compile-time dependencies (Dependency, Imports, Contains). Default: true.
    #[serde(default = "default_true")]
    pub compile_time_only: bool,
    /// Return JSON output instead of text tree. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Incremental Indexing GC --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IncrementalIndexingGcRequest {
    pub project_id: String,
    /// Target generation to GC up to (exclusive). If omitted, uses active_generation.
    #[serde(default)]
    pub target_generation: Option<u64>,
    /// Also compact LanceDB (reclaim tombstone space). Default: true.
    #[serde(default = "default_true")]
    pub compact_vectors: bool,
}

// -------------------- Antipattern Index Management --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AntipatternIndexRequest {
    pub project_id: String,
    /// Action: "stats" (default), "list", "search", "clear".
    #[serde(default = "default_antipattern_action")]
    pub action: String,
    /// Search query (required if action="search").
    #[serde(default)]
    pub query: Option<String>,
    /// Filter by file pattern (substring match on paths).
    #[serde(default)]
    pub file_filter: Option<String>,
    /// Max results for list/search. Default: 50.
    #[serde(default = "default_limit_50")]
    pub limit: usize,
}

// -------------------- Vector Search --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VectorSearchRequest {
    pub project_id: String,
    /// The text to embed and search for semantically.
    pub query: String,
    /// Namespace to search within (default "memory").
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    /// Number of results to return (default 10, max 200).
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Use MMR (Maximal Marginal Relevance) for diverse results (default false).
    #[serde(default)]
    pub use_mmr: bool,
    /// Only include results whose path starts with one of these prefixes.
    #[serde(default)]
    pub include_path_prefixes: Option<Vec<String>>,
    /// Exclude results whose path starts with any of these prefixes.
    #[serde(default)]
    pub exclude_path_prefixes: Option<Vec<String>>,
    /// Filter to specific programming languages.
    #[serde(default)]
    pub language_filters: Option<Vec<String>>,
    /// Include full chunk content in results (default false).
    #[serde(default)]
    pub include_content: bool,
    /// Max characters of content to include per result (default 1200, max 20000).
    #[serde(default = "default_max_content_chars")]
    pub max_content_chars: usize,
}

// -------------------- Jobs --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListJobsRequest {
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelJobRequest {
    pub job_id: String,
}

// -------------------- Instrumentation --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetInstrumentationPackRequest {
    pub language: String, // "csharp" or "vb"
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestInstrumentationLogsRequest {
    pub project_id: String,
    pub log_content: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeArtifactKindRequest {
    IisLog,
    CustomTrace,
    PageLifecycleSnapshot,
    SqlProfilerExport,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifactInputRequest {
    pub kind: RuntimeArtifactKindRequest,
    pub content: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestRuntimeArtifactsRequest {
    pub project_id: String,
    pub artifacts: Vec<RuntimeArtifactInputRequest>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetInstrumentationPackResult {
    pub snippet: String,
    pub instructions: String,
}

impl SearchMemoryRequest {
    pub fn sanitized_max_results(&self) -> usize {
        self.max_results.clamp(1, MAX_SEARCH_RESULTS)
    }

    pub fn sanitized_max_content_chars_per_result(&self) -> usize {
        self.max_content_chars_per_result
            .clamp(1, MAX_CONTENT_CHARS_PER_RESULT)
    }
}

impl Default for SearchMemoryRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            project_id: String::new(),
            namespace: default_namespace_memory(),
            max_results: default_top_k(),
            use_mmr: true,
            fts_mode: FtsMode::default(),
            include_content: true,
            max_content_chars_per_result: default_max_content_chars(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }
    }
}

impl TraverseGraphRequest {
    pub fn sanitized_max_hops(&self) -> usize {
        self.max_hops.clamp(1, MAX_GRAPH_HOPS)
    }
}

impl QueryGraphNodesRequest {
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_SEARCH_RESULTS)
    }
}

impl IndexGitHistoryRequest {
    pub fn sanitized_max_commits(&self) -> usize {
        self.max_commits.clamp(1, MAX_GIT_COMMITS)
    }

    pub fn sanitized_mode(&self) -> GitHistoryMode {
        self.mode.unwrap_or_default()
    }
}

impl AnalyzeRevertsRequest {
    pub fn sanitized_max_commits(&self) -> usize {
        self.max_commits.clamp(1, MAX_GIT_COMMITS)
    }
}

impl SearchHistoryRequest {
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_SEARCH_RESULTS)
    }
}

impl AnalyzeTemporalCouplingsRequest {
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_TEMPORAL_RESULTS)
    }

    pub fn sanitized_min_frequency(&self) -> usize {
        self.min_frequency.clamp(1, MAX_TEMPORAL_MIN_FREQUENCY)
    }
}

impl DreamProjectRequest {
    pub fn sanitized_max_clusters(&self) -> usize {
        self.max_clusters.clamp(1, MAX_DREAM_PAIRS)
    }
    pub fn sanitized_min_edge_weight(&self) -> u32 {
        self.min_edge_weight.clamp(1, 100)
    }
    pub fn sanitized_min_cluster_size(&self) -> usize {
        self.min_cluster_size.clamp(2, 50)
    }
    pub fn sanitized_timeout_secs(&self) -> u64 {
        self.timeout_secs.clamp(5, 300)
    }
}

impl AnalyzeFileCodingStyleRequest {
    pub fn sanitized_diff_limit(&self) -> usize {
        self.diff_limit.clamp(1, MAX_DIFF_LIMIT)
    }
}

impl DetectDesignPatternsRequest {
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_ANTIPATTERN_RESULTS)
    }
}

impl TraceUiActionRequest {
    pub fn sanitized_max_depth(&self) -> usize {
        (self.max_depth as usize).clamp(1, MAX_GRAPH_HOPS)
    }

    pub fn sanitized_max_paths(&self) -> usize {
        self.max_paths.clamp(1, MAX_TRACE_PATHS)
    }
}

impl TraceUiEventRequest {
    pub fn sanitized_max_hops(&self) -> usize {
        (self.max_hops as usize).clamp(1, MAX_GRAPH_HOPS)
    }

    pub fn sanitized_max_paths(&self) -> usize {
        self.max_paths.clamp(1, MAX_TRACE_PATHS)
    }
}

impl FindDeadMethodsRequest {
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_SYMBOL_REFS)
    }
}

impl ImmuneCheckRequest {
    pub fn sanitized_top_k(&self) -> usize {
        self.top_k.clamp(1, MAX_IMMUNE_TOP_K)
    }
}

impl AntiPatternGuardRequest {
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_IMMUNE_TOP_K)
    }
}

impl SuggestMigrationBoundariesRequest {
    pub fn sanitized_min_frequency(&self) -> u32 {
        self.min_frequency.clamp(1, 1000)
    }
    pub fn sanitized_max_clusters(&self) -> usize {
        self.max_clusters.clamp(1, 50)
    }
    pub fn sanitized_timeout_secs(&self) -> u64 {
        self.timeout_secs.clamp(10, 300)
    }
}

impl UpdateProjectRequest {
    pub fn sanitized_max_commits(&self) -> usize {
        self.max_commits.clamp(1, MAX_GIT_COMMITS)
    }
}

impl GraphSearchRequest {
    pub fn sanitized_max_results(&self) -> usize {
        self.max_results.clamp(1, MAX_SEARCH_RESULTS)
    }
    pub fn sanitized_hop_depth(&self) -> usize {
        self.hop_depth.clamp(1, 4)
    }
    pub fn sanitized_max_content_chars(&self) -> usize {
        self.max_content_chars
            .clamp(0, MAX_CONTENT_CHARS_PER_RESULT)
    }
}

impl FindSymbolReferencesRequest {
    pub fn sanitized_max_incoming(&self) -> usize {
        self.max_incoming.clamp(1, MAX_SYMBOL_REFS)
    }
    pub fn sanitized_max_outgoing_per_kind(&self) -> usize {
        self.max_outgoing_per_kind.clamp(1, MAX_SYMBOL_REFS)
    }
}

impl RepairProjectRequest {
    pub fn sanitized_max_commits(&self) -> usize {
        self.max_commits.clamp(1, MAX_GIT_COMMITS)
    }
}

impl AstDependencyGraphRequest {
    pub fn sanitized_max_depth(&self) -> usize {
        (self.max_depth as usize).clamp(1, MAX_AST_DEPTH)
    }
}

impl AntipatternIndexRequest {
    pub fn sanitized_limit(&self) -> usize {
        self.limit.clamp(1, MAX_ANTIPATTERN_RESULTS)
    }
}

impl GenerateMigrationBlueprintRequest {
    pub fn sanitized_max_depth(&self) -> usize {
        (self.max_depth as usize).clamp(1, 8)
    }
}

impl VectorSearchRequest {
    pub fn sanitized_top_k(&self) -> usize {
        self.top_k.clamp(1, MAX_SEARCH_RESULTS)
    }
    pub fn sanitized_max_content_chars(&self) -> usize {
        self.max_content_chars
            .clamp(0, MAX_CONTENT_CHARS_PER_RESULT)
    }
}

// -------------------- Observability --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMetricsRequest {
    /// Return raw JSON instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Integrity --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckIntegrityRequest {
    pub project_id: String,
    /// Auto-repair mismatches if found (overrides config). Default: use config value.
    #[serde(default)]
    pub auto_repair: Option<bool>,
}

// -------------------- Safety --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvaluateSafetyRequest {
    pub project_id: String,
    /// Files affected by the proposed edit.
    pub affected_files: Vec<String>,
    /// Type of refactoring (e.g. "rename", "extract", "inline", "move", "delete").
    pub refactor_type: String,
    /// Number of nodes affected by impact analysis.
    #[serde(default)]
    pub impact_node_count: u64,
    /// Confidence from impact analysis (0.0–1.0).
    #[serde(default = "default_safety_confidence")]
    pub impact_confidence: f64,
    /// Test coverage of affected files (0.0–1.0, or -1.0 if unknown). Default: -1.0.
    #[serde(default = "default_unknown_coverage")]
    pub test_coverage: f64,
    /// Anti-pattern guard passed for affected files. Default: true.
    #[serde(default = "default_true")]
    pub anti_pattern_clear: bool,
    /// Number of downstream dependents (callers, importers). Default: 0.
    #[serde(default)]
    pub downstream_dependents: u64,
    /// Whether the edit touches shared/global state. Default: false.
    #[serde(default)]
    pub touches_global_state: bool,
    /// Whether the edit modifies database schema or queries. Default: false.
    #[serde(default)]
    pub touches_database: bool,
}

fn default_safety_confidence() -> f64 {
    0.5
}
fn default_unknown_coverage() -> f64 {
    -1.0
}

// -------------------- Migration Plan --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateMigrationPlanRequest {
    pub project_id: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Benchmark --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkRetrievalRequest {
    pub project_id: String,
    /// Custom benchmark queries with known-relevant paths (JSON array).
    /// If empty, uses auto-generated legacy WebForms queries.
    #[serde(default)]
    pub custom_queries: Option<Vec<BenchmarkQueryInput>>,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkQueryInput {
    pub query: String,
    pub relevant_paths: Vec<String>,
}

// -------------------- Confidence Scoring --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetExtractionConfidenceRequest {
    pub project_id: String,
    /// Type of extraction to score: "event_wiring", "sql_trace", "control_binding".
    pub extraction_type: String,
    /// The source code or AST data to evaluate.
    pub source_content: String,
    /// Optional: codebehind file content (for event_wiring).
    #[serde(default)]
    pub codebehind_content: Option<String>,
}

// -------------------- Checkpoint Status --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckpointStatusRequest {
    /// Filter by project_id. If empty, returns all checkpoints.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Filter by job_id. If empty, returns all for project.
    #[serde(default)]
    pub job_id: Option<String>,
}

// -------------------- Memory Budget --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMemoryBudgetRequest {
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Blast Radius --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ComputeBlastRadiusRequest {
    pub project_id: String,
    /// File path to analyze (project-relative). Mutually exclusive with symbol_fqn.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Fully qualified symbol name to analyze. Mutually exclusive with file_path.
    #[serde(default)]
    pub symbol_fqn: Option<String>,
    /// Include agentic migration guidance in the response. Default: true.
    #[serde(default = "default_true")]
    pub include_guidance: bool,
}

// -------------------- Autonomous Decision Gate --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutonomousDecisionGateRequest {
    pub project_id: String,
    /// Diff or code snippet representing the proposed change.
    pub proposed_change: String,
    /// Target files affected by the change.
    #[serde(default)]
    pub target_files: Vec<String>,
    /// Risk profile: "low", "medium", or "high". Default: "medium".
    #[serde(default = "default_risk_profile")]
    pub risk_profile: String,
    /// Whether runtime evidence (instrumentation logs) is required. Default: false.
    #[serde(default)]
    pub require_runtime_evidence: bool,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
    /// Pre-computed extraction confidence score (0.0–1.0). If not provided, skipped.
    #[serde(default)]
    pub extraction_confidence: Option<f64>,
    /// Extraction type that was scored (e.g., "event_wiring", "sql_trace").
    #[serde(default)]
    pub extraction_type: Option<String>,
    /// Pre-computed immune check verdict ("PASS", "WARN", "BLOCK"). If not provided, skipped.
    #[serde(default)]
    pub immune_verdict: Option<String>,
    /// Immune similarity score (0.0–1.0).
    #[serde(default)]
    pub immune_confidence: Option<f32>,
    /// Whether the trace for this change used a fallback candidate resolution.
    #[serde(default)]
    pub trace_used_fallback: bool,
    /// Number of ambiguous candidates found during trace resolution.
    #[serde(default)]
    pub trace_candidate_count: usize,
    /// Whether runtime instrumentation evidence has been collected.
    #[serde(default)]
    pub has_runtime_evidence: bool,

    // ── vNext fields ──
    /// Evidence depth: "fast", "standard", or "deep". Default: "standard".
    /// Controls how much evidence ADP gathers itself via the Evidence Orchestration Engine.
    #[serde(default = "default_evidence_depth")]
    pub evidence_depth: String,
    /// Runtime evidence batch JSON for reconciliation scoring.
    /// If provided, replaces the boolean `has_runtime_evidence` with rich reconciliation data.
    #[serde(default)]
    pub runtime_evidence_batch: Option<serde_json::Value>,
    /// Migration class for calibrated thresholds (e.g., "data_access", "webforms_page").
    #[serde(default)]
    pub migration_class: Option<String>,
    /// Wave items for plan-level evaluation. If provided, evaluates an entire
    /// migration wave instead of a single patch.
    #[serde(default)]
    pub wave_items: Option<Vec<WaveItemInput>>,
}

/// A single item in a migration wave for plan-level ADP evaluation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaveItemInput {
    /// File path for this wave item.
    pub file_path: String,
    /// Description of the change for this item.
    pub change_description: String,
    /// Risk profile for this item. Default: "medium".
    #[serde(default = "default_risk_profile")]
    pub risk_profile: String,
}

fn default_evidence_depth() -> String {
    "standard".to_string()
}

fn default_risk_profile() -> String {
    "medium".to_string()
}

// -------------------- Graph Centrality Rerank --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GraphCentralityRerankRequest {
    pub project_id: String,
    /// Search query to run before reranking. If provided, results are searched then reranked.
    /// If omitted, returns top-N most central nodes without search.
    #[serde(default)]
    pub query: Option<String>,
    /// Specific node IDs to score. If provided, only these nodes are scored (no search).
    #[serde(default)]
    pub node_ids: Option<Vec<String>>,
    /// Number of results to return. Default: 10.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
    /// Centrality algorithm blend weights.
    /// Weights for: PageRank (default 0.5), degree (default 0.3), betweenness (default 0.2).
    #[serde(default = "default_pr_weight")]
    pub pagerank_weight: f32,
    #[serde(default = "default_degree_weight")]
    pub degree_weight: f32,
    #[serde(default = "default_betweenness_weight")]
    pub betweenness_weight: f32,
    /// Number of pivot nodes for betweenness approximation (default 50, max 500).
    #[serde(default = "default_betweenness_samples")]
    pub betweenness_samples: usize,
    /// Namespace for search (default "memory"). Only used if query is provided.
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    /// Include node metadata (type, file_path, name) in output. Default: true.
    #[serde(default = "default_true")]
    pub include_metadata: bool,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

fn default_pr_weight() -> f32 {
    0.5
}
fn default_degree_weight() -> f32 {
    0.3
}
fn default_betweenness_weight() -> f32 {
    0.2
}
fn default_betweenness_samples() -> usize {
    50
}

pub const MAX_BETWEENNESS_SAMPLES: usize = 500;

impl GraphCentralityRerankRequest {
    pub fn sanitized_top_k(&self) -> usize {
        self.top_k.clamp(1, MAX_SEARCH_RESULTS)
    }
    pub fn sanitized_betweenness_samples(&self) -> usize {
        self.betweenness_samples.clamp(10, MAX_BETWEENNESS_SAMPLES)
    }
}

// -------------------- Detect Design Patterns --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DetectDesignPatternsRequest {
    pub project_id: String,
    /// Filter to specific pattern names (e.g., ["God Object", "Session Soup"]).
    /// Empty means return all detected patterns.
    #[serde(default)]
    pub pattern_filter: Vec<String>,
    /// Maximum number of patterns to return. Default: 50.
    #[serde(default = "default_limit_50")]
    pub limit: usize,
}

// -------------------- Phase 30: Migration Engine --------------------

fn default_output_format() -> String {
    "full".into()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateMigrationScaffoldRequest {
    pub project_id: String,
    /// Path of the legacy file to scaffold from.
    pub file_path: String,
    /// Target stack: "blazor", "react", or "angular". Default: "blazor".
    #[serde(default)]
    pub target_stack: TargetStack,
    /// Also generate a test scaffold. Default: false.
    #[serde(default)]
    pub include_test_scaffold: bool,
    /// "full" (default) or "diff" (shows what maps to what).
    #[serde(default = "default_output_format")]
    pub output_format: String,
}

fn default_instrument_language() -> String {
    "csharp".into()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateInstrumentationCodeRequest {
    pub project_id: String,
    /// Files to instrument (empty = all files with relevant edges).
    #[serde(default)]
    pub target_files: Vec<String>,
    /// Language: "csharp" or "vb". Default: "csharp".
    #[serde(default = "default_instrument_language")]
    pub language: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconcileRuntimeEvidenceRequest {
    pub project_id: String,
    /// JSON-serialized RuntimeEvidenceBatch.
    pub evidence_json: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestStateMigrationRequest {
    pub project_id: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateCharacterizationTestsRequest {
    pub project_id: String,
    /// File path to generate tests for.
    pub file_path: String,
    /// Test framework: "nunit", "xunit", or "mstest". Default: "nunit".
    #[serde(default)]
    pub framework: TestFramework,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Strangler Fig --------------------

fn default_legacy_url() -> String {
    "http://localhost:5000".into()
}
fn default_modern_url() -> String {
    "http://localhost:5001".into()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateStranglerFigRequest {
    pub project_id: String,
    /// Base URL of the legacy application. Default: "http://localhost:5000".
    #[serde(default = "default_legacy_url")]
    pub legacy_base_url: String,
    /// Base URL of the modern application. Default: "http://localhost:5001".
    #[serde(default = "default_modern_url")]
    pub modern_base_url: String,
}

// -------------------- Phase 31: Migration Workflow Engine --------------------

/// Ticket 7: Map validation controls to modern equivalents.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapValidationControlsRequest {
    pub project_id: String,
    /// Project-relative path to the .aspx, .ascx, or .master file.
    pub file_path: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 8: Map authentication/authorization configuration.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapAuthConfigRequest {
    pub project_id: String,
    /// Optional file scope — if provided, only scan this file for code-level
    /// auth checks. If omitted, scans all indexed code files.
    #[serde(default)]
    pub file_scope: Option<String>,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 3: Map page lifecycle events to modern equivalents.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapPageLifecycleRequest {
    pub project_id: String,
    /// Project-relative path to the code-behind file (.aspx.vb, .aspx.cs).
    pub file_path: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 4: Analyze ViewState dependencies (explicit and implicit).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeViewStateDepsRequest {
    pub project_id: String,
    /// Project-relative path to the code-behind file (.aspx.vb, .aspx.cs).
    pub file_path: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 6: Map UpdatePanel / AJAX regions to modern component boundaries.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapAjaxRegionsRequest {
    pub project_id: String,
    /// Project-relative path to the .aspx file.
    pub file_path: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 2: Trace data flow from an event handler through SQL/state/binding.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TraceDataFlowRequest {
    pub project_id: String,
    /// Project-relative path to the code-behind file.
    pub file_path: String,
    /// Name of the event handler to trace (e.g. "btnSearch_Click").
    pub entry_point: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 1: Get a complete migration dossier for a single page.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMigrationDossierRequest {
    pub project_id: String,
    /// Project-relative path to the .aspx (or .ascx / .master) file.
    pub file_path: String,
    /// Target stack for scaffold preview: "blazor", "react", "angular". Default: "blazor".
    #[serde(default)]
    pub target_stack: TargetStack,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 5: Check migration coverage — what did the modern code miss?
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckMigrationCoverageRequest {
    pub project_id: String,
    /// Project-relative path to the original legacy file.
    pub original_file: String,
    /// The generated modern code to verify against the graph.
    pub modern_code: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 9a: Update migration status for a file.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateMigrationStatusRequest {
    pub project_id: String,
    /// Project-relative path of the file whose status is being updated.
    pub file_path: String,
    /// Status: "not_started", "in_progress", "migrated", "verified", "blocked".
    pub status: String,
    /// Free-form notes (PR links, comments, blockers).
    #[serde(default)]
    pub notes: String,
    /// Optional risk score (0–100).
    #[serde(default)]
    pub risk_score: Option<u8>,
    /// Reason the file is blocked (only relevant when status = "blocked").
    #[serde(default)]
    pub blocked_reason: Option<String>,
    /// File paths that must be migrated before this one.
    #[serde(default)]
    pub blocking_dependencies: Vec<String>,
}

/// Ticket 9b: Get migration progress for a project.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMigrationProgressRequest {
    pub project_id: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Ticket 10: Suggest optimal migration order based on dependency graph.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestMigrationOrderRequest {
    pub project_id: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Phase 31: Full Project Migration --------------------

fn default_max_files() -> usize {
    200
}

fn default_llm_max_pages() -> usize {
    50
}

/// Analyze an entire project for migration in one call.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeFullProjectMigrationRequest {
    pub project_id: String,
    /// Target stack: "blazor", "react", or "angular". Default: "blazor".
    #[serde(default)]
    pub target_stack: TargetStack,
    /// Maximum number of markup files to analyze. Default: 200.
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    /// Return JSON output instead of markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
    /// Use the configured LLM backend to enhance per-page dossiers with a
    /// narrative business purpose and migration-specific Blazor guidance.
    /// Also enables the existing business-logic enhancement pass for
    /// code-file method summaries. Default: false.
    ///
    /// With this flag set, the top `llm_max_pages` pages (by deterministic
    /// complexity score, then by blast radius as tiebreaker) receive an
    /// async LLM call each. Lower-complexity pages keep the deterministic
    /// analysis only, which keeps token cost bounded.
    #[serde(default)]
    pub use_llm: bool,
    /// Maximum number of page dossiers to enhance with the LLM. Ignored
    /// unless `use_llm: true`. The top N pages by deterministic complexity
    /// score receive one LLM call each; the rest stay deterministic-only.
    /// Default: 50.
    #[serde(default = "default_llm_max_pages")]
    pub llm_max_pages: usize,
}

// ── Phase 36: Business Logic Comprehension ───────────────────────────────────

fn default_max_concurrent() -> usize {
    2
}

/// Analyze business logic of methods using the local LLM.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeBusinessLogicRequest {
    pub project_id: String,
    /// Specific file to analyze. If omitted, analyzes all code-behind files.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Specific method to analyze (requires file_path).
    #[serde(default)]
    pub method_name: Option<String>,
    /// Re-analyze even if cached results exist. Default: false.
    #[serde(default)]
    pub force_refresh: bool,
    /// Max concurrent LLM calls. Default: 2.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Return JSON output instead of markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Query business logic summaries using natural language.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueryBusinessLogicRequest {
    pub project_id: String,
    /// Natural language query about business logic.
    pub query: String,
    /// Maximum number of results. Default: 5.
    #[serde(default = "default_limit_5")]
    pub top_k: usize,
}

impl QueryBusinessLogicRequest {
    /// MCP1: clamp top_k to MAX_SEARCH_RESULTS to prevent resource amplification.
    pub fn sanitized_top_k(&self) -> usize {
        self.top_k.clamp(1, MAX_SEARCH_RESULTS)
    }
}

// ── Phase 37: Wiring — Expose Existing Services ──────────────────────────────

fn default_sp_limit() -> usize {
    500
}

/// Full database analysis: schema, stored procedures, triggers, call chains.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeDatabaseIntelligenceRequest {
    pub project_id: String,
    /// Optional: path to a specific .sql file to analyze in isolation.
    /// If omitted, uses all .sql files already indexed for the project.
    #[serde(default)]
    pub sql_file_path: Option<String>,
    /// Maximum stored procedures to summarize (avoid runaway cost). Default: 500.
    #[serde(default = "default_sp_limit")]
    pub sp_limit: usize,
    /// Output as JSON instead of Markdown report. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Retrieve deep analysis for a single stored procedure by name.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetSpDetailsRequest {
    pub project_id: String,
    /// Name of the stored procedure.
    pub sp_name: String,
    /// If true, re-analyze even if cached. Default: false.
    #[serde(default)]
    pub force_refresh: bool,
}

/// Get all triggers for a project, optionally filtered by table.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTriggersRequest {
    pub project_id: String,
    /// Filter to triggers on a specific table name.
    #[serde(default)]
    pub table_name: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Detect synchronous patterns hazardous for async/await migration.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeSyncHazardsRequest {
    pub project_id: String,
    /// Specific file to analyze. If omitted, scans all indexed .vb/.cs files.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Only return hazards at or above this severity: "medium" | "high" | "critical". Default: "medium".
    #[serde(default)]
    pub min_severity: MinSeverity,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Get jQuery usage inventory for a project.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetJQueryInventoryRequest {
    pub project_id: String,
    /// Filter to files matching this glob pattern (e.g., "*.js" or "checkout*").
    #[serde(default)]
    pub file_filter: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Reconstruct session/state workflow flows from graph edges.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetSessionWorkflowsRequest {
    pub project_id: String,
    /// Filter to a specific scope: "session" | "application" | "cache" | "viewstate" | "cookie".
    /// If omitted, returns all scopes.
    #[serde(default)]
    pub scope_filter: Option<String>,
    /// Filter to a specific state key name (partial match, case-insensitive).
    #[serde(default)]
    pub key_filter: Option<String>,
    /// Only include keys with problematic patterns (MissingWriter, MissingReader, ComplexWorkflow).
    #[serde(default)]
    pub warnings_only: bool,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Detect VB.NET → C# translation traps (14 categories of semantic differences).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetVbTranslationTrapsRequest {
    pub project_id: String,
    /// Specific .vb file to analyze. If omitted, analyzes all indexed .vb files.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Filter to a specific risk type: "silent_bug" | "compile_error". If omitted, returns both.
    #[serde(default)]
    pub risk_filter: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Detect C# migration/modernization diagnostics.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCsharpDiagnosticsRequest {
    pub project_id: String,
    /// Specific .cs file to analyze. If omitted, analyzes all .cs files.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Detect C migration diagnostics (buffer/ownership/unsafe API heuristics).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCDiagnosticsRequest {
    pub project_id: String,
    /// Specific C source/header file to analyze. If omitted, analyzes all .c/.h files.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Detect C++ migration diagnostics (RAII/new-delete/exception-safety heuristics).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCppDiagnosticsRequest {
    pub project_id: String,
    /// Specific C++ file to analyze. If omitted, analyzes all .cpp/.cc/.cxx/.hpp/.hh/.hxx files.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Detect Rust migration diagnostics (panic/blocking-in-async/unsafe boundary heuristics).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetRustDiagnosticsRequest {
    pub project_id: String,
    /// Specific .rs file to analyze. If omitted, analyzes all .rs files.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// ── Phase 38: The Access Layer ────────────────────────────────────────────────

fn default_context_lines() -> u32 {
    5
}
fn default_max_callers() -> usize {
    3
}

/// Retrieve method metadata from the method index (sub-200ms per-method queries).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMethodInfoRequest {
    pub project_id: String,
    /// Fully qualified name (e.g., "MyNamespace.CheckoutPage.ProcessOrder"),
    /// partial match (e.g., "CheckoutPage.ProcessOrder"), or just "ProcessOrder".
    pub fqn_or_name: String,
    /// If ambiguous, filter by file path (partial match).
    #[serde(default)]
    pub file_path: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Retrieve the complete, untruncated source code of a method.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetFullMethodBodyRequest {
    pub project_id: String,
    /// Fully qualified name from method index. Mutually exclusive with file_path + line_start.
    #[serde(default)]
    pub fqn: Option<String>,
    /// Explicit file path (alternative to FQN lookup).
    #[serde(default)]
    pub file_path: Option<String>,
    /// Start line (1-based). Required if file_path is used instead of fqn.
    #[serde(default)]
    pub line_start: Option<u32>,
    /// End line (1-based). Required if file_path is used instead of fqn.
    #[serde(default)]
    pub line_end: Option<u32>,
    /// Lines of context above/below the method. Default: 5.
    #[serde(default = "default_context_lines")]
    pub context_lines: u32,
    /// Also return bodies of top callers. Default: false.
    #[serde(default)]
    pub include_caller_bodies: bool,
    /// Maximum caller bodies to include. Default: 3.
    #[serde(default = "default_max_callers")]
    pub max_callers: usize,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

fn default_max_callers_50() -> usize {
    50
}

/// "Everything I need before touching a method" — assembles method info, full body,
/// callers, database footprint, session state, VB traps, sync hazards, blast radius,
/// and business logic into a single response.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetMethodEditContextRequest {
    pub project_id: String,
    /// Relative path to the code file containing the method.
    pub file_path: String,
    /// Method name to analyze.
    pub method_name: String,
    /// Class name (optional, for disambiguation).
    #[serde(default)]
    pub class_name: Option<String>,
    /// Include the complete, untruncated method body. Default: true.
    #[serde(default = "default_true")]
    pub include_full_body: bool,
    /// Include full bodies of the top N callers. Default: true.
    #[serde(default = "default_true")]
    pub include_caller_bodies: bool,
    /// Maximum callers to include. Default: 50.
    #[serde(default = "default_max_callers_50")]
    pub max_callers: usize,
    /// Include business logic analysis from DocStore. Default: true.
    #[serde(default = "default_true")]
    pub include_business_logic: bool,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

/// Full page context: control tree, all event handlers with bodies, data layer,
/// state, AJAX, validation, auth, and coding style for a WebForms page.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetPageContextRequest {
    pub project_id: String,
    /// Path to the .aspx/.ascx/.master file.
    pub aspx_file: String,
    /// Include full bodies of all event handlers. Default: true.
    #[serde(default = "default_true")]
    pub include_method_bodies: bool,
    /// Include master page structure analysis. Default: true.
    #[serde(default = "default_true")]
    pub include_master_page: bool,
    /// Include code-behind analysis. Default: true.
    #[serde(default = "default_true")]
    pub include_codebehind: bool,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// ── Phase 38-5: prepare_implementation_context ────────────────────────────────

fn default_max_pattern_examples() -> usize {
    3
}

/// Assemble everything an LLM needs to generate correct code for a method modification:
/// coding style profile, pattern examples from callers, database schema for referenced tables,
/// SP signatures, session state context, control mappings, VB traps, and sync hazards.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareImplementationContextRequest {
    pub project_id: String,
    /// The file containing the method to be modified.
    pub file_path: String,
    /// The method name to prepare context for.
    pub method_name: String,
    /// Optional class name for disambiguation.
    #[serde(default)]
    pub class_name: Option<String>,
    /// Target framework for migration (e.g., "blazor", "razor-pages", "react").
    /// If provided, control mappings will include migration-specific guidance.
    #[serde(default)]
    pub target_stack: Option<String>,
    /// Include coding style analysis from git history. Default: true.
    #[serde(default = "default_true")]
    pub include_style_profile: bool,
    /// Include pattern examples from callers. Default: true.
    #[serde(default = "default_true")]
    pub include_pattern_examples: bool,
    /// Maximum number of caller-based pattern examples. Default: 3.
    #[serde(default = "default_max_pattern_examples")]
    pub max_pattern_examples: usize,
    /// Include database schema for referenced tables. Default: true.
    #[serde(default = "default_true")]
    pub include_db_schema: bool,
    /// Include SP signatures for called stored procedures. Default: true.
    #[serde(default = "default_true")]
    pub include_sp_signatures: bool,
    /// Include session state flow context. Default: true.
    #[serde(default = "default_true")]
    pub include_state_context: bool,
    /// Include control mappings for referenced controls. Default: true.
    #[serde(default = "default_true")]
    pub include_control_mappings: bool,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// ── Phase 38-6: validate_generated_code ───────────────────────────────────────

/// Validate generated/modified code against the project's extracted knowledge:
/// SQL validity, VB trap avoidance, state key consistency, SP call correctness,
/// control ID validity, caller compatibility, and sync hazard introduction.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidateGeneratedCodeRequest {
    pub project_id: String,
    /// The generated or modified code to validate.
    pub code: String,
    /// The language of the code: "vb" or "csharp". Default: "csharp".
    #[serde(default = "default_csharp")]
    pub language: String,
    /// The file path this code is intended for (used for context resolution).
    #[serde(default)]
    pub target_file: Option<String>,
    /// The method name being replaced/modified (used for caller compatibility check).
    #[serde(default)]
    pub original_method_name: Option<String>,
    /// Known table names the original code accessed (used for SQL validation).
    #[serde(default)]
    pub expected_tables: Vec<String>,
    /// Known SP names the original code called (used for SP validation).
    #[serde(default)]
    pub expected_sps: Vec<String>,
    /// Known session keys the original code used (used for state consistency).
    #[serde(default)]
    pub expected_session_keys: Vec<String>,
    /// Known control IDs that should be referenced (used for control validation).
    #[serde(default)]
    pub expected_control_ids: Vec<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

fn default_csharp() -> String {
    "csharp".to_string()
}

// ── Phase 38-7: validate_sql_fragment ─────────────────────────────────────────

/// Validate a SQL fragment against the project's schema knowledge: table/column
/// existence, SP parameter types, join correctness, and common SQL anti-patterns.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidateSqlFragmentRequest {
    pub project_id: String,
    /// The SQL code to validate.
    pub sql: String,
    /// Context: which code file this SQL appears in (for cross-referencing).
    #[serde(default)]
    pub source_file: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// ── Phase 38-8: find_tests_for_method ─────────────────────────────────────────

/// Find existing tests that exercise a given method, by searching for references
/// to the method name in test files (files matching *Test*, *Spec*, *_test*).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindTestsForMethodRequest {
    pub project_id: String,
    /// Fully qualified method name or partial name.
    pub method_name: String,
    /// Optional: specific file to narrow the search.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// ── Phase 38-9: find_dead_methods ─────────────────────────────────────────────

fn default_dead_method_limit() -> usize {
    100
}

/// Find methods with zero callers, no Handles clause, and no lifecycle hooks —
/// candidates for dead code removal.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindDeadMethodsRequest {
    pub project_id: String,
    /// Filter to a specific file path (partial match).
    #[serde(default)]
    pub file_path: Option<String>,
    /// Maximum results. Default: 100.
    #[serde(default = "default_dead_method_limit")]
    pub limit: usize,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// ── Phase 38-10: check_edit_safety ────────────────────────────────────────────

/// Standalone edit safety check: returns green/yellow/red verdict for a method
/// without the full edit context overhead. Faster alternative to get_method_edit_context.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckEditSafetyRequest {
    pub project_id: String,
    /// Relative path to the file containing the method.
    pub file_path: String,
    /// Method name.
    pub method_name: String,
    /// Optional class name for disambiguation.
    #[serde(default)]
    pub class_name: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

#[cfg(test)]
mod unknown_field_tests {
    use super::*;

    #[test]
    fn index_project_rejects_unknown_fields() {
        // Valid canonical project_type must deserialize
        let good = r#"{"directory":"/tmp","project_name":"n","project_type":"dotnet_webforms_cs"}"#;
        let result: Result<IndexProjectRequest, _> = serde_json::from_str(good);
        assert!(result.is_ok(), "valid JSON must deserialize: {:?}", result);

        // Unknown field must be rejected
        let bad = r#"{"directory":"/tmp","project_name":"n","project_type":"dotnet_webforms_cs","max_reslts":5}"#;
        let result: Result<IndexProjectRequest, _> = serde_json::from_str(bad);
        assert!(result.is_err(), "unknown field max_reslts must be rejected");
    }

    #[test]
    fn index_project_rejects_unknown_project_type() {
        // ENG-AUD-2026-EXH-P1-0001: unknown project_type values must be rejected
        // at deserialization, not silently broadened to the full extension set.
        let bad = r#"{"directory":"/tmp","project_name":"n","project_type":"aspnet_webforms"}"#;
        let result: Result<IndexProjectRequest, _> = serde_json::from_str(bad);
        assert!(
            result.is_err(),
            "unknown project_type must be rejected at deserialization"
        );

        let bad2 = r#"{"directory":"/tmp","project_name":"n","project_type":"dotnet_webform_cs"}"#;
        let result2: Result<IndexProjectRequest, _> = serde_json::from_str(bad2);
        assert!(result2.is_err(), "typo'd project_type must be rejected");
    }

    #[test]
    fn index_project_accepts_all_valid_project_types() {
        // All canonical and aliased values must deserialize.
        for s in &[
            "dotnet_webforms_cs",
            "dotnetwebformscs",
            "webforms_cs",
            "dotnet_webforms_vb",
            "dotnetwebformsvb",
            "webforms_vb",
            "general",
            "rust",
            "rustlang",
            "csharp",
            "c#",
            "cpp",
            "c++",
            "c",
        ] {
            let json = format!(
                r#"{{"directory":"/tmp","project_name":"n","project_type":"{}"}}"#,
                s
            );
            let result: Result<IndexProjectRequest, _> = serde_json::from_str(&json);
            assert!(
                result.is_ok(),
                "project_type {:?} should be valid, got: {:?}",
                s,
                result
            );
        }
    }

    #[test]
    fn update_project_rejects_unknown_fields() {
        let bad = r#"{"project_id":"p1","projcet_type":"aspnet"}"#;
        let result: Result<UpdateProjectRequest, _> = serde_json::from_str(bad);
        assert!(
            result.is_err(),
            "unknown field projcet_type must be rejected"
        );
    }

    #[test]
    fn project_type_from_registry_supports_legacy_values() {
        assert_eq!(
            ProjectType::from_registry_str("DotNet_WebForms_CS"),
            Some(ProjectType::DotnetWebformsCs)
        );
        assert_eq!(
            ProjectType::from_registry_str("webformsvb"),
            Some(ProjectType::DotnetWebformsVb)
        );
        assert_eq!(
            ProjectType::from_registry_str("c#"),
            Some(ProjectType::CSharp)
        );
        assert_eq!(
            ProjectType::from_registry_str("c++"),
            Some(ProjectType::Cpp)
        );
        assert_eq!(
            ProjectType::from_registry_str("unknown"),
            None,
            "unknown registry values should be surfaced to fallback handling"
        );
    }

    #[test]
    fn search_memory_rejects_unknown_fields() {
        // valid
        let good = r#"{"project_id":"p1","query":"hello"}"#;
        let result: Result<SearchMemoryRequest, _> = serde_json::from_str(good);
        assert!(
            result.is_ok(),
            "valid search JSON must deserialize: {:?}",
            result
        );

        // typo in field name
        let bad = r#"{"project_id":"p1","qurey":"hello","top_kk":5}"#;
        let result: Result<SearchMemoryRequest, _> = serde_json::from_str(bad);
        assert!(result.is_err(), "typo field qurey/top_kk must be rejected");
    }

    #[test]
    fn graph_search_rejects_unknown_fields() {
        let good = r#"{"project_id":"p1","query":"hello"}"#;
        let result: Result<GraphSearchRequest, _> = serde_json::from_str(good);
        assert!(
            result.is_ok(),
            "valid graph search JSON must deserialize: {:?}",
            result
        );

        let bad = r#"{"project_id":"p1","query":"hello","unknwon_field":true}"#;
        let result: Result<GraphSearchRequest, _> = serde_json::from_str(bad);
        assert!(result.is_err(), "unknown field must be rejected");
    }

    #[test]
    fn adp_request_rejects_unknown_fields() {
        let good = r#"{"project_id":"p1","proposed_change":"diff"}"#;
        let result: Result<AutonomousDecisionGateRequest, _> = serde_json::from_str(good);
        assert!(
            result.is_ok(),
            "valid ADP JSON must deserialize: {:?}",
            result
        );

        let bad = r#"{"project_id":"p1","proposed_change":"diff","evidnece_depth":"fast"}"#;
        let result: Result<AutonomousDecisionGateRequest, _> = serde_json::from_str(bad);
        assert!(
            result.is_err(),
            "typo field evidnece_depth must be rejected"
        );
    }
}

// ENG-AUD-2026-S1-0001: All tool-facing request structs must reject unknown fields.
// The tests below verify that `#[serde(deny_unknown_fields)]` is active on five
// representative structs added in this fix.
#[cfg(test)]
mod unknown_field_request_tests {
    use super::*;

    #[test]
    fn project_id_request_rejects_unknown_field() {
        let good = r#"{"project_id":"proj-1"}"#;
        assert!(
            serde_json::from_str::<ProjectIdRequest>(good).is_ok(),
            "valid JSON must deserialize"
        );
        let bad = r#"{"project_id":"proj-1","typo_key":1}"#;
        assert!(
            serde_json::from_str::<ProjectIdRequest>(bad).is_err(),
            "unknown field typo_key must be rejected"
        );
    }

    #[test]
    fn watch_project_request_rejects_unknown_field() {
        let good = r#"{"project_id":"proj-1","enabled":true}"#;
        assert!(
            serde_json::from_str::<WatchProjectRequest>(good).is_ok(),
            "valid JSON must deserialize"
        );
        let bad = r#"{"project_id":"proj-1","enabled":true,"typo_key":1}"#;
        assert!(
            serde_json::from_str::<WatchProjectRequest>(bad).is_err(),
            "unknown field typo_key must be rejected"
        );
    }

    #[test]
    fn search_history_request_rejects_unknown_field() {
        let good = r#"{"query":"foo","project_id":"proj-1"}"#;
        assert!(
            serde_json::from_str::<SearchHistoryRequest>(good).is_ok(),
            "valid JSON must deserialize"
        );
        let bad = r#"{"query":"foo","project_id":"proj-1","typo_key":1}"#;
        assert!(
            serde_json::from_str::<SearchHistoryRequest>(bad).is_err(),
            "unknown field typo_key must be rejected"
        );
    }

    #[test]
    fn dream_project_request_rejects_unknown_field() {
        let good = r#"{"project_id":"proj-1"}"#;
        assert!(
            serde_json::from_str::<DreamProjectRequest>(good).is_ok(),
            "valid JSON must deserialize"
        );
        let bad = r#"{"project_id":"proj-1","typo_key":1}"#;
        assert!(
            serde_json::from_str::<DreamProjectRequest>(bad).is_err(),
            "unknown field typo_key must be rejected"
        );
    }

    #[test]
    fn vector_search_request_rejects_unknown_field() {
        let good = r#"{"project_id":"proj-1","query":"find me"}"#;
        assert!(
            serde_json::from_str::<VectorSearchRequest>(good).is_ok(),
            "valid JSON must deserialize"
        );
        let bad = r#"{"project_id":"proj-1","query":"find me","typo_key":1}"#;
        assert!(
            serde_json::from_str::<VectorSearchRequest>(bad).is_err(),
            "unknown field typo_key must be rejected"
        );
    }
}
