use schemars::JsonSchema;
use serde::Deserialize;

// -------------------- Default value functions --------------------

pub fn default_true() -> bool {
    true
}
pub fn default_top_k() -> usize {
    10
}
pub fn default_max_commits() -> usize {
    200
}
pub fn default_namespace_memory() -> String {
    "memory".to_string()
}
pub fn default_fts_strict() -> String {
    "strict".to_string()
}
pub fn default_max_content_chars() -> usize {
    1200
}
pub fn default_priority() -> i32 {
    5
}
pub fn default_direction_in() -> String {
    "in".to_string()
}
pub fn default_direction_both() -> String {
    "both".to_string()
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
pub fn default_direction_outgoing() -> String {
    "outgoing".to_string()
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

// -------------------- Project lifecycle --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IndexProjectRequest {
    pub directory: String,
    pub project_name: String,
    pub project_type: String,
    #[serde(default = "default_true")]
    pub wait: bool,
    #[serde(default = "default_true")]
    pub dedupe_by_directory: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct ProjectIdRequest {
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WatchProjectRequest {
    pub project_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

// -------------------- Repair --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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

#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
pub struct SearchMemoryRequest {
    pub query: String,
    pub project_id: String,
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    #[serde(default = "default_top_k")]
    pub max_results: usize,
    #[serde(default = "default_true")]
    pub use_mmr: bool,
    #[serde(default = "default_fts_strict")]
    pub fts_mode: String,
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
pub struct GetChunkRequest {
    pub project_id: String,
    /// Per-instance document identity (required).
    pub doc_id: String,
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    #[serde(default)]
    pub inject_rules: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
    #[serde(default = "default_fts_strict")]
    pub fts_mode: String,
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
pub struct AnalyzeErrorStackRequest {
    pub traceback: String,
    pub project_id: String,
}

// -------------------- Memory bank --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateMemoryBankRequest {
    pub project_id: String,
    #[serde(default)]
    pub section_id: Option<String>,
    pub section: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MemorySectionRequest {
    pub project_id: String,
    pub section: String,
}

// -------------------- Repo rules --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct DeleteRepoRuleRequest {
    pub project_id: String,
    pub rule_id: String,
}

// -------------------- Graph --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct FindReferencesRequest {
    pub project_id: String,
    pub node_id: String,
    #[serde(default)]
    pub edge_kind: Option<String>,
    #[serde(default = "default_direction_in")]
    pub direction: String, // "in", "out", "both"
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TraverseGraphRequest {
    pub project_id: String,
    pub node_id: String,
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
    #[serde(default)]
    pub edge_kinds: Option<Vec<String>>,
    #[serde(default = "default_direction_both")]
    pub direction: String, // "in", "out", "both"
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ImpactAnalysisRequest {
    pub project_id: String,
    pub file_path: Option<String>,
    pub symbol_fqn: Option<String>,
    #[serde(default = "default_limit_50")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetTableSchemaRequest {
    pub project_id: String,
    pub table_name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TraceStateUsageRequest {
    pub project_id: String,
    pub state_type: String,
    pub state_key: String,
    #[serde(default = "default_top_k")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TraceUiActionRequest {
    pub project_id: String,
    pub query: String,
    #[serde(default = "default_max_depth_3")]
    pub max_depth: u8,
    #[serde(default = "default_limit_5")]
    pub max_paths: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct ExportCapturePackRequest {
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetUiBlueprintRequest {
    pub project_id: String,
    /// Project-relative path to the .aspx, .ascx, .Designer.vb, or .Designer.cs file.
    pub file_path: String,
}

// -------------------- Git --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IndexGitHistoryRequest {
    pub project_id: String,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
    #[serde(default)]
    pub index_antipatterns: bool,
    #[serde(default = "default_true")]
    pub wait: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
    #[serde(default = "default_fts_strict")]
    pub fts_mode: String,
    /// Enable MMR reranking for diversity. Default: false.
    #[serde(default)]
    pub use_mmr: bool,
    /// Max characters per content preview (0 = no content). Default: 800.
    #[serde(default = "default_content_preview_800")]
    pub max_content_chars: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct AnalyzeRevertsRequest {
    pub project_id: String,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IngestZipHistoryRequest {
    pub project_id: String,
    pub directory: String,
    #[serde(default = "default_true")]
    pub wait: bool,
}

// -------------------- Cognitive --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct AnalyzeFileCodingStyleRequest {
    pub project_id: String,
    pub file_path: String,
    #[serde(default = "default_diff_limit")]
    pub diff_limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct ImmuneCheckRequest {
    pub project_id: String,
    /// Code snippet to check against the anti-pattern index.
    pub code: String,
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
pub struct AstDependencyGraphRequest {
    pub project_id: String,
    /// File path (project-relative) or node ID to root the dependency tree from.
    pub entry: String,
    /// Maximum depth of dependency traversal. Default: 3, max: 12.
    #[serde(default = "default_max_depth_3")]
    pub max_depth: u8,
    /// Direction: "outgoing" (what this depends on), "incoming" (what depends on this), "both". Default: "outgoing".
    #[serde(default = "default_direction_outgoing")]
    pub direction: String,
    /// Only include compile-time dependencies (Dependency, Imports, Contains). Default: true.
    #[serde(default = "default_true")]
    pub compile_time_only: bool,
    /// Return JSON output instead of text tree. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Incremental Indexing GC --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct ListJobsRequest {
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CancelJobRequest {
    pub job_id: String,
}

// -------------------- Instrumentation --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetInstrumentationPackRequest {
    pub language: String, // "csharp" or "vb"
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IngestInstrumentationLogsRequest {
    pub project_id: String,
    pub log_content: String,
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

impl TraverseGraphRequest {
    pub fn sanitized_max_hops(&self) -> usize {
        self.max_hops.clamp(1, MAX_GRAPH_HOPS)
    }
}

impl IndexGitHistoryRequest {
    pub fn sanitized_max_commits(&self) -> usize {
        self.max_commits.clamp(1, MAX_GIT_COMMITS)
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
pub struct GetMetricsRequest {
    /// Return raw JSON instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Integrity --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CheckIntegrityRequest {
    pub project_id: String,
    /// Auto-repair mismatches if found (overrides config). Default: use config value.
    #[serde(default)]
    pub auto_repair: Option<bool>,
}

// -------------------- Safety --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct GenerateMigrationPlanRequest {
    pub project_id: String,
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

// -------------------- Benchmark --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct BenchmarkQueryInput {
    pub query: String,
    pub relevant_paths: Vec<String>,
}

// -------------------- Confidence Scoring --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
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
pub struct GetMemoryBudgetRequest {
    /// Return JSON output instead of human-readable text. Default: false.
    #[serde(default)]
    pub output_json: bool,
}
