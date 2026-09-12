//! Phase 38: The Access Layer
//!
//! Fast, targeted per-method and per-file queries that assemble already-extracted
//! data from the graph and disk in sub-200ms. These tools are the foundation
//! for all pre-edit, generation, and validation workflows.

use crate::models::{
    CheckEditSafetyRequest, FindDeadMethodsRequest, FindTestsForMethodRequest,
    GetFullMethodBodyRequest, GetMethodEditContextRequest, GetMethodInfoRequest,
    GetPageContextRequest, MAX_SQL_LENGTH, PrepareImplementationContextRequest,
    ValidateGeneratedCodeRequest, ValidateSqlFragmentRequest,
};
use crate::services::full_project_migration_service as full_mig;
use crate::tools::Engram;
use engram_core::safe_join;
use engram_graph::{EdgeKind, GraphStore, Node};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// Resolve application-relative directives from hosting evidence, never a
/// repository-specific folder name. Nested web.config files can configure a
/// subdirectory, so prefer an explicit application marker or the outermost
/// configuration within the registered project.
pub(crate) fn discover_web_application_root(project: &Path, page: &Path) -> std::path::PathBuf {
    let mut configured = None;
    for dir in page.parent().into_iter().flat_map(Path::ancestors) {
        if !dir.starts_with(project) {
            break;
        }
        let mut app_marker = false;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if name == "global.asax" || name.ends_with(".vbproj") || name.ends_with(".csproj") {
                    app_marker = true;
                }
                if name == "web.config" {
                    configured = Some(dir.to_path_buf());
                }
            }
        }
        if app_marker {
            return dir.to_path_buf();
        }
    }
    configured.unwrap_or_else(|| project.to_path_buf())
}

#[derive(Serialize)]
struct FreshAccessResponse<'a, T: Serialize> {
    #[serde(flatten)]
    result: &'a T,
    freshness: AccessFreshness,
}

#[derive(Serialize)]
struct AccessFreshness {
    warning: Option<String>,
    details: String,
}

// ── Shared Output Types ──────────────────────────────────────────────────────

/// Comprehensive method metadata assembled from graph + disk.
#[derive(Debug, Clone, Serialize)]
pub struct MethodInfoResult {
    pub fqn: String,
    pub file_path: String,
    pub class_name: String,
    pub method_name: String,
    pub signature: String,
    pub return_type: String,
    pub access_level: String,
    pub line_start: u32,
    pub line_end: u32,
    pub line_count: u32,
    pub language: String,
    pub method_kind: String,
    pub effects: Vec<String>,
    pub calls_methods: Vec<String>,
    pub called_by: Vec<CallerLocation>,
    pub handles_clause: Vec<String>,
    pub db_tables_accessed: Vec<String>,
    pub stored_procs_called: Vec<String>,
    pub session_keys_read: Vec<String>,
    pub session_keys_written: Vec<String>,
    pub complexity_score: u32,
    pub body_preview: Option<String>,
}

/// A caller with file location context.
#[derive(Debug, Clone, Serialize)]
pub struct CallerLocation {
    pub fqn: String,
    pub file_path: String,
    pub line: u32,
    /// `line` is the caller declaration start, not a verified invocation site.
    pub line_kind: &'static str,
    pub line_end: u32,
    /// Edge kind that made this a caller (`calls`, `dependency`, …).
    pub edge_kind: String,
}

/// Result of get_full_method_body.
#[derive(Debug, Clone, Serialize)]
pub struct MethodBodyResult {
    /// Explicit ranges do not establish method boundaries, even if they happen to match.
    pub retrieval_scope: &'static str,
    pub boundary_guidance: &'static str,
    /// Verification of the exact complete-file bytes used for the returned lines.
    pub source_verification: &'static str,
    /// BLAKE3 hex digest of those complete-file bytes, before line normalization.
    pub source_file_hash: String,
    pub fqn: String,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub source_code: String,
    pub surrounding_context: String,
    pub language: String,
    pub caller_bodies: Vec<CallerBody>,
    pub caller_expansion: CallerExpansion,
}

/// Execution of optional caller-body expansion, not a claim of consumer coverage.
#[derive(Debug, Clone, Serialize)]
pub struct CallerExpansion {
    pub requested: bool,
    pub attempted: bool,
    pub status: String,
    pub returned: usize,
    pub cap: usize,
    pub truncated: bool,
    pub omission_reason: Option<String>,
    pub next_action: Option<String>,
    pub omissions: Vec<String>,
    pub coverage_interpretation: String,
}

/// A caller's full body for pattern understanding.
#[derive(Debug, Clone, Serialize)]
pub struct CallerBody {
    pub fqn: String,
    pub file_path: String,
    /// Start of the caller declaration/body, not the invocation location.
    pub line_start: u32,
    pub line_end: u32,
    pub source_code: String,
    pub how_it_calls: String,
}

// ── Phase 38-3 Output Types ──────────────────────────────────────────────────

