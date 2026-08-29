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
    /// Keep in sync with `from_registry_str`, which already accepted
    /// "csharp" — the serde alias list had drifted behind it.
    #[serde(
        alias = "csharp",
        alias = "c#",
        alias = "cs",
        alias = "c_sharp",
        alias = "dotnet_csharp"
    )]
    CSharp,
    /// C++ projects.
    #[serde(alias = "c++", alias = "cxx")]
    Cpp,
    /// C projects.
    #[serde(alias = "ansi_c")]
    C,
    /// MiniLang — native systems language compiled by MiniLangCompiler.
    /// Indexes `.ml`/`.mlinc` alongside the polyglot compiler sources and
    /// conformance-test goldens that share the repository.
    ///
    /// `minilang` (no underscore) is REQUIRED, not decorative: `rename_all =
    /// "snake_case"` makes the canonical wire name `mini_lang`, but
    /// `as_str()` below returns `"minilang"`. Without this alias the type
    /// cannot round-trip its own output — a real `index_project` call was
    /// rejected with "unknown variant `minilang`" on 2026-07-28. Same drift
    /// the `CSharp` comment above records.
    #[serde(alias = "mini_lang", alias = "minilang", alias = "ml")]
    MiniLang,
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
            Self::MiniLang => "minilang",
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
        } else if ["minilang", "mini_lang", "ml"]
            .iter()
            .any(|x| v.eq_ignore_ascii_case(x))
        {
            Some(Self::MiniLang)
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
pub struct RefreshCorporaRequest {
    pub project_id: String,
    /// Optional Azure DevOps PAT. When present (together with the three
    /// ado_* fields), a fourth refresh stage runs: incremental
    /// code-review-history ingest (anti-pattern clusters + wontFix
    /// suppressions) continuing from the registry's last_pr_id marker —
    /// repeat calls only process new PRs. The token is never logged and
    /// never persisted; omitting it skips the stage (the corpora then
    /// rot until the next explicit ingest_code_review_history call).
    #[serde(default)]
    pub pat_token: Option<String>,
    /// Azure DevOps organisation (required when pat_token is set).
    #[serde(default)]
    pub ado_org: Option<String>,
    /// Azure DevOps project (required when pat_token is set).
    #[serde(default)]
    pub ado_project: Option<String>,
    /// Azure DevOps repository (required when pat_token is set).
    #[serde(default)]
    pub ado_repo: Option<String>,
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

/// P0-5: staleness visibility for agents.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetIndexFreshnessRequest {
    pub project_id: String,
    /// Also stat the project directory and count files modified since the
    /// last index completed. Costs one directory walk; default true.
    #[serde(default = "default_true")]
    pub check_disk: bool,
}

/// Planning: every touchpoint of a domain concept, grouped by role.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetConceptFootprintRequest {
    pub project_id: String,
    /// Domain term to map, e.g. "photo", "code category", "OrderStatus".
    pub concept: String,
    /// Cap per output group. Default 8, ceiling 500; a cut is always
    /// reported as "… and N more" — raise this to see a whole section.
    #[serde(default = "default_footprint_group_cap")]
    pub max_per_group: usize,
}

pub fn default_footprint_group_cap() -> usize {
    // Was 15 (×2 for the consumers group = ~17 KB, the largest single response a
    // review still emitted). 8 keeps the map readable while halving the payload;
    // callers wanting the full fan-out pass a larger max_per_group.
    8
}

/// Planning: historical changes most similar to a planned file set, plus the
/// recurring companion artifacts missing from it.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindSimilarChangesRequest {
    pub project_id: String,
    /// Files you plan to change or have changed (project-relative paths;
    /// they do not need to exist yet).
    pub files: Vec<String>,
    /// How many recent commits to scan. Default 500.
    #[serde(default = "default_similar_max_commits")]
    pub max_commits: usize,
    /// How many similar commits to report. Default 5.
    #[serde(default = "default_similar_top")]
    pub top: usize,
}

pub fn default_similar_max_commits() -> usize {
    500
}
pub fn default_similar_top() -> usize {
    5
}

impl FindSimilarChangesRequest {
    pub fn sanitized_max_commits(&self) -> usize {
        self.max_commits.clamp(10, 5_000)
    }
    pub fn sanitized_top(&self) -> usize {
        self.top.clamp(1, 20)
    }
}

/// Planning: permission checks + settings that gate an area of the codebase.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MapGuardsAndSettingsRequest {
    pub project_id: String,
    /// File path, directory prefix, or exact function name to scope the
    /// analysis to. Omit for a project-wide view.
    #[serde(default)]
    pub scope: Option<String>,
    /// Return the full report (every function's verdict, full lists,
    /// coverage) as JSON instead of markdown.
    #[serde(default)]
    pub output_json: bool,
}

/// One external review finding (CTO comment, SonarQube issue, CodeRabbit nit).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReviewFindingIn {
    /// Project-relative file the finding applies to (optional).
    #[serde(default)]
    pub file: Option<String>,
    /// Rule/check identifier (e.g. "csharpsquid:S2076", "cto:always-audit").
    #[serde(default)]
    pub rule: Option<String>,
    /// The finding text — what was wrong and what to do instead.
    pub message: String,
    /// blocker|critical|major|minor|info (free-form accepted).
    #[serde(default)]
    pub severity: Option<String>,
}

/// Ingest external review findings into the anti-pattern index so
/// immune_check / pre_commit_review catch the same mistake next time.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestReviewFindingsRequest {
    pub project_id: String,
    /// Findings as a list (CTO comments, manual entries).
    #[serde(default)]
    pub findings: Option<Vec<ReviewFindingIn>>,
    /// Raw SonarQube issues export JSON (`{"issues":[...]}` shape from
    /// /api/issues/search). Parsed in addition to `findings`.
    #[serde(default)]
    pub sonarqube_json: Option<String>,
    /// Promote blocker/critical findings with a file to repo rules. Default true.
    #[serde(default = "default_true")]
    pub promote_rules: bool,
}

