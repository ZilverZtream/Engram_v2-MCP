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
    pub line_end: u32,
    /// Edge kind that made this a caller (`calls`, `dependency`, …).
    pub edge_kind: String,
}

/// Result of get_full_method_body.
#[derive(Debug, Clone, Serialize)]
pub struct MethodBodyResult {
    pub fqn: String,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub source_code: String,
    pub surrounding_context: String,
    pub language: String,
    pub caller_bodies: Vec<CallerBody>,
}

/// A caller's full body for pattern understanding.
#[derive(Debug, Clone, Serialize)]
pub struct CallerBody {
    pub fqn: String,
    pub file_path: String,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EditContextCompleteness {
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

impl EditContextCompleteness {
    pub fn all_complete() -> Self {
        Self {
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
    pub method_body: Option<String>,
    pub style_profile: Option<String>,
    pub pattern_examples: Vec<PatternExample>,
    pub schema_snippets: Vec<TableSchemaSnippet>,
    pub sp_signatures: Vec<SpSignatureSnippet>,
    pub state_context: Vec<StateContextSnippet>,
    pub control_mappings: Vec<ControlMappingSnippet>,
    pub vb_traps: Vec<VbTrapSummary>,
    pub language_diagnostics: Vec<LanguageDiagnosticSummary>,
    pub sync_hazards: Vec<SyncHazardSummary>,
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
    pub nullable: bool,
}

/// Stored procedure signature snippet.
#[derive(Debug, Clone, Serialize)]
pub struct SpSignatureSnippet {
    pub sp_name: String,
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
    pub checks: Vec<ValidationCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationCheck {
    pub category: String,
    pub status: String,
    pub details: Vec<String>,
}

// ── Phase 38-7 Output Types ──────────────────────────────────────────────────

/// SQL fragment validation report.
#[derive(Debug, Clone, Serialize)]
pub struct SqlValidationReport {
    pub verdict: String,
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
    pub method_name: String,
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

fn fqn_from_node(node: &Node) -> String {
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

/// Extract the declaring class for a node: from the namespace when it
/// carries a type, else from the qualified NAME's second-to-last dot
/// segment (`_ata.huvud.CreateFromMarkers` → `huvud`).
fn class_of_node(node: &Node) -> String {
    let ns = node.namespace.trim();
    if !ns.is_empty() && ns != "default" && !is_search_namespace(ns) {
        return class_from_namespace(ns);
    }
    let mut parts = node.name.rsplit('.');
    parts.next(); // method segment
    parts.next().unwrap_or("").to_string()
}

/// Extract class name from an FQN-like namespace string.
fn class_from_namespace(namespace: &str) -> String {
    // namespace might be "MyApp.Pages.CheckoutPage" or just "CheckoutPage"
    namespace
        .rsplit('.')
        .next()
        .unwrap_or(namespace)
        .to_string()
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
#[derive(Debug, Clone, Default)]
pub(crate) struct MethodInfoCoverage {
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
            "Private".to_string()
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
            "Sub".to_string()
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

/// Read lines from a file (1-based inclusive range), with optional context.
fn read_lines_from_file(
    file_path: &Path,
    line_start: u32,
    line_end: u32,
    context_lines: u32,
) -> std::io::Result<(String, String)> {
    let content = std::fs::read_to_string(file_path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as u32;

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
    let mut candidates = graph
        .query_nodes(
            project_id,
            Some("function"),
            Some(method_name),
            Some(file_path),
            50,
        )
        .map_err(|e| format!("method lookup failed: {e}"))?;
    if let Some(cls) = class_name {
        let cls_lower = cls.to_lowercase();
        candidates.retain(|n| n.namespace.to_lowercase().contains(&cls_lower));
    }
    if candidates.is_empty() {
        return Err(method_not_found_message(
            graph,
            project_id,
            method_name,
            Some(file_path),
        ));
    }
    let exact: Vec<Node> = candidates
        .iter()
        .filter(|n| n.name.eq_ignore_ascii_case(method_name))
        .cloned()
        .collect();
    if !exact.is_empty() {
        candidates = exact;
    }
    // Same-name methods in DIFFERENT classes: describing the wrong one
    // poisons the edit that follows.
    let mut namespaces: Vec<&str> = candidates.iter().map(|n| n.namespace.as_str()).collect();
    namespaces.sort_unstable();
    namespaces.dedup();
    if namespaces.len() > 1 {
        let mut msg = format!(
            "AMBIGUOUS: '{}' exists in {} classes in '{}'. Re-call with class_name set:\n",
            method_name,
            namespaces.len(),
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
    let mut md = String::from("## Coverage\n\n");
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
        md.push_str(&format!("- {name}: {}\n", provider_text(st)));
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
                "No callers found — may be invoked via reflection or dynamic dispatch".to_string(),
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

fn render_method_body_markdown(result: &MethodBodyResult) -> String {
    let mut md = String::with_capacity(4_000);

    let lang_tag = if result.language.contains("vb") {
        "vb"
    } else {
        "csharp"
    };

    md.push_str(&format!("# Method Body: `{}`\n\n", result.fqn));
    md.push_str(&format!(
        "**File**: `{}` (lines {}–{})\n\n",
        result.file_path, result.line_start, result.line_end
    ));

    if !result.surrounding_context.is_empty() {
        md.push_str("## Context (above method)\n\n```");
        md.push_str(lang_tag);
        md.push('\n');
        md.push_str(&result.surrounding_context);
        md.push_str("\n```\n\n");
    }

    md.push_str("## Full Method Body\n\n```");
    md.push_str(lang_tag);
    md.push('\n');
    md.push_str(&result.source_code);
    md.push_str("\n```\n\n");

    if !result.caller_bodies.is_empty() {
        md.push_str("## Caller Bodies\n\n");
        for cb in &result.caller_bodies {
            md.push_str(&format!(
                "### `{}` (`{}` lines {}–{}) — {}\n\n```{}\n{}\n```\n\n",
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
                    "- `{}` — {}:{} — {}\n",
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
                "### `{}` (`{}` lines {}–{}) — {}\n\n```{}\n{}\n```\n\n",
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
            "- **Blast radius**: {:.0} ({})\n",
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
                        if col.nullable { "Yes" } else { "No" },
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

fn render_validation_report_markdown(report: &ValidationReport) -> String {
    let mut md = String::with_capacity(4_000);

    let badge = match report.overall_verdict.as_str() {
        "PASS" => "PASS",
        "WARN" => "WARN",
        "FAIL" => "FAIL",
        _ => "UNKNOWN",
    };

    md.push_str(&format!("# Code Validation Report: {}\n\n", badge));

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

    md.push_str(&format!("# SQL Validation: {}\n\n", report.verdict));

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
            let (resolved_fqn, resolved_file, resolved_start, resolved_end, language) =
                if let Some(ref fqn_query) = fqn {
                    // Resolve via graph node lookup
                    let node = resolve_unique_function(&graph, &project_id, fqn_query)?;

                    (
                        fqn_from_node(&node),
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

            // Read the method body from disk
            let full_path = safe_join(Path::new(&project_dir), &resolved_file)
                .map_err(|e| format!("Path validation failed for '{}': {e}", resolved_file))?;
            let (body, context) =
                read_lines_from_file(&full_path, resolved_start, resolved_end, context_lines)
                    .map_err(|e| format!("Cannot read '{}': {}", resolved_file, e))?;

            // Optionally get caller bodies
            let mut caller_bodies = Vec::new();
            if include_callers
                && let Some(ref fqn_query) = fqn {
                    if let Ok(target_node) = resolve_unique_function(&graph, &project_id, fqn_query)
                        .as_ref()
                    {
                        let callers = crate::handlers::incoming_caller_edges(
                            &graph,
                            &project_id,
                            &target_node.node_id,
                            max_callers,
                        );

                        for (source_id, kind, _weight) in callers.iter().take(max_callers) {
                            if let Ok(Some(src_node)) = graph.get_node(&project_id, source_id) {
                                let Ok(src_full) = safe_join(Path::new(&project_dir), src_node.file_path.as_str()) else { continue };
                                if let Ok((src_body, _)) = read_lines_from_file(
                                    &src_full,
                                    src_node.start_line,
                                    src_node.end_line,
                                    0,
                                ) {
                                    caller_bodies.push(CallerBody {
                                        fqn: fqn_from_node(&src_node),
                                        file_path: src_node.file_path.as_str().to_string(),
                                        line_start: src_node.start_line,
                                        line_end: src_node.end_line,
                                        source_code: src_body,
                                        how_it_calls: format!(
                                            "direct call ({} edge to {})",
                                            kind.as_str(),
                                            resolved_fqn
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }

            Ok(MethodBodyResult {
                fqn: resolved_fqn,
                file_path: resolved_file,
                line_start: resolved_start,
                line_end: resolved_end,
                source_code: body,
                surrounding_context: context,
                language,
                caller_bodies,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let body_result = result.map_err(|e| McpError::invalid_params(e, None))?;

        if output_json {
            let json = serde_json::to_string_pretty(&body_result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let (banner, footer) = self
            .access_freshness(
                &req.project_id,
                &rec.directory,
                Some(body_result.file_path.as_str()),
            )
            .await;
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
            for c in ev.method_info.called_by.iter().take(max_callers) {
                let source_code = if include_caller_bodies {
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
                vb_traps: ev.vb_traps,
                sync_hazards: ev.sync_hazards,
                edit_safety: ev.edit_safety,
                business_logic: None,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut ctx = result.map_err(|e| McpError::invalid_params(e, None))?;

        if req.include_business_logic {
            ctx.business_logic = Some(
                self.business_logic_for_method(&req.project_id, &ctx.method_info)
                    .await,
            );
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
        let gen_ = self.get_active_generation(project_id).await.unwrap_or(1);
        let query = engram_index::HybridQuery {
            text: format!("{} {}", info.class_name, info.method_name),
            project_id: project_id.to_string(),
            namespace: "business_logic".to_string(),
            generation: gen_,
            top_k: 5,
            fts_mode: "loose".to_string(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        };
        match ps
            .search
            .search(&query, None, &tokio_util::sync::CancellationToken::new())
            .await
        {
            Ok(hits) if hits.is_empty() => BusinessLogicSection {
                hits: Vec::new(),
                note: "no business-logic analysis stored for this method — run \
                       analyze_business_logic (file mode) to populate the business_logic namespace"
                    .into(),
            },
            Ok(hits) => {
                let n = hits.len();
                BusinessLogicSection {
                    hits: hits
                        .into_iter()
                        .map(|h| BusinessLogicHit {
                            path: h.path.as_str().to_string(),
                            score: h.score as f32,
                            content: h.snippet.unwrap_or_default(),
                        })
                        .collect(),
                    note: format!("{n} stored business-logic document(s) matched (top 5)"),
                }
            }
            Err(e) => BusinessLogicSection {
                hits: Vec::new(),
                note: format!("business-logic search FAILED: {e}"),
            },
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
            let (cb_path, cb_content, language) = {
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
                let re = regex::Regex::new(r#"(?i)MasterPageFile\s*=\s*"([^"]+)""#).ok();
                re.and_then(|r| r.captures(&aspx_content).map(|cap| cap[1].to_string()))
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
            let method_nodes: Vec<Node> = if include_cb {
                match graph.query_nodes(
                    &project_id,
                    Some("function"),
                    None,
                    Some(&cb_path),
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
            Ok(PageContextResult {
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

        if output_json {
            let json = serde_json::to_string_pretty(&ctx)
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
            md.truncate(MAX_PAGE_CONTEXT_BYTES);
            // Snap back to the last newline so we don't cut mid-table-row.
            if let Some(nl) = md.rfind('\n') {
                md.truncate(nl + 1);
            }
            md.push_str(
                "\n\n> ⚠️ **Response truncated** — too many edges to display in full. \
                 Use `output_json: true` or narrow the query to a specific method.\n",
            );
        }

        let (banner, footer) = self
            .access_freshness(&req.project_id, &rec.directory, Some(&req.aspx_file))
            .await;
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
        let target_stack = req.target_stack.clone();
        let include_pattern_examples = req.include_pattern_examples;
        let max_pattern_examples = req.max_pattern_examples;
        let include_db_schema = req.include_db_schema;
        let include_sp_signatures = req.include_sp_signatures;
        let include_state_context = req.include_state_context;
        let include_control_mappings = req.include_control_mappings;
        let output_json = req.output_json;

        // Style profile must run async (it may touch git), so do it outside spawn_blocking
        let style_profile = if req.include_style_profile {
            let result = self
                .cognitive_analyze_file_style(&req.project_id, &req.file_path, 50)
                .await;
            result.style_guide
        } else {
            None
        };

        let result = tokio::task::spawn_blocking(move || {
            // 1. Resolve the target method
            let mut candidates = graph
                .query_nodes(
                    &project_id,
                    Some("function"),
                    Some(&method_name),
                    Some(&file_path),
                    50,
                )
                .unwrap_or_default();

            if let Some(ref cls) = class_name {
                let cls_lower = cls.to_lowercase();
                candidates.retain(|n| n.namespace.to_lowercase().contains(&cls_lower));
            }

            if candidates.is_empty() {
                return Err(method_not_found_message(
                    &graph,
                    &project_id,
                    &method_name,
                    Some(&file_path),
                ));
            }

            let node = &candidates[0];
            let method_info = build_method_info_from_node(node, &graph, &project_id);

            // 2. Read the method body from disk
            let full_path = safe_join(Path::new(&project_dir), &file_path)
                .map_err(|e| format!("Path validation: {e}"))?;
            let method_body = read_lines_from_file(&full_path, node.start_line, node.end_line, 0)
                .ok()
                .map(|(body, _)| body);

            // 3. Pattern examples from callers
            let mut pattern_examples: Vec<PatternExample> = Vec::new();
            if include_pattern_examples {
                let callers = crate::handlers::incoming_caller_edges(
                    &graph,
                    &project_id,
                    &node.node_id,
                    max_pattern_examples * 2,
                );

                for (source_id, kind, _weight) in callers.iter().take(max_pattern_examples) {
                    if let Ok(Some(src_node)) = graph.get_node(&project_id, source_id) {
                        let Ok(src_full) =
                            safe_join(Path::new(&project_dir), src_node.file_path.as_str())
                        else {
                            continue;
                        };
                        if let Ok((src_body, _)) = read_lines_from_file(
                            &src_full,
                            src_node.start_line,
                            src_node.end_line,
                            0,
                        ) {
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
                    }
                }
            }

            // 4. Database schema for referenced tables
            let mut schema_snippets: Vec<TableSchemaSnippet> = Vec::new();
            if include_db_schema && !method_info.db_tables_accessed.is_empty() {
                // Look up db_table nodes in the graph for column information
                for table_name in &method_info.db_tables_accessed {
                    let table_nodes = graph
                        .query_nodes(&project_id, Some("db_table"), Some(table_name), None, 1)
                        .unwrap_or_default();

                    let mut columns = Vec::new();
                    if let Some(tn) = table_nodes.first() {
                        // Find HasColumn edges from this table
                        if let Ok(edges) =
                            graph.list_edges_by_kind(&project_id, EdgeKind::HasColumn, 5000)
                        {
                            for e in &edges {
                                if e.source_id == tn.node_id {
                                    // Single get_node call for both data_type and nullable
                                    let col_node =
                                        graph.get_node(&project_id, &e.target_id).ok().flatten();

                                    let col_type = col_node
                                        .as_ref()
                                        .and_then(|cn| {
                                            cn.metadata
                                                .as_ref()
                                                .and_then(|m| m.get("data_type"))
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string())
                                        })
                                        .unwrap_or_default();

                                    let nullable = col_node
                                        .as_ref()
                                        .and_then(|cn| {
                                            cn.metadata
                                                .as_ref()
                                                .and_then(|m| m.get("nullable"))
                                                .and_then(|v| v.as_bool())
                                        })
                                        .unwrap_or(true);

                                    // Extract column name from target_id
                                    let col_name = e
                                        .target_id
                                        .rsplit('.')
                                        .next()
                                        .unwrap_or(&e.target_id)
                                        .to_string();

                                    columns.push(ColumnSnippet {
                                        name: col_name,
                                        data_type: col_type,
                                        nullable,
                                    });
                                }
                            }
                        }
                    }

                    schema_snippets.push(TableSchemaSnippet {
                        table_name: table_name.clone(),
                        columns,
                    });
                }
            }

            // 5. SP signatures for referenced stored procedures
            let mut sp_signatures: Vec<SpSignatureSnippet> = Vec::new();
            if include_sp_signatures && !method_info.stored_procs_called.is_empty() {
                // Look up graph nodes for SP metadata. The full_project_migration_service
                // stores SP info as graph metadata during indexing.
                for sp_name in &method_info.stored_procs_called {
                    // Check for SQL files containing this SP definition
                    let sp_clean = sp_name
                        .rsplit('.')
                        .next()
                        .unwrap_or(sp_name)
                        .trim_start_matches('[')
                        .trim_end_matches(']');

                    // Try to find the SP in indexed SQL files via graph
                    let sp_nodes = graph
                        .query_nodes(&project_id, Some("function"), Some(sp_clean), None, 5)
                        .unwrap_or_default();

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

                    if let Ok(edges) =
                        graph.list_edges_by_kind(&project_id, EdgeKind::ReadsState, 5000)
                    {
                        for e in &edges {
                            if e.target_id == key && e.source_id != node.node_id {
                                other_readers.push(
                                    e.source_id
                                        .rsplit('\0')
                                        .next()
                                        .unwrap_or(&e.source_id)
                                        .to_string(),
                                );
                            }
                        }
                    }

                    if let Ok(edges) =
                        graph.list_edges_by_kind(&project_id, EdgeKind::WritesState, 5000)
                    {
                        for e in &edges {
                            if e.target_id == key && e.source_id != node.node_id {
                                other_writers.push(
                                    e.source_id
                                        .rsplit('\0')
                                        .next()
                                        .unwrap_or(&e.source_id)
                                        .to_string(),
                                );
                            }
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

            Ok(ImplementationContext {
                method_info,
                method_body,
                style_profile: None, // filled in later from async result
                pattern_examples,
                schema_snippets,
                sp_signatures,
                state_context,
                control_mappings,
                vb_traps,
                language_diagnostics,
                sync_hazards,
            })
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut ctx = result.map_err(|e| McpError::invalid_params(e, None))?;
        ctx.style_profile = style_profile;

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
        req: ValidateGeneratedCodeRequest,
    ) -> Result<CallToolResult, McpError> {
        let _rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let code = req.code.clone();
        let language = req.language.clone();
        let target_file = req.target_file.clone();
        let original_method = req.original_method_name.clone();
        let expected_tables = req.expected_tables.clone();
        let expected_sps = req.expected_sps.clone();
        let expected_session_keys = req.expected_session_keys.clone();
        let expected_control_ids = req.expected_control_ids.clone();
        let output_json = req.output_json;

        let result = tokio::task::spawn_blocking(move || {
            let mut checks: Vec<ValidationCheck> = Vec::new();
            let is_vb = language.starts_with("vb");
            let code_lower = code.to_lowercase();

            // ── Check 1: SQL Table References ─────────────────────────────
            if !expected_tables.is_empty() {
                let mut missing_tables = Vec::new();
                let mut found_tables = Vec::new();
                let mut unknown_tables = Vec::new();

                for table in &expected_tables {
                    if code_lower.contains(&table.to_lowercase()) {
                        found_tables.push(table.clone());
                    } else {
                        missing_tables.push(table.clone());
                    }
                }

                // Detect new table references in the code not in the expected list
                let known_tables: HashSet<String> = {
                    let graph_tables = graph
                        .query_nodes(&project_id, Some("db_table"), None, None, 5000)
                        .unwrap_or_default();
                    graph_tables.iter().map(|n| n.name.to_lowercase()).collect()
                };

                // Table refs (schema-qualifier aware — see
                // referenced_sql_tables).
                for tbl_orig in referenced_sql_tables(&code) {
                    let tbl = tbl_orig.to_lowercase();
                    if !known_tables.contains(&tbl)
                        && !expected_tables.iter().any(|t| t.to_lowercase() == tbl)
                    {
                        unknown_tables.push(tbl_orig);
                    }
                }

                let status = if !missing_tables.is_empty() || !unknown_tables.is_empty() {
                    "warn"
                } else {
                    "pass"
                };

                let mut details = Vec::new();
                if !missing_tables.is_empty() {
                    details.push(format!(
                        "Expected tables not referenced: {}",
                        missing_tables.join(", ")
                    ));
                }
                if !unknown_tables.is_empty() {
                    details.push(format!(
                        "Unknown tables referenced: {}",
                        unknown_tables.join(", ")
                    ));
                }
                if details.is_empty() {
                    details.push(format!("All {} expected tables found", found_tables.len()));
                }

                checks.push(ValidationCheck {
                    category: "sql_tables".to_string(),
                    status: status.to_string(),
                    details,
                });
            }

            // ── Check 2: VB Translation Trap Avoidance ────────────────────
            if is_vb {
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
                    details.push("No VB translation traps detected".to_string());
                } else {
                    details.push(format!(
                        "{} traps detected ({} silent bugs, {} compile errors)",
                        report.total_traps, report.silent_bug_count, report.compile_error_count
                    ));
                    for trap in report.traps.iter().take(5) {
                        details.push(format!(
                            "  {}: {} — {}",
                            trap.trap, trap.risk, trap.guidance
                        ));
                    }
                }

                checks.push(ValidationCheck {
                    category: "vb_traps".to_string(),
                    status: status.to_string(),
                    details,
                });
            }

            // ── Check 3: Session Key Consistency ──────────────────────────
            if !expected_session_keys.is_empty() {
                let mut missing_keys = Vec::new();
                let mut found_keys = Vec::new();

                for key in &expected_session_keys {
                    if code.contains(key) {
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

                checks.push(ValidationCheck {
                    category: "session_keys".to_string(),
                    status: status.to_string(),
                    details,
                });
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

                    if code_lower.contains(&sp_clean.to_lowercase()) {
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

                checks.push(ValidationCheck {
                    category: "stored_procs".to_string(),
                    status: status.to_string(),
                    details,
                });
            }

            // ── Check 5: Control ID Validity ──────────────────────────────
            if !expected_control_ids.is_empty() {
                let mut missing_ids = Vec::new();
                let mut found_ids = Vec::new();

                for id in &expected_control_ids {
                    if code.contains(id) {
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

                checks.push(ValidationCheck {
                    category: "control_ids".to_string(),
                    status: status.to_string(),
                    details,
                });
            }

            // ── Check 6: Caller Compatibility (signature check) ───────────
            if let Some(ref orig_name) = original_method {
                // Verify the generated code preserves the method signature pattern
                let has_method_def = code.contains(orig_name);
                let status = if has_method_def { "pass" } else { "warn" };

                let details = if has_method_def {
                    vec![format!(
                        "Method name '{}' preserved in generated code",
                        orig_name
                    )]
                } else {
                    vec![
                        format!(
                            "Original method name '{}' not found in generated code",
                            orig_name
                        ),
                        "Callers may break if the method signature changed".to_string(),
                    ]
                };

                // Check if the method's callers exist in the graph and the signature is compatible
                if let Some(ref tfile) = target_file {
                    let candidates = graph
                        .query_nodes(
                            &project_id,
                            Some("function"),
                            Some(orig_name),
                            Some(tfile),
                            1,
                        )
                        .unwrap_or_default();

                    if let Some(orig_node) = candidates.first() {
                        let caller_count = crate::handlers::incoming_caller_edges(
                            &graph,
                            &project_id,
                            &orig_node.node_id,
                            100,
                        )
                        .len();

                        if caller_count > 0 {
                            let mut d = details;
                            d.push(format!(
                                "{} callers depend on this method — ensure signature is preserved",
                                caller_count
                            ));
                            checks.push(ValidationCheck {
                                category: "caller_compatibility".to_string(),
                                status: status.to_string(),
                                details: d,
                            });
                        } else {
                            checks.push(ValidationCheck {
                                category: "caller_compatibility".to_string(),
                                status: status.to_string(),
                                details,
                            });
                        }
                    } else {
                        checks.push(ValidationCheck {
                            category: "caller_compatibility".to_string(),
                            status: status.to_string(),
                            details,
                        });
                    }
                } else {
                    checks.push(ValidationCheck {
                        category: "caller_compatibility".to_string(),
                        status: status.to_string(),
                        details,
                    });
                }
            }

            // ── Check 7: Sync Hazard Introduction ─────────────────────────
            {
                let report = engram_index::sync_hazard_detector::detect_sync_hazards(&code, is_vb);

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

                checks.push(ValidationCheck {
                    category: "sync_hazards".to_string(),
                    status: status.to_string(),
                    details,
                });
            }

            // ── Compute overall verdict ───────────────────────────────────
            let has_fail = checks.iter().any(|c| c.status == "fail");
            let has_warn = checks.iter().any(|c| c.status == "warn");
            let overall = if has_fail {
                "FAIL"
            } else if has_warn {
                "WARN"
            } else {
                "PASS"
            };

            Ok(ValidationReport {
                overall_verdict: overall.to_string(),
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
            let sql_lower = sql.to_lowercase();

            // 1. Extract table names referenced in SQL (schema-qualifier
            // aware — see referenced_sql_tables).
            let referenced_tables: Vec<String> = referenced_sql_tables(&sql);

            // 2. Check table existence in the graph
            let known_tables: HashSet<String> = graph
                .query_nodes(&project_id, Some("db_table"), None, None, 5000)
                .unwrap_or_default()
                .iter()
                .map(|n| n.name.to_lowercase())
                .collect();

            for tbl in &referenced_tables {
                if !known_tables.contains(&tbl.to_lowercase()) {
                    issues.push(SqlValidationIssue {
                        severity: "warn".to_string(),
                        category: "unknown_table".to_string(),
                        message: format!(
                            "Table '{}' not found in project schema. It may be a temp table, CTE, or not yet indexed.",
                            tbl
                        ),
                    });
                }
            }

            // 3. Check column references — scoped to tables referenced in this SQL only.
            //    The pattern `table.column` is only checked when the left side matches
            //    a table that appears in FROM/JOIN/etc of this query, not all known tables.
            //    This prevents false positives from C# identifiers like `HttpContext.Request`.
            let referenced_tables_lower: HashSet<String> = referenced_tables
                .iter()
                .map(|t| t.to_lowercase())
                .collect();

            let col_ref_re = regex::Regex::new(r"(?i)\b(\w+)\.(\w+)\b").ok();
            if let Some(re) = col_ref_re {
                // Skip common SQL schema prefixes and aggregate keywords
                let skip_prefixes: HashSet<&str> = [
                    "sys", "dbo", "count", "max", "min", "sum", "avg", "top",
                    "cast", "convert", "isnull", "coalesce", "case", "information_schema",
                ]
                .into_iter()
                .collect();

                for cap in re.captures_iter(&sql) {
                    let tbl_alias = &cap[1];
                    let col_name = &cap[2];
                    let tbl_lower = tbl_alias.to_lowercase();

                    // Only validate if the left side matches a table actually referenced
                    // in this SQL fragment AND it's a known table in the schema
                    if !skip_prefixes.contains(tbl_lower.as_str())
                        && referenced_tables_lower.contains(&tbl_lower)
                        && known_tables.contains(&tbl_lower)
                    {
                        let col_exists = graph
                            .query_nodes(
                                &project_id,
                                Some("db_column"),
                                Some(col_name),
                                None,
                                1,
                            )
                            .unwrap_or_default()
                            .iter()
                            .any(|n| {
                                n.namespace.to_lowercase() == tbl_lower
                                    || n.file_path
                                        .as_str()
                                        .to_lowercase()
                                        .contains(&tbl_lower)
                            });

                        if !col_exists {
                            issues.push(SqlValidationIssue {
                                severity: "info".to_string(),
                                category: "unknown_column".to_string(),
                                message: format!(
                                    "Column '{}.{}' not confirmed in schema (may be alias or not indexed)",
                                    tbl_alias, col_name
                                ),
                            });
                        }
                    }
                }
            }

            // 4. Common SQL anti-patterns
            if sql_lower.contains("select *") {
                issues.push(SqlValidationIssue {
                    severity: "warn".to_string(),
                    category: "anti_pattern".to_string(),
                    message: "SELECT * detected — prefer explicit column lists for maintainability and performance".to_string(),
                });
            }
            if sql_lower.contains("nolock") {
                issues.push(SqlValidationIssue {
                    severity: "info".to_string(),
                    category: "anti_pattern".to_string(),
                    message: "NOLOCK hint detected — may cause dirty reads. Consider READ COMMITTED SNAPSHOT.".to_string(),
                });
            }
            if regex::Regex::new(r"(?i)\bLIKE\s+'%")
                .ok()
                .map(|re| re.is_match(&sql))
                .unwrap_or(false)
            {
                issues.push(SqlValidationIssue {
                    severity: "info".to_string(),
                    category: "anti_pattern".to_string(),
                    message: "Leading wildcard LIKE '%...' detected — cannot use indexes, consider full-text search".to_string(),
                });
            }

            // 5. String concatenation SQL injection risk
            // L-1 fix: broadened patterns to catch single-quoted strings,
            // leading/trailing concat (not just "lit" + var + "lit"), VB-style
            // & operator, C# string interpolation, and String.Format().
            // Previous patterns only matched the symmetric "lit"+var+"lit"
            // form and missed the much more common trailing/leading variants.
            let concat_patterns = [
                // String literal immediately followed by + or & (leading concat)
                r#"["']\s*[\+&]\s*\w"#,
                // + or & immediately followed by a string literal (trailing concat)
                r#"\w\s*[\+&]\s*["']"#,
                // C# string interpolation: $"...{var}..." or $'...{var}...'
                r#"\$\s*["'][^"']*\{[^}]+\}"#,
                // String.Format / string.Format (C#/VB)
                r#"(?i)string\s*\.\s*Format\s*\("#,
            ];
            for pat in &concat_patterns {
                if let Ok(re) = regex::Regex::new(pat)
                    && re.is_match(&sql) {
                        issues.push(SqlValidationIssue {
                            severity: "fail".to_string(),
                            category: "sql_injection".to_string(),
                            message: "Potential SQL injection: string concatenation detected in SQL. Use parameterized queries.".to_string(),
                        });
                        break;
                    }
            }

            let has_fail = issues.iter().any(|i| i.severity == "fail");
            let has_warn = issues.iter().any(|i| i.severity == "warn");
            let verdict = if has_fail {
                "FAIL"
            } else if has_warn {
                "WARN"
            } else if issues.is_empty() {
                "PASS"
            } else {
                "INFO"
            };

            Ok(SqlValidationReport {
                verdict: verdict.to_string(),
                tables_referenced: referenced_tables,
                issues,
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
            // Strategy: query all function nodes matching the name, then check if
            // they're in test files. Also look for test files that reference the method.

            // 1. Find the target method node
            let target_candidates = graph
                .query_nodes(
                    &project_id,
                    Some("function"),
                    Some(&method_name),
                    file_filter.as_deref(),
                    10,
                )
                .unwrap_or_default();

            // 2. Find test files: look for file nodes whose path contains test patterns
            let all_files = graph
                .query_nodes(&project_id, Some("file"), None, None, 10000)
                .unwrap_or_default();

            let test_files: Vec<&Node> = all_files
                .iter()
                .filter(|n| {
                    let fp = n.file_path.as_str().to_lowercase();
                    fp.contains("test") || fp.contains("spec") || fp.contains("_test")
                })
                .collect();

            // 3. For each test file, find function nodes that reference our method
            let mut test_hits: Vec<TestHit> = Vec::new();

            for test_file in &test_files {
                let test_methods = graph
                    .query_nodes(
                        &project_id,
                        Some("function"),
                        None,
                        Some(test_file.file_path.as_str()),
                        500,
                    )
                    .unwrap_or_default();

                for tm in &test_methods {
                    // Check if this test method has a caller edge (Calls or
                    // Dependency) to our target
                    let mut references_target = false;

                    for tc in &target_candidates {
                        let incoming = crate::handlers::incoming_caller_edges(
                            &graph,
                            &project_id,
                            &tc.node_id,
                            500,
                        );
                        if incoming.iter().any(|(src, _, _)| src == &tm.node_id) {
                            references_target = true;
                            break;
                        }
                    }

                    // Also check by name containment in the test method name
                    if !references_target && tm.name.contains(&method_name) {
                        references_target = true;
                    }

                    if references_target {
                        test_hits.push(TestHit {
                            test_name: tm.name.clone(),
                            test_file: tm.file_path.as_str().to_string(),
                            line_start: tm.start_line,
                            line_end: tm.end_line,
                            match_type: if tm.name.contains(&method_name) {
                                "name_match".to_string()
                            } else {
                                "dependency_edge".to_string()
                            },
                        });
                    }
                }
            }

            Ok(TestSearchResult {
                method_name: method_name.clone(),
                test_hits,
                test_files_searched: test_files.len(),
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

        let mut md = format!("# Tests for `{}`\n\n", report.method_name);
        md.push_str(&format!(
            "Searched {} test files.\n\n",
            report.test_files_searched
        ));

        if report.test_hits.is_empty() {
            md.push_str("**No tests found.** Consider writing characterization tests before modifying this method.\n");
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