/// Full pre-edit context for a method.
#[derive(Debug, Clone, Serialize)]
pub struct MethodEditContextResult {
    pub method_info: MethodInfoResult,
    pub full_source: Option<String>,
    pub caller_bodies: Vec<CallerBody>,
    pub caller_excerpts: Vec<super::caller_excerpts::CallerExcerpt>,
    pub unresolved_caller_leads: super::unresolved_callers::UnresolvedCallerLeads,
    /// Bounded shipped-file history leads, including explicit lookup failures.
    pub historical_changes: Option<String>,
    pub vb_traps: Vec<VbTrapSummary>,
    pub sync_hazards: Vec<SyncHazardSummary>,
    /// `None` when the blast provider failed — never a fake 0.0.
    pub blast_radius_score: Option<f32>,
    pub risk_band: String,
    pub edit_safety: EditSafetyResult,
    /// Present only when `include_business_logic` was requested.
    pub business_logic: Option<BusinessLogicSection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VbTrapSummary {
    pub location: String,
    pub trap: String,
    pub risk: String,
    pub guidance: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncHazardSummary {
    pub line: u32,
    pub pattern: String,
    pub severity: String,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageDiagnosticSummary {
    pub location: String,
    pub category: String,
    pub severity: String,
    pub evidence: String,
    pub guidance: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditSafetyResult {
    pub verdict: String,
    pub confidence: f32,
    pub reasons: Vec<String>,
    pub pre_edit_checklist: Vec<String>,
    pub post_edit_checklist: Vec<String>,
    /// What each evidence provider delivered. A verdict is only as good as
    /// the evidence behind it; missing or capped providers are listed here
    /// and floor the verdict (never green on missing evidence).
    pub completeness: EditContextCompleteness,
}

/// What one evidence provider actually delivered (row-2 audit). Missing or
/// truncated evidence must be visible to the verdict and to the reader —
/// `None`/empty was previously indistinguishable from "nothing there".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProviderStatus {
    Complete,
    Truncated {
        shown: usize,
        cap: usize,
        known_total: Option<usize>,
    },
    Failed {
        reason: String,
    },
    NotRun {
        reason: String,
    },
}

impl Default for ProviderStatus {
    fn default() -> Self {
        ProviderStatus::NotRun {
            reason: "not run".into(),
        }
    }
}

impl ProviderStatus {
    /// Failed or never ran — the verdict must not treat the axis as "clean".
    pub fn is_missing(&self) -> bool {
        matches!(
            self,
            ProviderStatus::Failed { .. } | ProviderStatus::NotRun { .. }
        )
    }
}

/// Per-provider completeness for the pre-edit oracle. Shared by
/// `get_method_edit_context` and `check_edit_safety` so both tools report
/// (and floor on) the same facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditContextCompleteness {
    #[serde(default = "coverage_interpretation")]
    pub coverage_interpretation: String,
    pub blast: ProviderStatus,
    pub callers: ProviderStatus,
    /// Incoming caller edges whose source node does not exist (dangling).
    /// Quarantined from the caller list; reported so "no callers" is never
    /// confused with "callers not resolvable".
    pub callers_dangling: usize,
    pub body: ProviderStatus,
    pub complexity: ProviderStatus,
    pub db_tables: ProviderStatus,
    pub stored_procs: ProviderStatus,
    pub session_reads: ProviderStatus,
    pub session_writes: ProviderStatus,
    pub vb_traps: ProviderStatus,
    pub sync_hazards: ProviderStatus,
}

fn coverage_interpretation() -> String {
    "Provider statuses describe query execution and available indexed evidence, not exhaustive extraction, correct binding of every call, or runtime coverage. Complete caller/blast queries and zero indexed callers do not establish absence of consumers or side effects.".into()
}

impl Default for EditContextCompleteness {
    fn default() -> Self {
        Self {
            coverage_interpretation: coverage_interpretation(),
            blast: Default::default(),
            callers: Default::default(),
            callers_dangling: 0,
            body: Default::default(),
            complexity: Default::default(),
            db_tables: Default::default(),
            stored_procs: Default::default(),
            session_reads: Default::default(),
            session_writes: Default::default(),
            vb_traps: Default::default(),
            sync_hazards: Default::default(),
        }
    }
}

impl EditContextCompleteness {
    pub fn all_complete() -> Self {
        Self {
            coverage_interpretation: coverage_interpretation(),
            blast: ProviderStatus::Complete,
            callers: ProviderStatus::Complete,
            callers_dangling: 0,
            body: ProviderStatus::Complete,
            complexity: ProviderStatus::Complete,
            db_tables: ProviderStatus::Complete,
            stored_procs: ProviderStatus::Complete,
            session_reads: ProviderStatus::Complete,
            session_writes: ProviderStatus::Complete,
            vb_traps: ProviderStatus::Complete,
            sync_hazards: ProviderStatus::Complete,
        }
    }
}

/// Business-rule evidence for the method (from the `business_logic`
/// namespace populated by `analyze_business_logic`).
#[derive(Debug, Clone, Serialize)]
pub struct BusinessLogicSection {
    pub hits: Vec<BusinessLogicHit>,
    /// Human note: how many stored, or how to populate the namespace.
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BusinessLogicHit {
    pub path: String,
    pub score: f32,
    pub content: String,
}

// ── Phase 38-4 Output Types ──────────────────────────────────────────────────

/// Full page context for a WebForms page.
#[derive(Debug, Clone, Serialize)]
pub struct PageContextResult {
    pub composition: super::page_composition::Composition,
    pub aspx_file: String,
    pub codebehind_file: String,
    pub class_name: String,
    pub master_page: Option<String>,
    pub content_placeholders: Vec<String>,
    pub language: String,
    pub ui_coverage_confidence: f32,
    pub dynamic_ui_detected: bool,
    pub dynamic_ui_evidence: Vec<String>,
    pub runtime_controls_warning: Option<String>,
    pub runtime_observed_edges: usize,
    pub controls: Vec<ControlInfo>,
    pub methods: Vec<PageMethodSummary>,
    pub tables_used: Vec<String>,
    pub stored_procs_called: Vec<String>,
    pub session_keys: Vec<String>,
    pub runtime_sql_observations: Vec<String>,
    pub update_panels: Vec<UpdatePanelSummary>,
    pub has_script_manager: bool,
    pub vb_trap_count: usize,
    pub vb_traps_summary: Vec<String>,
    pub requires_authentication: bool,
    pub total_methods: usize,
    /// What each graph/file provider behind this page context delivered.
    pub completeness: PageContextCompleteness,
    /// External audit 2026-08-29 row 5 v3: the house style of the page's
    /// territory — nearest siblings and the idioms they share.
    pub house_style: Option<crate::services::house_style::HouseStyle>,
}

/// Per-provider completeness for `get_page_context` (row-2 audit D6/D7).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PageContextCompleteness {
    pub codebehind: ProviderStatus,
    pub methods: ProviderStatus,
    pub controls: ProviderStatus,
    pub runtime: ProviderStatus,
    pub wiring: ProviderStatus,
    pub data_edges: ProviderStatus,
    pub master_page: ProviderStatus,
    pub ajax: ProviderStatus,
}

/// Fold two provider statuses: Failed > Truncated > NotRun > Complete.
fn worse_status(a: ProviderStatus, b: ProviderStatus) -> ProviderStatus {
    fn rank(p: &ProviderStatus) -> u8 {
        match p {
            ProviderStatus::Complete => 0,
            ProviderStatus::NotRun { .. } => 1,
            ProviderStatus::Truncated { .. } => 2,
            ProviderStatus::Failed { .. } => 3,
        }
    }
    if rank(&b) > rank(&a) { b } else { a }
}

/// A server control found in ASPX markup.
#[derive(Debug, Clone, Serialize)]
pub struct ControlInfo {
    pub server_id: String,
    pub control_type: String,
    pub line: u32,
    pub event_handler: Option<String>,
    pub causes_validation: Option<bool>,
    pub validation_group: Option<String>,
    pub observed_at_runtime: bool,
}

/// A method in the code-behind with optional full body.
#[derive(Debug, Clone, Serialize)]
pub struct PageMethodSummary {
    pub name: String,
    pub signature: String,
    pub kind: String,
    pub line_start: u32,
    pub line_end: u32,
    pub handles_clause: Vec<String>,
    pub effects: Vec<String>,
    pub full_body: Option<String>,
    pub observed_at_runtime: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdatePanelSummary {
    pub panel_id: String,
    pub update_mode: String,
    pub controls_inside: Vec<String>,
}

// ── Phase 38-5 Output Types ──────────────────────────────────────────────────

/// Complete implementation context for LLM code generation.
#[derive(Debug, Clone, Serialize)]
pub struct ImplementationContext {
    pub method_info: MethodInfoResult,
    pub method_coverage: MethodInfoCoverage,
    pub coverage_interpretation: String,
    pub method_body: Option<String>,
    pub style_profile: Option<String>,
    pub style_basis: Option<crate::services::cognitive_service::StyleBasis>,
    pub pattern_examples: Vec<PatternExample>,
    pub schema_snippets: Vec<TableSchemaSnippet>,
    pub sp_signatures: Vec<SpSignatureSnippet>,
    pub state_context: Vec<StateContextSnippet>,
    pub control_mappings: Vec<ControlMappingSnippet>,
    pub vb_traps: Vec<VbTrapSummary>,
    pub language_diagnostics: Vec<LanguageDiagnosticSummary>,
    pub sync_hazards: Vec<SyncHazardSummary>,
    /// Round-5 P0: non-fatal provider failures (e.g. body-read errors) that
    /// would otherwise be swallowed. Never silently dropped.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A caller pattern example showing how existing code interacts with this method.
#[derive(Debug, Clone, Serialize)]
pub struct PatternExample {
    pub caller_fqn: String,
    pub caller_file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub source_code: String,
    pub call_pattern: String,
}

/// Table schema snippet for referenced database tables.
#[derive(Debug, Clone, Serialize)]
pub struct TableSchemaSnippet {
    pub table_name: String,
    pub columns: Vec<ColumnSnippet>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnSnippet {
    pub name: String,
    pub data_type: String,
    pub nullable: Option<bool>,
}

/// Stored procedure signature snippet.
#[derive(Debug, Clone, Serialize)]
pub struct SpSignatureSnippet {
    pub sp_name: String,
    pub coverage: String,
    pub parameters: Vec<String>,
    pub tables_read: Vec<String>,
    pub tables_written: Vec<String>,
}

/// Session/state key context showing cross-method dependencies.
#[derive(Debug, Clone, Serialize)]
pub struct StateContextSnippet {
    pub key: String,
    pub this_method_reads: bool,
    pub this_method_writes: bool,
    pub other_readers: Vec<String>,
    pub other_writers: Vec<String>,
}

/// Control mapping from legacy WebForms to modern framework.
#[derive(Debug, Clone, Serialize)]
pub struct ControlMappingSnippet {
    pub control_id: String,
    pub legacy_type: String,
    pub modern_equivalent: String,
    pub event_mappings: Vec<(String, String)>,
    pub migration_notes: Vec<String>,
}

// ── Phase 38-6 Output Types ──────────────────────────────────────────────────

/// Validation report for generated code.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub overall_verdict: String,
    /// Round-7 P1-3: what the verdict is actually based on — target resolution
    /// status and how many project-CONTRACT checks ran — so a caller can audit
    /// WHY a result is PASS/WARN/INSUFFICIENT, not just read the verdict.
    pub coverage: ValidationCoverage,
    pub checks: Vec<ValidationCheck>,
}

/// Round-8 P0-1: the EVIDENCE CLASS of a validation check — what KIND of thing
/// it verified. Only a project-DERIVED `Verified` check can earn a PASS; a
/// caller-supplied assertion or a project-independent language lint cannot. A
/// count of "checks that ran" is not coverage of the project's contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageClass {
    /// A PROJECT-DERIVED invariant (from the graph/index — the real schema, the
    /// resolved target method) was checked against the code. Earns PASS.
    Verified,
    /// A CALLER-SUPPLIED expectation was checked for presence. The caller could
    /// assert anything; we only confirmed the token appears. Never earns PASS.
    AssertionOnly,
    /// A language lint independent of THIS project (VB traps, sync hazards).
    /// Says nothing about project correctness. Never earns PASS.
    GenericLint,
    /// A meta note (target-file existence, an unresolved-caller advisory, a
    /// language/target mismatch). Not contract coverage.
    Meta,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationCheck {
    pub category: String,
    pub status: String,
    /// Round-8 P0-1: the evidence class this check contributes.
    pub coverage_class: CoverageClass,
    pub details: Vec<String>,
}

impl ValidationCheck {
    pub fn new(
        category: &str,
        status: &str,
        coverage_class: CoverageClass,
        details: Vec<String>,
    ) -> Self {
        ValidationCheck {
            category: category.to_string(),
            status: status.to_string(),
            coverage_class,
            details,
        }
    }
}

// ── Phase 38-7 Output Types ──────────────────────────────────────────────────

/// SQL fragment validation report.
#[derive(Debug, Clone, Serialize)]
pub struct SqlValidationReport {
    pub verdict: String,
    pub coverage: String,
    pub tables_referenced: Vec<String>,
    pub issues: Vec<SqlValidationIssue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlValidationIssue {
    pub severity: String,
    pub category: String,
    pub message: String,
}

// ── Phase 38-8 Output Types ──────────────────────────────────────────────────

/// Test search result for a method.
#[derive(Debug, Clone, Serialize)]
pub struct TestSearchResult {
    pub warnings: Vec<String>,
    pub method_name: String,
    pub target_node_id: String,
    pub target_file: String,
    pub target_start_line: u32,
    pub test_hits: Vec<TestHit>,
    pub test_files_searched: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestHit {
    pub test_name: String,
    pub test_file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub match_type: String,
}

// ── Phase 38-9 Output Types ──────────────────────────────────────────────────

/// Dead method analysis report.
#[derive(Debug, Clone, Serialize)]
pub struct DeadMethodReport {
    pub dead_methods: Vec<DeadMethodInfo>,
    pub total_methods: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeadMethodInfo {
    pub fqn: String,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub method_kind: String,
    pub line_count: u32,
    pub access_level: String,
    /// Non-empty when static analysis alone cannot confirm the method is truly
    /// unreachable — e.g. public methods that may be invoked via reflection,
    /// `Type.GetMethod(...).Invoke(...)`, dynamic binding, or callers in
    /// assemblies not present in this project.  (L-2 fix)
    pub confidence_note: String,
}

// ── Internal Helpers ─────────────────────────────────────────────────────────

/// Build an FQN from a graph node. The node_id typically has the format
/// `project_id\0file_path::ClassName.MethodName` or similar.
/// TODO-11: resolve an FQN query to exactly ONE function node or fail with
/// a disambiguation list. `query_nodes` matches case-insensitive substrings,
/// so "Page_Load" hits every page's handler - silently taking the first
/// match is how agents read or edit the wrong method.
/// Should a language diagnostic reported at `line` appear in the
/// implementation context for the method spanning
/// `[method_start, method_end]`?
///
/// `claimable` is every method/function range in the same file.
///
/// Method-range filtering alone silently drops DECLARATION-level findings.
/// MiniLang's MLC6013 strong-`Ref` cycle is reported on the `Type` line,
/// which lies outside every function range, so it was computed on every
/// call and could never reach an agent — a diagnostic that exists only in
/// a corpus census is not a feature.
///
/// A finding is kept when it is inside THIS method, or when no method in
/// the file can claim it. A finding inside a DIFFERENT method is still
/// excluded, so this rescues declaration-level context without widening
/// the filter into noise.
pub(crate) fn diagnostic_belongs_to_context(
    line: u32,
    method_start: u32,
    method_end: u32,
    claimable: &[(u32, u32)],
) -> bool {
    let in_this_method = line >= method_start && line <= method_end;
    let claimed_elsewhere = claimable.iter().any(|(s, e)| line >= *s && line <= *e);
    in_this_method || !claimed_elsewhere
}

fn resolve_unique_function(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    fqn_query: &str,
) -> Result<engram_graph::Node, String> {
    let mut candidates = graph
        .query_nodes(project_id, Some("function"), Some(fqn_query), None, 25)
        .unwrap_or_default();
    // A dotted query like "_admin.PageA.Page_Load" won't substring-match a
    // node NAMED "Page_Load" whose full identity lives in metadata.fqn -
    // retry on the terminal segment and keep only exact-FQN survivors.
    if candidates.is_empty() && fqn_query.contains('.') {
        let short = fqn_query.rsplit('.').next().unwrap_or(fqn_query);
        candidates = graph
            .query_nodes(project_id, Some("function"), Some(short), None, 50)
            .unwrap_or_default()
            .into_iter()
            .filter(|n| {
                n.metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|f| f.eq_ignore_ascii_case(fqn_query))
            })
            .collect();
    }
    if candidates.is_empty() {
        return Err(method_not_found_message(graph, project_id, fqn_query, None));
    }
    // Prefer exact name / exact metadata-FQN equality over substring hits.
    let exact: Vec<&engram_graph::Node> = candidates
        .iter()
        .filter(|n| {
            n.name.eq_ignore_ascii_case(fqn_query)
                || n.metadata
                    .as_ref()
                    .and_then(|m| m.get("fqn"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|f| f.eq_ignore_ascii_case(fqn_query))
        })
        .collect();
    let pool: Vec<&engram_graph::Node> = if exact.is_empty() {
        candidates.iter().collect()
    } else {
        exact
    };
    if pool.len() == 1 {
        return Ok(pool[0].clone());
    }
    let mut msg = format!(
        "AMBIGUOUS: {} methods match '{}'. Re-call with the exact FQN below (or pass file_path + line range):
",
        pool.len(),
        fqn_query
    );
    for n in pool.iter().take(10) {
        msg.push_str(&format!(
            "- {} ({}:{}-{}) node_id={}
",
            fqn_from_node(n),
            n.file_path,
            n.start_line,
            n.end_line,
            n.node_id
        ));
    }
    if pool.len() > 10 {
        msg.push_str(&format!(
            "... and {} more
",
            pool.len() - 10
        ));
    }
    Err(msg)
}

/// On a lookup miss, rank up to `max` nearest method names so the agent can
/// self-correct in one step instead of dead-ending on "ensure the project is
/// indexed" (which is almost never the actual problem — typos and wrong
/// class prefixes are).
fn suggest_similar_methods(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    query: &str,
    file_path: Option<&str>,
    max: usize,
) -> Vec<String> {
    let terminal = query.rsplit('.').next().unwrap_or(query);
    let terminal_chars: Vec<char> = terminal.chars().collect();

    // Candidate pool: prefer functions in the caller-supplied file (one
    // scan); otherwise probe with progressively shorter name prefixes.
    let mut pool: Vec<Node> = Vec::new();
    if let Some(fp) = file_path {
        pool = graph
            .query_nodes(project_id, Some("function"), None, Some(fp), 200)
            .unwrap_or_default();
    }
    if pool.is_empty() && !terminal_chars.is_empty() {
        let lens = [
            terminal_chars.len() * 2 / 3,
            terminal_chars.len() / 2,
            4usize,
        ];
        for len in lens {
            let len = len.clamp(3, terminal_chars.len());
            let prefix: String = terminal_chars.iter().take(len).collect();
            pool = graph
                .query_nodes(project_id, Some("function"), Some(&prefix), None, 50)
                .unwrap_or_default();
            if !pool.is_empty() {
                break;
            }
        }
    }

    let target = terminal.to_lowercase();
    let mut scored: Vec<(i64, String)> = pool
        .iter()
        .map(|n| {
            let name = n.name.to_lowercase();
            let mut score = 0i64;
            if name == target {
                score += 1000;
            }
            if name.contains(&target) || target.contains(&name) {
                score += 200;
            }
            let common_prefix = name
                .chars()
                .zip(target.chars())
                .take_while(|(a, b)| a == b)
                .count() as i64;
            score += common_prefix * 10;
            score -= (name.chars().count() as i64 - target.chars().count() as i64).abs();
            (
                score,
                format!("{} ({}:{})", fqn_from_node(n), n.file_path, n.start_line),
            )
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(max).map(|(_, s)| s).collect()
}

/// Uniform miss message for method lookups: names the query, offers the
/// nearest matches, and only then mentions indexing as a possibility.
fn method_not_found_message(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    query: &str,
    file_path: Option<&str>,
) -> String {
    let suggestions = suggest_similar_methods(graph, project_id, query, file_path, 5);
    let mut msg = match file_path {
        Some(fp) => format!("No method '{query}' found in '{fp}'."),
        None => format!("No method found matching '{query}'."),
    };
    if suggestions.is_empty() {
        msg.push_str(" No similar names found either — check the file path, or ensure the project is indexed (get_index_freshness).");
    } else {
        msg.push_str(" Did you mean:\n");
        for s in &suggestions {
            msg.push_str(&format!("- {s}\n"));
        }
        msg.push_str("Re-call with one of these exact names.");
    }
    // A miss can just mean the method is in a file added/edited since the last
    // index — not that it's absent. Point at the working tree so the agent
    // doesn't infer a failure contract from the name or give up.
    msg.push_str(
        "\nIf the method is in a file changed since the last index it is invisible here — \
         grep_project / read the working tree before inferring its return/failure contract \
         from the name.",
    );
    msg
}

/// True when `node.namespace` holds a SEARCH-namespace constant
/// ("memory", "history", …) rather than a declaring type. Nodes minted
/// by the main ingest carry the search namespace there while their NAME
/// is already fully qualified — rendering it produced headers like
/// `Method: memory._ata.huvud.CreateFromMarkers` / `Class: memory`.
fn is_search_namespace(ns: &str) -> bool {
    engram_core::namespaces::KNOWN_NAMESPACES.contains(&ns)
}

pub(super) fn fqn_from_node(node: &Node) -> String {
    // Tree-sitter nodes retain a bare name and a search namespace. Their
    // declaration identity is carried separately by the parser.
    if let Some(fqn) = node
        .metadata
        .as_ref()
        .and_then(|m| m.get("fqn"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|fqn| !fqn.is_empty())
    {
        return fqn.to_string();
    }
    // The node's namespace often holds the class name, and name holds the method.
    // Build: namespace.name (skip namespace if it's "default", empty, or a
    // search-namespace constant — the name is already qualified then).
    let ns = node.namespace.trim();
    if ns.is_empty() || ns == "default" || is_search_namespace(ns) {
        node.name.clone()
    } else {
        format!("{}.{}", ns, node.name)
    }
}

/// Extract the declaring class from the canonical declaration identity,
/// including parser metadata and legacy namespace/name representations.
fn class_of_node(node: &Node) -> String {
    let fqn = fqn_from_node(node);
    let mut parts = fqn.rsplit('.');
    parts.next(); // method segment
    parts.next().unwrap_or("").to_string()
}

/// The bare method identifier for a node whose `name` may be class-qualified.
///
/// The graph stores function names class-qualified (`orders.GetAll`) with a
/// SEARCH namespace, not the declaring class — so a bare `method_name` must be
/// compared against this tail, never against `node.name` directly.
fn bare_method_name(node: &Node) -> &str {
    node.name.rsplit('.').next().unwrap_or(node.name.as_str())
}

/// Extract string metadata field from Node.
fn meta_str(node: &Node, key: &str) -> String {
    node.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Extract comma-separated metadata field as Vec<String>.
fn meta_csv(node: &Node, key: &str) -> Vec<String> {
    node.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn meta_bool(node: &Node, key: &str) -> bool {
    node.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn edge_meta_str(edge: &engram_graph::Edge, key: &str) -> String {
    edge.metadata
        .as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Check if an edge has runtime provenance metadata.
/// Kept for future runtime-evidence integration.
#[allow(dead_code)]
fn edge_has_runtime_provenance(edge: &engram_graph::Edge) -> bool {
    if matches!(
        edge.edge_kind,
        EdgeKind::ObservedRuntimeControl | EdgeKind::ObservedRuntimeSql
    ) {
        return true;
    }
    edge.metadata
        .as_ref()
        .and_then(|m| m.get("source"))
        .and_then(|v| v.as_str())
        .map(|v| v.contains("runtime"))
        .unwrap_or(false)
}

/// Caller list display cap. The TOTAL is still counted exactly (up to
/// `CALLER_COUNT_CEILING`) so the verdict and the renderer can say
/// "3 shown of 98" instead of presenting the cap as the count.
const CALLER_CAP: usize = 50;
/// Above this many distinct callers the total is reported as a lower bound.
const CALLER_COUNT_CEILING: usize = 5_000;
/// Per-kind cap on the method's own outgoing data/state edges.
const DATA_EDGE_CAP: usize = 200;

/// What the per-node edge lookups behind a `MethodInfoResult` delivered.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MethodInfoCoverage {
    pub callers: ProviderStatus,
    pub callers_dangling: usize,
    pub db_tables: ProviderStatus,
    pub stored_procs: ProviderStatus,
    pub session_reads: ProviderStatus,
    pub session_writes: ProviderStatus,
}

/// Outgoing edge targets of ONE kind for ONE node, exact up to
/// `DATA_EDGE_CAP` (O(degree) adjacency seek — never a project-wide
/// first-N-edges scan filtered by name suffix).
fn outgoing_targets(
    graph: &GraphStore,
    project_id: &str,
    kind: EdgeKind,
    node_id: &str,
) -> (Vec<String>, ProviderStatus) {
    match graph.neighbors(project_id, kind, node_id, DATA_EDGE_CAP + 1) {
        Ok(v) => {
            let truncated = v.len() > DATA_EDGE_CAP;
            let mut targets: Vec<String> = v
                .into_iter()
                .take(DATA_EDGE_CAP)
                .map(|(id, _)| id)
                .collect();
            targets.sort();
            targets.dedup();
            let status = if truncated {
                ProviderStatus::Truncated {
                    shown: DATA_EDGE_CAP,
                    cap: DATA_EDGE_CAP,
                    known_total: None,
                }
            } else {
                ProviderStatus::Complete
            };
            (targets, status)
        }
        Err(e) => (
            Vec::new(),
            ProviderStatus::Failed {
                reason: e.to_string(),
            },
        ),
    }
}

/// Build MethodInfoResult from a graph Node + edge lookups (coverage dropped —
/// for callers that only need the facts).
fn build_method_info_from_node(
    node: &Node,
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> MethodInfoResult {
    build_method_info_with_coverage(node, graph, project_id).0
}

/// Build MethodInfoResult from a graph Node + edge lookups, reporting what
/// each lookup delivered.
fn build_method_info_with_coverage(
    node: &Node,
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> (MethodInfoResult, MethodInfoCoverage) {
    let fqn = fqn_from_node(node);
    let effects = meta_csv(node, "effects");
    let kind = full_mig::classify_method_kind_pub(&node.name, &effects, &node.metadata);

    let signature = meta_str(node, "signature");
    let return_type = meta_str(node, "return_type");
    let access_level = {
        let al = meta_str(node, "access_level");
        if al.is_empty() {
            // Round-5 P0: do NOT fabricate "Private" — the VB extractor does
            // not populate access_level, and guessing wrong (Check_pr_id is
            // Public Shared) causes bad edits. Say what is true: unknown.
            "unknown".to_string()
        } else {
            al
        }
    };
    let handles_clause = meta_csv(node, "handles_clause");

    let line_count = if node.end_line >= node.start_line {
        node.end_line - node.start_line + 1
    } else {
        1
    };

    // Called-by: incoming caller edges. The total is counted exactly (up to
    // CALLER_COUNT_CEILING); the LIST is capped at CALLER_CAP for display.
    // Sources whose node does not exist are dangling: quarantined from the
    // list and COUNTED, so "no callers" is never confused with "callers not
    // resolvable".
    let mut called_by: Vec<CallerLocation> = Vec::new();
    let mut callers_dangling = 0usize;
    let mut callers_status = ProviderStatus::Complete;
    match crate::handlers::incoming_caller_edges_checked(
        graph,
        project_id,
        &node.node_id,
        CALLER_COUNT_CEILING,
    ) {
        Ok((edges, over_ceiling)) => {
            let distinct = edges.len();
            for (source_id, kind, _weight) in &edges {
                match graph.get_node(project_id, source_id) {
                    Ok(Some(src)) => {
                        if called_by.len() < CALLER_CAP {
                            called_by.push(CallerLocation {
                                fqn: fqn_from_node(&src),
                                file_path: src.file_path.as_str().to_string(),
                                line: src.start_line,
                                line_kind: "declaration",
                                line_end: src.end_line,
                                edge_kind: kind.as_str().to_string(),
                            });
                        }
                    }
                    Ok(None) => callers_dangling += 1,
                    Err(e) => {
                        callers_status = ProviderStatus::Failed {
                            reason: format!("caller node lookup failed: {e}"),
                        };
                        break;
                    }
                }
            }
            let resolved_total = distinct.saturating_sub(callers_dangling);
            if !matches!(callers_status, ProviderStatus::Failed { .. })
                && (over_ceiling || resolved_total > called_by.len())
            {
                callers_status = ProviderStatus::Truncated {
                    shown: called_by.len(),
                    cap: CALLER_CAP,
                    known_total: if over_ceiling {
                        None
                    } else {
                        Some(resolved_total)
                    },
                };
            }
        }
        Err(e) => {
            callers_status = ProviderStatus::Failed {
                reason: format!("caller lookup failed: {e}"),
            };
        }
    }

    // Calls-methods: extract from metadata (populated during extraction).
    // We don't have a direct find_outgoing_edges API; metadata is the authoritative source.
    let calls_from_meta = meta_csv(node, "calls_methods");

    // Data/state edges: exact per-node adjacency seeks (row-2 audit D5 —
    // the previous project-wide first-5000-edges scans were silently
    // truncated on large graphs and suffix-matched other methods' edges).
    let (db_tables, db_tables_status) =
        outgoing_targets(graph, project_id, EdgeKind::QueriesTable, &node.node_id);
    let (stored_procs, stored_procs_status) =
        outgoing_targets(graph, project_id, EdgeKind::SqlCalls, &node.node_id);
    let (session_reads, session_reads_status) =
        outgoing_targets(graph, project_id, EdgeKind::ReadsState, &node.node_id);
    let (session_writes, session_writes_status) =
        outgoing_targets(graph, project_id, EdgeKind::WritesState, &node.node_id);

    // Compute complexity from body if available, else from metadata
    let complexity = node
        .metadata
        .as_ref()
        .and_then(|m| m.get("complexity_score"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let body_preview = node
        .metadata
        .as_ref()
        .and_then(|m| m.get("body_preview"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let info = MethodInfoResult {
        fqn,
        file_path: node.file_path.as_str().to_string(),
        class_name: class_of_node(node),
        method_name: node.name.clone(),
        signature: if signature.is_empty() {
            node.name.clone()
        } else {
            signature
        },
        return_type: if return_type.is_empty() {
            // Round-5 P0: do NOT fabricate "Sub" — a Function As Boolean shown
            // as Sub is a lie that causes bad edits. Unknown until the
            // extractor populates it.
            "unknown".to_string()
        } else {
            return_type
        },
        access_level,
        line_start: node.start_line,
        line_end: node.end_line,
        line_count,
        language: node.language.clone(),
        method_kind: kind.to_string(),
        effects,
        calls_methods: calls_from_meta,
        called_by,
        handles_clause,
        db_tables_accessed: db_tables,
        stored_procs_called: stored_procs,
        session_keys_read: session_reads,
        session_keys_written: session_writes,
        complexity_score: complexity,
        body_preview,
    };
    let coverage = MethodInfoCoverage {
        callers: callers_status,
        callers_dangling,
        db_tables: db_tables_status,
        stored_procs: stored_procs_status,
        session_reads: session_reads_status,
        session_writes: session_writes_status,
    };
    (info, coverage)
}

/// Table names referenced by FROM/JOIN/INTO/UPDATE/DELETE in a SQL
/// fragment, deduped, original case preserved. Consumes an optional
/// `[schema].`/`db.schema.` qualifier so the returned name is the TABLE,
/// not `dbo` — the codebase's universal `[dbo].[table]` bracket style
/// previously yielded the schema and false unknown_table warnings.
/// One source of truth for both validate_sql_fragment and
/// validate_generated_code.
pub(crate) fn referenced_sql_tables(sql: &str) -> Vec<String> {
    use std::sync::LazyLock;
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(?:FROM|JOIN|INTO|UPDATE|DELETE\s+FROM|INSERT\s+INTO)\s+(?:\[?\w+\]?\.){0,2}\[?(\w+)\]?",
        )
        .expect("valid table-ref regex")
    });
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cap in RE.captures_iter(sql) {
        // VB and C# LINQ: `From item In collection` / `join item in ...`.
        // The range variable is not a SQL table. This helper is also used
        // on generated source, where treating it as one causes false schema
        // failures (and can falsely verify a coincidentally named table).
        let matched = cap.get(0).expect("whole table-ref match");
        if sql[matched.end()..]
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("in"))
        {
            continue;
        }
        let t = cap[1].to_string();
        if seen.insert(t.to_lowercase()) {
            out.push(t);
        }
    }
    out
}

/// Rough cyclomatic-complexity estimate: 1 + decision points, via a
/// language-agnostic keyword scan (VB/C#/TS/JS). Comment lines skipped.
/// Good enough for the green/yellow/red edit-safety thresholds that
/// consume it — no extractor persists a real score yet.
pub(crate) fn estimate_complexity(body: &str) -> u32 {
    let mut score = 1u32;
    for line in body.lines() {
        let t = line.trim_start().to_ascii_lowercase();
        if t.starts_with('\'') || t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        for kw in [
            "if ",
            "elseif ",
            "else if",
            "case ",
            "for ",
            "for each",
            "foreach",
            "while ",
            "catch",
            "&&",
            "||",
            " andalso ",
            " orelse ",
        ] {
            score += t.matches(kw).count() as u32;
        }
    }
    score
}

/// Reject known source-fingerprint mismatches before using indexed spans.
pub(crate) fn verify_indexed_source_span(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    root: &str,
    file: &str,
) -> Result<(), String> {
    let path = safe_join(Path::new(root), file).map_err(|e| e.to_string())?;
    let content = std::fs::read(path).map_err(|e| format!("Cannot verify {file}: {e}"))?;
    verify_indexed_source_bytes(graph, project_id, file, &content).map(|_| ())
}

fn verify_indexed_source_bytes(
    graph: &engram_graph::GraphStore,
    project_id: &str,
    file: &str,
    content: &[u8],
) -> Result<&'static str, String> {
    let node = graph
        .get_node(project_id, &format!("file:{}", file.replace('\\', "/")))
        .map_err(|e| format!("Cannot verify source fingerprint: {e}"))?;
    let hash = node
        .as_ref()
        .and_then(|n| n.metadata.as_ref())
        .and_then(|m| m.get("file_hash"))
        .and_then(|h| h.as_str());
    // Legacy graphs may lack fingerprints. Report that explicitly to callers
    // that expose verification, without manufacturing a successful comparison.
    if let Some(hash) = hash {
        if blake3::hash(content).to_hex().as_str() != hash {
            return Err(format!(
                "Stale method spans withheld: {file} changed since indexing. Refresh the index before resolving method/caller line ranges, or read the current file directly."
            ));
        }
        return Ok("matched_indexed_fingerprint");
    }
    Ok("unverified_missing_indexed_fingerprint")
}

fn read_lines_from_file(
    file_path: &Path,
    line_start: u32,
    line_end: u32,
    context_lines: u32,
) -> std::io::Result<(String, String)> {
    let content = std::fs::read_to_string(file_path)?;
    source_lines(&content, line_start, line_end, context_lines)
}

fn source_lines(
    content: &str,
    line_start: u32,
    line_end: u32,
    context_lines: u32,
) -> std::io::Result<(String, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as u32;

    if line_start == 0 || line_end < line_start || line_end > total {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Invalid source range {line_start}..{line_end}; file has {total} lines. Use 1-based bounds within the file."),
        ));
    }

    // Method body (1-based to 0-based)
    let start_idx = (line_start.saturating_sub(1)) as usize;
    let end_idx = (line_end.min(total)) as usize;
    let body: String = lines.get(start_idx..end_idx).unwrap_or(&[]).join("\n");

    // Context above — use saturating_add to avoid u32 overflow when context_lines == u32::MAX
    let ctx_start = line_start.saturating_sub(context_lines.saturating_add(1)) as usize;
    let ctx_end = start_idx;
    let context: String = lines.get(ctx_start..ctx_end).unwrap_or(&[]).join("\n");

    Ok((body, context))
}

/// Select the ONE method node a request means. Refuses cross-class AND
/// same-class-overload ambiguity instead of taking `candidates[0]`, and
/// prefers an exact name over the substring match `query_nodes` performs
/// ("Load" must not resolve to "Page_Load"). Row-2 audit D9.
fn select_method_node(
    graph: &GraphStore,
    project_id: &str,
    file_path: &str,
    method_name: &str,
    class_name: Option<&str>,
    line: Option<u32>,
) -> Result<Node, String> {
    // Round-7 P1-5: query_nodes matches by SUBSTRING and caps BEFORE matching,
    // so an exact method beyond the first 50 substring neighbours is silently
    // lost. query_nodes_by_symbol_name applies the exact/suffix match rule
    // DURING the scan, so the cap bounds MATCHES, not candidates inspected —
    // the exact declaration is never crowded out.
    let mut candidates: Vec<Node> = graph
        .query_nodes_by_symbol_name(project_id, method_name, Some(file_path), 200)
        .map_err(|e| format!("method lookup failed: {e}"))?
        .into_iter()
        .filter(|n| {
            matches!(
                n.node_type.as_str(),
                "function" | "method" | "sub" | "procedure"
            )
        })
        .collect();
    if let Some(cls) = class_name {
        // The declaring class lives in the qualified NAME (`orders.GetAll`) or a
        // real namespace, never in the search namespace — match via class_of_node.
        let cls_lower = cls.to_lowercase();
        candidates.retain(|n| class_of_node(n).to_lowercase() == cls_lower);
    }
    if candidates.is_empty() {
        return Err(method_not_found_message(
            graph,
            project_id,
            method_name,
            Some(file_path),
        ));
    }
    // query_nodes matches by SUBSTRING, so `GetAll` also returns `GetAllHistory`.
    // Prefer an EXACT match on the bare method identifier (the tail of the
    // qualified name), so a substring sibling never masquerades as ambiguity.
    let exact: Vec<Node> = candidates
        .iter()
        .filter(|n| bare_method_name(n).eq_ignore_ascii_case(method_name))
        .cloned()
        .collect();
    if !exact.is_empty() {
        candidates = exact;
    }
    // Same-name methods in DIFFERENT classes: describing the wrong one
    // poisons the edit that follows. The class is derived from the node, not
    // read from the (search-)namespace, which is identical across classes.
    let mut classes: Vec<String> = candidates.iter().map(class_of_node).collect();
    classes.sort_unstable();
    classes.dedup();
    if classes.len() > 1 {
        let mut msg = format!(
            "AMBIGUOUS: '{}' exists in {} classes in '{}'. Re-call with class_name set:\n",
            method_name,
            classes.len(),
            file_path
        );
        for n in candidates.iter().take(10) {
            msg.push_str(&format!(
                "- {} (lines {}-{})\n",
                fqn_from_node(n),
                n.start_line,
                n.end_line
            ));
        }
        return Err(msg);
    }
    if let Some(l) = line {
        candidates.retain(|n| n.start_line == l);
        if candidates.is_empty() {
            return Err(format!(
                "No declaration of '{method_name}' starts at line {l} in '{file_path}'. \
                 Omit `line` to see the candidates."
            ));
        }
    }
    // Same-class overloads (distinct spans): a verdict for the wrong overload
    // is worse than no verdict.
    let mut starts: Vec<u32> = candidates.iter().map(|n| n.start_line).collect();
    starts.sort_unstable();
    starts.dedup();
    if starts.len() > 1 {
        let mut msg = format!(
            "AMBIGUOUS: '{}' is declared {} times in '{}' (overloads). Re-call with line=<start line>:\n",
            method_name,
            starts.len(),
            file_path
        );
        for n in candidates.iter().take(10) {
            let sig = meta_str(n, "signature");
            msg.push_str(&format!(
                "- {} (lines {}-{}) {} -> line={}\n",
                fqn_from_node(n),
                n.start_line,
                n.end_line,
                if sig.is_empty() {
                    String::new()
                } else {
                    format!("— `{sig}`")
                },
                n.start_line
            ));
        }
        return Err(msg);
    }
    Ok(candidates.swap_remove(0))
}

/// Everything the pre-edit oracle knows about ONE method, with per-provider
/// completeness. Shared by `get_method_edit_context` and `check_edit_safety`
/// so both compute the verdict from the SAME facts (row-2 audit D3/D10).
struct EditEvidence {
    node: Node,
    method_info: MethodInfoResult,
    full_body: Option<String>,
    vb_traps: Vec<VbTrapSummary>,
    sync_hazards: Vec<SyncHazardSummary>,
    blast: Option<crate::services::blast_radius_service::BlastRadiusReport>,
    edit_safety: EditSafetyResult,
}

fn assemble_edit_evidence(
    graph: &Arc<GraphStore>,
    project_id: &str,
    project_dir: &str,
    node: Node,
) -> Result<EditEvidence, String> {
    let (mut method_info, cov) = build_method_info_with_coverage(&node, graph, project_id);
    let mut completeness = EditContextCompleteness {
        callers: cov.callers,
        callers_dangling: cov.callers_dangling,
        db_tables: cov.db_tables,
        stored_procs: cov.stored_procs,
        session_reads: cov.session_reads,
        session_writes: cov.session_writes,
        ..Default::default()
    };
    let file_path = node.file_path.as_str().to_string();
    let full_path = safe_join(Path::new(project_dir), &file_path)
        .map_err(|e| format!("Path validation: {e}"))?;

    // Body — always read: complexity and the hazard scans depend on it.
    verify_indexed_source_span(graph, project_id, project_dir, &file_path)?;
    let full_body = match read_lines_from_file(&full_path, node.start_line, node.end_line, 0) {
        Ok((body, _)) => {
            completeness.body = ProviderStatus::Complete;
            Some(body)
        }
        Err(e) => {
            completeness.body = ProviderStatus::Failed {
                reason: format!("could not read {file_path}: {e}"),
            };
            None
        }
    };

    // Complexity: no extractor writes a complexity_score metadata key, so
    // estimate from the body; without a body it is NOT measured (never 0).
    if method_info.complexity_score > 0 {
        completeness.complexity = ProviderStatus::Complete;
    } else if let Some(ref body) = full_body {
        method_info.complexity_score = estimate_complexity(body);
        completeness.complexity = ProviderStatus::Complete;
    } else {
        completeness.complexity = ProviderStatus::NotRun {
            reason: "body not read; no extractor writes complexity_score".into(),
        };
    }

    // File-level scans filtered to the method span.
    let content = std::fs::read_to_string(&full_path);
    let is_vb = file_path.to_lowercase().ends_with(".vb");
    let vb_traps: Vec<VbTrapSummary> = match (&content, is_vb) {
        (Ok(c), true) => {
            completeness.vb_traps = ProviderStatus::Complete;
            let files = vec![(file_path.as_str(), c.as_str())];
            engram_index::vb_translation_traps::detect_vb_translation_traps(&files)
                .traps
                .into_iter()
                .filter(|t| {
                    t.location
                        .rsplit(':')
                        .next()
                        .and_then(|s| s.parse::<u32>().ok())
                        .map(|l| l >= node.start_line && l <= node.end_line)
                        .unwrap_or(false)
                })
                .map(|t| VbTrapSummary {
                    location: t.location,
                    trap: t.trap,
                    risk: t.risk,
                    guidance: t.guidance,
                })
                .collect()
        }
        (Ok(_), false) => {
            completeness.vb_traps = ProviderStatus::NotRun {
                reason: "not a VB file".into(),
            };
            Vec::new()
        }
        (Err(e), _) => {
            completeness.vb_traps = ProviderStatus::Failed {
                reason: e.to_string(),
            };
            Vec::new()
        }
    };
    let sync_hazards: Vec<SyncHazardSummary> = match &content {
        Ok(c) => {
            completeness.sync_hazards = ProviderStatus::Complete;
            engram_index::sync_hazard_detector::detect_sync_hazards(c, is_vb)
                .hazards
                .into_iter()
                .filter(|h| {
                    h.line_number >= node.start_line as usize
                        && h.line_number <= node.end_line as usize
                })
                .map(|h| SyncHazardSummary {
                    line: h.line_number as u32,
                    pattern: h.pattern_type,
                    severity: format!("{:?}", h.severity),
                    modern_equivalent: h.modern_equivalent,
                })
                .collect()
        }
        Err(e) => {
            completeness.sync_hazards = ProviderStatus::Failed {
                reason: e.to_string(),
            };
            Vec::new()
        }
    };

    // Blast radius: failure is reported, never converted to "no risk".
    let blast = match crate::services::blast_radius_service::compute_blast_radius(
        graph,
        project_id,
        &node.node_id,
        node.generation,
        false,
    ) {
        Ok(r) => {
            completeness.blast = if r.coverage.causal_truncated {
                ProviderStatus::Truncated {
                    shown: r.causal_dependents,
                    cap: r.coverage.cap_incoming,
                    known_total: None,
                }
            } else {
                ProviderStatus::Complete
            };
            Some(r)
        }
        Err(e) => {
            completeness.blast = ProviderStatus::Failed {
                reason: e.to_string(),
            };
            None
        }
    };

    let edit_safety = compute_edit_safety(&method_info, blast.as_ref(), &completeness);
    Ok(EditEvidence {
        node,
        method_info,
        full_body,
        vb_traps,
        sync_hazards,
        blast,
        edit_safety,
    })
}

fn provider_text(p: &ProviderStatus) -> String {
    match p {
        ProviderStatus::Complete => "complete".into(),
        ProviderStatus::Truncated {
            shown,
            cap,
            known_total: Some(t),
        } => format!("{shown} shown of {t} (display cap {cap})"),
        ProviderStatus::Truncated { shown, cap, .. } => {
            format!("≥{shown} (capped at {cap}; total unknown)")
        }
        ProviderStatus::Failed { reason } => format!("FAILED — {reason}"),
        ProviderStatus::NotRun { reason } => format!("not run — {reason}"),
    }
}

/// Markdown block listing what every provider delivered.
fn render_coverage_block(c: &EditContextCompleteness) -> String {
    let mut md = format!("## Coverage\n\n{}\n\n", c.coverage_interpretation);
    for (name, st) in [
        ("blast radius", &c.blast),
        ("callers", &c.callers),
        ("body", &c.body),
        ("complexity", &c.complexity),
        ("db tables", &c.db_tables),
        ("stored procs", &c.stored_procs),
        ("session reads", &c.session_reads),
        ("session writes", &c.session_writes),
        ("vb traps", &c.vb_traps),
        ("sync hazards", &c.sync_hazards),
    ] {
        if name == "callers" && c.callers_dangling > 0 {
            md.push_str(&format!(
                "- callers: partial evidence — {} dangling caller edge(s); provider scan: {}; returned callers are a lower bound\n",
                c.callers_dangling,
                provider_text(st),
            ));
        } else {
            md.push_str(&format!("- {name}: {}\n", provider_text(st)));
        }
    }
    if c.callers_dangling > 0 {
        md.push_str(&format!(
            "- dangling caller edges (source not indexed, quarantined): {}\n",
            c.callers_dangling
        ));
    }
    md.push('\n');
    md
}

/// Shared edit safety computation used by both get_method_edit_context (38-3) and
/// check_edit_safety (38-10). Centralizes all scoring logic so thresholds and
/// reason messages never drift between the two tools.
fn compute_edit_safety(
    method_info: &MethodInfoResult,
    blast_radius: Option<&crate::services::blast_radius_service::BlastRadiusReport>,
    completeness: &EditContextCompleteness,
) -> EditSafetyResult {
    // A missing blast report is UNKNOWN risk, never 0.0 (row-2 audit D1):
    // the numeric contribution stays 0 but the provider floor below keeps
    // the verdict off green and says why.
    let br_score = blast_radius
        .map(|b| b.migration_risk as f32 * 10.0)
        .unwrap_or(0.0);
    let blast_status = if blast_radius.is_none() && !completeness.blast.is_missing() {
        ProviderStatus::NotRun {
            reason: "no blast report".into(),
        }
    } else {
        completeness.blast.clone()
    };
    // Callers: thresholds use the EXACT total when known; the display list
    // may be capped. Text never presents a cap as a count (audit D4).
    let listed_callers = method_info.called_by.len();
    let callers_known_total = match &completeness.callers {
        ProviderStatus::Truncated {
            known_total: Some(t),
            ..
        } => Some(*t),
        ProviderStatus::Complete => Some(listed_callers),
        _ => None,
    };
    let caller_count = callers_known_total.unwrap_or(listed_callers);
    // Row-4 audit A9: name the counting rule so the number can be reconciled
    // with find_symbol_references (all-kinds edges) and blast_radius (causal).
    let caller_count_text = match (&completeness.callers, callers_known_total) {
        (ProviderStatus::Truncated { .. }, Some(t)) => {
            format!("{t} distinct callers (calls+dependency, dedup by caller)")
        }
        (ProviderStatus::Truncated { shown, .. }, None) => {
            format!("≥{shown} distinct callers (calls+dependency, dedup by caller; capped)")
        }
        _ => format!("{listed_callers} distinct callers (calls+dependency, dedup by caller)"),
    };
    let callers_unknown = completeness.callers.is_missing();
    let has_session_writes = !method_info.session_keys_written.is_empty();
    let has_triggers = blast_radius
        .map(|b| !b.seam_candidates.is_empty())
        .unwrap_or(false);
    let has_on_error = method_info
        .effects
        .iter()
        .any(|e| e.contains("On_Error_Resume_Next") || e.contains("OnErrorResumeNext"));
    let complexity = method_info.complexity_score;
    let is_web_service = method_info.method_kind == "WebMethod";
    // "No callers found" is only an orphan when the caller lookup ran to
    // completion AND no incoming edge was left unresolved (audit D11).
    let is_orphan = method_info.called_by.is_empty()
        && method_info.handles_clause.is_empty()
        && method_info.method_kind != "Lifecycle"
        && !callers_unknown
        && completeness.callers_dangling == 0;

    let mut reasons = Vec::new();
    let mut pre_checklist = Vec::new();
    let mut post_checklist = Vec::new();

    // ── RED: high-risk conditions ───────────────────────────────────────
    let verdict = if br_score > 60.0
        || caller_count > 15
        || is_web_service
        || has_on_error
        || complexity > 40
        || is_orphan
    {
        if has_on_error {
            reasons.push("On Error Resume Next makes behavior unknowable".to_string());
        }
        if br_score > 60.0 {
            reasons.push(format!(
                "Blast radius score {:.0} — high overall impact",
                br_score
            ));
        }
        if caller_count > 15 {
            reasons.push(format!("{caller_count_text} — high blast radius"));
        }
        if is_web_service {
            reasons.push("WebMethod — external consumers may depend on exact behavior".to_string());
        }
        if complexity > 40 {
            reasons.push(format!(
                "Complexity {} — hard to reason about changes",
                complexity
            ));
        }
        if is_orphan {
            reasons.push(
                "No bound callers found in the index — unresolved overloads, extraction gaps, reflection or dynamic dispatch may hide consumers".to_string(),
            );
        }
        if has_triggers {
            reasons.push("Seam candidates present — downstream triggers may fire".to_string());
        }
        pre_checklist.push("Write characterization tests before modifying".to_string());
        pre_checklist.push("Identify all callers including dynamic invocations".to_string());
        post_checklist.push("Run full regression suite".to_string());
        post_checklist.push("Verify all callers still compile".to_string());
        "red"
    }
    // ── YELLOW: moderate-risk conditions ─────────────────────────────────
    else if br_score > 20.0
        || caller_count > 3
        || has_session_writes
        || has_triggers
        || complexity > 15
    {
        if br_score > 20.0 {
            reasons.push(format!(
                "Blast radius score {:.0} — moderate overall impact",
                br_score
            ));
        }
        if caller_count > 3 {
            reasons.push(format!("{caller_count_text} — moderate blast radius"));
        }
        if has_session_writes {
            reasons.push("Writes session state — changes affect other pages".to_string());
        }
        if has_triggers {
            reasons.push("Seam candidates present — downstream triggers may fire".to_string());
        }
        if complexity > 15 {
            reasons.push(format!("Complexity {} — moderate", complexity));
        }
        pre_checklist.push("Review all callers for compatibility".to_string());
        if has_session_writes {
            pre_checklist.push("Audit session key consumers across all pages".to_string());
        }
        post_checklist.push("Test affected pages".to_string());
        "yellow"
    }
    // ── GREEN: safe ─────────────────────────────────────────────────────
    else {
        reasons.push("Low blast radius, few callers, no complex state".to_string());
        "green"
    };

    // Coverage floor (auditor P0: coverage was discarded before decisions).
    // A verdict computed from a TRUNCATED causal sweep can be missing the very
    // callers that would have raised it — incomplete evidence is never green.
    let incomplete_causal = blast_radius
        .map(|b| b.coverage.causal_truncated)
        .unwrap_or(false);
    let verdict = if incomplete_causal && verdict == "green" {
        reasons.push(
            "Blast-radius causal coverage INCOMPLETE (fetch cap hit) — dependents may be \
             hidden; safety is unknown, not green"
                .to_string(),
        );
        pre_checklist.push(
            "Enumerate callers with impact_analysis (per-tier coverage) before editing".into(),
        );
        "yellow"
    } else {
        if incomplete_causal {
            reasons.push(
                "Blast-radius causal coverage INCOMPLETE — treat the risk inputs as lower bounds"
                    .to_string(),
            );
        }
        verdict
    };

    // Provider floor (row-2 audit A2): evidence that FAILED or never ran is
    // not "clean". Required axes: blast radius, callers, complexity.
    let mut missing: Vec<String> = Vec::new();
    for (name, status) in [
        ("blast radius", &blast_status),
        ("callers", &completeness.callers),
        ("complexity", &completeness.complexity),
    ] {
        match status {
            ProviderStatus::Failed { reason } => {
                missing.push(format!("{name} unknown (provider FAILED: {reason})"))
            }
            ProviderStatus::NotRun { reason } => {
                missing.push(format!("{name} not measured ({reason})"))
            }
            _ => {}
        }
    }
    if completeness.callers_dangling > 0 {
        reasons.push(format!(
            "{} dangling caller edge(s) (source symbol not indexed) — fan-in is a lower bound",
            completeness.callers_dangling
        ));
    }
    let verdict = if !missing.is_empty() && verdict == "green" {
        reasons.push(format!(
            "Evidence INCOMPLETE — {} — safety is unknown, not green",
            missing.join("; ")
        ));
        pre_checklist.push(
            "Gather the missing evidence (impact_analysis / find_symbol_references, or reindex) \
             before editing"
                .to_string(),
        );
        "yellow"
    } else {
        if !missing.is_empty() {
            reasons.push(format!("Evidence INCOMPLETE — {}", missing.join("; ")));
        }
        verdict
    };

    let confidence = match verdict {
        "green" => 0.9,
        "yellow" if incomplete_causal || !missing.is_empty() => 0.5,
        "yellow" => 0.7,
        _ => 0.5,
    };

    let mut completeness = completeness.clone();
    completeness.blast = blast_status;
    EditSafetyResult {
        verdict: verdict.to_string(),
        confidence,
        reasons,
        pre_edit_checklist: pre_checklist,
        post_edit_checklist: post_checklist,
        completeness,
    }
}

// ── Render Helpers ───────────────────────────────────────────────────────────

fn render_method_info_markdown(info: &MethodInfoResult) -> String {
    let mut md = String::with_capacity(2_000);

    md.push_str(&format!("# Method: `{}`\n\n", info.fqn));
    md.push_str(&format!("- **File**: `{}`\n", info.file_path));
    md.push_str(&format!("- **Class**: `{}`\n", info.class_name));
    md.push_str(&format!("- **Signature**: `{}`\n", info.signature));
    md.push_str(&format!("- **Return type**: `{}`\n", info.return_type));
    md.push_str(&format!("- **Access**: `{}`\n", info.access_level));
    md.push_str(&format!(
        "- **Lines**: {}–{} ({} lines)\n",
        info.line_start, info.line_end, info.line_count
    ));
    md.push_str(&format!("- **Language**: {}\n", info.language));
    md.push_str(&format!("- **Kind**: {}\n", info.method_kind));
    md.push_str(&format!("- **Complexity**: {}\n", info.complexity_score));

    if !info.effects.is_empty() {
        md.push_str(&format!("- **Effects**: {}\n", info.effects.join(", ")));
    }
    if !info.handles_clause.is_empty() {
        md.push_str(&format!(
            "- **Handles**: {}\n",
            info.handles_clause.join(", ")
        ));
    }
    md.push('\n');

    if !info.called_by.is_empty() {
        md.push_str("## Called By\n\n");
        for c in &info.called_by {
            md.push_str(&format!(
                "- `{}` (`{}` line {})\n",
                c.fqn, c.file_path, c.line
            ));
        }
        md.push('\n');
    }

    if !info.calls_methods.is_empty() {
        md.push_str("## Calls\n\n");
        for m in &info.calls_methods {
            md.push_str(&format!("- `{}`\n", m));
        }
        md.push('\n');
    }

    if !info.db_tables_accessed.is_empty() {
        md.push_str("## Database Tables\n\n");
        for t in &info.db_tables_accessed {
            md.push_str(&format!("- `{}`\n", t));
        }
        md.push('\n');
    }

    if !info.stored_procs_called.is_empty() {
        md.push_str("## Stored Procedures\n\n");
        for sp in &info.stored_procs_called {
            md.push_str(&format!("- `{}`\n", sp));
        }
        md.push('\n');
    }

    if !info.session_keys_read.is_empty() || !info.session_keys_written.is_empty() {
        md.push_str("## Session/State Keys\n\n");
        for k in &info.session_keys_read {
            md.push_str(&format!("- reads `{}`\n", k));
        }
        for k in &info.session_keys_written {
            md.push_str(&format!("- writes `{}`\n", k));
        }
        md.push('\n');
    }

    if let Some(ref preview) = info.body_preview {
        let lang_tag = if info.language.contains("vb") {
            "vb"
        } else if info.language.contains("csharp") || info.language.contains("cs") {
            "csharp"
        } else {
            &info.language
        };
        md.push_str(&format!("## Body Preview\n\n```{}\n", lang_tag));
        md.push_str(preview);
        md.push_str("\n```\n");
    }

    md
}

fn expand_caller_bodies(
    graph: &GraphStore,
    project: &str,
    root: &str,
    target: Option<&Node>,
    requested: bool,
    limit: usize,
) -> (Vec<CallerBody>, CallerExpansion) {
    // The checked lookup needs one lookahead; avoid overflow for usize::MAX.
    let cap = limit.min(usize::MAX - 1);
    let mut expansion = CallerExpansion {
        requested,
        attempted: false,
        status: "not_requested".into(),
        returned: 0,
        cap,
        truncated: false,
        omission_reason: None,
        next_action: None,
        omissions: Vec::new(),
        coverage_interpretation: coverage_interpretation(),
    };
    let mut bodies = Vec::new();
    if !requested {
        return (bodies, expansion);
    }
    let Some(target) = target else {
        expansion.status = "unsupported_direct_range".into();
        expansion.omission_reason = Some(
            "Direct file/line ranges do not identify a unique indexed method for caller expansion."
                .into(),
        );
        expansion.next_action = Some("Resolve a unique indexed method with get_method_info, then call get_full_method_body with its fqn and include_caller_bodies=true. For ambiguous overloads, use get_method_edit_context with file_path, method_name and line.".into());
        return (bodies, expansion);
    };
    if cap == 0 {
        expansion.status = "omitted".into();
        expansion.omission_reason = Some("max_callers=0 disables caller expansion.".into());
        expansion.next_action =
            Some("Set max_callers above zero to request indexed caller bodies.".into());
        return (bodies, expansion);
    }
    expansion.attempted = true;
    let (callers, truncated) = match crate::handlers::incoming_caller_edges_checked(
        graph,
        project,
        &target.node_id,
        cap,
    ) {
        Ok(value) => value,
        Err(error) => {
            expansion.status = "failed".into();
            expansion.omission_reason = Some(format!("Indexed caller query failed: {error}"));
            expansion.next_action = Some("Retry the caller query after restoring graph access; inspect source references for consumers.".into());
            return (bodies, expansion);
        }
    };
    expansion.truncated = truncated;
    for (source_id, kind, _) in callers {
        let read = || -> Result<CallerBody, String> {
            let source = graph
                .get_node(project, &source_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Caller source node is missing from the index".to_string())?;
            verify_indexed_source_span(graph, project, root, source.file_path.as_str())?;
            let path =
                safe_join(Path::new(root), source.file_path.as_str()).map_err(|e| e.to_string())?;
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("Cannot read {}: {e}", source.file_path))?;
            let lines: Vec<_> = text.lines().collect();
            if source.start_line == 0 || source.end_line < source.start_line {
                return Err("Indexed caller line span is invalid; refresh the index".into());
            }
            let span = lines
                .get(source.start_line as usize - 1..source.end_line as usize)
                .ok_or_else(|| {
                    "Indexed caller line span exceeds available source; refresh the index"
                        .to_string()
                })?;
            Ok(CallerBody {
                fqn: fqn_from_node(&source),
                file_path: source.file_path.to_string(),
                line_start: source.start_line,
                line_end: source.end_line,
                source_code: span.join("\n"),
                how_it_calls: format!(
                    "indexed {} edge to {}",
                    kind.as_str(),
                    fqn_from_node(target)
                ),
            })
        };
        match read() {
            Ok(body) => bodies.push(body),
            Err(error) => expansion.omissions.push(format!("{source_id}: {error}")),
        }
    }
    expansion.returned = bodies.len();
    expansion.status = if !expansion.omissions.is_empty() {
        "partial"
    } else if truncated {
        "truncated"
    } else {
        "complete"
    }
    .into();
    if !expansion.omissions.is_empty() {
        expansion.omission_reason =
            Some("Some indexed caller bodies were withheld or unavailable; see omissions.".into());
        expansion.next_action = Some("Restore missing source or refresh stale index entries, then retry. If truncated is true, increase max_callers as well. Inspect source references for additional consumers.".into());
    } else if truncated {
        expansion.omission_reason = Some("Additional indexed callers exceed max_callers.".into());
        expansion.next_action = Some("Increase max_callers for more indexed caller bodies; inspect source references for additional consumers.".into());
    } else if bodies.is_empty() {
        expansion.next_action = Some("No indexed callers were returned. Search source references and inspect binding before concluding there are no consumers.".into());
    }
    (bodies, expansion)
}

fn render_method_body_markdown(result: &MethodBodyResult) -> String {
    let mut md = String::with_capacity(4_000);

    let lang_tag = if result.language.contains("vb") {
        "vb"
    } else {
        "csharp"
    };

    let direct_range = result.retrieval_scope == "explicit_source_range";
    if direct_range {
        md.push_str("# Requested Source Range\n\n");
    } else {
        md.push_str(&format!("# Method Body: `{}`\n\n", result.fqn));
    }
    md.push_str(result.boundary_guidance);
    md.push_str("\n\n");
    md.push_str(&format!("**Source verification**: `{}`\n\n**Source file BLAKE3**: `{}`\n\n", result.source_verification, result.source_file_hash));
    md.push_str(&format!(
        "**File**: `{}` (lines {}–{})\n\n",
        result.file_path, result.line_start, result.line_end
    ));

    if !result.surrounding_context.is_empty() {
        md.push_str("## Context (above returned source)\n\n```");
        md.push_str(lang_tag);
        md.push('\n');
        md.push_str(&result.surrounding_context);
        md.push_str("\n```\n\n");
    }

    md.push_str(if direct_range {
        "## Requested Source Lines\n\n```"
    } else if result.source_verification != "matched_indexed_fingerprint" {
        "## Indexed Method Span (source fingerprint unverified)\n\n```"
    } else {
        "## Full Method Body\n\n```"
    });
    md.push_str(lang_tag);
    md.push('\n');
    md.push_str(&result.source_code);
    md.push_str("\n```\n\n");

    let expansion = &result.caller_expansion;
    md.push_str(&format!(
        "## Caller Expansion\n\nRequested: {}; attempted: {}; status: {}; returned: {}; cap: {}; truncated: {}.\n\n{}\n\n",
        expansion.requested, expansion.attempted, expansion.status, expansion.returned,
        expansion.cap, expansion.truncated, expansion.coverage_interpretation,
    ));
    if let Some(reason) = &expansion.omission_reason {
        md.push_str(&format!("Omission reason: {reason}\n\n"));
    }
    for omission in &expansion.omissions {
        md.push_str(&format!("- {omission}\n"));
    }
    if let Some(action) = &expansion.next_action {
        md.push_str(&format!("\nNext action: {action}\n\n"));
    }

    if !result.caller_bodies.is_empty() {
        md.push_str("## Caller Bodies\n\n");
        for cb in &result.caller_bodies {
            md.push_str(&format!(
                "### `{}` (`{}` declaration/body lines {}–{}) — {}\n\n```{}\n{}\n```\n\n",
                cb.fqn,
                cb.file_path,
                cb.line_start,
                cb.line_end,
                cb.how_it_calls,
                lang_tag,
                cb.source_code,
            ));
        }
    }

    md
}

/// Extract server controls from ASPX markup.
///
/// Handles both regular closing tags (`>`) and self-closing tags (`/>`).
/// Captures all event handlers (OnClick, OnSelectedIndexChanged, etc.)
/// concatenated with `;` if multiple events are present on one control.
fn extract_aspx_controls(aspx_content: &str) -> Vec<ControlInfo> {
    // Match <asp:Type ... ID="foo" ... > or <asp:Type ... ID="foo" ... />
    static CONTROL_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)<asp:(\w+)[^>]*\bID\s*=\s*"([^"]+)"[^>]*/?\s*>"#)
            .expect("control regex")
    });
    // Match all On<Event>="handler" attributes (global, captures all occurrences)
    static EVENT_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)\bOn(\w+)\s*=\s*"([^"]+)""#).expect("event regex")
    });
    static CAUSES_VALIDATION_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| {
            regex::Regex::new(r#"(?i)\bCausesValidation\s*=\s*"(true|false)""#)
                .expect("causes validation regex")
        });
    static VALIDATION_GROUP_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| {
            regex::Regex::new(r#"(?i)\bValidationGroup\s*=\s*"([^"]+)""#)
                .expect("validation group regex")
        });

    let mut controls = Vec::new();
    for (line_idx, line) in aspx_content.lines().enumerate() {
        if let Some(cap) = CONTROL_RE.captures(line) {
            let control_type = cap[1].to_string();
            let server_id = cap[2].to_string();
            let line_num = (line_idx + 1) as u32;

            // Collect ALL event handlers on this control (not just the first)
            let event_handlers: Vec<String> = EVENT_RE
                .captures_iter(line)
                .map(|c| c[2].to_string())
                .collect();
            let event_handler = if event_handlers.is_empty() {
                None
            } else {
                Some(event_handlers.join("; "))
            };

            let causes_validation = CAUSES_VALIDATION_RE
                .captures(line)
                .map(|c| c[1].eq_ignore_ascii_case("true"));

            let validation_group = VALIDATION_GROUP_RE.captures(line).map(|c| c[1].to_string());

            controls.push(ControlInfo {
                server_id,
                control_type,
                line: line_num,
                event_handler,
                causes_validation,
                validation_group,
                observed_at_runtime: false,
            });
        }
    }
    controls
}

fn render_method_edit_context_markdown(ctx: &MethodEditContextResult) -> String {
    let mut md = String::with_capacity(8_000);

    // Header with verdict badge
    let badge = match ctx.edit_safety.verdict.as_str() {
        "green" => "🟢 GREEN",
        "yellow" => "🟡 YELLOW",
        "red" => "🔴 RED",
        _ => "⚪ UNKNOWN",
    };
    md.push_str(&format!(
        "# Edit Context: `{}`  {}\n\n",
        ctx.method_info.fqn, badge
    ));

    // Method identity
    md.push_str(&render_method_info_markdown(&ctx.method_info));

    // Full source
    if let Some(ref src) = ctx.full_source {
        let lang = if ctx.method_info.language.contains("vb") {
            "vb"
        } else {
            "csharp"
        };
        md.push_str("## Full Source\n\n```");
        md.push_str(lang);
        md.push('\n');
        md.push_str(src);
        md.push_str("\n```\n\n");
    }

    // Callers: compact identity lines by default; fenced bodies only when
    // the caller opted into include_caller_bodies (source_code non-empty).
    if !ctx.caller_bodies.is_empty() {
        let total = match &ctx.edit_safety.completeness.callers {
            ProviderStatus::Truncated {
                known_total: Some(t),
                ..
            } => t.to_string(),
            ProviderStatus::Truncated { shown, .. } => format!("≥{shown} (capped)"),
            ProviderStatus::Complete => ctx.method_info.called_by.len().to_string(),
            _ => "unknown".to_string(),
        };
        md.push_str(&format!(
            "## Callers ({} shown of {} distinct callers — calls+dependency, dedup by caller)\n\n",
            ctx.caller_bodies.len(),
            total
        ));
        for cb in &ctx.caller_bodies {
            if cb.source_code.is_empty() {
                md.push_str(&format!(
                    "- `{}` — declaration: {}:{} — {}\n",
                    cb.fqn, cb.file_path, cb.line_start, cb.how_it_calls,
                ));
                continue;
            }
            let lang = if cb.file_path.to_lowercase().ends_with(".vb") {
                "vb"
            } else {
                "csharp"
            };
            md.push_str(&format!(
                "### `{}` (`{}` declaration/body lines {}–{}) — {}\n\n```{}\n{}\n```\n\n",
                cb.fqn,
                cb.file_path,
                cb.line_start,
                cb.line_end,
                cb.how_it_calls,
                lang,
                cb.source_code,
            ));
        }
        if ctx
            .caller_bodies
            .first()
            .is_some_and(|cb| cb.source_code.is_empty())
        {
            md.push_str(
                "\n(caller bodies omitted — re-call with include_caller_bodies=true to read them)\n",
            );
        }
        md.push('\n');
    }

    if let Some(history) = &ctx.historical_changes {
        md.push_str("## Historical changes to inspect\n\n");
        md.push_str(history);
        md.push_str("\n\n");
    }
    if !ctx.caller_excerpts.is_empty() {
        md.push_str("## Caller source excerpts\n\n");
        for excerpt in &ctx.caller_excerpts {
            md.push_str(&format!(
                "### {} ({}) — {}\n\n{}\n\n",
                excerpt.caller, excerpt.file_path, excerpt.status, excerpt.detail
            ));
            for line in excerpt.numbered_source.lines() {
                md.push_str(&format!("    {line}\n"));
            }
            md.push('\n');
        }
    }

    md.push_str("## Unresolved caller inspection leads\n\n");
    md.push_str(&format!(
        "{} (status: {}; truncated: {})\n\n",
        ctx.unresolved_caller_leads.detail,
        ctx.unresolved_caller_leads.status,
        ctx.unresolved_caller_leads.truncated
    ));
    for excerpt in &ctx.unresolved_caller_leads.excerpts {
        md.push_str(&format!(
            "### {} ({}) — {}\n\n{}\n\n",
            excerpt.caller, excerpt.file_path, excerpt.status, excerpt.detail
        ));
        for line in excerpt.numbered_source.lines() {
            md.push_str(&format!("    {line}\n"));
        }
        md.push('\n');
    }

    // VB traps
    if !ctx.vb_traps.is_empty() {
        md.push_str("## VB Translation Traps\n\n");
        md.push_str("| Location | Trap | Risk | Guidance |\n");
        md.push_str("|----------|------|------|----------|\n");
        for t in &ctx.vb_traps {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                t.location, t.trap, t.risk, t.guidance,
            ));
        }
        md.push('\n');
    }