/// Generate the Claude Code integration pack (workflow rules + reminder hooks).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerateAgentIntegrationRequest {
    pub project_id: String,
    /// Write `.claude/rules/engram-workflow.md`, `.claude/settings.json`
    /// and `AGENTS.md` into the project (never overwrites an existing
    /// settings.json or AGENTS.md; the `.mcp.json` entry is only emitted).
    /// Default false: return contents.
    #[serde(default)]
    pub write_files: bool,
    /// Generate Windows (PowerShell) hook commands. Default true.
    #[serde(default = "default_true")]
    pub windows: bool,
}

/// Planning: one-call implementation brief for a (one-line) user story.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanUserStoryRequest {
    pub project_id: String,
    /// The user story, verbatim — e.g. "As an admin I would like to set
    /// minimum number of photos required".
    pub story: String,
    /// Override the automatically extracted domain concepts (max 3 used).
    #[serde(default)]
    pub concepts: Option<Vec<String>>,
}

/// Planning: the ranked, co-change-confirmed, family-complete set of files a
/// user story is likely to require — one call. Concept-footprint + git co-change
/// + structural graph, fused with co-change-first ranking, .NET family expansion,
/// and vendor-noise filtering. (Validated on the pilot eval: this packaging flips
/// Engram from hurting to helping a code-gen agent.)
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetChangeSetRequest {
    pub project_id: String,
    /// Point-in-time replay: only surface approved exemplars merged
    /// STRICTLY BEFORE this date (YYYY-MM-DD). Keeps evaluations and
    /// historical replays leak-free. Omit for the full corpus.
    #[serde(default)]
    pub merged_before: Option<String>,
    /// The user story, verbatim.
    pub story: String,
    /// Override the automatically extracted domain concepts (max 3 used).
    #[serde(default)]
    pub concepts: Option<Vec<String>>,
    /// The FULL work-item text (bug report / US description / acceptance
    /// criteria) when the story references one. Input parity is the #1
    /// one-shot lever measured (arm-B run 3 vs 4: F1 22 -> 71 from this
    /// alone): a bare title under-determines the fix; the full item names
    /// the defect class. Merged into concept extraction and rendered as
    /// its own dossier section.
    #[serde(default)]
    pub work_item_text: Option<String>,
    /// Azure DevOps PAT for AUTO-FETCHING the work item when the story
    /// references an id (e.g. "Bug #847") and `work_item_text` is not
    /// provided. Per-call only — never persisted (same stance as
    /// refresh_corpora). When omitted, the server falls back to its own
    /// `ADO_PAT` env var, so live agent sessions (which never hold
    /// credentials) still get input parity. Org/project default from the
    /// coordinates saved by the last refresh_corpora stage-4 run.
    #[serde(default)]
    pub pat_token: Option<String>,
    /// Return the structured JSON payload (concepts, per-file evidence with
    /// rationale, per-arm coverage, omissions) instead of markdown.
    /// Default false.
    #[serde(default)]
    pub output_json: bool,
    /// Also retrieve on the story's index-corroborated entity names
    /// (parenthesized glosses, noun phrases, compound splits) on top of the
    /// three document-order concepts. Off by default: on the 5-PR gate the
    /// extra concepts inflated the weak tier past the tail cap and cost
    /// recall (89.2% -> 86.5%). The candidates are always REPORTED in
    /// `coverage.concept_candidates`.
    #[serde(default)]
    pub expand_concepts: bool,
}

/// Stage-3 quality gates: ingest a project's accumulated "what to avoid" knowledge
/// (coding/agent rules, copilot-instructions.md, CodeRabbit/SonarQube findings,
/// the DevOps recurring-issues board) into a searchable `quality_gate` namespace.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestQualityGatesRequest {
    pub project_id: String,
    /// Project-relative path to the source file to ingest.
    pub source_path: String,
    /// Source kind: copilot | rules | coderabbit | sonarqube | board | text.
    pub source_type: String,
    /// When true, EVERY existing rule in the project's `quality_gate` namespace
    /// is purged before this source is ingested (all sources, not just this
    /// file) — the way to replace a corpus. Re-ingesting without it accumulates
    /// (rules dedup by content).
    #[serde(default)]
    pub clear_existing: bool,
}

/// Stage-3 distillation: turn a raw finding corpus (CodeRabbit/SonarQube history)
/// into GENERIC, deduplicated project rules via LLM summarization of clustered
/// findings, then index them into the `quality_gate` namespace. Use this for
/// finding corpora (coderabbit/sonarqube); use ingest_quality_gates for already-
/// generic sources (copilot-instructions, the board).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DistillQualityGatesRequest {
    pub project_id: String,
    /// Project-relative path to the raw findings source (JSON or markdown).
    pub source_path: String,
    /// Source kind: coderabbit | sonarqube | board | text | copilot | rules.
    pub source_type: String,
    /// Findings per LLM batch (clamped 10..=200, default 50).
    #[serde(default = "default_distill_batch_size")]
    pub batch_size: usize,
    /// Max concurrent LLM calls (clamped 1..=12, default 6).
    #[serde(default = "default_distill_concurrency")]
    pub max_concurrent: usize,
}

fn default_distill_batch_size() -> usize {
    50
}

fn default_distill_concurrency() -> usize {
    6
}

/// Stage-3 pre-push audit: retrieve the quality-gate rules most relevant to a
/// proposed change so the agent can fix known issues BEFORE the first push.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrePushAuditRequest {
    pub project_id: String,
    /// The proposed code or unified diff to audit.
    pub code: String,
    /// Optional path of the file being changed — rules scoped to it rank first.
    #[serde(default)]
    pub file_path: Option<String>,
    /// Max rules to return (default 12, max 50).
    #[serde(default = "default_audit_top_k")]
    pub top_k: usize,
}

fn default_audit_top_k() -> usize {
    12
}

/// Planning: concrete exemplars of how this codebase implements a pattern.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindImplementationPatternRequest {
    pub project_id: String,
    /// What you want examples of, e.g. "admin settings page save",
    /// "dropdown bound to lookup table", "file upload validation".
    pub pattern_query: String,
    /// How many exemplar files to expand. Default 3.
    #[serde(default = "default_pattern_examples")]
    pub max_examples: usize,
    /// Return the full result (exemplars, shapes, common shapes, coverage)
    /// as JSON instead of markdown.
    #[serde(default)]
    pub output_json: bool,
}

