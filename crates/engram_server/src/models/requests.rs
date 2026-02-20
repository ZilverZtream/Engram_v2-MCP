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

pub const MAX_SEARCH_RESULTS: usize = 200;
pub const MAX_CONTENT_CHARS_PER_RESULT: usize = 20_000;
pub const MAX_GRAPH_HOPS: usize = 8;
pub const MAX_GIT_COMMITS: usize = 10_000;
pub const MAX_TEMPORAL_RESULTS: usize = 200;
pub const MAX_TEMPORAL_MIN_FREQUENCY: usize = 1_000;
pub const MAX_DREAM_PAIRS: usize = 500;
pub const MAX_DIFF_LIMIT: usize = 200;
pub const MAX_IMMUNE_TOP_K: usize = 200;

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
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindSymbolReferencesRequest {
    pub symbol_name: String,
    pub project_id: String,
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
    pub author_filter: Option<String>,
    #[serde(default)]
    pub date_after: Option<u64>,
    #[serde(default)]
    pub date_before: Option<u64>,
    #[serde(default = "default_limit_5")]
    pub limit: usize,
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
    #[serde(default)]
    pub wait: bool,
    #[serde(default = "default_max_pairs")]
    pub max_pairs: usize,
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
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ImmuneCheckRequest {
    pub project_id: String,
    pub code: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AntiPatternGuardRequest {
    pub project_id: String,
    pub code: String,
    #[serde(default = "default_top_k")]
    pub limit: usize,
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

impl GraphSearchRequest {
    pub fn sanitized_max_results(&self) -> usize {
        self.max_results.clamp(1, MAX_SEARCH_RESULTS)
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
    pub fn sanitized_max_pairs(&self) -> usize {
        self.max_pairs.clamp(1, MAX_DREAM_PAIRS)
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
}

impl UpdateProjectRequest {
    pub fn sanitized_max_commits(&self) -> usize {
        self.max_commits.clamp(1, MAX_GIT_COMMITS)
    }
}