    // Sync hazards
    if !ctx.sync_hazards.is_empty() {
        md.push_str("## Sync Hazards\n\n");
        md.push_str("| Line | Pattern | Severity | Modern Equivalent |\n");
        md.push_str("|------|---------|----------|-------------------|\n");
        for h in &ctx.sync_hazards {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                h.line, h.pattern, h.severity, h.modern_equivalent,
            ));
        }
        md.push('\n');
    }

    // Edit safety verdict
    md.push_str("## Edit Safety\n\n");
    md.push_str(&format!(
        "- **Verdict**: {} (confidence {:.0}%)\n",
        badge,
        ctx.edit_safety.confidence * 100.0
    ));
    match ctx.blast_radius_score {
        Some(score) => md.push_str(&format!(
            "- **Blast radius**: {:.0} ({}) — estimate from indexed evidence; incomplete extraction or binding can omit consumers and side effects.\n",
            score, ctx.risk_band
        )),
        None => md.push_str(&format!(
            "- **Blast radius**: UNKNOWN — {}\n",
            provider_text(&ctx.edit_safety.completeness.blast)
        )),
    }
    for r in &ctx.edit_safety.reasons {
        md.push_str(&format!("- {}\n", r));
    }
    md.push('\n');

    if !ctx.edit_safety.pre_edit_checklist.is_empty() {
        md.push_str("### Pre-Edit Checklist\n\n");
        for item in &ctx.edit_safety.pre_edit_checklist {
            md.push_str(&format!("- [ ] {}\n", item));
        }
        md.push('\n');
    }

    if !ctx.edit_safety.post_edit_checklist.is_empty() {
        md.push_str("### Post-Edit Checklist\n\n");
        for item in &ctx.edit_safety.post_edit_checklist {
            md.push_str(&format!("- [ ] {}\n", item));
        }
        md.push('\n');
    }

    md.push_str(&render_coverage_block(&ctx.edit_safety.completeness));

    if let Some(ref bl) = ctx.business_logic {
        md.push_str("## Business Logic\n\n");
        md.push_str(&format!("_{}_\n\n", bl.note));
        for h in &bl.hits {
            let body: String = h.content.chars().take(600).collect();
            md.push_str(&format!(
                "### {} (score {:.3})\n\n{}\n\n",
                h.path, h.score, body
            ));
        }
    }

    md
}