pub fn default_pattern_examples() -> usize {
    3
}

/// TODO-28: one natural-language entry point. All fields past `question` are
/// additive + serde-defaulted, so the legacy `{project_id, question}` call still
/// deserializes unchanged. `Default` lets call sites use `..Default::default()`
/// (empty depth/output_format parse to standard/markdown).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskCodebaseRequest {
    pub project_id: String,
    /// Any question about the codebase, in plain language.
    pub question: String,
    /// Optional conversation/session id (carried for future working memory).
    #[serde(default)]
    pub session_id: Option<String>,
    /// Optional current task / user story for context.
    #[serde(default)]
    pub task_context: Option<String>,
    /// Pin the answer to a branch/commit (branch is advisory in M1).
    #[serde(default)]
    pub as_of: Option<AsOf>,
    /// Who is asking (role/permissions — carried for future ACL).
    #[serde(default)]
    pub audience: Option<Audience>,
    /// "quick" | "standard" (default) | "deep" — controls arm breadth + budget.
    #[serde(default = "default_ask_depth")]
    pub depth: String,
    /// "best_effort" (default) | "require_current" — gate on freshness.
    #[serde(default = "default_ask_freshness")]
    pub freshness_policy: String,
    /// "markdown" (default) | "json" | "both".
    #[serde(default = "default_ask_output")]
    pub output_format: String,
    /// Overall retrieval deadline in ms (default 15000, clamped 1000..60000).
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AsOf {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Audience {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
}

fn default_ask_depth() -> String {
    "standard".into()
}
fn default_ask_freshness() -> String {
    "best_effort".into()
}
fn default_ask_output() -> String {
    "markdown".into()
}

/// TODO-29: open an edit session — snapshot intent before editing.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BeginEditSessionRequest {
    pub project_id: String,
    /// Files you INTEND to edit.
    pub planned_files: Vec<String>,
    /// Optional one-line description of the change.
    #[serde(default)]
    pub story: Option<String>,
}

/// TODO-29: close an edit session — verify completeness against intent.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteEditSessionRequest {
    pub project_id: String,
    /// Files actually edited. When omitted, the session's planned set is
    /// checked as-is.
    #[serde(default)]
    pub edited_files: Vec<String>,
    /// Optional: the get_change_set dossier this implementation was based
    /// on. When provided, the completion check RECONCILES the edited set
    /// against the dossier's own obligations — the file references inside
    /// its structured sections (co-change partners, resx families,
    /// history/log tables, permission gates, sibling controls) — and
    /// names every obligation the diff left unmet. This turns the dossier
    /// from advice into a contract; unmet items are the classic one-shot
    /// gaps.
    #[serde(default)]
    pub dossier: Option<String>,
}

/// TODO-29: edit-completeness check over an edited file set.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DetectIncompleteChangesRequest {
    pub project_id: String,
    /// Project-relative paths the agent has edited (or plans to commit).
    pub edited_files: Vec<String>,
    /// Max co-change partners to inspect per file. Default 5.
    #[serde(default = "default_partner_limit")]
    pub max_partners: usize,
}

pub fn default_partner_limit() -> usize {
    5
}

/// TODO-20: dependency cycle (SCC) inventory.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindDependencyCyclesRequest {
    pub project_id: String,
    /// Smallest component size to report. Default 2 (any real cycle).
    #[serde(default = "default_cycle_min_size")]
    pub min_size: usize,
    /// Maximum components to report. Default 20.
    #[serde(default = "default_cycle_limit")]
    pub limit: usize,
}

pub fn default_cycle_min_size() -> usize {
    2
}

pub fn default_cycle_limit() -> usize {
    20
}

/// TODO-14: shortest connection path between two graph identities.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindConnectionPathRequest {
    pub project_id: String,
    /// Start: node_id, symbol name, or FQN.
    pub from: String,
    /// Target: node_id, symbol name, or FQN.
    pub to: String,
    /// Maximum hops to search. Default 6.
    #[serde(default = "default_path_max_depth")]
    pub max_depth: usize,
}

pub fn default_path_max_depth() -> usize {
    6
}

/// P0-8: convert any Engram identifier into all its other identities.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveIdRequest {
    pub project_id: String,
    /// Any identifier: a graph node_id (e.g. "sym:...", "file:..."), a
    /// symbol name or FQN, or a search doc_id.
    pub id: String,
    /// Namespace used when `id` is tried as a doc_id. Default "memory".
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
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

// -------------------- Settings intelligence --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListSettingsRequest {
    pub project_id: String,
    /// Only settings declared in files whose path contains this substring.
    #[serde(default)]
    pub scope: Option<String>,
    /// Max settings rendered per category. Default 25 (raise up to 500 for
    /// the exhaustive dump) — the pilot corpus's full catalog at 100/category was
    /// ~58K chars, too heavy for a planning-phase call.
    #[serde(default = "default_settings_per_category")]
    pub max_per_category: usize,
}