fn render_page_context_markdown(ctx: &PageContextResult) -> String {
    let mut md = String::with_capacity(12_000);

    md.push_str(&format!("# Page Context: `{}`\n\n", ctx.aspx_file));
    md.push_str(&format!("- **Code-behind**: `{}`\n", ctx.codebehind_file));
    md.push_str(&format!("- **Class**: `{}`\n", ctx.class_name));
    md.push_str(&format!("- **Language**: {}\n", ctx.language));
    {
        let c = &ctx.completeness;
        md.push_str(&format!(
            "- **Coverage**: code-behind {} · methods {} · controls {} · runtime {} · wiring {} · data edges {} · master page {} · ajax {}\n",
            provider_text(&c.codebehind),
            provider_text(&c.methods),
            provider_text(&c.controls),
            provider_text(&c.runtime),
            provider_text(&c.wiring),
            provider_text(&c.data_edges),
            provider_text(&c.master_page),
            provider_text(&c.ajax),
        ));
    }
    if let Some(ref mp) = ctx.master_page {
        md.push_str(&format!("- **Master page**: `{}`\n", mp));
    }
    md.push_str("\n## Source composition\nStatic declarations; runtime verification not run.\n");
    for component in &ctx.composition.files {
        let parent = if component.declared_by.is_empty() {
            "entry page"
        } else {
            &component.declared_by
        };
        md.push_str(&format!(
            "- {}: `{}` (declared by `{parent}`)\n",
            component.kind, component.path
        ));
    }
    for binding in &ctx.composition.bindings {
        md.push_str(&format!(
            "- `{}` -> `{}` placeholder `{}`: {}\n",
            binding.child, binding.master, binding.placeholder, binding.status
        ));
    }
    for warning in &ctx.composition.warnings {
        md.push_str(&format!("- INCOMPLETE: {warning}\n"));
    }
    if !ctx.content_placeholders.is_empty() {
        md.push_str(&format!(
            "- **Content placeholders**: {}\n",
            ctx.content_placeholders.join(", ")
        ));
    }
    md.push_str(&format!("- **Total methods**: {}\n", ctx.total_methods));
    md.push_str(&format!(
        "- **Authentication required**: {}\n",
        ctx.requires_authentication
    ));
    if ctx.has_script_manager {
        md.push_str("- **ScriptManager**: present (AJAX enabled)\n");
    }
    if ctx.vb_trap_count > 0 {
        md.push_str(&format!("- **VB traps**: {} detected\n", ctx.vb_trap_count));
    }
    md.push_str(&format!(
        "- **UI coverage confidence**: {:.0}%\n",
        ctx.ui_coverage_confidence * 100.0
    ));
    md.push('\n');

    if let Some(ref warning) = ctx.runtime_controls_warning {
        md.push_str("> [!WARNING]\n");
        md.push_str(&format!("> {}\n\n", warning));
        if !ctx.dynamic_ui_evidence.is_empty() {
            md.push_str("> Evidence:\n");
            for evidence in &ctx.dynamic_ui_evidence {
                md.push_str(&format!("> - {}\n", evidence));
            }
            md.push('\n');
        }
    }

    // Controls
    if !ctx.controls.is_empty() {
        md.push_str("## Server Controls\n\n");
        md.push_str("| ID | Type | Line | Event Handler | Validation |\n");
        md.push_str("|----|------|------|---------------|------------|\n");
        for c in &ctx.controls {
            md.push_str(&format!(
                "| `{}`{} | {} | {} | {} | {} |\n",
                c.server_id,
                if c.observed_at_runtime {
                    " 🟢 observed at runtime"
                } else {
                    ""
                },
                c.control_type,
                c.line,
                c.event_handler.as_deref().unwrap_or("—"),
                c.causes_validation
                    .map(|v| if v { "Yes" } else { "No" })
                    .unwrap_or("—"),
            ));
        }
        md.push('\n');
    }

    // Update panels
    if !ctx.update_panels.is_empty() {
        md.push_str("## UpdatePanels\n\n");
        for p in &ctx.update_panels {
            md.push_str(&format!(
                "- **{}** (mode: {}) — controls: {}\n",
                p.panel_id,
                p.update_mode,
                p.controls_inside.join(", ")
            ));
        }
        md.push('\n');
    }

    // Methods
    if !ctx.methods.is_empty() {
        let lang = if ctx.language.contains("vb") {
            "vb"
        } else {
            "csharp"
        };

        md.push_str("## Methods\n\n");
        for m in &ctx.methods {
            md.push_str(&format!(
                "### `{}`{} ({}) — lines {}–{}\n",
                m.name,
                if m.observed_at_runtime {
                    " 🟢 observed at runtime"
                } else {
                    ""
                },
                m.kind,
                m.line_start,
                m.line_end
            ));
            if !m.handles_clause.is_empty() {
                md.push_str(&format!("Handles: {}\n", m.handles_clause.join(", ")));
            }
            if !m.effects.is_empty() {
                md.push_str(&format!("Effects: {}\n", m.effects.join(", ")));
            }
            if let Some(ref body) = m.full_body {
                md.push_str(&format!("\n```{}\n{}\n```\n", lang, body));
            }
            md.push('\n');
        }
        if ctx.methods.iter().all(|m| m.full_body.is_none()) && !ctx.methods.is_empty() {
            md.push_str(
                "(method bodies omitted — get_full_method_body(<fqn>) for one, \
                 or re-call with include_method_bodies=true for all)\n\n",
            );
        }
    }

    // Data layer
    if !ctx.tables_used.is_empty() {
        md.push_str("## Database Tables\n\n");
        for t in &ctx.tables_used {
            md.push_str(&format!("- `{}`\n", t));
        }
        md.push('\n');
    }
    if !ctx.stored_procs_called.is_empty() {
        md.push_str("## Stored Procedures\n\n");
        for sp in &ctx.stored_procs_called {
            md.push_str(&format!("- `{}`\n", sp));
        }
        md.push('\n');
    }

    if !ctx.runtime_sql_observations.is_empty() {
        md.push_str("## Runtime SQL Observations\n\n");
        for sql in &ctx.runtime_sql_observations {
            md.push_str(&format!("- `{}` (observed at runtime)\n", sql));
        }
        md.push('\n');
    }

    // Session keys
    if !ctx.session_keys.is_empty() {
        md.push_str("## Session/State Keys\n\n");
        for k in &ctx.session_keys {
            md.push_str(&format!("- `{}`\n", k));
        }
        md.push('\n');
    }

    // Row 5 v3: house style of the territory
    if let Some(hs) = &ctx.house_style {
        md.push_str(&crate::services::house_style::render_house_style(hs));
    }

    // VB traps summary
    if !ctx.vb_traps_summary.is_empty() {
        md.push_str("## VB Translation Traps\n\n");
        for t in &ctx.vb_traps_summary {
            md.push_str(&format!("- {}\n", t));
        }
        md.push('\n');
    }

    md
}

fn render_implementation_context_markdown(ctx: &ImplementationContext) -> String {
    let mut md = String::with_capacity(16_000);
    let lang_tag = if ctx.method_info.language.contains("vb") {
        "vb"
    } else {
        "csharp"
    };

    md.push_str(&format!(
        "# Implementation Context: `{}`\n\n",
        ctx.method_info.fqn
    ));
    md.push_str(&format!("{}\n\n", ctx.coverage_interpretation));

    if !ctx.warnings.is_empty() {
        md.push_str("## Warnings (partial evidence)\n\n");
        for w in &ctx.warnings {
            md.push_str(&format!("- {w}\n"));
        }
        md.push('\n');
    }

    // Method identity (compact)
    md.push_str(&format!(
        "**File**: `{}` | **Class**: `{}` | **Kind**: {} | **Lines**: {}–{}\n\n",
        ctx.method_info.file_path,
        ctx.method_info.class_name,
        ctx.method_info.method_kind,
        ctx.method_info.line_start,
        ctx.method_info.line_end,
    ));

    // Method body
    if let Some(ref body) = ctx.method_body {
        md.push_str("## Current Method Body\n\n```");
        md.push_str(lang_tag);
        md.push('\n');
        md.push_str(body);
        md.push_str("\n```\n\n");
    }

    // Coding style profile
    if let Some(ref style) = ctx.style_profile {
        md.push_str("## Coding Style Profile\n\n");
        md.push_str(style);
        md.push_str("\n\n");
    }

    // Pattern examples from callers
    if !ctx.pattern_examples.is_empty() {
        md.push_str(&format!(
            "## Pattern Examples ({} callers)\n\n",
            ctx.pattern_examples.len()
        ));
        for ex in &ctx.pattern_examples {
            md.push_str(&format!(
                "### `{}` (`{}` lines {}–{})\n\n{}\n\n```{}\n{}\n```\n\n",
                ex.caller_fqn,
                ex.caller_file,
                ex.line_start,
                ex.line_end,
                ex.call_pattern,
                lang_tag,
                ex.source_code,
            ));
        }
    }

    // Database schema
    if !ctx.schema_snippets.is_empty() {
        md.push_str("## Database Schema\n\n");
        for tbl in &ctx.schema_snippets {
            md.push_str(&format!("### Table: `{}`\n\n", tbl.table_name));
            if tbl.columns.is_empty() {
                md.push_str("(No column details indexed)\n\n");
            } else {
                md.push_str("| Column | Type | Nullable |\n");
                md.push_str("|--------|------|----------|\n");
                for col in &tbl.columns {
                    md.push_str(&format!(
                        "| `{}` | {} | {} |\n",
                        col.name,
                        if col.data_type.is_empty() {
                            "—"
                        } else {
                            &col.data_type
                        },
                        match col.nullable {
                            Some(true) => "Yes",
                            Some(false) => "No",
                            None => "Unknown",
                        },
                    ));
                }
                md.push('\n');
            }
        }
    }

    // SP signatures
    if !ctx.sp_signatures.is_empty() {
        md.push_str("## Stored Procedure Signatures\n\n");
        for sp in &ctx.sp_signatures {
            md.push_str(&format!("### `{}`\n\n", sp.sp_name));
            md.push_str(&format!("Coverage: {}\n", sp.coverage));
            if !sp.parameters.is_empty() {
                md.push_str(&format!("Parameters: {}\n", sp.parameters.join(", ")));
            }
            if !sp.tables_read.is_empty() {
                md.push_str(&format!("Reads: {}\n", sp.tables_read.join(", ")));
            }
            if !sp.tables_written.is_empty() {
                md.push_str(&format!("Writes: {}\n", sp.tables_written.join(", ")));
            }
            md.push('\n');
        }
    }

    // State context
    if !ctx.state_context.is_empty() {
        md.push_str("## Session/State Context\n\n");
        md.push_str("| Key | This Method | Other Readers | Other Writers |\n");
        md.push_str("|-----|-------------|---------------|---------------|\n");
        for sc in &ctx.state_context {
            let this_op = match (sc.this_method_reads, sc.this_method_writes) {
                (true, true) => "reads+writes",
                (true, false) => "reads",
                (false, true) => "writes",
                _ => "—",
            };
            md.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                sc.key,
                this_op,
                if sc.other_readers.is_empty() {
                    "—".to_string()
                } else {
                    sc.other_readers.join(", ")
                },
                if sc.other_writers.is_empty() {
                    "—".to_string()
                } else {
                    sc.other_writers.join(", ")
                },
            ));
        }
        md.push('\n');
    }

    // Control mappings
    if !ctx.control_mappings.is_empty() {
        md.push_str("## Control Mappings\n\n");
        for cm in &ctx.control_mappings {
            md.push_str(&format!("### `{}` ({})\n\n", cm.control_id, cm.legacy_type));
            md.push_str(&format!("Modern: `{}`\n", cm.modern_equivalent));
            if !cm.event_mappings.is_empty() {
                md.push_str("Events:\n");
                for (from, to) in &cm.event_mappings {
                    md.push_str(&format!("  - `{}` → `{}`\n", from, to));
                }
            }
            for note in &cm.migration_notes {
                md.push_str(&format!("- {}\n", note));
            }
            md.push('\n');
        }
    }

    // VB traps
    if !ctx.vb_traps.is_empty() {
        md.push_str("## VB Translation Traps\n\n");
        md.push_str("| Location | Trap | Risk | Guidance |\n");
        md.push_str("|----------|------|------|----------|\n");
        for t in &ctx.vb_traps {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                t.location, t.trap, t.risk, t.guidance,
            ));
        }
        md.push('\n');
    }

    if !ctx.language_diagnostics.is_empty() {
        md.push_str("## Language Diagnostics\n\n");
        md.push_str("| Location | Category | Severity | Evidence | Guidance |\n");
        md.push_str("|----------|----------|----------|----------|----------|\n");
        for d in &ctx.language_diagnostics {
            md.push_str(&format!(
                "| {} | {} | {} | `{}` | {} |\n",
                d.location,
                d.category,
                d.severity,
                d.evidence.replace('`', "'"),
                d.guidance
            ));
        }
        md.push('\n');
    }

    // Sync hazards
    if !ctx.sync_hazards.is_empty() {
        md.push_str("## Sync Hazards\n\n");
        md.push_str("| Line | Pattern | Severity | Modern Equivalent |\n");
        md.push_str("|------|---------|----------|-------------------|\n");
        for h in &ctx.sync_hazards {
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                h.line, h.pattern, h.severity, h.modern_equivalent,
            ));
        }
        md.push('\n');
    }

    md
}

/// Round-6: the resolution of the target file against the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetStatus {
    /// No target_file was supplied.
    Unspecified,
    /// The exact path is present in the index.
    Exists,
    /// change_kind=modify but the exact path is not in the index.
    NotFound,
    /// change_kind=create and the path is absent — expected, not a failure.
    NewTarget,
    /// The index lookup itself failed — we cannot know.
    ProviderFailed,
}

/// Round-6/8: what the validation actually COVERED, modelled apart from the
/// individual pass/warn/fail checks. A green generic-lint scan or a green
/// caller-assertion scan is not coverage of the project's contract.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ValidationCoverage {
    /// PROJECT-DERIVED verifications that completed (a resolved target method, a
    /// real-schema table-consistency check). ONLY these earn a PASS.
    pub verified_checks: usize,
    /// Caller-supplied expectations checked for presence — the caller could
    /// assert anything, so this is NOT coverage of the project's contract.
    pub assertion_checks: usize,
    /// Project-independent language lints run (VB traps, sync hazards).
    pub generic_lint_checks: usize,
    pub target: TargetStatus,
    /// The caller declared change_kind=modify — a modification can only be
    /// verified against an EXACT existing target.
    pub change_kind_modify: bool,
}

/// Round-6/8 (fail-closed, re-audited twice): the overall verdict.
///
/// A "post-generation safety net" only PASSES when it actually verified the
/// project's contract with a PROJECT-DERIVED check. Round-5 passed on an
/// always-on lint; round-7 still counted every non-excluded check — so a green
/// VB-trap lint or a caller's own asserted substring (even inside a comment)
/// earned PASS (round-8 P0-1, reproduced live). The rule now keys on the
/// EVIDENCE CLASS of the checks, not their count:
///
/// - any failing check                          => FAIL (incl. language/target
///   mismatch, a modify target NOT in the index, a critical hazard)
/// - the target lookup itself failed             => INSUFFICIENT (cannot know)
/// - change_kind=modify without an EXACT target  => INSUFFICIENT (nothing to
///   verify the modification against)
/// - no PROJECT-DERIVED verified check ran        => INSUFFICIENT (a caller
///   assertion or generic lint is not project coverage)
/// - any warning                                 => WARN
/// - otherwise                                   => PASS
/// Round-8 P0-1: the file extensions a language may legitimately target. A
/// mismatch (C# code for a `.vb` file) makes any content "verification"
/// meaningless. Returns a human message when the target's extension cannot host
/// the declared language; `None` when compatible, unknown, or no target given.
pub fn language_target_mismatch(language: &str, target: Option<&str>) -> Option<String> {
    let target = target?;
    let ext = target.rsplit('.').next().map(|e| e.to_lowercase())?;
    let lang = language.to_lowercase();
    let ok: &[&str] = if lang.starts_with("vb") {
        &[
            "vb", "vbhtml", "aspx", "ascx", "master", "asmx", "ashx", "asax",
        ]
    } else if lang.starts_with("cs") || lang == "c#" || lang == "csharp" {
        &[
            "cs", "cshtml", "aspx", "ascx", "master", "asmx", "ashx", "asax", "razor",
        ]
    } else if lang.starts_with("ts") {
        &["ts", "tsx", "d.ts"]
    } else if lang.starts_with("js") || lang.starts_with("javascript") {
        &["js", "jsx", "mjs", "cjs"]
    } else if lang.starts_with("sql") {
        &["sql", "dbml"]
    } else {
        // Unknown language — cannot assert a mismatch.
        return None;
    };
    if ok.contains(&ext.as_str()) {
        None
    } else {
        Some(format!(
            "declared language `{language}` cannot target `{target}` (.{ext}); the generated code is for a different file type — any content check against this target is meaningless"
        ))
    }
}

/// Round-8 P0-1: strip line and block comments so a token that appears ONLY in
/// a comment (`// audit_probe_key`) does not satisfy a presence check. This is a
/// lexical strip, not a full parse — it removes `'…` (VB) and `//…`, `/* … */`
/// (C-family) comments; string literals are intentionally left in place (a
/// token inside a real string literal is at least present in the emitted code).
pub fn strip_code_comments(code: &str, is_vb: bool) -> String {
    let mut out = String::with_capacity(code.len());
    if is_vb {
        for line in code.lines() {
            // A leading-or-inline `'` starts a comment unless inside a string.
            let mut in_str = false;
            let mut cut = line.len();
            for (i, ch) in line.char_indices() {
                match ch {
                    '"' => in_str = !in_str,
                    '\'' if !in_str => {
                        cut = i;
                        break;
                    }
                    _ => {}
                }
            }
            out.push_str(&line[..cut]);
            out.push('\n');
        }
        return out;
    }
    // C-family: // line and /* block */, string-literal aware for " and '.
    let b = code.as_bytes();
    let mut i = 0;
    let mut in_str: Option<u8> = None; // Some(quote) when inside a string
    while i < b.len() {
        let c = b[i];
        if let Some(q) = in_str {
            out.push(c as char);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => {
                in_str = Some(c);
                out.push(c as char);
                i += 1;
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
                out.push(' ');
            }
            _ => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

pub fn compute_validation_verdict(
    checks: &[ValidationCheck],
    coverage: &ValidationCoverage,
) -> String {
    if checks.iter().any(|c| c.status == "fail") {
        return "FAIL".to_string();
    }
    if matches!(coverage.target, TargetStatus::ProviderFailed) {
        return "INSUFFICIENT".to_string();
    }
    if coverage.change_kind_modify && !matches!(coverage.target, TargetStatus::Exists) {
        // A modification is verified AGAINST the existing file; without it in
        // the index there is nothing to check the change against.
        return "INSUFFICIENT".to_string();
    }
    if coverage.verified_checks == 0 {
        // Nothing PROJECT-DERIVED was verified. A passing generic lint, or a
        // green check of the caller's own asserted strings, says nothing about
        // whether this code is correct for THIS project.
        return "INSUFFICIENT".to_string();
    }
    if checks.iter().any(|c| c.status == "warn") {
        return "WARN".to_string();
    }
    "PASS".to_string()
}

fn render_validation_report_markdown(report: &ValidationReport) -> String {
    let mut md = String::with_capacity(4_000);

    let badge = match report.overall_verdict.as_str() {
        "PASS" => "PASS",
        "WARN" => "WARN",
        "FAIL" => "FAIL",
        // Round-7 P1-3: "INSUFFICIENT" means no PROJECT CONTRACT was verified —
        // a generic lint scan may still have run. Be precise, not hardcoded.
        "INSUFFICIENT" => "INSUFFICIENT (no project contract verified)",
        _ => "UNKNOWN",
    };

    md.push_str(&format!("# Code Validation Report: {}\n\n", badge));

    // Round-7 P1-3: surface the coverage the verdict rests on.
    let target_label = match report.coverage.target {
        TargetStatus::Unspecified => "no target file given",
        TargetStatus::Exists => "target file exists in index",
        TargetStatus::NotFound => "target file NOT in index",
        TargetStatus::NewTarget => "new file (create) — absence expected",
        TargetStatus::ProviderFailed => "target lookup FAILED (unknown)",
    };
    md.push_str(&format!(
        "_Coverage: {} project-derived verified, {} caller-assertion, {} generic-lint check(s); {}._\n\n",
        report.coverage.verified_checks,
        report.coverage.assertion_checks,
        report.coverage.generic_lint_checks,
        target_label
    ));

    if report.checks.is_empty() {
        md.push_str("No validation checks were performed (no expected values provided).\n");
        return md;
    }

    md.push_str("| Category | Status | Details |\n");
    md.push_str("|----------|--------|---------|\n");
    for check in &report.checks {
        let status_icon = match check.status.as_str() {
            "pass" => "PASS",
            "warn" => "WARN",
            "fail" => "FAIL",
            _ => "?",
        };
        let first_detail = check.details.first().map(|s| s.as_str()).unwrap_or("—");
        md.push_str(&format!(
            "| {} | {} | {} |\n",
            check.category, status_icon, first_detail,
        ));
    }
    md.push('\n');

    // Detailed check results
    for check in &report.checks {
        if check.details.len() > 1 || check.status != "pass" {
            md.push_str(&format!("### {} ({})\n\n", check.category, check.status));
            for detail in &check.details {
                md.push_str(&format!("- {}\n", detail));
            }
            md.push('\n');
        }
    }

    md
}

fn render_sql_validation_markdown(report: &SqlValidationReport) -> String {
    let mut md = String::with_capacity(2_000);

    md.push_str(&format!(
        "# SQL Validation: {}\n\n{}\n\n",
        report.verdict, report.coverage
    ));

    if !report.tables_referenced.is_empty() {
        md.push_str(&format!(
            "**Tables referenced**: {}\n\n",
            report.tables_referenced.join(", ")
        ));
    }

    if report.issues.is_empty() {
        md.push_str("No issues detected.\n");
    } else {
        md.push_str("| Severity | Category | Message |\n");
        md.push_str("|----------|----------|---------|\n");
        for issue in &report.issues {
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                issue.severity, issue.category, issue.message,
            ));
        }
    }

    md
}

// ── Tool Handlers ────────────────────────────────────────────────────────────

impl Engram {
    /// Freshness envelope for access-layer responses: optional per-file
    /// drift banner (file changed on disk AFTER the last index — the graph
    /// line numbers these tools read bodies by may be shifted) plus the
    /// standard one-line footer. The wall-clock footer alone cannot catch
    /// drift caused by the agent's own edits seconds ago.
    pub(crate) async fn access_freshness(
        &self,
        project_id: &str,
        project_dir: &str,
        rel_file: Option<&str>,
    ) -> (Option<String>, String) {
        let reg = self.state.registry.clone();
        let pid = project_id.to_string();
        let last_ms = tokio::task::spawn_blocking(move || {
            reg.get_meta(&pid, "last_index_completed_ms")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u64>().ok())
        })
        .await
        .unwrap_or(None);
        let banner = rel_file.and_then(|rf| {
            let abs = safe_join(Path::new(project_dir), rf).ok()?;
            crate::utils::envelope::stale_file_banner(
                rf,
                crate::utils::envelope::file_mtime_ms(&abs),
                last_ms,
            )
        });
        let gen_ = self.get_active_generation(project_id).await.unwrap_or(0);
        (banner, crate::utils::envelope::footer(gen_, last_ms))
    }

    // ── 38-1: get_method_info ─────────────────────────────────────────────

    pub async fn handle_get_method_info(
        &self,
        req: GetMethodInfoRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let fqn_or_name = req.fqn_or_name.clone();
        let file_filter = req.file_path.clone();
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            // Strategy: query all function nodes that match the name pattern,
            // then filter by file path if provided. This is fast because
            // query_nodes does an in-Redb prefix scan.
            let candidates = graph
                .query_nodes(
                    &project_id,
                    Some("function"),
                    Some(&fqn_or_name),
                    file_filter.as_deref(),
                    500,
                )
                .unwrap_or_default();

            if candidates.is_empty() {
                return Err(method_not_found_message(
                    &graph,
                    &project_id,
                    &fqn_or_name,
                    file_filter.as_deref(),
                ));
            }

            // Build full MethodInfoResult for each match
            let results: Vec<MethodInfoResult> = candidates
                .iter()
                .map(|n| build_method_info_from_node(n, &graph, &project_id))
                .collect();

            Ok(results)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let results = result.map_err(|e| McpError::invalid_params(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&results)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        if results.len() == 1 {
            let (banner, footer) = self
                .access_freshness(
                    &req.project_id,
                    &rec.directory,
                    Some(results[0].file_path.as_str()),
                )
                .await;
            let mut out = banner.unwrap_or_default();
            out.push_str(&render_method_info_markdown(&results[0]));
            out.push_str(&footer);
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        // Multiple matches: always render the summary table. Full detail
        // blocks (~2 KB each, with body previews) only for small result
        // sets — a bare name like `Page_Load` can match hundreds of
        // methods, and rendering 500 detail blocks buries the agent.
        const MAX_DETAILED: usize = 10;
        let mut md = format!("# {} Methods Found\n\n", results.len());
        md.push_str("| # | FQN | File | Lines | Kind | Complexity |\n");
        md.push_str("|---|-----|------|-------|------|------------|\n");
        for (i, r) in results.iter().enumerate() {
            md.push_str(&format!(
                "| {} | `{}` | `{}` | {}–{} | {} | {} |\n",
                i + 1,
                r.fqn,
                r.file_path,
                r.line_start,
                r.line_end,
                r.method_kind,
                r.complexity_score,
            ));
        }
        md.push('\n');

        if results.len() <= MAX_DETAILED {
            for r in &results {
                md.push_str("---\n\n");
                md.push_str(&render_method_info_markdown(r));
            }
        } else {
            md.push_str(&format!(
                "{} matches — detail blocks omitted. Narrow with `file_path` \
                 or a more specific FQN (e.g. `Class.Method`), then re-call.\n",
                results.len()
            ));
        }

        let (_, footer) = self
            .access_freshness(&req.project_id, &rec.directory, None)
            .await;
        md.push_str(&footer);
        Ok(CallToolResult::success(vec![Content::text(md)]))
    }

    // ── 38-2: get_full_method_body ────────────────────────────────────────

    pub async fn handle_get_full_method_body(
        &self,
        req: GetFullMethodBodyRequest,
    ) -> Result<CallToolResult, McpError> {
        if req.fqn.is_some()
            && (req.file_path.is_some() || req.line_start.is_some() || req.line_end.is_some())
        {
            return Err(McpError::invalid_params(
                "fqn and explicit file/line targets are mutually exclusive. Use get_method_edit_context with file_path, method_name and line to disambiguate overloads.",
                None,
            ));
        }
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let output_json = req.output_json;
        let context_lines = req.context_lines;
        let include_callers = req.include_caller_bodies;
        let max_callers = req.max_callers;

        // Resolve the target method: either by FQN or by explicit file+lines
        let fqn = req.fqn.clone();
        let file_path = req.file_path.clone();
        let line_start = req.line_start;
        let line_end = req.line_end;

        let result = tokio::task::spawn_blocking(move || {
            // Determine file_path, line_start, line_end
            let target_node = fqn.as_deref()
                .map(|query| resolve_unique_function(&graph, &project_id, query))
                .transpose()?;
            let (resolved_fqn, resolved_file, resolved_start, resolved_end, language) =
                if let Some(ref node) = target_node {
                    (
                        fqn_from_node(node),
                        node.file_path.as_str().to_string(),
                        node.start_line,
                        node.end_line,
                        node.language.clone(),
                    )
                } else if let (Some(fp), Some(start), Some(end)) =
                    (&file_path, line_start, line_end)
                {
                    let lang = if fp.to_lowercase().ends_with(".vb") {
                        "vbnet".to_string()
                    } else {
                        "csharp".to_string()
                    };
                    ("(direct)".to_string(), fp.clone(), start, end, lang)
                } else {
                    return Err(
                        "Either `fqn` or (`file_path` + `line_start` + `line_end`) must be provided."
                            .to_string(),
                    );
                };

            // Verify and slice one byte snapshot: a second read could observe
            // an intervening edit after the fingerprint check succeeded.
            let full_path = safe_join(Path::new(&project_dir), &resolved_file)
                .map_err(|e| format!("Path validation failed for '{}': {e}", resolved_file))?;
            let source = std::fs::read_to_string(&full_path)
                .map_err(|e| format!("Cannot read '{}': {e}", resolved_file))?;
            let source_file_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
            let source_verification = if target_node.is_some() {
                verify_indexed_source_bytes(&graph, &project_id, &resolved_file, source.as_bytes())?
            } else {
                "explicit_range_current_file_snapshot"
            };
            let (body, context) =
                source_lines(&source, resolved_start, resolved_end, context_lines)
                    .map_err(|e| format!("Cannot read '{}': {}", resolved_file, e))?;

            let (caller_bodies, caller_expansion) = expand_caller_bodies(
                &graph, &project_id, &project_dir, target_node.as_ref(),
                include_callers, max_callers,
            );

            Ok(MethodBodyResult {
                retrieval_scope: if target_node.is_some() { "indexed_method" } else { "explicit_source_range" },
                boundary_guidance: if target_node.is_some() {
                    "Returned the indexed method span. Consult source_verification: legacy entries without an indexed fingerprint have unverified boundaries. The file hash binds the returned lines to the loaded snapshot; later edits are not covered."
                } else {
                    "These are the requested source lines; method boundaries have not been resolved. A search chunk can end mid-method. Use a unique fqn, or get_method_edit_context with file_path, method_name and line, to resolve the method body."
                },
                source_verification,
                source_file_hash,
                fqn: resolved_fqn,
                file_path: resolved_file,
                line_start: resolved_start,
                line_end: resolved_end,
                source_code: body,
                surrounding_context: context,
                language,
                caller_bodies,
                caller_expansion,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let body_result = result.map_err(|e| McpError::invalid_params(e, None))?;

        let (banner, footer) = self
            .access_freshness(
                &req.project_id,
                &rec.directory,
                Some(body_result.file_path.as_str()),
            )
            .await;
        if output_json {
            let json = serde_json::to_string_pretty(&FreshAccessResponse {
                result: &body_result,
                freshness: AccessFreshness {
                    warning: banner,
                    details: footer,
                },
            })
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = banner.unwrap_or_default();
        out.push_str(&render_method_body_markdown(&body_result));
        out.push_str(&footer);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── 38-3: get_method_edit_context ─────────────────────────────────────

    pub async fn handle_get_method_edit_context(
        &self,
        req: GetMethodEditContextRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let file_path = req.file_path.clone();
        let method_name = req.method_name.clone();
        let class_name = req.class_name.clone();
        let line = req.line;
        let include_full_body = req.include_full_body;
        let include_caller_bodies = req.include_caller_bodies;
        let max_callers = req.max_callers;
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            let node = select_method_node(
                &graph,
                &project_id,
                &file_path,
                &method_name,
                class_name.as_deref(),
                line,
            )?;
            let ev = assemble_edit_evidence(&graph, &project_id, &project_dir, node)?;

            // Callers: identities always (from the same exact list the
            // verdict used); full SOURCE only on request — a well-connected
            // method returned tens of thousands of tokens from this section.
            let mut caller_bodies: Vec<CallerBody> = Vec::new();
            let mut caller_excerpts = Vec::new();
            let mut excerpt_budget = 64 * 1024 * 1024;
            for c in ev.method_info.called_by.iter().take(max_callers) {
                caller_excerpts.push(super::caller_excerpts::collect(
                    &graph,
                    &project_id,
                    &project_dir,
                    c,
                    &ev.node.name,
                    &mut excerpt_budget,
                ));
                let source_code = if include_caller_bodies {
                    verify_indexed_source_span(&graph, &project_id, &project_dir, &c.file_path)?;
                    match safe_join(Path::new(&project_dir), &c.file_path)
                        .ok()
                        .and_then(|p| read_lines_from_file(&p, c.line, c.line_end, 0).ok())
                    {
                        Some((body, _)) => body,
                        None => format!("(source unavailable: could not read {})", c.file_path),
                    }
                } else {
                    String::new()
                };
                caller_bodies.push(CallerBody {
                    fqn: c.fqn.clone(),
                    file_path: c.file_path.clone(),
                    line_start: c.line,
                    line_end: c.line_end,
                    source_code,
                    how_it_calls: format!("{} edge → {}", c.edge_kind, ev.node.name),
                });
            }

            let unresolved_caller_leads = super::unresolved_callers::lookup(
                &graph,
                &project_id,
                &project_dir,
                &ev.node,
                &ev.method_info.called_by,
                max_callers,
                &mut excerpt_budget,
            );
            Ok::<MethodEditContextResult, String>(MethodEditContextResult {
                blast_radius_score: ev.blast.as_ref().map(|b| b.migration_risk as f32 * 10.0),
                risk_band: ev
                    .blast
                    .as_ref()
                    .map(|b| format!("{:?}", b.risk_band))
                    .unwrap_or_else(|| "Unknown".to_string()),
                method_info: ev.method_info,
                full_source: if include_full_body {
                    ev.full_body
                } else {
                    None
                },
                caller_bodies,
                caller_excerpts,
                unresolved_caller_leads,
                historical_changes: None,
                vb_traps: ev.vb_traps,
                sync_hazards: ev.sync_hazards,
                edit_safety: ev.edit_safety,
                business_logic: None,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut ctx = result.map_err(|e| McpError::invalid_params(e, None))?;

        if req.include_history {
            let query = req
                .history_query
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    req.file_path
                        .replace('\\', "/")
                        .rsplit('/')
                        .next()
                        .unwrap_or(&req.file_path)
                        .to_string()
                });
            ctx.historical_changes = Some(
                match self
                    .handle_find_merged_work(crate::models::FindMergedWorkRequest {
                        project_id: req.project_id.clone(),
                        story: query,
                        file_paths: vec![req.file_path.clone()],
                        kind: None,
                        top: 2,
                        merged_before: req.merged_before.clone(),
                    })
                    .await
                {
                    Ok(response) => response
                        .content
                        .iter()
                        .filter_map(|c| c.as_text())
                        .map(|c| c.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(error) => format!(
                        "History lookup failed: {error}. No historical applicability or coverage claim is available."
                    ),
                },
            );
            if let Some(history) = &mut ctx.historical_changes {
                const HISTORY_BYTES: usize = 24_000;
                if history.len() > HISTORY_BYTES {
                    history.truncate(history.floor_char_boundary(HISTORY_BYTES));
                    history.push_str("\n[History section truncated at 24000 bytes. Call find_merged_work with the same file_paths, story and merged_before for the complete response.]\n");
                }
            }
        }

        if req.include_business_logic {
            ctx.business_logic = Some(
                self.business_logic_for_method(&req.project_id, &ctx.method_info)
                    .await,
            );
        }

        let (banner, footer) = self
            .access_freshness(&req.project_id, &rec.directory, Some(&req.file_path))
            .await;
        if output_json {
            let json = serde_json::to_string_pretty(&FreshAccessResponse {
                result: &ctx,
                freshness: AccessFreshness {
                    warning: banner,
                    details: footer,
                },
            })
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = banner.unwrap_or_default();
        out.push_str(&render_method_edit_context_markdown(&ctx));
        out.push_str(&footer);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Business-rule evidence for a method from the `business_logic`
    /// namespace. Never silent: an empty namespace says how to populate it
    /// and a failed lookup says it failed.
    async fn business_logic_for_method(
        &self,
        project_id: &str,
        info: &MethodInfoResult,
    ) -> BusinessLogicSection {
        let ps = match self.ensure_project_runtime(project_id).await {
            Ok(ps) => ps,
            Err(e) => {
                return BusinessLogicSection {
                    hits: Vec::new(),
                    note: format!("business-logic lookup FAILED: {e}"),
                };
            }
        };
        // Match persistence's path-stable identity, never a relevance-ranked
        // sibling method or constructor. Return the entire stored analysis.
        let base = format!(
            "__business_logic/{}/{}",
            info.file_path.replace('\\', "/"),
            info.method_name
                .rsplit(['.', ':'])
                .next()
                .unwrap_or(&info.method_name)
        );
        for path in [
            format!("{base}__L{}.md", info.line_start),
            format!("{base}.md"),
        ] {
            let hash = engram_core::ContentHash::compute(path.as_bytes());
            let id = engram_core::DocIdStr::compute(&path, 0, 0, &hash);
            match ps
                .search
                .get_doc_by_doc_id(project_id, "business_logic", 0, &id.0)
            {
                Ok(Some((_, _, content, _, _))) => {
                    let stored = content
                        .lines()
                        .find_map(|line| line.strip_prefix("# "))
                        .unwrap_or("")
                        .trim();
                    let matches_owner = stored == info.fqn
                        || (!stored.is_empty() && info.fqn.ends_with(&format!(".{stored}")));
                    let note = if matches_owner {
                        "Exact stored method identity; full analysis (inferred rules, verify against source).".to_string()
                    } else {
                        format!(
                            "STALE ANALYSIS OWNERSHIP: stored '{stored}', current '{}'. Evidence was generated with a different declaring owner; refresh analyze_business_logic for this method before relying on it. The stored text is preserved for inspection.",
                            info.fqn
                        )
                    };
                    return BusinessLogicSection {
                        hits: vec![BusinessLogicHit {
                            path,
                            score: 1.0,
                            content,
                        }],
                        note,
                    };
                }
                Ok(None) => {}
                Err(e) => {
                    return BusinessLogicSection {
                        hits: Vec::new(),
                        note: format!("business-logic document lookup FAILED: {e}"),
                    };
                }
            }
        }
        BusinessLogicSection {
            hits: Vec::new(),
            note: "no business-logic analysis stored for this exact method; run analyze_business_logic (file mode) to populate or refresh it".into(),
        }
    }

    // ── 38-4: get_page_context ────────────────────────────────────────────

    pub async fn handle_get_page_context(
        &self,
        req: GetPageContextRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let aspx_file = req.aspx_file.clone();
        let include_method_bodies = req.include_method_bodies;
        let include_master = req.include_master_page;
        let include_cb = req.include_codebehind;
        let include_house = req.include_house_style;
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            // 1. Read the ASPX file
            let aspx_full = safe_join(Path::new(&project_dir), &aspx_file)
                .map_err(|e| format!("Path validation for '{}': {e}", aspx_file))?;
            let aspx_content = std::fs::read_to_string(&aspx_full)
                .map_err(|e| format!("Cannot read '{}': {}", aspx_file, e))?;

            // 2. Find code-behind
            let cb_path_vb = format!("{}.vb", aspx_file);
            let cb_path_cs = format!("{}.cs", aspx_file);
            let declared_cb = regex::Regex::new(r#"(?is)<%@\s*(?:Page|Control|Master)\b[^%]*?\bCode(?:File|Behind)\s*=\s*(?:"([^"]+)"|'([^']+)')"#)
                .expect("valid directive regex")
                .captures(&aspx_content)
                .and_then(|cap| cap.get(1).or_else(|| cap.get(2)))
                .map(|m| m.as_str().replace('\\', "/"));
            let (cb_path, cb_content, language) = if let Some(raw) = declared_cb {
                // Resolve relative to the markup, then validate the canonical
                // file under the project root (including parent-relative paths).
                let candidate = if let Some(app_relative) = raw.strip_prefix("~/") {
                    discover_web_application_root(Path::new(&project_dir), &aspx_full)
                        .join(app_relative)
                } else {
                    aspx_full.parent().unwrap_or(Path::new(&project_dir)).join(&raw)
                };
                let root = std::fs::canonicalize(&project_dir).map_err(|e| e.to_string())?;
                match std::fs::canonicalize(&candidate) {
                    Ok(full) => {
                        let rel = full.strip_prefix(&root).map_err(|_| "code-behind escapes project root")?
                            .to_string_lossy().replace('\\', "/");
                        let language = if rel.to_lowercase().ends_with(".vb") { "vbnet" } else { "csharp" };
                        let content = std::fs::read_to_string(full).map_err(|e| format!("Cannot read declared code-behind: {e}"))?;
                        (rel, Some(content), language.to_string())
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None, "unknown".into()),
                    Err(e) => return Err(format!("Cannot resolve declared code-behind: {e}")),
                }
            } else {
                let vb_full = safe_join(Path::new(&project_dir), &cb_path_vb)
                    .map_err(|e| format!("Path validation: {e}"))?;
                let cs_full = safe_join(Path::new(&project_dir), &cb_path_cs)
                    .map_err(|e| format!("Path validation: {e}"))?;
                if let Ok(c) = std::fs::read_to_string(&vb_full) {
                    (cb_path_vb.clone(), Some(c), "vbnet".to_string())
                } else if let Ok(c) = std::fs::read_to_string(&cs_full) {
                    (cb_path_cs.clone(), Some(c), "csharp".to_string())
                } else {
                    (String::new(), None, "unknown".to_string())
                }
            };

            // 3. Extract class name from code-behind
            let class_name = cb_content
                .as_ref()
                .and_then(|c| {
                    // VB: Class ClassName or Partial Class ClassName
                    // C#: class ClassName or partial class ClassName
                    let re = regex::Regex::new(
                        r"(?im)(?:Partial\s+)?(?:Public\s+)?(?:Class|class)\s+(\w+)",
                    )
                    .ok()?;
                    re.captures(c).map(|cap| cap[1].to_string())
                })
                .unwrap_or_else(|| "Unknown".to_string());

            let mut page_cov = PageContextCompleteness::default();

            // 4. Extract master page from @Page directive (opt-out honoured)
            let master_page = if include_master {
                page_cov.master_page = ProviderStatus::Complete;
                let re = regex::Regex::new(r#"(?is)<%@\s*Page\b[^%]*?\bMasterPageFile\s*=\s*(?:"([^"]+)"|'([^']+)')"#).ok();
                re.and_then(|r| r.captures(&aspx_content).and_then(|cap| {
                    cap.get(1).or_else(|| cap.get(2)).map(|value| value.as_str().to_string())
                }))
            } else {
                page_cov.master_page = ProviderStatus::NotRun {
                    reason: "include_master_page=false".into(),
                };
                None
            };

            // 5. Extract ContentPlaceHolder IDs from aspx
            let content_placeholders: Vec<String> = {
                let re = regex::Regex::new(
                    r#"(?i)<asp:Content[^>]+ContentPlaceHolderID\s*=\s*"([^"]+)""#,
                )
                .expect("valid regex");
                re.captures_iter(&aspx_content)
                    .map(|c| c[1].to_string())
                    .collect()
            };

            // 6. Extract controls from ASPX (server controls with runat="server")
            let mut controls = extract_aspx_controls(&aspx_content);

            // 7. Methods from the code-behind via graph (opt-out honoured;
            //    cap+1 fetch so truncation is a fact, not a guess).
            const METHOD_CAP: usize = 500;
            let method_nodes: Vec<Node> = if include_cb && cb_content.is_none() {
                page_cov.codebehind = ProviderStatus::NotRun { reason: "code-behind source not found".into() };
                page_cov.methods = ProviderStatus::NotRun { reason: "no code-behind file to scope method lookup".into() };
                Vec::new()
            } else if include_cb {
                match graph.query_nodes_in_file(
                    &project_id,
                    Some("function"),
                    &cb_path,
                    METHOD_CAP + 1,
                ) {
                    Ok(mut v) => {
                        if v.len() > METHOD_CAP {
                            v.truncate(METHOD_CAP);
                            page_cov.methods = ProviderStatus::Truncated {
                                shown: METHOD_CAP,
                                cap: METHOD_CAP,
                                known_total: None,
                            };
                        } else {
                            page_cov.methods = ProviderStatus::Complete;
                        }
                        page_cov.codebehind = ProviderStatus::Complete;
                        v
                    }
                    Err(e) => {
                        page_cov.methods = ProviderStatus::Failed {
                            reason: e.to_string(),
                        };
                        page_cov.codebehind = ProviderStatus::Failed {
                            reason: e.to_string(),
                        };
                        Vec::new()
                    }
                }
            } else {
                page_cov.methods = ProviderStatus::NotRun {
                    reason: "include_codebehind=false".into(),
                };
                page_cov.codebehind = ProviderStatus::NotRun {
                    reason: "include_codebehind=false".into(),
                };
                Vec::new()
            };

            // Runtime evidence per METHOD NODE (O(degree) adjacency), never a
            // project-wide first-5000-edges scan filtered by name suffix.
            page_cov.runtime = ProviderStatus::Complete;
            let mut runtime_method_sources: HashSet<String> = HashSet::new();
            let mut runtime_observed_edges = 0usize;
            let mut runtime_sql_set: HashSet<String> = HashSet::new();
            for node in &method_nodes {
                for kind in [
                    EdgeKind::ObservedRuntimeControl,
                    EdgeKind::ObservedRuntimeSql,
                ] {
                    let is_sql = matches!(kind, EdgeKind::ObservedRuntimeSql);
                    match graph.neighbors(&project_id, kind, &node.node_id, DATA_EDGE_CAP + 1) {
                        Ok(v) => {
                            if !v.is_empty() {
                                runtime_method_sources.insert(node.node_id.clone());
                            }
                            if v.len() > DATA_EDGE_CAP {
                                page_cov.runtime = worse_status(
                                    page_cov.runtime.clone(),
                                    ProviderStatus::Truncated {
                                        shown: DATA_EDGE_CAP,
                                        cap: DATA_EDGE_CAP,
                                        known_total: None,
                                    },
                                );
                            }
                            runtime_observed_edges += v.len().min(DATA_EDGE_CAP);
                            if is_sql {
                                for (t, _) in v.into_iter().take(DATA_EDGE_CAP) {
                                    runtime_sql_set.insert(t);
                                }
                            }
                        }
                        Err(e) => {
                            page_cov.runtime = ProviderStatus::Failed {
                                reason: e.to_string(),
                            };
                        }
                    }
                }
            }

            // Control nodes of this page (runtime lookups + dynamic controls).
            let control_nodes: Vec<Node> =
                match graph.query_nodes(&project_id, Some("control"), None, Some(&cb_path), 1000) {
                    Ok(v) => {
                        page_cov.controls = ProviderStatus::Complete;
                        v
                    }
                    Err(e) => {
                        page_cov.controls = ProviderStatus::Failed {
                            reason: e.to_string(),
                        };
                        Vec::new()
                    }
                };

            let mut methods: Vec<PageMethodSummary> = Vec::new();
            for node in &method_nodes {
                let effects = meta_csv(node, "effects");
                let kind = full_mig::classify_method_kind_pub(&node.name, &effects, &node.metadata);

                let full_body = if include_method_bodies {
                    safe_join(Path::new(&project_dir), node.file_path.as_str())
                        .ok()
                        .and_then(|full_path| {
                            read_lines_from_file(&full_path, node.start_line, node.end_line, 0)
                                .ok()
                                .map(|(body, _)| body)
                        })
                } else {
                    None
                };

                let handles = meta_csv(node, "handles_clause");
                let signature = meta_str(node, "signature");

                methods.push(PageMethodSummary {
                    name: node.name.clone(),
                    signature: if signature.is_empty() {
                        node.name.clone()
                    } else {
                        signature
                    },
                    kind: kind.to_string(),
                    line_start: node.start_line,
                    line_end: node.end_line,
                    handles_clause: handles,
                    effects,
                    full_body,
                    observed_at_runtime: runtime_method_sources.contains(&node.node_id),
                });
            }

            // Sort methods by kind priority (Lifecycle first, then Events, etc.)
            methods.sort_by_key(|m| match m.kind.as_str() {
                "Lifecycle" => 0,
                "ControlEvent" => 1,
                "WebMethod" => 2,
                "DataAccess" => 3,
                _ => 4,
            });

            for c in &mut controls {
                let mut ids = vec![format!("control:{}:{}", aspx_file, c.server_id)];
                ids.extend(
                    control_nodes
                        .iter()
                        .filter(|n| n.name.eq_ignore_ascii_case(&c.server_id))
                        .map(|n| n.node_id.clone()),
                );
                c.observed_at_runtime = ids.iter().any(|id| {
                    matches!(
                        graph.find_incoming_edges_with_kind(
                            &project_id,
                            Some(EdgeKind::ObservedRuntimeControl),
                            id,
                            1,
                        ),
                        Ok(v) if !v.is_empty()
                    )
                });
            }

            // Runtime UI caveat detection for dynamic controls / wiring.
            // Event wiring may be recorded on either caller edge kind
            // (Dependency from the heuristic extractors, Calls from the
            // Roslyn path) — scan both.
            let mut dynamic_ui_evidence: Vec<String> = Vec::new();
            let mut add_handler_count = 0usize;
            let mut lifecycle_dynamic_methods: Vec<String> = Vec::new();
            let mut synthetic_dynamic_controls: Vec<String> = Vec::new();

            let mut method_names: HashSet<String> = HashSet::new();
            for node in &method_nodes {
                method_names.insert(node.name.to_ascii_lowercase());
            }

            // Event wiring per METHOD NODE (both directions, O(degree)).
            page_cov.wiring = ProviderStatus::Complete;
            let mut seen_wiring: HashSet<(String, String)> = HashSet::new();
            for node in &method_nodes {
                match graph.edges_touching_with_coverage(&project_id, &node.node_id, 500) {
                    Ok((edges, truncated)) => {
                        if truncated {
                            page_cov.wiring = worse_status(
                                page_cov.wiring.clone(),
                                ProviderStatus::Truncated {
                                    shown: 500,
                                    cap: 500,
                                    known_total: None,
                                },
                            );
                        }
                        for edge in edges {
                            if matches!(edge.edge_kind, EdgeKind::Dependency | EdgeKind::Calls)
                                && edge_meta_str(&edge, "kind").eq_ignore_ascii_case("event_wiring")
                                && edge_meta_str(&edge, "wiring").eq_ignore_ascii_case("AddHandler")
                                && seen_wiring
                                    .insert((edge.source_id.clone(), edge.target_id.clone()))
                            {
                                add_handler_count += 1;
                            }
                        }
                    }
                    Err(e) => {
                        page_cov.wiring = ProviderStatus::Failed {
                            reason: e.to_string(),
                        };
                    }
                }
            }

            for name in ["page_init", "oninit", "createchildcontrols"] {
                if method_names.contains(name) {
                    lifecycle_dynamic_methods.push(name.to_string());
                }
            }

            for control in &control_nodes {
                if meta_bool(control, "dynamic_control") {
                    synthetic_dynamic_controls.push(control.name.clone());
                }
            }

            if add_handler_count > 0 {
                dynamic_ui_evidence.push(format!(
                    "Detected {} AddHandler event_wiring edge(s) in related graph edges.",
                    add_handler_count
                ));
            }
            if !lifecycle_dynamic_methods.is_empty() {
                dynamic_ui_evidence.push(format!(
                    "Lifecycle method(s) commonly used for runtime UI creation found: {}.",
                    lifecycle_dynamic_methods.join(", ")
                ));
            }
            if !synthetic_dynamic_controls.is_empty() {
                dynamic_ui_evidence.push(format!(
                    "Synthetic dynamic controls indexed: {}.",
                    synthetic_dynamic_controls.join(", ")
                ));
            }

            let dynamic_ui_detected = !dynamic_ui_evidence.is_empty();
            let ui_coverage_confidence = if dynamic_ui_detected {
                (0.90_f32 - (dynamic_ui_evidence.len() as f32 * 0.12)).clamp(0.45, 0.85)
            } else {
                0.95
            };
            let runtime_controls_warning = dynamic_ui_detected.then_some(
                "Runtime controls likely present; static ASPX tree incomplete.".to_string(),
            );

            // 8. AJAX analysis (failure is reported in coverage, not hidden)
            let ajax_map = match crate::services::ajax_region_service::analyze_ajax_regions(
                &graph,
                &project_id,
                &aspx_file,
                &aspx_content,
            ) {
                Ok(m) => {
                    page_cov.ajax = ProviderStatus::Complete;
                    Some(m)
                }
                Err(e) => {
                    page_cov.ajax = ProviderStatus::Failed {
                        reason: e.to_string(),
                    };
                    None
                }
            };

            // 9. Tables, SPs and state keys of THIS page's methods: exact
            //    per-node adjacency seeks (the previous project-wide
            //    first-5000-edges scans were silently truncated on large
            //    graphs and suffix-matched other pages' same-named methods).
            page_cov.data_edges = ProviderStatus::Complete;
            let mut tables_set: HashSet<String> = HashSet::new();
            let mut sps_set: HashSet<String> = HashSet::new();
            let mut session_set: HashSet<String> = HashSet::new();
            for node in &method_nodes {
                let (t, st) =
                    outgoing_targets(&graph, &project_id, EdgeKind::QueriesTable, &node.node_id);
                tables_set.extend(t);
                page_cov.data_edges = worse_status(page_cov.data_edges.clone(), st);
                let (t, st) =
                    outgoing_targets(&graph, &project_id, EdgeKind::SqlCalls, &node.node_id);
                sps_set.extend(t);
                page_cov.data_edges = worse_status(page_cov.data_edges.clone(), st);
                for kind in [EdgeKind::ReadsState, EdgeKind::WritesState] {
                    let (t, st) = outgoing_targets(&graph, &project_id, kind, &node.node_id);
                    session_set.extend(t);
                    page_cov.data_edges = worse_status(page_cov.data_edges.clone(), st);
                }
            }

            let mut tables_used: Vec<String> = tables_set.into_iter().collect();
            let mut sps_called: Vec<String> = sps_set.into_iter().collect();
            let mut session_keys: Vec<String> = session_set.into_iter().collect();
            let mut runtime_sql_observations: Vec<String> = runtime_sql_set.into_iter().collect();
            tables_used.sort();
            sps_called.sort();
            session_keys.sort();
            runtime_sql_observations.sort();

            // 10. VB traps for the entire code-behind (code-behind analysis
            //     is opt-out via include_codebehind)
            let vb_traps = if let (true, Some(content)) = (include_cb, cb_content.as_ref()) {
                if language == "vbnet" {
                    let files = vec![(cb_path.as_str(), content.as_str())];
                    let report =
                        engram_index::vb_translation_traps::detect_vb_translation_traps(&files);
                    report.traps
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            // 11. Auth from @Page directive
            let requires_auth = aspx_content.contains("Authorize")
                || aspx_content.contains("<%@ Page")
                    && aspx_content.contains("RequiresAuthentication");

            let total_methods = methods.len();
            let vb_trap_count = vb_traps.len();
            let vb_traps_summary: Vec<String> = vb_traps
                .iter()
                .map(|t| format!("{}: {} ({})", t.location, t.trap, t.risk))
                .collect();

            // Row 5 v3: what next door looks like (nearest siblings + shared idioms).
            let house_style = if include_house {
                Some(crate::services::house_style::house_style_for(
                    Path::new(&project_dir),
                    &aspx_file,
                    &aspx_content,
                ))
            } else {
                None
            };
            let app_root = discover_web_application_root(Path::new(&project_dir), &aspx_full);
            let composition = super::page_composition::collect(Path::new(&project_dir), &app_root, &aspx_full, include_master, include_cb);
            Ok(PageContextResult {
                composition,
                aspx_file: aspx_file.clone(),
                house_style,
                codebehind_file: cb_path,
                class_name,
                master_page,
                content_placeholders,
                language,
                ui_coverage_confidence,
                dynamic_ui_detected,
                dynamic_ui_evidence,
                runtime_controls_warning,
                runtime_observed_edges,
                controls,
                methods,
                tables_used,
                stored_procs_called: sps_called,
                session_keys,
                runtime_sql_observations,
                update_panels: ajax_map
                    .as_ref()
                    .map(|a| {
                        a.update_panels
                            .iter()
                            .map(|p| UpdatePanelSummary {
                                panel_id: p.panel_id.clone(),
                                update_mode: p.update_mode.clone(),
                                controls_inside: p
                                    .controls_inside
                                    .iter()
                                    .map(|(id, ty)| format!("{}:{}", id, ty))
                                    .collect(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                has_script_manager: ajax_map
                    .as_ref()
                    .map(|a| a.has_script_manager)
                    .unwrap_or(false),
                vb_trap_count,
                vb_traps_summary,
                requires_authentication: requires_auth,
                total_methods,
                completeness: page_cov,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let ctx = result.map_err(|e: String| McpError::invalid_params(e, None))?;

        let (banner, footer) = self
            .access_freshness(&req.project_id, &rec.directory, Some(&req.aspx_file))
            .await;
        if output_json {
            let json = serde_json::to_string_pretty(&FreshAccessResponse {
                result: &ctx,
                freshness: AccessFreshness {
                    warning: banner,
                    details: footer,
                },
            })
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // M-6 fix: cap the rendered Markdown to prevent multi-megabyte
        // responses on projects with many edges (10 kinds × up to 5 000 edges
        // each = up to 50 000 rows, which can exceed several MB and cause MCP
        // transport timeouts or OOM on the client side).
        const MAX_PAGE_CONTEXT_BYTES: usize = 2_000_000; // 2 MB soft cap
        let mut md = render_page_context_markdown(&ctx);
        if md.len() > MAX_PAGE_CONTEXT_BYTES {
            md.truncate(md.floor_char_boundary(MAX_PAGE_CONTEXT_BYTES));
            // Snap back to the last newline so we don't cut mid-table-row.
            if let Some(nl) = md.rfind('\n') {
                md.truncate(nl + 1);
            }
            md.push_str(
                "\n\n> ⚠️ **Response truncated** — too many edges to display in full. \
                 Use `output_json: true` or narrow the query to a specific method.\n",
            );
        }

        let mut out = banner.unwrap_or_default();
        out.push_str(&md);
        out.push_str(&footer);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── 38-5: prepare_implementation_context ─────────────────────────────

    pub async fn handle_prepare_implementation_context(
        &self,
        req: PrepareImplementationContextRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let file_path = req.file_path.clone();
        let method_name = req.method_name.clone();
        let class_name = req.class_name.clone();
        let line = req.line;
        let target_stack = req.target_stack.clone();
        let include_pattern_examples = req.include_pattern_examples;
        let requested_pattern_examples = req.max_pattern_examples;
        let max_pattern_examples = req.max_pattern_examples.min(20);
        let include_db_schema = req.include_db_schema;
        let include_sp_signatures = req.include_sp_signatures;
        let include_state_context = req.include_state_context;
        let include_control_mappings = req.include_control_mappings;
        let output_json = req.output_json;

        // Alias keeps the closure's return-type annotation short enough to stay
        // on one line (the annotation is what pins the closure's error type to
        // String now that resolution propagates via `?` instead of a concrete
        // `return Err`).
        type PrepCtxResult = Result<ImplementationContext, String>;
        let result = tokio::task::spawn_blocking(move || -> PrepCtxResult {
            let mut warnings = Vec::new();
            if requested_pattern_examples > 20 { warnings.push("Caller pattern limit capped at 20".into()); }
            verify_indexed_source_span(&graph, &project_id, &project_dir, &file_path)?;
            // 1. Resolve the target method
            // Round-6: reuse the shared resolver instead of a hand-rolled
            // candidate scan. select_method_node prefers an EXACT name over
            // query_nodes' substring match (so "orders" does not falsely
            // collide with "orders_history"), refuses genuine cross-class /
            // overload ambiguity, and SURFACES a lookup failure instead of
            // silently returning candidates[0] on an empty/errored result.
            let resolved_node = select_method_node(
                &graph,
                &project_id,
                &file_path,
                &method_name,
                class_name.as_deref(),
                line,
            )?;
            let node = &resolved_node;
            let (method_info, method_coverage) = build_method_info_with_coverage(node, &graph, &project_id);
            for (name, status) in [
                ("callers", &method_coverage.callers),
                ("db_tables", &method_coverage.db_tables),
                ("stored_procs", &method_coverage.stored_procs),
                ("session_reads", &method_coverage.session_reads),
                ("session_writes", &method_coverage.session_writes),
            ] {
                if !matches!(status, ProviderStatus::Complete) {
                    warnings.push(format!("Method {name} evidence: {}; expanded context may omit dependencies", provider_text(status)));
                }
            }
            if method_coverage.callers_dangling > 0 {
                warnings.push(format!("{} indexed caller source nodes are unavailable", method_coverage.callers_dangling));
            }

            // 2. Read the method body from disk
            let full_path = safe_join(Path::new(&project_dir), &file_path)
                .map_err(|e| format!("Path validation: {e}"))?;
            // Round-5 P0: do NOT swallow a body-read failure with .ok() — a
            // caller must be able to tell "read failed" from "no body".
            let (method_body, body_read_error) =
                match read_lines_from_file(&full_path, node.start_line, node.end_line, 0) {
                    Ok((body, _)) => (Some(body), None),
                    Err(e) => (
                        None,
                        Some(format!("could not read method body from disk: {e}")),
                    ),
                };

            // 3. Pattern examples from callers
            let mut pattern_examples: Vec<PatternExample> = Vec::new();
            if include_pattern_examples && max_pattern_examples > 0 {
                let callers = match crate::handlers::incoming_caller_edges_checked(
                    &graph,
                    &project_id,
                    &node.node_id,
                    max_pattern_examples,
                ) {
                    Ok((callers, truncated)) => {
                        if truncated { warnings.push(format!("Caller patterns truncated at {max_pattern_examples}; use find_symbol_references for the wider set")); }
                        callers
                    }
                    Err(error) => {
                        warnings.push(format!("Caller pattern query failed: {error}"));
                        Vec::new()
                    }
                };
                for (source_id, kind, _weight) in callers.iter().take(max_pattern_examples) {
                    match graph.get_node(&project_id, source_id) {
                    Ok(Some(src_node)) => {
                        let Ok(src_full) =
                            safe_join(Path::new(&project_dir), src_node.file_path.as_str())
                        else {
                            warnings.push(format!("Caller pattern {source_id} withheld: invalid source path"));
                            continue;
                        };
                        if let Err(error) = verify_indexed_source_span(
                            &graph,
                            &project_id,
                            &project_dir,
                            src_node.file_path.as_str(),
                        ) {
                            warnings.push(format!("Caller pattern withheld: {error}"));
                            continue;
                        }
                        match read_lines_from_file(
                            &src_full,
                            src_node.start_line,
                            src_node.end_line,
                            0,
                        ) {
                          Ok((src_body, _)) => {
                            pattern_examples.push(PatternExample {
                                caller_fqn: fqn_from_node(&src_node),
                                caller_file: src_node.file_path.as_str().to_string(),
                                line_start: src_node.start_line,
                                line_end: src_node.end_line,
                                source_code: src_body,
                                call_pattern: format!(
                                    "Invokes {} via {} edge",
                                    method_name,
                                    kind.as_str()
                                ),
                            });
                          }
                          Err(error) => warnings.push(format!("Caller pattern {source_id} source unavailable: {error}")),
                        }
                    },
                    Ok(None) => warnings.push(format!("Caller pattern {source_id}: indexed source node unavailable")),
                    Err(error) => warnings.push(format!("Caller pattern {source_id}: source lookup failed: {error}")),
                    }
                }
            }

            // 4. Resolve exact table identities and only their own columns.
            let mut schema_snippets = Vec::new();
            if include_db_schema {
                for table_name in &method_info.db_tables_accessed {
                    let table_id = if table_name.starts_with("table:") { table_name.clone() } else { engram_core::ids::NodeId::table(table_name).0 };
                    let mut columns = Vec::new();
                    match graph.get_node(&project_id, &table_id) {
                        Ok(Some(table)) if table.node_type == "db_table" => {
                            match graph.neighbors(&project_id, EdgeKind::HasColumn, &table_id, 201) {
                                Ok(edges) => {
                                    if edges.len() > 200 { warnings.push(format!("Schema {table_name}: columns truncated at 200")); }
                                    for (column_id, _) in edges.into_iter().take(200) {
                                        match graph.get_node(&project_id, &column_id) {
                                            Ok(Some(column)) => {
                                                let nullable = column.metadata.as_ref().and_then(|meta| meta.get("nullable")).and_then(|value| value.as_bool().or_else(|| value.as_str().and_then(|text| text.parse::<bool>().ok())));
                                                let data_type = meta_str(&column, "data_type");
                                                if nullable.is_none() || data_type.is_empty() { warnings.push(format!("Schema {table_name}.{}: incomplete column contract", column.name)); }
                                                columns.push(ColumnSnippet { name: column.name, data_type, nullable });
                                            },
                                            Ok(None) => warnings.push(format!("Schema {table_name}: column node {column_id} unavailable")),
                                            Err(error) => warnings.push(format!("Schema {table_name}: column lookup failed: {error}")),
                                        }
                                    }
                                },
                                Err(error) => warnings.push(format!("Schema {table_name}: column edge lookup failed: {error}")),
                            }
                        },
                        Ok(_) => warnings.push(format!("Schema {table_name}: exact table identity unavailable; no similarly named table substituted")),
                        Err(error) => warnings.push(format!("Schema {table_name}: table lookup failed: {error}")),
                    }
                    schema_snippets.push(TableSchemaSnippet { table_name: table_name.clone(), columns });
                }
            }

            // 5. SP signatures for referenced stored procedures
            let mut sp_signatures: Vec<SpSignatureSnippet> = Vec::new();
            if include_sp_signatures && !method_info.stored_procs_called.is_empty() {
                // Look up graph nodes for SP metadata. The full_project_migration_service
                // stores SP info as graph metadata during indexing.
                for sp_name in &method_info.stored_procs_called {
                    // A SQL call/reference is not a procedure declaration. Only
                    // exact indexed SQL function identities may supply metadata.
                    let sp_nodes = match graph.get_node(&project_id, sp_name) {
                        Ok(Some(node)) if node.node_type == "function" && node.language.eq_ignore_ascii_case("sql") => vec![node],
                        Ok(_) => Vec::new(),
                        Err(error) => { warnings.push(format!("Procedure {sp_name}: identity lookup failed: {error}")); Vec::new() },
                    };
                    let coverage = if sp_nodes.is_empty() {
                        warnings.push(format!("Procedure {sp_name}: exact declaration/signature unavailable; inspect get_sp_details before generating a call"));
                        "unavailable; reference is not a verified procedure signature"
                    } else { "indexed metadata only; parameter contracts not compiled" };

                    let mut params = Vec::new();
                    let mut tables_read = Vec::new();
                    let mut tables_written = Vec::new();

                    for sp_node in &sp_nodes {
                        // Extract parameters from metadata
                        let param_str = meta_str(sp_node, "parameters");
                        if !param_str.is_empty() {
                            params = param_str
                                .split(',')
                                .map(|p| p.trim().to_string())
                                .filter(|p| !p.is_empty())
                                .collect();
                        }

                        // Tables read/written from effects metadata
                        let eff = meta_csv(sp_node, "effects");
                        for e in &eff {
                            if e.starts_with("reads:") {
                                tables_read.push(e.trim_start_matches("reads:").trim().to_string());
                            } else if e.starts_with("writes:") {
                                tables_written
                                    .push(e.trim_start_matches("writes:").trim().to_string());
                            }
                        }
                    }

                    sp_signatures.push(SpSignatureSnippet {
                        sp_name: sp_name.clone(),
                        coverage: coverage.into(),
                        parameters: params,
                        tables_read,
                        tables_written,
                    });
                }
            }

            // 6. Session state context
            let mut state_context: Vec<StateContextSnippet> = Vec::new();
            if include_state_context
                && (!method_info.session_keys_read.is_empty()
                    || !method_info.session_keys_written.is_empty())
            {
                let all_keys: HashSet<&str> = method_info
                    .session_keys_read
                    .iter()
                    .chain(method_info.session_keys_written.iter())
                    .map(|s| s.as_str())
                    .collect();

                for key in all_keys {
                    let is_read = method_info.session_keys_read.iter().any(|k| k == key);
                    let is_written = method_info.session_keys_written.iter().any(|k| k == key);

                    // Find other methods that use this same session key
                    let mut other_readers = Vec::new();
                    let mut other_writers = Vec::new();

                    for (kind, output) in [(EdgeKind::ReadsState, &mut other_readers), (EdgeKind::WritesState, &mut other_writers)] {
                        match graph.find_incoming_edges(&project_id, Some(kind.clone()), key, 201) {
                            Ok(edges) => {
                                if edges.len() > 200 { warnings.push(format!("State {key}: {} sites truncated at 200", kind.as_str())); }
                                output.extend(edges.into_iter().take(200).filter(|(source, _)| source != &node.node_id).map(|(source, _)| source));
                            },
                            Err(error) => warnings.push(format!("State {key}: {} lookup failed: {error}", kind.as_str())),
                        }
                    }

                    state_context.push(StateContextSnippet {
                        key: key.to_string(),
                        this_method_reads: is_read,
                        this_method_writes: is_written,
                        other_readers,
                        other_writers,
                    });
                }
            }

            // 7. Control mappings for referenced controls (requires the aspx file)
            let mut control_mappings: Vec<ControlMappingSnippet> = Vec::new();
            if include_control_mappings {
                // Determine the associated ASPX file (strip .vb/.cs extension)
                let aspx_base = file_path
                    .strip_suffix(".vb")
                    .or_else(|| file_path.strip_suffix(".cs"))
                    .unwrap_or(&file_path);

                if let Ok(aspx_full) = safe_join(Path::new(&project_dir), aspx_base)
                    && let Ok(aspx_content) = std::fs::read_to_string(&aspx_full)
                {
                    let controls = extract_aspx_controls(&aspx_content);

                    for ctrl in &controls {
                        // Check if this control is referenced by the target method
                        // (via Handles clause, effects, or body reference)
                        let is_relevant = method_info
                            .handles_clause
                            .iter()
                            .any(|h| h.contains(&ctrl.server_id))
                            || method_body
                                .as_ref()
                                .map(|b| b.contains(&ctrl.server_id))
                                .unwrap_or(false);

                        if is_relevant {
                            // Look up the control mapping
                            let mapping = engram_index::control_mapping::lookup(&ctrl.control_type);

                            let target_str = target_stack.as_deref().unwrap_or("blazor");
                            let modern_equivalent = mapping
                                .map(|m| match target_str {
                                    "blazor" => m.blazor_equivalent.to_string(),
                                    "react" => m.react_equivalent.to_string(),
                                    "angular" => m.angular_equivalent.to_string(),
                                    _ => m.blazor_equivalent.to_string(),
                                })
                                .unwrap_or_else(|| format!("<!-- {} -->", ctrl.control_type));

                            let migration_notes: Vec<String> = mapping
                                .map(|m| {
                                    let mut notes = Vec::new();
                                    if !m.notes.is_empty() {
                                        notes.push(m.notes.to_string());
                                    }
                                    for diff in m.breaking_differences {
                                        notes.push(format!("BREAKING: {}", diff));
                                    }
                                    if m.requires_databind_on_postback {
                                        notes.push(
                                            "Requires explicit databinding on postback".to_string(),
                                        );
                                    }
                                    notes
                                })
                                .unwrap_or_default();

                            let event_mappings: Vec<(String, String)> = mapping
                                .map(|m| {
                                    m.event_map
                                        .iter()
                                        .map(|(from, to)| (from.to_string(), to.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();

                            control_mappings.push(ControlMappingSnippet {
                                control_id: ctrl.server_id.clone(),
                                legacy_type: ctrl.control_type.clone(),
                                modern_equivalent,
                                event_mappings,
                                migration_notes,
                            });
                        }
                    }
                }
            }

            // 8. VB translation traps relevant to this method
            let vb_traps = if file_path.to_lowercase().ends_with(".vb") {
                let full_path = safe_join(Path::new(&project_dir), &file_path)
                    .map_err(|e| format!("Path validation: {e}"))?;
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    let files = vec![(file_path.as_str(), content.as_str())];
                    let report =
                        engram_index::vb_translation_traps::detect_vb_translation_traps(&files);
                    report
                        .traps
                        .into_iter()
                        .filter(|t| {
                            t.location
                                .rsplit(':')
                                .next()
                                .and_then(|s| s.parse::<u32>().ok())
                                .map(|line| line >= node.start_line && line <= node.end_line)
                                .unwrap_or(false)
                        })
                        .map(|t| VbTrapSummary {
                            location: t.location,
                            trap: t.trap,
                            risk: t.risk,
                            guidance: t.guidance,
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            // 9. Language-family diagnostics for non-VB methods
            let language_diagnostics = {
                let ext = Path::new(&file_path)
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_default();
                let family = match ext.as_str() {
                    "cs" => Some(engram_index::language_diagnostics::LanguageFamily::CSharp),
                    "c" | "h" => Some(engram_index::language_diagnostics::LanguageFamily::C),
                    "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => {
                        Some(engram_index::language_diagnostics::LanguageFamily::Cpp)
                    }
                    "rs" => Some(engram_index::language_diagnostics::LanguageFamily::Rust),
                    // VB.NET is the pilot corpus's primary language (.vb, .aspx.vb,
                    // .ascx.vb all have extension "vb"); it must get pre-edit
                    // risk diagnostics like every other first-class language.
                    "vb" => Some(engram_index::language_diagnostics::LanguageFamily::Vb),
                    "ml" | "mlinc" => {
                        Some(engram_index::language_diagnostics::LanguageFamily::MiniLang)
                    }
                    _ => None,
                };

                if let Some(family) = family {
                    let full_path = safe_join(Path::new(&project_dir), &file_path)
                        .map_err(|e| format!("Path validation: {e}"))?;
                    if let Ok(content) = std::fs::read_to_string(&full_path) {
                        let files = vec![(file_path.as_str(), content.as_str())];
                        let report =
                            engram_index::language_diagnostics::detect_language_diagnostics(
                                family, &files,
                            );
                        // Method-range filtering alone silently drops any
                        // DECLARATION-level finding: MiniLang's MLC6013
                        // strong-`Ref` cycle is reported on the `Type` line,
                        // which lies outside every function range, so the
                        // diagnostic was computed on every call and could
                        // never be seen by an agent.
                        //
                        // Keep a finding when it belongs to THIS method, or
                        // when no method in the file can claim it. A leaking
                        // type declared in the file being edited is exactly
                        // the context the caller needs; a finding inside a
                        // DIFFERENT method still stays out.
                        let claimable: Vec<(u32, u32)> = graph
                            .query_nodes(&project_id, None, None, Some(&file_path), 5000)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|n| {
                                n.file_path.as_str() == file_path
                                    && matches!(n.node_type.as_str(), "function" | "method")
                                    && n.start_line > 0
                                    && n.end_line >= n.start_line
                            })
                            .map(|n| (n.start_line, n.end_line))
                            .collect();

                        report
                            .diagnostics
                            .into_iter()
                            .filter(|d| {
                                d.location
                                    .rsplit(':')
                                    .next()
                                    .and_then(|s| s.parse::<u32>().ok())
                                    .is_some_and(|line| {
                                        diagnostic_belongs_to_context(
                                            line,
                                            node.start_line,
                                            node.end_line,
                                            &claimable,
                                        )
                                    })
                            })
                            .map(|d| LanguageDiagnosticSummary {
                                location: d.location,
                                category: d.category,
                                severity: d.severity,
                                evidence: d.evidence,
                                guidance: d.guidance,
                            })
                            .collect::<Vec<_>>()
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                }
            };

            // 10. Sync hazards in this method
            let sync_hazards = {
                let full_path = safe_join(Path::new(&project_dir), &file_path)
                    .map_err(|e| format!("Path validation: {e}"))?;
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    let is_vb = file_path.to_lowercase().ends_with(".vb");
                    let report =
                        engram_index::sync_hazard_detector::detect_sync_hazards(&content, is_vb);
                    report
                        .hazards
                        .into_iter()
                        .filter(|h| {
                            h.line_number >= node.start_line as usize
                                && h.line_number <= node.end_line as usize
                        })
                        .map(|h| SyncHazardSummary {
                            line: h.line_number as u32,
                            pattern: h.pattern_type,
                            severity: format!("{:?}", h.severity),
                            modern_equivalent: h.modern_equivalent,
                        })
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                }
            };

            if let Some(e) = body_read_error {
                warnings.push(e);
            }
            Ok(ImplementationContext {
                method_info,
                method_coverage,
                coverage_interpretation: coverage_interpretation(),
                method_body,
                style_profile: None, // filled in later from async result
                style_basis: None,
                pattern_examples,
                schema_snippets,
                sp_signatures,
                state_context,
                control_mappings,
                vb_traps,
                language_diagnostics,
                sync_hazards,
                warnings,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut ctx = result.map_err(|e| McpError::invalid_params(e, None))?;
        if req.include_style_profile {
            let style = crate::services::cognitive_service::analyze_file_style_deterministic(
                &self.state,
                &req.project_id,
                &req.file_path,
                50,
            )
            .await;
            ctx.warnings.extend(
                style
                    .basis
                    .failures
                    .iter()
                    .map(|failure| format!("Style provider: {failure}")),
            );
            if let Some(error) = style.error {
                ctx.warnings
                    .push(format!("Style profile unavailable: {error}"));
            }
            ctx.style_profile = style.style_guide;
            ctx.style_basis = Some(style.basis);
        }

        if output_json {
            let json = serde_json::to_string_pretty(&ctx)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let (banner, footer) = self
            .access_freshness(&req.project_id, &rec.directory, Some(&req.file_path))
            .await;
        let mut out = banner.unwrap_or_default();
        out.push_str(&render_implementation_context_markdown(&ctx));
        out.push_str(&footer);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── 38-6: validate_generated_code ────────────────────────────────────

    pub async fn handle_validate_generated_code(
        &self,
        mut req: ValidateGeneratedCodeRequest,
    ) -> Result<CallToolResult, McpError> {
        let input = crate::utils::candidate_code_input::resolve(self, &req.project_id,
            req.code.as_deref(), req.code_file.as_deref(), req.code_file_blake3.as_deref(), req.target_file.as_deref()).await?;
        let output_json = req.output_json;
        if input.evidence.is_some() { req.target_file = input.context.clone(); }
        let result = self.handle_validate_generated_code_resolved(req, input.code).await;
        crate::utils::candidate_code_input::attach(result, input.evidence, output_json)
    }

    async fn handle_validate_generated_code_resolved(
        &self,
        req: ValidateGeneratedCodeRequest,
        code: String,
    ) -> Result<CallToolResult, McpError> {
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let language = req.language.clone();
        let target_file = req.target_file.clone();
        let original_method = req.original_method_name.clone();
        let expected_tables = req.expected_tables.clone();
        let expected_sps = req.expected_sps.clone();
        let expected_session_keys = req.expected_session_keys.clone();
        let expected_control_ids = req.expected_control_ids.clone();
        let change_kind = req.change_kind;
        let output_json = req.output_json;
        let include_migration_advice = req.include_migration_advice;

        let result = tokio::task::spawn_blocking(move || {
            let mut checks: Vec<ValidationCheck> = Vec::new();
            let is_vb = language.starts_with("vb");

            // Round-6/8: resolve the target against the index EXACTLY. Change kind
            // is now a typed enum (a typo is rejected at deserialization), so the
            // modify/create semantics can no longer be bypassed by an unknown
            // value.
            let is_create = change_kind == crate::models::ChangeKind::Create;
            let target_status = match &target_file {
                None => TargetStatus::Unspecified,
                Some(tf) => {
                    let norm = tf.replace('\\', "/");
                    let want = norm.to_lowercase();
                    // Identity, not substring: a file node is keyed `file:{rel-path}`
                    // and its `name` is the BASENAME, so `query_nodes(name=<full
                    // path>)` can never match a real file (round-7 P0-1). Resolve
                    // the exact file id first; fall back to a basename query +
                    // exact case-insensitive path match to tolerate case/spelling
                    // drift in the caller's path.
                    match graph.get_node(&project_id, &format!("file:{norm}")) {
                        Ok(Some(n)) if n.node_type == "file" => TargetStatus::Exists,
                        Ok(_) => {
                            let basename = norm.rsplit('/').next().unwrap_or(norm.as_str());
                            match graph.query_nodes(
                                &project_id,
                                Some("file"),
                                Some(basename),
                                None,
                                200,
                            ) {
                                Ok(nodes) => {
                                    let exact = nodes.iter().any(|n| {
                                        n.file_path.as_str().replace('\\', "/").to_lowercase()
                                            == want
                                    });
                                    if exact {
                                        TargetStatus::Exists
                                    } else if is_create {
                                        TargetStatus::NewTarget
                                    } else {
                                        TargetStatus::NotFound
                                    }
                                }
                                Err(_) => TargetStatus::ProviderFailed,
                            }
                        }
                        Err(_) => TargetStatus::ProviderFailed,
                    }
                }
            };
            match target_status {
                TargetStatus::NotFound => checks.push(ValidationCheck::new(
                    "target_file",
                    "fail",
                    CoverageClass::Meta,
                    vec![format!(
                        "target file `{}` is not in the indexed project (change_kind=modify) — cannot verify a modification against a file that is not there; pass change_kind=create if it is new",
                        target_file.as_deref().unwrap_or("")
                    )],
                )),
                TargetStatus::ProviderFailed => checks.push(ValidationCheck::new(
                    "target_file",
                    "warn",
                    CoverageClass::Meta,
                    vec![
                        "the index lookup for the target file FAILED — target existence is UNKNOWN, not verified".to_string(),
                    ],
                )),
                // Round-8 P1-3: `create` targeting a file that ALREADY exists is
                // an error — you cannot create what is already there.
                TargetStatus::Exists if is_create => checks.push(ValidationCheck::new(
                    "target_file",
                    "fail",
                    CoverageClass::Meta,
                    vec![format!(
                        "change_kind=create but target file `{}` ALREADY exists in the index — a create must not overwrite an existing file; use change_kind=modify",
                        target_file.as_deref().unwrap_or("")
                    )],
                )),
                _ => {}
            }

            // ── Round-8 P0-1: language / target-extension compatibility ───
            // C# code aimed at a .vb file (or vice-versa) is never valid for the
            // target; a substring "verification" of it is meaningless. Fail the
            // mismatch outright so it can never reach PASS.
            if let Some(mismatch) = language_target_mismatch(&language, target_file.as_deref()) {
                checks.push(ValidationCheck::new(
                    "language_mismatch",
                    "fail",
                    CoverageClass::Meta,
                    vec![mismatch],
                ));
            }

            // Round-8 P0-1: comments must not satisfy presence checks (a lone
            // `// audit_probe_key` earned PASS). Strip comments once, up front,
            // for every caller-assertion substring test below.
            let code_nocomments = strip_code_comments(&code, is_vb);
            let code_nc_lower = code_nocomments.to_lowercase();

            // ── Check 1: SQL tables. Round-8 P0-1 (re-audited): the caller's
            // expected_tables is a CALLER ASSERTION and must NEVER whitelist
            // schema existence. It is split into two independent checks:
            //   (1) expected_tables  — AssertionOnly: the caller's tokens appear.
            //   (2) schema_consistency — Verified: EVERY parsed table reference in
            //       the code resolves to an INDEXED table. A referenced table that
            //       is not in the schema is UNKNOWN regardless of what the caller
            //       "expected" (the fake-table false-PASS). Verified is earned only
            //       when the schema was available AND every reference resolved.
            let known_tables: HashSet<String> = {
                let graph_tables = graph
                    .query_nodes(&project_id, Some("db_table"), None, None, 5000)
                    .unwrap_or_default();
                graph_tables.iter().map(|n| n.name.to_lowercase()).collect()
            };
            // Comment-stripped so a table named only in a comment is not a ref.
            let referenced = referenced_sql_tables(&code_nocomments);

            // (1) caller-assertion presence check.
            if !expected_tables.is_empty() {
                let mut missing_tables = Vec::new();
                let mut found_tables = Vec::new();
                for table in &expected_tables {
                    if contains_identifier_literal(&code_nc_lower, &table.to_lowercase()) {
                        found_tables.push(table.clone());
                    } else {
                        missing_tables.push(table.clone());
                    }
                }
                let (status, detail) = if missing_tables.is_empty() {
                    (
                        "pass",
                        format!(
                            "All {} caller-expected table token(s) appear in the code (ASSERTION only — presence, not correctness)",
                            found_tables.len()
                        ),
                    )
                } else {
                    (
                        "warn",
                        format!("Expected table literal(s) not found: {}. ORM references/mappings are unverified; missing literals do not establish missing table access", missing_tables.join(", ")),
                    )
                };
                checks.push(ValidationCheck::new(
                    "expected_tables",
                    status,
                    CoverageClass::AssertionOnly,
                    vec![detail],
                ));
            }

            // (2) project verification: every referenced table resolves to schema.
            if !referenced.is_empty() {
                if known_tables.is_empty() {
                    // Schema unavailable/unindexed — we CANNOT verify. Explicit,
                    // and NOT counted as a project-derived verification.
                    checks.push(ValidationCheck::new(
                        "schema_consistency",
                        "warn",
                        CoverageClass::Meta,
                        vec![format!(
                            "{} table reference(s) found but the project schema is unavailable — cannot verify: {}",
                            referenced.len(),
                            referenced.join(", ")
                        )],
                    ));
                } else {
                    let unknown: Vec<String> = referenced
                        .iter()
                        .filter(|t| !known_tables.contains(&t.to_lowercase()))
                        .cloned()
                        .collect();
                    if unknown.is_empty() {
                        checks.push(ValidationCheck::new(
                            "schema_consistency",
                            "pass",
                            CoverageClass::Verified,
                            vec![format!(
                                "All {} referenced table(s) resolve to the indexed schema",
                                referenced.len()
                            )],
                        ));
                    } else {
                        // A referenced table NOT in the schema — never a clean pass,
                        // even if the caller "expected" it. WARN (could be a temp
                        // table/CTE), and NOT a successful verification, so it does
                        // not count toward the PASS-earning Verified checks.
                        checks.push(ValidationCheck::new(
                            "schema_consistency",
                            "warn",
                            CoverageClass::Meta,
                            vec![format!(
                                "Referenced table(s) NOT in the project schema (unknown — verify they are real, not just caller-expected): {}",
                                unknown.join(", ")
                            )],
                        ));
                    }
                }
            }

            // ── Check 2: VB Translation Trap Avoidance ────────────────────
            if is_vb && include_migration_advice {
                let files = vec![("generated_code.vb", code.as_str())];
                let report =
                    engram_index::vb_translation_traps::detect_vb_translation_traps(&files);

                let status = if report.silent_bug_count > 0 {
                    "fail"
                } else if report.total_traps > 0 {
                    "warn"
                } else {
                    "pass"
                };

                let mut details = Vec::new();
                if report.total_traps == 0 {
                    details.push("No VB-to-C# translation traps detected (migration advice requested)".to_string());
                } else {
                    details.push(format!(
                        "{} VB-to-C# migration traps ({} potential silent translation bugs, {} translation compile errors)",
                        report.total_traps, report.silent_bug_count, report.compile_error_count
                    ));
                    for trap in report.traps.iter().take(5) {
                        details.push(format!(
                            "  {}: {} — {}",
                            trap.trap, trap.risk, trap.guidance
                        ));
                    }
                }

                checks.push(ValidationCheck::new(
                    "vb_traps",
                    status,
                    CoverageClass::GenericLint,
                    details,
                ));
            }

            // ── Check 3: Session Key Consistency ──────────────────────────
            if !expected_session_keys.is_empty() {
                let mut missing_keys = Vec::new();
                let mut found_keys = Vec::new();

                for key in &expected_session_keys {
                    // Round-8 P0-1: comment-stripped — a key named only in a
                    // comment (`// audit_probe_key`) must not count as handled.
                    if code_nocomments.contains(key) {
                        found_keys.push(key.clone());
                    } else {
                        missing_keys.push(key.clone());
                    }
                }

                let status = if !missing_keys.is_empty() {
                    "warn"
                } else {
                    "pass"
                };

                let mut details = Vec::new();
                if missing_keys.is_empty() {
                    details.push(format!(
                        "All {} expected session keys referenced",
                        found_keys.len()
                    ));
                } else {
                    details.push(format!("Missing session keys: {}", missing_keys.join(", ")));
                    details.push(
                        "The original code used these keys — ensure they're still handled"
                            .to_string(),
                    );
                }

                checks.push(ValidationCheck::new(
                    "session_keys",
                    status,
                    CoverageClass::AssertionOnly,
                    details,
                ));
            }

            // ── Check 4: SP Call Correctness ──────────────────────────────
            if !expected_sps.is_empty() {
                let mut missing_sps = Vec::new();
                let mut found_sps = Vec::new();

                for sp in &expected_sps {
                    let sp_clean = sp
                        .rsplit('.')
                        .next()
                        .unwrap_or(sp)
                        .trim_start_matches('[')
                        .trim_end_matches(']');

                    if code_nc_lower.contains(&sp_clean.to_lowercase()) {
                        found_sps.push(sp.clone());
                    } else {
                        missing_sps.push(sp.clone());
                    }
                }

                let status = if !missing_sps.is_empty() {
                    "warn"
                } else {
                    "pass"
                };

                let mut details = Vec::new();
                if missing_sps.is_empty() {
                    details.push(format!(
                        "All {} expected stored procedures referenced",
                        found_sps.len()
                    ));
                } else {
                    details.push(format!("Missing SP references: {}", missing_sps.join(", ")));
                }

                checks.push(ValidationCheck::new(
                    "stored_procs",
                    status,
                    CoverageClass::AssertionOnly,
                    details,
                ));
            }

            // ── Check 5: Control ID Validity ──────────────────────────────
            if !expected_control_ids.is_empty() {
                let mut missing_ids = Vec::new();
                let mut found_ids = Vec::new();

                for id in &expected_control_ids {
                    // Round-8 P0-1: comment-stripped presence check.
                    if code_nocomments.contains(id) {
                        found_ids.push(id.clone());
                    } else {
                        missing_ids.push(id.clone());
                    }
                }

                let status = if !missing_ids.is_empty() {
                    "warn"
                } else {
                    "pass"
                };

                let mut details = Vec::new();
                if missing_ids.is_empty() {
                    details.push(format!(
                        "All {} expected control IDs referenced",
                        found_ids.len()
                    ));
                } else {
                    details.push(format!("Missing control IDs: {}", missing_ids.join(", ")));
                }

                checks.push(ValidationCheck::new(
                    "control_ids",
                    status,
                    CoverageClass::AssertionOnly,
                    details,
                ));
            }

            // ── Check 6: Caller Compatibility ─────────────────────────────
            // Round-7 P0-2: a substring name-presence test is NOT caller
            // compatibility and must never earn contract coverage. Real
            // coverage requires resolving the EXACT original method (exact
            // target file + exact name); even then this tool does not parse or
            // compare signatures, so it surfaces the caller impact as an
            // advisory WARN — never a clean PASS. Without a resolved method the
            // note is a non-coverage `advisory` that cannot earn PASS.
            if let Some(ref orig_name) = original_method {
                let has_name = code.contains(orig_name);
                let resolved = target_file.as_ref().and_then(|tfile| {
                    select_method_node(&graph, &project_id, tfile, orig_name, None, None).ok()
                });
                match resolved {
                    Some(node) => {
                        let caller_count = crate::handlers::incoming_caller_edges(
                            &graph,
                            &project_id,
                            &node.node_id,
                            100,
                        )
                        .len();
                        checks.push(ValidationCheck::new(
                            "caller_compatibility",
                            "warn",
                            // Round-8 P0-1: this is the one PROJECT-DERIVED
                            // verification here — the EXACT target method was
                            // resolved in the graph. It stays WARN (no signature
                            // parse), but it is real coverage.
                            CoverageClass::Verified,
                            vec![
                                format!(
                                    "Resolved `{}` in `{}`; {} caller(s) depend on it.",
                                    orig_name,
                                    node.file_path.as_str(),
                                    caller_count
                                ),
                                "This tool does NOT parse or compare signatures — you must verify name, parameters, types/modifiers, return type, accessibility, static/shared, and generic arity are preserved.".to_string(),
                            ],
                        ));
                    }
                    None => {
                        let msg = if has_name {
                            format!(
                                "`{}` appears in the generated code, but no such method was resolved (no exact target file / method) — caller compatibility is NOT verified.",
                                orig_name
                            )
                        } else {
                            format!(
                                "`{}` was not found in the generated code and no method was resolved — caller compatibility is NOT verified.",
                                orig_name
                            )
                        };
                        checks.push(ValidationCheck::new(
                            "advisory",
                            "warn",
                            CoverageClass::Meta,
                            vec![msg],
                        ));
                    }
                }
            }

            // ── Check 7: Sync Hazard Introduction ─────────────────────────
            {
                use engram_index::sync_hazard_detector::HazardSeverity;
                let mut report = engram_index::sync_hazard_detector::detect_sync_hazards(&code, is_vb);
                if !include_migration_advice {
                    // These APIs are valid in WebForms. Their replacements
                    // are relevant to an ASP.NET Core migration, not native
                    // code correctness. Keep real blocking/locking hazards.
                    report.hazards.retain(|h| !matches!(h.pattern_type.as_str(),
                        "http_context_current" | "configuration_manager" | "web_configuration_manager"));
                    report.critical_count = report.hazards.iter().filter(|h| h.severity == HazardSeverity::Critical).count();
                    report.high_count = report.hazards.iter().filter(|h| h.severity == HazardSeverity::High).count();
                    report.medium_count = report.hazards.iter().filter(|h| h.severity == HazardSeverity::Medium).count();
                }

                let status = if report.critical_count > 0 {
                    "fail"
                } else if report.high_count > 0 {
                    "warn"
                } else {
                    "pass"
                };

                let mut details = Vec::new();
                if report.hazards.is_empty() {
                    details.push("No sync hazards detected in generated code".to_string());
                } else {
                    details.push(format!(
                        "{} sync hazards: {} critical, {} high, {} medium",
                        report.hazards.len(),
                        report.critical_count,
                        report.high_count,
                        report.medium_count,
                    ));
                    for h in report.hazards.iter().take(5) {
                        details.push(format!(
                            "  Line {}: {} ({:?}) → {}",
                            h.line_number, h.pattern_type, h.severity, h.modern_equivalent
                        ));
                    }
                }

                checks.push(ValidationCheck::new(
                    "sync_hazards",
                    status,
                    CoverageClass::GenericLint,
                    details,
                ));
            }

            // ── Compute overall verdict ───────────────────────────────────
            // Round-8 P0-1: coverage is counted by EVIDENCE CLASS, not by "any
            // check that ran". Only PROJECT-DERIVED `Verified` checks earn a
            // PASS; caller assertions and generic lints do not.
            let verified_checks = checks
                .iter()
                .filter(|c| c.coverage_class == CoverageClass::Verified)
                .count();
            let assertion_checks = checks
                .iter()
                .filter(|c| c.coverage_class == CoverageClass::AssertionOnly)
                .count();
            let generic_lint_checks = checks
                .iter()
                .filter(|c| c.coverage_class == CoverageClass::GenericLint)
                .count();
            let change_kind_modify = change_kind == crate::models::ChangeKind::Modify;
            let coverage = ValidationCoverage {
                verified_checks,
                assertion_checks,
                generic_lint_checks,
                target: target_status,
                change_kind_modify,
            };
            let overall = compute_validation_verdict(&checks, &coverage);

            Ok(ValidationReport {
                overall_verdict: overall,
                coverage,
                checks,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let report = result.map_err(|e: String| McpError::invalid_params(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            render_validation_report_markdown(&report),
        )]))
    }

    // ── 38-7: validate_sql_fragment ──────────────────────────────────────

    pub async fn handle_validate_sql_fragment(
        &self,
        req: ValidateSqlFragmentRequest,
    ) -> Result<CallToolResult, McpError> {
        if req.sql.trim().is_empty() {
            return Err(McpError::invalid_params("sql must not be blank", None));
        }
        if req.sql.len() > MAX_SQL_LENGTH {
            return Err(McpError::invalid_params(
                format!(
                    "sql exceeds maximum length of {} bytes (got {})",
                    MAX_SQL_LENGTH,
                    req.sql.len()
                ),
                None,
            ));
        }
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let sql = req.sql.clone();
        let _source_file = req.source_file.clone();
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            let mut issues: Vec<SqlValidationIssue> = Vec::new();

            let binding = super::sql_binding::bind(&sql, &graph, &project_id)?;
            let referenced_tables = binding.tables;
            let schema_covered = binding.complete;
            issues.extend(binding.issues);
            let coverage = if schema_covered {
                "Supported T-SQL statement identifiers checked against indexed schema, including aliases, joins, supported nested scopes and ORDER BY. Types, function contracts, aggregate legality, nullability/default contracts, application-side injection risk, permissions and execution were not checked."
            } else {
                issues.push(SqlValidationIssue {
                    severity: "info".into(), category: "incomplete_validation".into(),
                    message: "Statement binding incomplete. See binding issues for unsupported clauses or missing declaration evidence. Wildcard expansion and non-SELECT contracts may require additional validation; use database compilation/execution checks.".into(),
                });
                "PARTIAL: T-SQL parsing and available identifier evidence; statement binding incomplete."
            };

            // SQL text does not establish how an application constructed it.
            // In particular, legal SQL concatenation is not injection evidence.
            let has_fail = issues.iter().any(|i| i.severity == "fail");
            let has_warn = issues.iter().any(|i| i.severity == "warn");
            let verdict = if has_fail {
                "FAIL"
            } else if has_warn {
                "WARN"
            } else if !schema_covered {
                "INSUFFICIENT"
            } else if issues.is_empty() {
                "PASS"
            } else {
                "INFO"
            };

            Ok(SqlValidationReport {
                verdict: verdict.to_string(),
                coverage: coverage.into(),
                tables_referenced: referenced_tables,
                issues,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let report = result.map_err(|e: String| McpError::internal_error(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            render_sql_validation_markdown(&report),
        )]))
    }

    // ── 38-8: find_tests_for_method ──────────────────────────────────────

    pub async fn handle_find_tests_for_method(
        &self,
        req: FindTestsForMethodRequest,
    ) -> Result<CallToolResult, McpError> {
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let method_name = req.method_name.clone();
        let file_filter = req.file_path.clone();
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            if method_name.trim().is_empty() { return Err("method_name must not be blank".to_string()); }
            if req.start_line.is_some() && (req.start_line == Some(0) || file_filter.as_deref().is_none_or(|p| p.trim().is_empty())) {
                return Err("start_line must be positive and requires file_path".to_string());
            }
            let candidates = graph.query_nodes_by_symbol_name(&project_id, &method_name, file_filter.as_deref(), 201)
                .map_err(|e| format!("method lookup failed: {e}"))?;
            if candidates.len() >= 201 { return Err("INCOMPLETE: declaration lookup reached its cap; narrow method_name and file_path".to_string()); }
            let candidates: Vec<_> = candidates.into_iter()
                .filter(|n| matches!(n.node_type.as_str(), "function" | "method" | "sub" | "procedure"))
                .filter(|n| file_filter.as_deref().is_none_or(|p| n.file_path.as_str().eq_ignore_ascii_case(p)))
                .filter(|n| req.start_line.is_none_or(|line| n.start_line == line))
                .collect();
            if candidates.len() != 1 {
                return Err(format!("{}: {} declarations match '{}'. Supply an exact qualified method name, file_path and, for overloads, start_line. Candidates: {}",
                    if candidates.is_empty() { "NOT_FOUND" } else { "AMBIGUOUS" }, candidates.len(), method_name,
                    candidates.iter().take(10).map(|n| format!("{} at {}:{}", fqn_from_node(n), n.file_path, n.start_line)).collect::<Vec<_>>().join("; ")));
            }
            let target = &candidates[0];
            let mut warnings = vec!["Static indexed test candidates only; source positions and test execution are unverified. Name matches are heuristic, not proof of coverage.".to_string()];
            let (incoming, capped) = crate::handlers::incoming_caller_edges_checked(&graph, &project_id, &target.node_id, 2000)
                .map_err(|e| format!("caller lookup failed: {e}"))?;
            if capped { warnings.push("Caller candidates truncated at 2000.".into()); }
            let caller_ids: std::collections::HashSet<_> = incoming.into_iter().map(|(id, _, _)| id).collect();
            let mut methods = graph.query_nodes(&project_id, Some("function"), None, None, 20001)
                .map_err(|e| format!("test-name lookup failed: {e}"))?;
            if methods.len() > 20000 { warnings.push("Name-based candidate scan truncated at 20000 indexed functions; direct callers are looked up separately.".into()); methods.truncate(20000); }
            let mut seen: std::collections::HashSet<_> = methods.iter().map(|n| n.node_id.clone()).collect();
            for id in &caller_ids {
                if seen.insert(id.clone()) {
                    match graph.get_node(&project_id, id).map_err(|e| format!("caller node lookup failed: {e}"))? {
                        Some(node) => methods.push(node),
                        None => warnings.push(format!("Caller node unavailable: {id}")),
                    }
                }
            }
            let mut test_files = std::collections::HashSet::new();
            let mut test_hits = Vec::new();
            let terminal = bare_method_name(target).to_lowercase();
            for tm in methods {
                if !crate::services::pre_commit_review_service::is_test_path(&format!("/{}", tm.file_path.as_str())) { continue; }
                test_files.insert(tm.file_path.as_str().to_string());
                let direct = caller_ids.contains(&tm.node_id);
                if direct || tm.name.to_lowercase().contains(&terminal) {
                    test_hits.push(TestHit {
                        test_name: tm.name, test_file: tm.file_path.as_str().to_string(),
                        line_start: tm.start_line, line_end: tm.end_line,
                        match_type: if direct { "dependency_edge" } else { "name_match_heuristic" }.into(),
                    });
                }
            }
            test_hits.sort_by(|a, b| a.match_type.cmp(&b.match_type).then(a.test_file.cmp(&b.test_file)).then(a.line_start.cmp(&b.line_start)));
            if test_hits.len() > 200 { warnings.push("Test candidates truncated at 200 (direct edges first).".into()); test_hits.truncate(200); }
            Ok(TestSearchResult { method_name: fqn_from_node(target), target_node_id: target.node_id.clone(), target_file: target.file_path.to_string(), target_start_line: target.start_line, test_hits, test_files_searched: test_files.len(), warnings })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let report = result.map_err(|e: String| McpError::invalid_params(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut md = format!("# Tests for `{}`\n\n", report.method_name);
        md.push_str(&format!(
            "Indexed declaration: {}:{} (`{}`)\n\n",
            report.target_file, report.target_start_line, report.target_node_id
        ));
        md.push_str(&format!(
            "Searched {} test files.\n\n",
            report.test_files_searched
        ));

        for warning in &report.warnings {
            md.push_str(&format!("Coverage: {warning}\n\n"));
        }
        if report.test_hits.is_empty() {
            md.push_str("**No test candidates found within indexed coverage.** Consider writing characterization tests before modifying this method.\n");
        } else {
            md.push_str(&format!("## {} Tests Found\n\n", report.test_hits.len()));
            md.push_str("| Test Name | File | Lines | Match Type |\n");
            md.push_str("|-----------|------|-------|------------|\n");
            for hit in &report.test_hits {
                md.push_str(&format!(
                    "| `{}` | `{}` | {}–{} | {} |\n",
                    hit.test_name, hit.test_file, hit.line_start, hit.line_end, hit.match_type,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(md)]))
    }

    // ── 38-9: find_dead_methods ──────────────────────────────────────────

    pub async fn handle_find_dead_methods(
        &self,
        req: FindDeadMethodsRequest,
    ) -> Result<CallToolResult, McpError> {
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let file_filter = req.file_path.clone();
        let limit = req.sanitized_limit();
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            let all_methods = graph
                .query_nodes(
                    &project_id,
                    Some("function"),
                    None,
                    file_filter.as_deref(),
                    10000,
                )
                .unwrap_or_default();

            let mut dead_methods: Vec<DeadMethodInfo> = Vec::new();

            for node in &all_methods {
                let effects = meta_csv(node, "effects");
                let kind = full_mig::classify_method_kind_pub(&node.name, &effects, &node.metadata);
                let kind_str = kind.to_string(); // cache — avoid multiple to_string() calls

                // Skip framework-invoked methods that never have explicit callers:
                // - Lifecycle: Page_Load, Page_Init, etc. (invoked by ASP.NET pipeline)
                // - ControlEvent: Button1_Click, etc. (invoked via Handles clause / ASPX binding)
                // - WebMethod: invoked by HTTP clients
                if kind_str == "Lifecycle" || kind_str == "ControlEvent" || kind_str == "WebMethod"
                {
                    continue;
                }

                // Skip methods with Handles clause — invoked by events
                let handles = meta_csv(node, "handles_clause");
                if !handles.is_empty() {
                    continue;
                }

                // Check for incoming caller edges (Calls + Dependency)
                let caller_count =
                    crate::handlers::incoming_caller_edges(&graph, &project_id, &node.node_id, 1)
                        .len();

                if caller_count == 0 {
                    // L-2 fix: public methods with no static callers may still
                    // be live if called via reflection, dynamic binding, or
                    // from assemblies not included in this project.  Surface a
                    // confidence note so callers don't blindly delete them.
                    let access = meta_str(node, "access_level");
                    let confidence_note = if access.eq_ignore_ascii_case("public")
                        || access.eq_ignore_ascii_case("protected")
                    {
                        "Low confidence: non-private method — may be invoked via \
                         reflection, Type.GetMethod(), dynamic binding, or from \
                         an assembly not present in this project. Verify before removing."
                            .to_string()
                    } else {
                        String::new()
                    };

                    dead_methods.push(DeadMethodInfo {
                        fqn: fqn_from_node(node),
                        file_path: node.file_path.as_str().to_string(),
                        line_start: node.start_line,
                        line_end: node.end_line,
                        method_kind: kind_str,
                        line_count: if node.end_line >= node.start_line {
                            node.end_line - node.start_line + 1
                        } else {
                            1
                        },
                        access_level: access,
                        confidence_note,
                    });

                    if dead_methods.len() >= limit {
                        break;
                    }
                }
            }

            // Sort by line count descending (largest dead methods first)
            dead_methods.sort_by(|a, b| b.line_count.cmp(&a.line_count));

            Ok(DeadMethodReport {
                dead_methods,
                total_methods: all_methods.len(),
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let report = result.map_err(|e: String| McpError::invalid_params(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let total_dead_lines: u32 = report.dead_methods.iter().map(|m| m.line_count).sum();

        let mut md = format!(
            "# Dead Method Analysis\n\n- **Total methods**: {}\n- **Dead methods**: {} ({:.1}%)\n- **Dead lines**: {}\n\n",
            report.total_methods,
            report.dead_methods.len(),
            if report.total_methods > 0 {
                report.dead_methods.len() as f64 / report.total_methods as f64 * 100.0
            } else {
                0.0
            },
            total_dead_lines,
        );

        if report.dead_methods.is_empty() {
            md.push_str("No dead methods found.\n");
        } else {
            let low_confidence_count = report
                .dead_methods
                .iter()
                .filter(|m| !m.confidence_note.is_empty())
                .count();
            if low_confidence_count > 0 {
                md.push_str(&format!(
                    "> ⚠️ **{} of {} results are low-confidence** (public/protected methods — \
                     may be reflection-invoked). Review `confidence_note` before removing.\n\n",
                    low_confidence_count,
                    report.dead_methods.len(),
                ));
            }
            md.push_str("| FQN | File | Lines | Kind | Access | Confidence |\n");
            md.push_str("|-----|------|-------|------|--------|------------|\n");
            for m in &report.dead_methods {
                let confidence = if m.confidence_note.is_empty() {
                    "High".to_string()
                } else {
                    format!("⚠️ Low — {}", m.confidence_note)
                };
                md.push_str(&format!(
                    "| `{}` | `{}` | {}–{} ({}) | {} | {} | {} |\n",
                    m.fqn,
                    m.file_path,
                    m.line_start,
                    m.line_end,
                    m.line_count,
                    m.method_kind,
                    m.access_level,
                    confidence,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(md)]))
    }

    // ── 38-10: check_edit_safety ─────────────────────────────────────────

    pub async fn handle_check_edit_safety(
        &self,
        req: CheckEditSafetyRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        let project_dir = rec.directory.clone();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let file_path = req.file_path.clone();
        let method_name = req.method_name.clone();
        let class_name = req.class_name.clone();
        let line = req.line;
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            let node = select_method_node(
                &graph,
                &project_id,
                &file_path,
                &method_name,
                class_name.as_deref(),
                line,
            )?;
            // Same evidence assembly as get_method_edit_context: the verdict
            // is computed from identical facts (body read, complexity
            // estimated, blast attempted, coverage recorded).
            let ev = assemble_edit_evidence(&graph, &project_id, &project_dir, node)?;
            Ok::<EditSafetyResult, String>(ev.edit_safety)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let safety = result.map_err(|e| McpError::invalid_params(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&safety)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let badge = match safety.verdict.as_str() {
            "green" => "SAFE TO EDIT",
            "yellow" => "CAUTION",
            "red" => "HIGH RISK",
            _ => "UNKNOWN",
        };

        let mut md = format!("# Edit Safety: `{}`\n\n", req.method_name);
        md.push_str(&format!(
            "**Verdict**: {} (confidence {:.0}%)\n\n",
            badge,
            safety.confidence * 100.0
        ));

        for r in &safety.reasons {
            md.push_str(&format!("- {}\n", r));
        }

        if !safety.pre_edit_checklist.is_empty() {
            md.push_str("\n### Pre-Edit Checklist\n\n");
            for item in &safety.pre_edit_checklist {
                md.push_str(&format!("- [ ] {}\n", item));
            }
        }

        if !safety.post_edit_checklist.is_empty() {
            md.push_str("\n### Post-Edit Checklist\n\n");
            for item in &safety.post_edit_checklist {
                md.push_str(&format!("- [ ] {}\n", item));
            }
        }

        md.push('\n');
        md.push_str(&render_coverage_block(&safety.completeness));

        md.push_str(
            "next: find_symbol_references(<method>) for the caller list; \
             get_method_edit_context before making the edit.\n",
        );
        let (banner, footer) = self
            .access_freshness(&req.project_id, &rec.directory, Some(&req.file_path))
            .await;
        let mut out = banner.unwrap_or_default();
        out.push_str(&md);
        out.push_str(&footer);
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

#[cfg(test)]
mod referenced_sql_tables_tests {
    use super::referenced_sql_tables;

    #[test]
    fn linq_aliases_do_not_become_sql_tables() {
        assert!(
            referenced_sql_tables("From ath In records Join pr In projects Select ath").is_empty()
        );
        assert!(
            referenced_sql_tables(
                "from item in records join other in values on item.Id equals other.Id select item"
            )
            .is_empty()
        );
        assert_eq!(
            referenced_sql_tables("SELECT * FROM inventory WHERE id IN (1, 2)"),
            vec!["inventory"]
        );
        assert_eq!(
            referenced_sql_tables("From item In rows\nDim sql = \"SELECT * FROM [dbo].[orders]\""),
            vec!["orders"]
        );
    }

    #[test]
    fn schema_qualified_names_yield_the_table_not_the_schema() {
        assert_eq!(
            referenced_sql_tables("SELECT * FROM [dbo].[io_pr_iom]"),
            vec!["io_pr_iom"]
        );
        assert_eq!(
            referenced_sql_tables("SELECT * FROM dbo.projekt p"),
            vec!["projekt"]
        );
        assert_eq!(
            referenced_sql_tables("UPDATE [mydb].[dbo].[resurs] SET x = 1"),
            vec!["resurs"]
        );
        assert_eq!(
            referenced_sql_tables("SELECT * FROM planner_ak_aktiviteter"),
            vec!["planner_ak_aktiviteter"]
        );
        // JOIN + dedup + qualifier mix.
        let t = referenced_sql_tables(
            "SELECT * FROM [dbo].[a] JOIN b ON a.id=b.id JOIN [dbo].[a] c ON 1=1",
        );
        assert_eq!(t, vec!["a", "b"]);
    }
}

#[cfg(test)]
mod resolve_unique_function_tests {
    use super::resolve_unique_function;

    fn store() -> (tempfile::TempDir, engram_graph::GraphStore) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let g = engram_graph::GraphStore::open(&tmp.path().join("g.redb")).expect("open");
        (tmp, g)
    }

    fn func(node_id: &str, name: &str, file: &str, fqn: Option<&str>) -> engram_graph::Node {
        let metadata = fqn.map(|f| serde_json::json!({"fqn": f}));
        engram_graph::Node {
            node_id: node_id.to_string(),
            node_type: "function".to_string(),
            name: name.to_string(),
            namespace: "memory".to_string(),
            language: "vbnet".to_string(),
            file_path: engram_core::RelPath::new(file),
            start_line: 1,
            end_line: 5,
            generation: 1,
            metadata,
        }
    }

    #[test]
    fn substring_collision_is_an_error_with_candidates() {
        let (_t, g) = store();
        g.upsert_nodes(
            "p",
            &[
                func(
                    "sym:function:a.aspx.vb:PageA.Page_Load:1",
                    "PageA.Page_Load",
                    "a.aspx.vb",
                    None,
                ),
                func(
                    "sym:function:b.aspx.vb:PageB.Page_Load:1",
                    "PageB.Page_Load",
                    "b.aspx.vb",
                    None,
                ),
            ],
        )
        .unwrap();
        let err = resolve_unique_function(&g, "p", "Page_Load").unwrap_err();
        assert!(
            err.contains("AMBIGUOUS"),
            "must refuse to pick silently: {err}"
        );
        assert!(err.contains("PageA.Page_Load") && err.contains("PageB.Page_Load"));
    }

    #[test]
    fn exact_name_beats_substring_hits() {
        let (_t, g) = store();
        g.upsert_nodes(
            "p",
            &[
                func("sym:function:a.vb:Save:1", "Save", "a.vb", None),
                func("sym:function:b.vb:SaveAll:1", "SaveAll", "b.vb", None),
            ],
        )
        .unwrap();
        let node = resolve_unique_function(&g, "p", "Save").expect("exact match wins");
        assert_eq!(node.name, "Save");
    }

    #[test]
    fn exact_metadata_fqn_disambiguates() {
        let (_t, g) = store();
        g.upsert_nodes(
            "p",
            &[
                func(
                    "sym:function:a.aspx.vb:Page_Load:1",
                    "Page_Load",
                    "a.aspx.vb",
                    Some("_admin.PageA.Page_Load"),
                ),
                func(
                    "sym:function:b.aspx.vb:Page_Load:1",
                    "Page_Load",
                    "b.aspx.vb",
                    Some("_pub.PageB.Page_Load"),
                ),
            ],
        )
        .unwrap();
        let node = resolve_unique_function(&g, "p", "_admin.PageA.Page_Load").expect("fqn match");
        assert_eq!(node.file_path.as_str(), "a.aspx.vb");
    }

    #[test]
    fn missing_method_is_a_clear_error() {
        let (_t, g) = store();
        let err = resolve_unique_function(&g, "p", "Ghost").unwrap_err();
        assert!(err.contains("No method found"));
    }
}

#[cfg(test)]
mod diagnostic_scope_tests {
    use super::diagnostic_belongs_to_context;

    /// Real shape from tests/negative/diagnostics/mlc6013_definite_ref_cycle.ml:
    /// the strong-Ref cycle is reported on the `Type` line (1), while the
    /// file's only callable is the synthetic module entry at 5..=5.
    #[test]
    fn type_declaration_finding_reaches_every_method_in_the_file() {
        let claimable = [(5u32, 5u32)];
        assert!(
            diagnostic_belongs_to_context(1, 5, 5, &claimable),
            "MLC6013 on the Type line must not be dropped — no method can \
             ever claim it, so a method-range filter hides it forever"
        );
    }

    #[test]
    fn finding_inside_this_method_is_kept() {
        let claimable = [(10u32, 20u32), (30u32, 40u32)];
        assert!(diagnostic_belongs_to_context(15, 10, 20, &claimable));
    }

    /// The rescue must not turn into "show everything": a finding that
    /// belongs to a DIFFERENT method stays out.
    #[test]
    fn finding_inside_another_method_is_excluded() {
        let claimable = [(10u32, 20u32), (30u32, 40u32)];
        assert!(
            !diagnostic_belongs_to_context(35, 10, 20, &claimable),
            "a finding owned by another method must not leak into this context"
        );
    }

    #[test]
    fn finding_between_methods_is_kept_as_file_level_context() {
        let claimable = [(10u32, 20u32), (30u32, 40u32)];
        assert!(diagnostic_belongs_to_context(25, 10, 20, &claimable));
    }

    /// With no method ranges known, nothing can be claimed, so a
    /// declaration-level finding must still surface rather than vanish.
    #[test]
    fn empty_claimable_keeps_the_finding() {
        assert!(diagnostic_belongs_to_context(1, 5, 5, &[]));
    }
}

#[cfg(test)]
mod edit_safety_tests {
    //! Row-2 audit (docs/audits/02): a verdict computed from providers that
    //! failed or never ran must not be green, a caller cap must render as a
    //! lower bound, and "no callers found" must not become RED when the
    //! callers were simply not resolvable.
    use super::*;

    fn info(callers: usize, complexity: u32, session_writes: usize) -> MethodInfoResult {
        MethodInfoResult {
            fqn: "ns.cls.M".into(),
            file_path: "Site/App_Code/x.vb".into(),
            class_name: "cls".into(),
            method_name: "M".into(),
            signature: "M()".into(),
            return_type: "Sub".into(),
            access_level: "Public".into(),
            line_start: 1,
            line_end: 10,
            line_count: 10,
            language: "vbnet".into(),
            method_kind: "Helper".into(),
            effects: vec![],
            calls_methods: vec![],
            called_by: (0..callers)
                .map(|i| CallerLocation {
                    fqn: format!("ns.c{i}.F"),
                    file_path: format!("Site/App_Code/c{i}.vb"),
                    line: 1,
                    line_kind: "declaration",
                    line_end: 3,
                    edge_kind: "calls".into(),
                })
                .collect(),
            handles_clause: vec![],
            db_tables_accessed: vec![],
            stored_procs_called: vec![],
            session_keys_read: vec![],
            session_keys_written: (0..session_writes)
                .map(|i| format!("Session:k{i}"))
                .collect(),
            complexity_score: complexity,
            body_preview: None,
        }
    }

    fn complete() -> EditContextCompleteness {
        EditContextCompleteness::all_complete()
    }

    #[test]
    fn failed_blast_provider_is_never_green() {
        // Low-risk facts on every other axis: 1 caller, trivial complexity,
        // no session writes. Today this renders GREEN with risk 0.0.
        let mut c = complete();
        c.blast = ProviderStatus::Failed {
            reason: "graph read failed".into(),
        };
        let r = compute_edit_safety(&info(1, 3, 0), None, &c);
        assert_ne!(
            r.verdict, "green",
            "missing blast evidence rendered green: {r:?}"
        );
        assert!(
            r.reasons
                .iter()
                .any(|s| s.contains("blast") && s.contains("graph read failed")),
            "reason must name the failed provider: {:?}",
            r.reasons
        );
        assert!(r.confidence <= 0.5, "confidence {} > 0.5", r.confidence);
        assert_eq!(r.completeness.blast, c.blast);
    }

    #[test]
    fn unmeasured_complexity_is_never_green() {
        let mut c = complete();
        c.complexity = ProviderStatus::NotRun {
            reason: "body not read".into(),
        };
        let r = compute_edit_safety(&info(1, 0, 0), None, &c);
        assert_ne!(r.verdict, "green");
        assert!(
            r.reasons
                .iter()
                .any(|s| s.contains("complexity") && s.contains("body not read")),
            "{:?}",
            r.reasons
        );
    }

    #[test]
    fn unresolvable_callers_are_not_an_orphan_red() {
        // called_by is empty because the caller lookup FAILED, not because
        // nobody calls it. Today: RED "may be invoked via reflection".
        let mut c = complete();
        c.callers = ProviderStatus::Failed {
            reason: "adjacency read failed".into(),
        };
        let r = compute_edit_safety(&info(0, 3, 0), None, &c);
        assert_ne!(
            r.verdict, "red",
            "provider failure became an orphan RED: {r:?}"
        );
        assert!(
            r.reasons.iter().any(|s| s.contains("callers unknown")),
            "{:?}",
            r.reasons
        );
        assert!(
            !r.reasons.iter().any(|s| s.contains("reflection")),
            "the reflection guess must not appear when callers were not resolved: {:?}",
            r.reasons
        );
    }

    #[test]
    fn dangling_callers_are_not_an_orphan_red() {
        // Incoming edges exist but every source node is unresolved.
        let mut c = complete();
        c.callers_dangling = 3;
        let r = compute_edit_safety(&info(0, 3, 0), None, &c);
        assert_ne!(r.verdict, "red", "{r:?}");
        assert!(
            r.reasons.iter().any(|s| s.contains("3 dangling")),
            "{:?}",
            r.reasons
        );
    }

    #[test]
    fn capped_callers_render_as_a_lower_bound() {
        let mut c = complete();
        c.callers = ProviderStatus::Truncated {
            shown: 50,
            cap: 50,
            known_total: None,
        };
        let r = compute_edit_safety(&info(50, 3, 0), None, &c);
        assert_eq!(r.verdict, "red");
        assert!(
            r.reasons.iter().any(|s| s.contains("≥50 distinct callers")),
            "cap must render as a lower bound, got {:?}",
            r.reasons
        );
        assert!(
            !r.reasons
                .iter()
                .any(|s| s.starts_with("50 distinct callers")),
            "a bare capped count must not appear: {:?}",
            r.reasons
        );
    }

    #[test]
    fn exact_caller_total_is_used_when_known() {
        let mut c = complete();
        c.callers = ProviderStatus::Truncated {
            shown: 50,
            cap: 50,
            known_total: Some(98),
        };
        let r = compute_edit_safety(&info(50, 3, 0), None, &c);
        assert!(
            r.reasons.iter().any(|s| s.contains("98 distinct callers")),
            "{:?}",
            r.reasons
        );
    }

    #[test]
    fn completeness_travels_in_the_result_json() {
        let mut c = complete();
        c.session_writes = ProviderStatus::Truncated {
            shown: 200,
            cap: 200,
            known_total: None,
        };
        let r = compute_edit_safety(&info(1, 3, 0), None, &c);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["completeness"]["session_writes"]["status"], "truncated");
        assert_eq!(v["completeness"]["session_writes"]["cap"], 200);
        assert_eq!(v["completeness"]["callers"]["status"], "complete");
    }
}

/// Literal presence only: reject prefixes/suffixes inside longer identifiers.
/// This intentionally does not infer SQL or ORM reference semantics.
fn contains_identifier_literal(source: &str, literal: &str) -> bool {
    if literal.is_empty() {
        return false;
    }
    let identifier_char = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '$';
    source.match_indices(literal).any(|(start, matched)| {
        !source[..start].chars().next_back().is_some_and(identifier_char)
            && !source[start + matched.len()..].chars().next().is_some_and(identifier_char)
    })
}