fn default_settings_per_category() -> usize {
    25
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeTableRequest {
    pub project_id: String,
    /// Table name (lowercase, as stored in the graph).
    pub table: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeSettingRequest {
    pub project_id: String,
    /// Setting name (dotted store paths like ConfigSettings.Multitenant.IsMaster work).
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeriveTestMatrixRequest {
    pub project_id: String,
    /// The changed/planned files. The matrix derives from the settings,
    /// role gates, and shared-state keys wired to THESE files' methods.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetSettingRequest {
    pub project_id: String,
    /// Setting name (web.config key, connection string, Session/Application
    /// key, or settings-store member). Exact match preferred; substring ok.
    pub name: String,
}

// -------------------- Merged-work corpus --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestMergedPrsRequest {
    pub project_id: String,
    /// Ignore the incremental watermark and re-walk/re-render the whole
    /// corpus (stable pr:<id> pks make this an in-place upsert). Use after
    /// upgrades that change the PR doc format or generation scheme.
    #[serde(default)]
    pub rebuild: bool,
    /// Max first-parent commits to walk on the FIRST ingest (later runs are
    /// incremental from a watermark). Default 5000.
    #[serde(default = "default_pr_ingest_max_commits")]
    pub max_commits: usize,
    /// Leak-free replay: only ingest commits merged STRICTLY BEFORE this
    /// date (YYYY-MM-DD). Point-in-time eval snapshots use this so the
    /// corpus (and every gate mining it) knows nothing past the snapshot.
    #[serde(default)]
    pub merged_before: Option<String>,
}

fn default_pr_ingest_max_commits() -> usize {
    5000
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindMergedWorkRequest {
    pub project_id: String,
    /// Story/task description or domain terms (matched against PR titles,
    /// bodies, file paths, and domains).
    pub story: String,
    /// Ultra-coarse change-kind filter: ui-markup | ui-code | js | database |
    /// settings | resources | api | backend. Omit for all kinds.
    #[serde(default)]
    pub kind: Option<String>,
    /// How many exemplar PRs to return. Default 3.
    #[serde(default = "default_find_merged_top")]
    pub top: usize,
    /// Point-in-time replay: only exemplars merged STRICTLY BEFORE this
    /// date (YYYY-MM-DD). Use for leak-free evaluation or "what did the
    /// team know at the time" questions. Omit for the full corpus.
    #[serde(default)]
    pub merged_before: Option<String>,
}

fn default_find_merged_top() -> usize {
    3
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
    /// Skip this many ranked results before returning `max_results` —
    /// page 2 is `offset: 10`. Ranking is deterministic for a fixed
    /// index generation, so pages don't overlap. Default: 0.
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_true")]
    pub use_mmr: bool,
    /// Full-text search mode. Default: "strict".
    #[serde(default)]
    pub fts_mode: FtsMode,
    /// Include chunk content bodies (up to `max_content_chars_per_result`
    /// each) in every hit. Default: false — hits carry a 500-char snippet
    /// already; fetch full source for the hits that matter via
    /// `get_chunk(doc_id)`. Setting this true on a default 10-result
    /// search adds ~3K tokens per call.
    #[serde(default)]
    pub include_content: bool,
    #[serde(default = "default_max_content_chars")]
    pub max_content_chars_per_result: usize,
    #[serde(default)]
    pub include_path_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_path_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub language_filters: Option<Vec<String>>,
    /// NOT IMPLEMENTED - supplying it is rejected rather than ignored.
    /// The index has no generic metadata field to filter on. Use
    /// `include_path_prefixes` / `exclude_path_prefixes` /
    /// `language_filters`, which are applied.
    #[serde(default)]
    pub metadata_filter: Option<serde_json::Value>,
    /// Which namespaces to search. `"code"` (default) — the `memory`
    /// namespace (or an explicit `namespace` override); today's behaviour,
    /// unchanged. `"knowledge"` — all curated knowledge namespaces
    /// (memory_bank, insights, business_logic, antipattern, wontfix_patterns,
    /// quality_gate), fused by rank and labelled by source. `"all"` — code
    /// plus knowledge. When scope is knowledge/all the `namespace` field is
    /// ignored.
    #[serde(default = "default_search_scope")]
    pub search_scope: String,
    /// Only return hits whose indexed author matches (history / PR docs carry
    /// an author; code and memory notes do not). Default: none.
    #[serde(default)]
    pub author_filter: Option<String>,
    /// Only return hits with an indexed timestamp at or after this unix-ms
    /// value. Default: none. Useful for "insights from this week".
    #[serde(default)]
    pub date_after: Option<u64>,
    /// Only return hits with an indexed timestamp before this unix-ms value.
    /// Default: none.
    #[serde(default)]
    pub date_before: Option<u64>,
    /// When `false`, short-circuits the hybrid pipeline: skips vector
    /// search, RRF fusion, and MMR reranking, returning pure FTS
    /// results ranked by BM25. Use this when you want to find where
    /// a literal identifier appears and don't need semantic matching.
    /// Default: `true` (full hybrid search).
    #[serde(default = "default_true")]
    pub semantic: bool,
    /// Also fold in USER-LEVEL memory (the reserved `__user__` project) when
    /// the scope covers knowledge. This is how a standing preference recorded
    /// once surfaces in every project's recall. Default: true. No effect on
    /// `code` scope, or when searching the user project itself.
    #[serde(default = "default_true")]
    pub include_user_memory: bool,
}

/// Fast literal / regex search over the indexed file set. Prefilters
/// via the existing Tantivy trigram index and verifies the actual
/// literal inside each candidate chunk — beats ripgrep on warm queries
/// because the index already knows which chunks contain the token.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepProjectRequest {
    pub project_id: String,
    pub pattern: String,
    /// Treat `pattern` as a regex. Default: false (literal).
    #[serde(default)]
    pub regex: bool,
    /// Force case-sensitive or case-insensitive. When omitted, smart
    /// case applies: case-insensitive unless the pattern contains any
    /// uppercase ASCII character.
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// Regex-only: let `.` match newlines. Default: false.
    #[serde(default)]
    pub multiline: bool,
    /// Only scan files whose indexed path starts with this prefix.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Only scan chunks in this language.
    #[serde(default)]
    pub language: Option<String>,
    /// Lines of context before each match. Default: 0.
    #[serde(default)]
    pub context_before: usize,
    /// Lines of context after each match. Default: 0.
    #[serde(default)]
    pub context_after: usize,
    /// Cap on returned matches. Default: 200.
    #[serde(default = "default_grep_max_results")]
    pub max_results: usize,
    /// How to handle staleness between the index and disk:
    /// `"strict"` (default) — fingerprint every tracked file, surface
    /// stale paths in the result; `"warn"` — note staleness but don't
    /// prioritise re-scanning; `"off"` — skip the check entirely.
    #[serde(default = "default_grep_freshness")]
    pub freshness: String,
    /// Namespace to search. Default: "memory" (same as search tools).
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    /// Return structured JSON instead of rendered Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
}

fn default_grep_max_results() -> usize {
    // A review greps high-frequency tokens (`.Contains(`, `pr_id`, `innerHTML`)
    // and only needs to know WHERE a pattern lives, not 200 instances. At 200,
    // a single common-token grep returned ~40 KB; a review makes DOZENS, and the
    // accumulated tool output (re-sent every agent turn) blew the model's request
    // budget (HTTP 400). 20 locates a pattern or a handful of call sites; callers
    // who want an exhaustive sweep pass a larger `max_results` explicitly.
    20
}

fn default_grep_freshness() -> String {
    "strict".into()
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

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateMemoryBankRequest {
    pub project_id: String,
    #[serde(default)]
    pub section_id: Option<String>,
    /// The section's human title.
    pub section: String,
    /// Markdown body. Long bodies are chunked so search can hit any part.
    pub content: String,
    /// Append `content` to the existing section (with a newline) instead of
    /// replacing it. On a section that does not exist yet, behaves as a
    /// create. Default: false (replace).
    #[serde(default)]
    pub append: bool,
    /// Optimistic-concurrency guard. If set and the section already exists
    /// with a different `updated_at_ms`, the write is REJECTED as a conflict
    /// so a concurrent session's change is not silently clobbered. Read the
    /// section, then pass the `updated_at_ms` you saw.
    #[serde(default)]
    pub expected_updated_at_ms: Option<u64>,
    /// The writing agent / session, recorded on the section.
    #[serde(default)]
    pub author: Option<String>,
    /// One of: preference, decision, gotcha, reference, note. Rejected if it
    /// is anything else. Default: unset.
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional review-by date (unix ms).
    #[serde(default)]
    pub review_after_ms: Option<u64>,
    /// Free-form tags. Replaces the section's tags when provided.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Files (project-relative paths) or symbol node_ids this memory is
    /// about. Replaces the section's list when provided. Drives the
    /// staleness signal: a note is flagged stale when a referenced file is
    /// re-indexed after the note was last written.
    #[serde(default)]
    pub related_files: Option<Vec<String>>,
}

/// The controlled vocabulary for `MemorySection::kind`.
pub const MEMORY_KINDS: &[&str] = &["preference", "decision", "gotcha", "reference", "note"];

/// Restore or copy a memory section from portable markdown (as produced by
/// `export_capture_pack`). The front-matter's created_at / kind / author /
/// tags / related_files are preserved, so a restored note keeps its identity.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportMemoryBankRequest {
    pub project_id: String,
    /// The section as portable markdown (a `---` front-matter block then the
    /// body). A plain body with no front-matter is imported as content only.
    pub markdown: String,
    /// Overrides the section_id from the markdown (e.g. to copy a note under a
    /// new id, or when the markdown carries none).
    #[serde(default)]
    pub section_id: Option<String>,
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
    // Agents routinely pass the generic `name`/`table`; accept them so a
    // wrong guess doesn't hard-fail under deny_unknown_fields (knowledge-
    // pack friction 2026-07-06).
    #[serde(alias = "name", alias = "table")]
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
    /// Ignore the last/oldest watermarks and re-walk history from HEAD.
    /// Use after a wipe-reindex: the graph edges are deleted but the
    /// watermarks survive in the registry, so a normal run reports
    /// "fully indexed" while the temporal data is gone.
    #[serde(default)]
    pub force: bool,
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
    /// Reserved: edges are not currently injected into the response, so this
    /// is a no-op whichever way it is set. Kept for forward compatibility.
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
    /// NOT IMPLEMENTED - supplying it is rejected rather than ignored, so
    /// the blueprint is never presented as filtered when it is not. The BFS
    /// covers all edge kinds.
    #[serde(default)]
    pub include_edge_kinds: Option<Vec<String>>,
    /// Reserved: the BFS does not currently skip dead-code nodes, so this is
    /// a no-op whichever way it is set. Kept for forward compatibility.
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
    /// Generation to GC up to (exclusive). If omitted: the GRAPH is purged below the
    /// last full-index generation (incremental generations must never purge the
    /// graph) and the search index against active_generation.
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

    /// Clamp `offset` so `offset + max_results` never exceeds the engine's
    /// hard result cap — pages past the cap return empty rather than
    /// re-ranking the world.
    pub fn sanitized_offset(&self) -> usize {
        self.offset
            .min(MAX_SEARCH_RESULTS.saturating_sub(self.sanitized_max_results()))
    }

    pub fn sanitized_max_content_chars_per_result(&self) -> usize {
        self.max_content_chars_per_result
            .clamp(1, MAX_CONTENT_CHARS_PER_RESULT)
    }
}

fn default_search_scope() -> String {
    "code".into()
}

impl Default for SearchMemoryRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            project_id: String::new(),
            namespace: default_namespace_memory(),
            max_results: default_top_k(),
            offset: 0,
            use_mmr: true,
            fts_mode: FtsMode::default(),
            include_content: false,
            max_content_chars_per_result: default_max_content_chars(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
            semantic: true,
            search_scope: default_search_scope(),
            author_filter: None,
            date_after: None,
            date_before: None,
            include_user_memory: true,
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
    /// NOT IMPLEMENTED - supplying it is rejected rather than ignored, so a
    /// gate verdict is never reported as evidence-backed when the evidence
    /// was dropped. Use the boolean `has_runtime_evidence`.
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
    /// Reserved: no analysis cache exists, so every call already re-analyzes
    /// and this is a no-op. Kept for forward compatibility.
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
    #[serde(alias = "name", alias = "sp")]
    pub sp_name: String,
    /// Reserved: no analysis cache exists, so every call already re-analyzes
    /// and this is a no-op. Kept for forward compatibility.
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

/// Token-economy default for the pre-edit oracle: 3 callers, signatures
/// only. `get_method_edit_context` is the tool agents are told to call
/// before EVERY method edit; with the old defaults (50 callers, full
/// bodies) a well-connected method returned tens of thousands of tokens
/// per call. Agents that genuinely need caller bodies opt in explicitly.
fn default_max_callers_3() -> usize {
    3
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
    #[serde(alias = "name", alias = "method")]
    pub method_name: String,
    /// Class name (optional, for disambiguation).
    #[serde(default)]
    pub class_name: Option<String>,
    /// Include the complete, untruncated method body. Default: true.
    #[serde(default = "default_true")]
    pub include_full_body: bool,
    /// Include full bodies of the top N callers. Default: false —
    /// callers are listed as signature + file:line; set true only when
    /// you need to read caller implementations (large output).
    #[serde(default)]
    pub include_caller_bodies: bool,
    /// Maximum callers to include. Default: 3. Raise deliberately when
    /// auditing a hot method's full fan-in.
    #[serde(default = "default_max_callers_3")]
    pub max_callers: usize,
    /// Include business-rule evidence for this method from the
    /// `business_logic` namespace (populated by `analyze_business_logic`).
    /// Default: true.
    #[serde(default = "default_true")]
    pub include_business_logic: bool,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
    /// Start line of the overload to analyze when the same class declares
    /// the method more than once (the AMBIGUOUS error lists them).
    #[serde(default)]
    pub line: Option<u32>,
}

/// Full page context: control tree, all event handlers with bodies, data layer,
/// state, AJAX, validation, auth, and coding style for a WebForms page.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetPageContextRequest {
    pub project_id: String,
    /// Path to the .aspx/.ascx/.master file.
    pub aspx_file: String,
    /// Include full bodies of all event handlers. Default: false — a page
    /// with 30 handlers rendered ~50K chars of source; the summary
    /// (controls, handler signatures, effects, data layer) is what page
    /// orientation needs. Fetch specific bodies with get_full_method_body.
    #[serde(default)]
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
    #[serde(alias = "name", alias = "method")]
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
    #[serde(alias = "name", alias = "method")]
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
    #[serde(alias = "name", alias = "method")]
    pub method_name: String,
    /// Optional class name for disambiguation.
    #[serde(default)]
    pub class_name: Option<String>,
    /// Output as JSON instead of Markdown. Default: false.
    #[serde(default)]
    pub output_json: bool,
    /// Start line of the overload to analyze when the same class declares
    /// the method more than once (the AMBIGUOUS error lists them).
    #[serde(default)]
    pub line: Option<u32>,
}

// ── produce_claude_md ──────────────────────────────────────────────────────

fn default_max_root_lines() -> usize {
    60
}

/// Generate `CLAUDE.md` (+ optional `AGENTS.md`) and a `.claude/rules/`
/// directory from the project's indexed graph. Language-agnostic —
/// sections are driven entirely by what the graph contains.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProduceClaudeMdRequest {
    pub project_id: String,
    /// Merge engram-derived rules with any existing
    /// `CLAUDE.md` / `AGENTS.md` at the project root. Human-authored
    /// rules take priority on conflicts; engram attaches quantitative
    /// evidence rather than duplicating. Default: true.
    #[serde(default = "default_true")]
    pub merge_existing: bool,
    /// Also generate `AGENTS.md` for cross-tool interoperability
    /// (Codex / Copilot / Cursor). Default: false.
    #[serde(default)]
    pub generate_agents_md: bool,
    /// Write files to the project directory. Creates
    /// `<project_dir>/CLAUDE.md` and `<project_dir>/.claude/rules/*`.
    /// When false, all generated content is returned as the tool's
    /// response text. Default: false.
    ///
    /// **Safety:** when an existing `CLAUDE.md` is found at the
    /// project root, this tool will NEVER overwrite it unless
    /// `overwrite_existing=true` is also set. By default the engram
    /// output is diverted to `CLAUDE.engram.md` so your hand-authored
    /// content is never clobbered.
    #[serde(default)]
    pub write_to_disk: bool,
    /// Maximum lines for the root `CLAUDE.md`. Default: 60 (the
    /// attention-budget sweet spot). Hard floor: 20. Hard cap: 300.
    #[serde(default = "default_max_root_lines")]
    pub max_root_lines: usize,
    /// Opt in to overwriting an existing `CLAUDE.md` at the project
    /// root. When false (default), a pre-existing CLAUDE.md is left
    /// untouched and the engram output is diverted to
    /// `CLAUDE.engram.md` instead. When true, the existing file is
    /// backed up to `CLAUDE.md.<unix_ts>.bak` BEFORE the overwrite,
    /// so the clobber is always recoverable.
    #[serde(default)]
    pub overwrite_existing: bool,
    /// How to combine the engram-generated output with the existing
    /// `CLAUDE.md`, when `overwrite_existing=true` AND a CLAUDE.md is
    /// present. Options:
    ///
    /// - `"splice"` (default) — preserve every byte of the existing
    ///   file; if the `<!-- engram:begin --> ... <!-- engram:end -->`
    ///   markers are present, replace their content with the new
    ///   engram block; otherwise append the engram block at the end.
    ///   Safe but may leave redundancy (engram critical-rules +
    ///   existing critical-rules both present).
    ///
    /// - `"optimize"` — section-level rewrite. Headings that engram
    ///   owns (Critical rules, Danger zones, language conventions)
    ///   are replaced with the fresh engram output. Headings that
    ///   engram does NOT own (domain context, architecture
    ///   decisions, onboarding) are preserved verbatim. Produces a
    ///   tighter CLAUDE.md without losing unique human insight.
    ///
    /// - `"replace"` — full overwrite with the engram-generated
    ///   content. Back-up still runs first (recoverable), but the
    ///   new file contains only engram output.
    #[serde(default = "default_merge_mode")]
    pub merge_mode: String,
    /// Opt into an LLM curation pass on the critical-rules section.
    /// When true, the deterministic rules pipeline (noise filter +
    /// keyword meta-clustering + render thresholds) runs first as
    /// normal, then its ~8 candidates are handed to the configured
    /// LLM backend with project context. The LLM drops edge-case
    /// noise the keyword filter missed, merges near-duplicates the
    /// clusterer split, and rewrites each surviving rule in
    /// project-idiomatic voice using vocabulary from the evidence
    /// (class names, framework helpers, etc).
    ///
    /// Results are cached in the registry keyed by
    /// `blake3(candidates + project_context)` — reruns against the
    /// same inputs spend zero tokens. On any LLM failure (no
    /// backend, timeout, parse error) the deterministic baseline
    /// is used untouched. Default: false.
    #[serde(default)]
    pub use_llm: bool,
}

fn default_merge_mode() -> String {
    "splice".into()
}

// ─── Code-review history ingestion ──────────────────────────────────────────

fn default_code_review_source() -> String {
    "json_file".into()
}
fn default_min_fix_rate() -> f32 {
    0.5
}
fn default_token_overlap() -> f32 {
    0.4
}
fn default_promote_fix_rate() -> f32 {
    0.7
}
fn default_promote_min_prs() -> usize {
    3
}
fn default_promote_lift() -> f32 {
    0.15
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestCodeReviewHistoryRequest {
    pub project_id: String,
    /// Source type: `"json_file"` (default) for a pre-scraped JSONL
    /// file, or `"azure_devops"` for live fetch via PAT.
    #[serde(default = "default_code_review_source")]
    pub source: String,
    /// For `json_file`: absolute or project-relative path to the JSONL
    /// file produced by the reference scraper. Ignored for other
    /// sources.
    #[serde(default)]
    pub file_path: Option<String>,
    /// For `azure_devops`: Personal Access Token. Never logged, never
    /// persisted to the registry — used only to drive the live fetch
    /// for this request.
    #[serde(default)]
    pub pat_token: Option<String>,
    /// Azure DevOps organisation name (live fetch only).
    #[serde(default)]
    pub org: Option<String>,
    /// Azure DevOps project name (live fetch only).
    #[serde(default)]
    pub project: Option<String>,
    /// Azure DevOps repo name (live fetch only).
    #[serde(default)]
    pub repo: Option<String>,
    /// Optional cap on the number of PRs fetched (newest-first).
    #[serde(default)]
    pub max_prs: Option<usize>,
    /// Minimum fix rate (fixed / (fixed + wontFix)) for a cluster to
    /// be indexed as a positive anti-pattern. WontFix clusters are
    /// always indexed into the suppression namespace regardless of
    /// this value. Default 0.5.
    #[serde(default = "default_min_fix_rate")]
    pub min_fix_rate: f32,
    /// Token overlap threshold for Jaccard clustering. Lower =
    /// fewer, larger clusters. Default 0.4.
    #[serde(default = "default_token_overlap")]
    pub token_overlap_threshold: f32,
    /// Force a full rescan — ignore the registry's last_pr_id marker.
    /// Use when you've rerun the scraper with a different filter and
    /// want to rebuild the index from scratch.
    #[serde(default)]
    pub force_full_rescan: bool,
    /// When true, classify ambiguous `closed` threads (resolved
    /// manually, no `✅ Addressed in commits` marker) via the
    /// configured `llm_backend`. Results are cached per-finding under
    /// `cr_llm:<hash>` in the registry so the classifier spends
    /// tokens at most once per unique finding across all runs.
    /// Off by default — the deterministic path works fine without it.
    #[serde(default)]
    pub use_llm_for_ambiguous: bool,
    /// Minimum fix rate required for a cluster to auto-promote to a
    /// `cr_*` repo rule (injected into chunk reads for files matching
    /// the cluster's file pattern). Default 0.7. Lower to 0.5 to be
    /// more permissive; raise to 0.9 to require near-unanimous agreement.
    #[serde(default = "default_promote_fix_rate")]
    pub promote_min_fix_rate: f32,
    /// Minimum number of distinct PRs a cluster must span before it
    /// auto-promotes to a `cr_*` repo rule. Default 3. Drop to 1 to
    /// treat every clean rule as a repo rule; raise to 5+ to require
    /// "pattern seen repeatedly across the team's work" before
    /// promotion.
    #[serde(default = "default_promote_min_prs")]
    pub promote_min_prs: usize,
    /// Author-adjusted promotion threshold: a cluster ALSO promotes to a repo
    /// rule when its fix-rate is at least this far ABOVE the mean of its
    /// authors' own baselines (the "lift"), fixed by ≥2 distinct authors across
    /// ≥2 PRs. De-confounds a corpus where one high-volume author dismisses most
    /// findings. Default 0.15; raise to require a bigger surprise, lower (even
    /// negative) to promote more permissively.
    #[serde(default = "default_promote_lift")]
    pub promote_min_lift: f32,
}

// ─── Support KB ──────────────────────────────────────────────────────────────

fn default_max_features() -> usize {
    50
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProduceSupportKbRequest {
    pub project_id: String,
    /// Write the generated cards to `<project>/support-kb/` (engram-owned
    /// directory, regenerated wholesale). Default false — the tool always
    /// returns the summary + index inline.
    #[serde(default)]
    pub write_to_disk: bool,
    /// Cap on generated feature cards. Default 50.
    #[serde(default = "default_max_features")]
    pub max_features: usize,
}

// ─── Explain change ──────────────────────────────────────────────────────────

fn default_explain_diff() -> String {
    "staged".into()
}
fn default_explain_style() -> String {
    "conventional".into()
}
fn default_explain_format() -> String {
    "markdown".into()
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExplainChangeRequest {
    pub project_id: String,
    /// Same shape as `pre_commit_review.diff`: a raw unified-diff
    /// string, one of `"staged"` / `"unstaged"` / `"head"`, or a
    /// path ending in `.patch` / `.diff`. Default `"staged"`.
    #[serde(default = "default_explain_diff")]
    pub diff: String,
    /// Commit-subject style. `"conventional"` → `feat(scope): …`.
    /// `"plain"` → natural prose (`Added in scope: …`). Default
    /// `"conventional"`.
    #[serde(default = "default_explain_style")]
    pub subject_style: String,
    /// Output format — `"markdown"` (default, human-readable) or
    /// `"json"` (structured schema for CI pipelines).
    #[serde(default = "default_explain_format")]
    pub output_format: String,
    /// Include a Keep-a-Changelog-formatted `### Added / Fixed /
    /// Changed` entry in the output bundle. Default `true`.
    #[serde(default = "default_true")]
    pub include_changelog: bool,
    /// Reserved for a future LLM polish pass. No-op in the current
    /// build — the deterministic renderer produces the final output
    /// directly.
    #[serde(default)]
    pub use_llm: bool,
}

// ─── Pre-commit review ───────────────────────────────────────────────────────

fn default_diff_source() -> String {
    "staged".into()
}
fn default_max_findings() -> usize {
    30
}
fn default_min_severity() -> String {
    "style".into()
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreCommitReviewRequest {
    pub project_id: String,
    /// The diff to review. Accepts:
    /// - A raw unified-diff string (`git diff` output)
    /// - `"staged"` — runs the `git diff --staged` equivalent via git2
    /// - `"unstaged"` — runs the working-tree diff via git2
    /// - `"head"` — runs the equivalent of `git diff HEAD~1`
    /// - A path ending in `.patch` or `.diff` — reads from disk
    ///
    /// Default: `"staged"`.
    #[serde(default = "default_diff_source")]
    pub diff: String,
    /// Maximum findings to return. Default 30.
    #[serde(default = "default_max_findings")]
    pub max_findings: usize,
    /// Minimum severity to include (`"critical"`, `"warning"`, `"info"`,
    /// `"style"`). Default `"style"` (include everything).
    #[serde(default = "default_min_severity")]
    pub min_severity: String,
    /// Skip specific gates by name — e.g. `["temporal", "audit"]`. Gate
    /// names: `immune`, `blast_radius`, `style`, `temporal`, `state`,
    /// `audit`, `antipattern`, `new_file`, `test_coverage`,
    /// `secret_leakage`.
    #[serde(default)]
    pub skip_gates: Vec<String>,
    /// Return the structured JSON payload instead of rendered markdown.
    /// CI integrations want `true`; humans want `false`. Default `false`.
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

#[cfg(test)]
mod project_type_round_trip_tests {
    use super::ProjectType;

    /// Every variant. Kept honest by `_exhaustiveness_guard` below: adding a
    /// variant forces a new match arm there, which puts a maintainer in this
    /// module with this list in view.
    const ALL: &[ProjectType] = &[
        ProjectType::DotnetWebformsCs,
        ProjectType::DotnetWebformsVb,
        ProjectType::General,
        ProjectType::Rust,
        ProjectType::CSharp,
        ProjectType::Cpp,
        ProjectType::C,
        ProjectType::MiniLang,
    ];

    fn _exhaustiveness_guard(pt: ProjectType) {
        match pt {
            ProjectType::DotnetWebformsCs
            | ProjectType::DotnetWebformsVb
            | ProjectType::General
            | ProjectType::Rust
            | ProjectType::CSharp
            | ProjectType::Cpp
            | ProjectType::C
            | ProjectType::MiniLang => {}
        }
    }

    /// `as_str()` is what this type reports itself as; serde is what it
    /// accepts. When those drift, a caller that reads a project type back and
    /// feeds it to `index_project` gets "unknown variant" for a name the type
    /// itself produced.
    ///
    /// This is not hypothetical. `rename_all = "snake_case"` makes the
    /// canonical wire name `mini_lang`, but `MiniLang::as_str()` returns
    /// `minilang`, and no alias covered it — a real `index_project` call was
    /// rejected on 2026-07-28 with:
    ///
    /// ```text
    /// unknown variant `minilang`, expected one of ... `mini_lang`, `ml`
    /// ```
    ///
    /// The `CSharp` variant carries a comment recording the SAME drift
    /// (`from_registry_str` accepted `csharp` while the serde list did not),
    /// so this had already happened once before it happened to MiniLang.
    /// Pinned for every variant rather than just the two known offenders.
    #[test]
    fn as_str_deserializes_back_to_the_same_variant() {
        for pt in ALL {
            let s = pt.as_str();
            let json = format!("\"{s}\"");
            let back: ProjectType = serde_json::from_str(&json).unwrap_or_else(|e| {
                panic!(
                    "ProjectType::{pt:?}.as_str() == {s:?}, but serde REJECTS that \
                     string: {e}\nThe type cannot round-trip its own output. Add \
                     `#[serde(alias = {s:?})]` to the variant."
                )
            });
            assert_eq!(
                back, *pt,
                "{s:?} deserialized to {back:?}, not {pt:?} — two variants claim \
                 the same wire name"
            );
        }
    }

    /// The registry path must accept `as_str()` output too: registry records
    /// persist exactly that string, so a mismatch makes stored projects
    /// unopenable.
    #[test]
    fn as_str_parses_back_through_from_registry_str() {
        for pt in ALL {
            let s = pt.as_str();
            assert_eq!(
                ProjectType::from_registry_str(s),
                Some(*pt),
                "from_registry_str({s:?}) failed to return {pt:?}; a persisted \
                 registry record written from as_str() would not load"
            );
        }
    }

    /// The serialized form must also be accepted back. `Serialize` emits the
    /// `rename_all` name (`mini_lang`), which is a DIFFERENT string from
    /// `as_str()` (`minilang`) — both must work, and this pins the second one.
    #[test]
    fn serialized_form_deserializes_back_to_the_same_variant() {
        for pt in ALL {
            let json = serde_json::to_string(pt).expect("ProjectType serializes");
            let back: ProjectType = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("{pt:?} serialized to {json} but will not parse: {e}"));
            assert_eq!(back, *pt);
        }
    }

    /// The exact string that failed in production.
    #[test]
    fn index_project_accepts_the_minilang_spelling_that_was_rejected() {
        for spelling in ["minilang", "mini_lang", "ml"] {
            let json = format!("\"{spelling}\"");
            let parsed: ProjectType = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("project_type {spelling:?} must parse: {e}"));
            assert_eq!(parsed, ProjectType::MiniLang);
        }
    }
}

/// External audit 2026-08-29 (auditor P0 #6): the tiered tool surface.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListAdvancedToolsRequest {
    /// Optional case-insensitive substring on tool name or description.
    #[serde(default)]
    pub filter: Option<String>,
}
