//! Migration Coverage Verification Service.
//!
//! Compares what the graph says should exist in a legacy file against what
//! appears in the modern replacement code, identifying gaps in migration.
//!
//! Queries graph edges connected to the original file (event handlers, SQL
//! queries, data bindings, state access, API calls, controls) and checks
//! whether the modern code contains evidence of handling each item.  Reports
//! a per-category breakdown and an overall coverage score (0.0–1.0).

use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

// ─── Output types ─────────────────────────────────────────────────────────────

/// Per-category coverage summary.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CoverageCategory {
    /// Human-readable category label.
    pub name: String,
    /// Total items found in the graph for this category.
    pub total: usize,
    /// Number of items found in the modern code.
    pub covered: usize,
    /// Names of items **not** found in the modern code.
    pub missing_names: Vec<String>,
}

impl CoverageCategory {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }
}

/// A single covered or missing item.
#[derive(Debug, Clone, Serialize)]
pub struct CoverageItem {
    /// Category label (e.g. "event_handler", "sql_table").
    pub category: String,
    /// Canonical name of the item.
    pub name: String,
    /// Human-readable description.
    pub description: String,
}

/// Full migration coverage report for one file.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationCoverageReport {
    /// Original legacy file path.
    pub original_file: String,
    /// Fraction of graph-known elements present in the modern code (0.0–1.0).
    pub coverage_score: f64,
    /// Total number of graph-known items checked.
    pub total_items: usize,
    /// Number of those items found in the modern code.
    pub covered_items: usize,
    /// Number of those items **not** found in the modern code.
    pub missing_items: usize,

    /// Event-handler coverage (function nodes with Contains edge from file).
    pub event_handlers: CoverageCategory,
    /// Data-binding coverage (DataBinding edges).
    pub data_bindings: CoverageCategory,
    /// SQL / table coverage (SqlCalls + QueriesTable edges).
    pub sql_queries: CoverageCategory,
    /// State-access coverage (ReadsState + WritesState edges).
    pub state_access: CoverageCategory,
    /// API-call coverage (ApiCall edges).
    pub api_calls: CoverageCategory,
    /// Control coverage (control nodes whose file_path matches).
    pub controls: CoverageCategory,

    /// Items confirmed present in the modern code.
    pub covered: Vec<CoverageItem>,
    /// Items absent from the modern code.
    pub missing: Vec<CoverageItem>,

    /// Actionable recommendations for missing items.
    pub recommendations: Vec<String>,
    /// High-level assessment sentence.
    pub assessment: String,
}

// ─── Pre-compiled regex ───────────────────────────────────────────────────────

/// Matches a word boundary-aware identifier in modern code (case-insensitive
/// prefix checked on the lowercase copy).
static WORD_RE_CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, Regex>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

// ─── Public API ───────────────────────────────────────────────────────────────

/// Check migration coverage for a single file.
///
/// # Arguments
/// * `graph`         — shared graph store
/// * `project_id`    — project identifier used as the graph namespace key
/// * `original_file` — path of the legacy file (as stored in the graph)
/// * `modern_code`   — full text of the modern replacement file/component
///
/// # Returns
/// A [`MigrationCoverageReport`] with per-category breakdowns, covered/missing
/// item lists, recommendations, and an overall coverage score.
pub fn check_migration_coverage(
    graph: &Arc<GraphStore>,
    project_id: &str,
    original_file: &str,
    modern_code: &str,
) -> anyhow::Result<MigrationCoverageReport> {
    let modern_lower = modern_code.to_lowercase();

    // ── 1. Collect items from the graph ──────────────────────────────────────

    let event_handler_names = collect_event_handlers(graph, project_id, original_file)?;
    let sql_table_names = collect_sql_tables(graph, project_id, original_file)?;
    let binding_names = collect_data_bindings(graph, project_id, original_file)?;
    let state_key_names = collect_state_keys(graph, project_id, original_file)?;
    let api_endpoint_names = collect_api_calls(graph, project_id, original_file)?;
    let control_names = collect_controls(graph, project_id, original_file)?;

    // ── 2. Check each item against the modern code ────────────────────────────

    let mut event_handlers = CoverageCategory::new("Event Handlers");
    let mut data_bindings = CoverageCategory::new("Data Bindings");
    let mut sql_queries = CoverageCategory::new("SQL Tables / Queries");
    let mut state_access = CoverageCategory::new("State Access");
    let mut api_calls = CoverageCategory::new("API Calls");
    let mut controls = CoverageCategory::new("Controls");

    let mut covered_items: Vec<CoverageItem> = Vec::new();
    let mut missing_items: Vec<CoverageItem> = Vec::new();

    // Event handlers
    for name in &event_handler_names {
        event_handlers.total += 1;
        if name_present_in_modern(name, &modern_lower) {
            event_handlers.covered += 1;
            covered_items.push(CoverageItem {
                category: "event_handler".into(),
                name: name.clone(),
                description: format!("Handler '{name}' found in modern code"),
            });
        } else {
            event_handlers.missing_names.push(name.clone());
            missing_items.push(CoverageItem {
                category: "event_handler".into(),
                name: name.clone(),
                description: format!("Handler '{name}' not found in modern code"),
            });
        }
    }

    // SQL tables
    for name in &sql_table_names {
        sql_queries.total += 1;
        if name_present_in_modern(name, &modern_lower) {
            sql_queries.covered += 1;
            covered_items.push(CoverageItem {
                category: "sql_table".into(),
                name: name.clone(),
                description: format!("Table/query '{name}' referenced in modern code"),
            });
        } else {
            sql_queries.missing_names.push(name.clone());
            missing_items.push(CoverageItem {
                category: "sql_table".into(),
                name: name.clone(),
                description: format!("Table/query '{name}' not referenced in modern code"),
            });
        }
    }

    // Data bindings
    for name in &binding_names {
        data_bindings.total += 1;
        if name_present_in_modern(name, &modern_lower) {
            data_bindings.covered += 1;
            covered_items.push(CoverageItem {
                category: "data_binding".into(),
                name: name.clone(),
                description: format!("Bound field '{name}' found in modern code"),
            });
        } else {
            data_bindings.missing_names.push(name.clone());
            missing_items.push(CoverageItem {
                category: "data_binding".into(),
                name: name.clone(),
                description: format!("Bound field '{name}' not found in modern code"),
            });
        }
    }

    // State keys
    for name in &state_key_names {
        state_access.total += 1;
        if name_present_in_modern(name, &modern_lower) {
            state_access.covered += 1;
            covered_items.push(CoverageItem {
                category: "state_key".into(),
                name: name.clone(),
                description: format!("State key '{name}' referenced in modern code"),
            });
        } else {
            state_access.missing_names.push(name.clone());
            missing_items.push(CoverageItem {
                category: "state_key".into(),
                name: name.clone(),
                description: format!("State key '{name}' not referenced in modern code"),
            });
        }
    }

    // API calls
    for name in &api_endpoint_names {
        api_calls.total += 1;
        if name_present_in_modern(name, &modern_lower) {
            api_calls.covered += 1;
            covered_items.push(CoverageItem {
                category: "api_call".into(),
                name: name.clone(),
                description: format!("API endpoint '{name}' found in modern code"),
            });
        } else {
            api_calls.missing_names.push(name.clone());
            missing_items.push(CoverageItem {
                category: "api_call".into(),
                name: name.clone(),
                description: format!("API endpoint '{name}' not found in modern code"),
            });
        }
    }

    // Controls
    for name in &control_names {
        controls.total += 1;
        if name_present_in_modern(name, &modern_lower) {
            controls.covered += 1;
            covered_items.push(CoverageItem {
                category: "control".into(),
                name: name.clone(),
                description: format!("Control '{name}' mapped in modern code"),
            });
        } else {
            controls.missing_names.push(name.clone());
            missing_items.push(CoverageItem {
                category: "control".into(),
                name: name.clone(),
                description: format!("Control '{name}' not mapped in modern code"),
            });
        }
    }

    // ── 3. Totals + score ─────────────────────────────────────────────────────

    let total_items = event_handlers.total
        + sql_queries.total
        + data_bindings.total
        + state_access.total
        + api_calls.total
        + controls.total;

    let covered_count = event_handlers.covered
        + sql_queries.covered
        + data_bindings.covered
        + state_access.covered
        + api_calls.covered
        + controls.covered;

    let missing_count = total_items - covered_count;

    let coverage_score = if total_items == 0 {
        1.0 // nothing to migrate → trivially complete
    } else {
        covered_count as f64 / total_items as f64
    };

    // ── 4. Recommendations ────────────────────────────────────────────────────

    let mut recommendations: Vec<String> = Vec::new();

    for name in &event_handlers.missing_names {
        recommendations.push(format!(
            "Missing handler: {name} — add {} functionality",
            handler_action_hint(name)
        ));
    }
    for name in &sql_queries.missing_names {
        recommendations.push(format!(
            "Missing table reference: {name} — add data access for {name} table"
        ));
    }
    for name in &data_bindings.missing_names {
        recommendations.push(format!(
            "Missing data binding: {name} — bind '{name}' field in the modern component"
        ));
    }
    for name in &state_access.missing_names {
        recommendations.push(format!(
            "Missing state key: {name} — add {name} state management (useState / store)"
        ));
    }
    for name in &api_calls.missing_names {
        recommendations.push(format!(
            "Missing API call: {name} — add HTTP client call to '{name}' endpoint"
        ));
    }
    for name in &controls.missing_names {
        recommendations.push(format!(
            "Missing control: {name} — add equivalent UI element for '{name}'"
        ));
    }

    // ── 5. Assessment ─────────────────────────────────────────────────────────

    let assessment = match coverage_score {
        s if s >= 1.0 => {
            "Complete: all graph-known elements are represented in modern code".to_string()
        }
        s if s >= 0.8 => {
            format!(
                "Near-complete: minor gaps may need attention ({missing_count} item(s) missing)"
            )
        }
        s if s >= 0.5 => format!(
            "Partial: significant functionality not yet migrated \
             ({missing_count} of {total_items} item(s) missing)"
        ),
        _ => format!(
            "Incomplete: majority of functionality still needs migration \
             ({missing_count} of {total_items} item(s) missing)"
        ),
    };

    Ok(MigrationCoverageReport {
        original_file: original_file.to_string(),
        coverage_score,
        total_items,
        covered_items: covered_count,
        missing_items: missing_count,
        event_handlers,
        data_bindings,
        sql_queries,
        state_access,
        api_calls,
        controls,
        covered: covered_items,
        missing: missing_items,
        recommendations,
        assessment,
    })
}

/// Render the report as a Markdown document.
pub fn format_coverage_report(report: &MigrationCoverageReport) -> String {
    let mut out = String::with_capacity(2048);

    out.push_str("# Migration Coverage Report\n\n");
    out.push_str(&format!("**File:** `{}`\n\n", report.original_file));
    out.push_str(&format!(
        "**Overall Coverage:** {:.1}%  ({}/{} items)\n\n",
        report.coverage_score * 100.0,
        report.covered_items,
        report.total_items
    ));
    out.push_str(&format!("**Assessment:** {}\n\n", report.assessment));

    // Per-category table
    out.push_str("## Coverage by Category\n\n");
    out.push_str("| Category | Total | Covered | Missing |\n");
    out.push_str("|----------|-------|---------|--------|\n");

    for cat in [
        &report.event_handlers,
        &report.data_bindings,
        &report.sql_queries,
        &report.state_access,
        &report.api_calls,
        &report.controls,
    ] {
        let missing = cat.total.saturating_sub(cat.covered);
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            cat.name, cat.total, cat.covered, missing
        ));
    }
    out.push('\n');

    // Missing items + recommendations
    if !report.missing.is_empty() {
        out.push_str("## Missing Items\n\n");
        for item in &report.missing {
            out.push_str(&format!(
                "- **[{}]** `{}` — {}\n",
                item.category, item.name, item.description
            ));
        }
        out.push('\n');
    }

    if !report.recommendations.is_empty() {
        out.push_str("## Recommendations\n\n");
        for rec in &report.recommendations {
            out.push_str(&format!("- {rec}\n"));
        }
        out.push('\n');
    }

    // Covered items summary
    if !report.covered.is_empty() {
        out.push_str("## Covered Items\n\n");
        for item in &report.covered {
            out.push_str(&format!("- [x] **[{}]** `{}`\n", item.category, item.name));
        }
        out.push('\n');
    }

    out
}

// ─── Graph collectors ─────────────────────────────────────────────────────────

/// Collect function names for a file by walking Contains edges and also by
/// querying function nodes whose `file_path` matches.
fn collect_event_handlers(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<Vec<String>> {
    let file_node_id = format!("file:{file_path}");

    // Event handlers via Contains outgoing edges
    let contains_neighbors = graph.neighbors(project_id, EdgeKind::Contains, &file_node_id, 500)?;
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for (node_id, _weight) in &contains_neighbors {
        if let Some(node) = graph.get_node(project_id, node_id)? {
            if node.node_type == "function" && !node.name.is_empty() {
                if seen.insert(node.name.clone()) {
                    names.push(node.name.clone());
                }
            }
        }
    }

    // Also collect function nodes directly associated with the file
    let fn_nodes = graph.query_nodes(project_id, Some("function"), None, Some(file_path), 2000)?;
    for node in fn_nodes {
        if !node.name.is_empty() && seen.insert(node.name.clone()) {
            names.push(node.name.clone());
        }
    }

    Ok(names)
}

/// Collect SQL table / stored-procedure names from SqlCalls and QueriesTable edges.
fn collect_sql_tables(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for kind in [EdgeKind::SqlCalls, EdgeKind::QueriesTable] {
        let edges = graph.list_edges_by_kind(project_id, kind, 5000)?;
        for edge in edges {
            if !edge.source_id.contains(file_path) {
                // Also check metadata file_path
                let meta_match = edge
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("file_path"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|fp| fp == file_path);
                if !meta_match {
                    continue;
                }
            }
            let table = extract_name_from_target(&edge.target_id);
            if !table.is_empty() && seen.insert(table.clone()) {
                names.push(table);
            }
        }
    }

    Ok(names)
}

/// Collect bound-field names from DataBinding edges.
fn collect_data_bindings(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let edges = graph.list_edges_by_kind(project_id, EdgeKind::DataBinding, 5000)?;
    for edge in edges {
        let matches = edge.source_id.contains(file_path)
            || edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("file_path"))
                .and_then(|v| v.as_str())
                .is_some_and(|fp| fp == file_path);
        if !matches {
            continue;
        }
        let field = extract_name_from_target(&edge.target_id);
        if !field.is_empty() && seen.insert(field.clone()) {
            names.push(field);
        }
    }

    Ok(names)
}

/// Collect state-key names from ReadsState and WritesState edges.
fn collect_state_keys(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for kind in [EdgeKind::ReadsState, EdgeKind::WritesState] {
        let edges = graph.list_edges_by_kind(project_id, kind, 5000)?;
        for edge in edges {
            let matches = edge.source_id.contains(file_path)
                || edge
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("file_path"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|fp| fp == file_path);
            if !matches {
                continue;
            }
            let key = extract_state_key(&edge.target_id);
            if !key.is_empty() && seen.insert(key.clone()) {
                names.push(key);
            }
        }
    }

    Ok(names)
}

/// Collect API endpoint names from ApiCall edges.
fn collect_api_calls(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let edges = graph.list_edges_by_kind(project_id, EdgeKind::ApiCall, 5000)?;
    for edge in edges {
        let matches = edge.source_id.contains(file_path)
            || edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("file_path"))
                .and_then(|v| v.as_str())
                .is_some_and(|fp| fp == file_path);
        if !matches {
            continue;
        }
        let endpoint = extract_name_from_target(&edge.target_id);
        if !endpoint.is_empty() && seen.insert(endpoint.clone()) {
            names.push(endpoint);
        }
    }

    Ok(names)
}

/// Collect control IDs for a file from control nodes and RegistersControl edges.
fn collect_controls(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_path: &str,
) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Control nodes whose file_path matches
    let control_nodes =
        graph.query_nodes(project_id, Some("control"), None, Some(file_path), 2000)?;
    for node in control_nodes {
        if !node.name.is_empty() && seen.insert(node.name.clone()) {
            names.push(node.name.clone());
        }
    }

    // RegistersControl edges from the file
    let edges = graph.list_edges_by_kind(project_id, EdgeKind::RegistersControl, 5000)?;
    for edge in edges {
        let matches = edge.source_id.contains(file_path)
            || edge
                .metadata
                .as_ref()
                .and_then(|m| m.get("file_path"))
                .and_then(|v| v.as_str())
                .is_some_and(|fp| fp == file_path);
        if !matches {
            continue;
        }
        let ctrl = extract_name_from_target(&edge.target_id);
        if !ctrl.is_empty() && seen.insert(ctrl.clone()) {
            names.push(ctrl);
        }
    }

    Ok(names)
}

// ─── Matching helpers ─────────────────────────────────────────────────────────

/// Returns `true` if the item's name (or any flexible variant) appears in the
/// lowercased modern code.
///
/// Matching strategy (tried in order):
/// 1. Exact word-boundary match of the original name (case-insensitive)
/// 2. camelCase conversion
/// 3. PascalCase conversion
/// 4. Strip common WebForms control prefixes (btn, txt, gv, ddl, lbl, rpt, …)
/// 5. "Core" word extracted from the name (e.g. "btnSearch" → "search")
fn name_present_in_modern(name: &str, modern_lower: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    let candidates = build_name_candidates(name);
    for candidate in &candidates {
        if candidate.is_empty() {
            continue;
        }
        if word_match(candidate, modern_lower) {
            return true;
        }
    }
    false
}

/// Build all name variants for matching.
fn build_name_candidates(name: &str) -> Vec<String> {
    let mut cands: Vec<String> = Vec::with_capacity(8);

    // 1. Original
    cands.push(name.to_lowercase());

    // 2. camelCase
    cands.push(to_camel_case(name).to_lowercase());

    // 3. PascalCase
    cands.push(to_pascal_case(name).to_lowercase());

    // 4. Strip known WebForms prefixes, add the stripped version
    let stripped = strip_control_prefix(name);
    if stripped != name {
        cands.push(stripped.to_lowercase());
        cands.push(to_camel_case(&stripped).to_lowercase());
        cands.push(to_pascal_case(&stripped).to_lowercase());
    }

    // 5. Strip _Click / _Command / _Changed suffix from event handler names
    let base = strip_event_suffix(name);
    if base != name {
        cands.push(base.to_lowercase());
        let stripped_base = strip_control_prefix(&base);
        cands.push(stripped_base.to_lowercase());
        cands.push(to_camel_case(&stripped_base).to_lowercase());
        cands.push(to_pascal_case(&stripped_base).to_lowercase());
    }

    // 6. State key: strip "state:" / "session:" / "viewstate:" / "application:" prefix
    let state_stripped = strip_state_prefix(name);
    if state_stripped != name {
        cands.push(state_stripped.to_lowercase());
        cands.push(to_camel_case(&state_stripped).to_lowercase());
    }

    // 7. Graph-style prefixes: strip "table:", "ctrl:", "api:", etc.
    let graph_stripped = strip_graph_prefix(name);
    if graph_stripped != name {
        cands.push(graph_stripped.to_lowercase());
        cands.push(to_camel_case(&graph_stripped).to_lowercase());
    }

    // Deduplicate while preserving order
    let mut seen: HashSet<String> = HashSet::with_capacity(cands.len());
    cands.retain(|c| !c.is_empty() && seen.insert(c.clone()));
    cands
}

/// Case-insensitive word-boundary search.  Uses a pre-compiled regex per
/// unique pattern to avoid recompiling on every call.
fn word_match(needle: &str, haystack: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    // Fast path: plain substring check first (avoids regex overhead when the
    // needle contains non-word characters like "/" in API paths)
    if !haystack.contains(needle) {
        return false;
    }
    // Verify with word boundary regex if possible, but only for pure
    // identifier-like names (no slashes, dots, etc.)
    if needle.chars().all(|c| c.is_alphanumeric() || c == '_') {
        let pattern = format!(r"(?i)\b{}\b", regex::escape(needle));
        let regex = {
            let mut cache = WORD_RE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if !cache.contains_key(needle) {
                if let Ok(re) = Regex::new(&pattern) {
                    cache.insert(needle.to_string(), re);
                } else {
                    // Fallback: substring was already confirmed above
                    return true;
                }
            }
            cache.get(needle).map(|re| re.is_match(haystack))
        };
        return regex.unwrap_or(true);
    }
    // Non-identifier path (e.g. URL segment): substring match already confirmed
    true
}

// ─── Name extraction helpers ──────────────────────────────────────────────────

/// Extract a usable name from a graph target_id.
///
/// Handles formats like:
/// - `"table:Orders"`        → `"Orders"`
/// - `"state:SortColumn"`    → `"SortColumn"`
/// - `"api:/api/customers"`  → `"/api/customers"`
/// - `"ctrl:gvResults"`      → `"gvResults"`
/// - `"Orders"`              → `"Orders"`
fn extract_name_from_target(target_id: &str) -> String {
    // Known prefix:value patterns
    for prefix in &[
        "table:", "ctrl:", "control:", "api:", "field:", "col:", "column:", "binding:", "proc:",
        "sp:",
    ] {
        if let Some(rest) = target_id.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    // Generic: last segment after ':' if there is one
    if let Some(pos) = target_id.rfind(':') {
        let rest = target_id[pos + 1..].trim();
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    target_id.trim().to_string()
}

/// Extract the bare key from a state target_id.
///
/// Handles `"state:Session[SortColumn]"`, `"state:ViewState[Foo]"`, and
/// plain `"SortColumn"`.
fn extract_state_key(target_id: &str) -> String {
    let s = target_id.strip_prefix("state:").unwrap_or(target_id).trim();

    // Strip store prefix (e.g. "Session[", "ViewState[")
    for prefix in &["session[", "viewstate[", "application[", "cache["] {
        if s.to_lowercase().starts_with(prefix) {
            let rest = &s[prefix.len()..];
            return rest
                .trim_end_matches(|c: char| c == ']' || c == '"' || c == '\'')
                .to_string();
        }
    }
    // Try "Session:" / colon style
    for prefix in &["session:", "viewstate:", "application:", "cache:"] {
        if s.to_lowercase().starts_with(prefix) {
            return s[prefix.len()..]
                .trim_matches(|c: char| c == '"' || c == '\'')
                .to_string();
        }
    }
    s.to_string()
}

/// Strip common WebForms control-type prefixes.
///
/// Examples: `"btnDelete"` → `"Delete"`, `"txtEmail"` → `"Email"`,
/// `"gvResults"` → `"Results"`, `"ddlCountry"` → `"Country"`.
fn strip_control_prefix(name: &str) -> String {
    const PREFIXES: &[&str] = &[
        "btn", "txt", "gv", "ddl", "lbl", "rpt", "chk", "rb", "rbl", "cb", "img", "hyp", "lit",
        "ph", "pnl", "fv", "dv", "wiz", "ml", "lb", "tb", "hf", "cal",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().next().is_some_and(|c| c.is_uppercase()) {
                return rest.to_string();
            }
        }
    }
    name.to_string()
}

/// Strip trailing event-handler suffixes (`_Click`, `_Command`, etc.).
fn strip_event_suffix(name: &str) -> String {
    const SUFFIXES: &[&str] = &[
        "_Click",
        "_Command",
        "_SelectedIndexChanged",
        "_Changed",
        "_CheckedChanged",
        "_RowCommand",
        "_RowEditing",
        "_RowDeleting",
        "_RowUpdating",
        "_RowCancelingEdit",
        "_PageIndexChanging",
        "_Sorting",
        "_ItemCommand",
        "_ItemDataBound",
        "_Load",
        "_Init",
        "_PreRender",
    ];
    for suffix in SUFFIXES {
        if let Some(base) = name.strip_suffix(suffix) {
            return base.to_string();
        }
    }
    name.to_string()
}

/// Strip state-store prefix for cleaner key extraction.
/// Handles `state:Session[SortColumn]` → `SortColumn` and `session:Theme` → `Theme`.
fn strip_state_prefix(name: &str) -> String {
    for prefix in &[
        "state:",
        "session:",
        "viewstate:",
        "application:",
        "cache:",
        "cookie:",
        "querystring:",
    ] {
        if name.to_lowercase().starts_with(prefix) {
            let remainder = &name[prefix.len()..];
            let remainder = remainder.trim_matches(|c: char| c == '"' || c == '\'');
            // Handle Store[Key] pattern: extract just the key inside brackets
            if let Some(bracket_start) = remainder.find('[') {
                if let Some(bracket_end) = remainder.find(']') {
                    if bracket_end > bracket_start + 1 {
                        return remainder[bracket_start + 1..bracket_end].to_string();
                    }
                }
            }
            // Fallback: strip brackets from ends
            return remainder
                .trim_matches(|c: char| c == '[' || c == ']')
                .to_string();
        }
    }
    name.to_string()
}

/// Strip graph-style prefixes (`table:`, `ctrl:`, etc.) for name matching.
fn strip_graph_prefix(name: &str) -> String {
    for prefix in &[
        "table:", "ctrl:", "control:", "api:", "field:", "col:", "column:", "binding:", "proc:",
        "sp:",
    ] {
        if name.to_lowercase().starts_with(prefix) {
            return name[prefix.len()..].trim().to_string();
        }
    }
    name.to_string()
}

// ─── Case-conversion helpers ──────────────────────────────────────────────────

fn to_camel_case(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let mut result = String::new();
    let mut capitalize_next = false;
    for (i, c) in clean.chars().enumerate() {
        if c == '_' {
            capitalize_next = true;
            continue;
        }
        if i == 0 {
            result.push(c.to_lowercase().next().unwrap_or(c));
        } else if capitalize_next {
            result.push(c.to_uppercase().next().unwrap_or(c));
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn to_pascal_case(s: &str) -> String {
    let camel = to_camel_case(s);
    let mut chars = camel.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

// ─── Recommendation hint ──────────────────────────────────────────────────────

/// Produce a concise action hint from an event-handler name.
fn handler_action_hint(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("delete") || lower.contains("remove") {
        "delete".to_string()
    } else if lower.contains("save") || lower.contains("update") || lower.contains("edit") {
        "save/update".to_string()
    } else if lower.contains("search") || lower.contains("filter") || lower.contains("find") {
        "search/filter".to_string()
    } else if lower.contains("load") || lower.contains("init") || lower.contains("page_load") {
        "data loading/initialization".to_string()
    } else if lower.contains("submit") || lower.contains("confirm") {
        "submit/confirm".to_string()
    } else if lower.contains("cancel") {
        "cancel".to_string()
    } else if lower.contains("click") {
        "button action".to_string()
    } else if lower.contains("change") || lower.contains("select") {
        "selection change".to_string()
    } else {
        // Use the stripped base name as a hint
        let stripped = strip_event_suffix(&strip_control_prefix(name));
        if stripped != name && !stripped.is_empty() {
            stripped.to_lowercase()
        } else {
            "functionality".to_string()
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Name candidate generation ─────────────────────────────────────────────

    #[test]
    fn build_candidates_includes_prefix_stripped_variants() {
        let cands = build_name_candidates("btnSearch");
        assert!(
            cands.contains(&"btnsearch".to_string()) || cands.contains(&"search".to_string()),
            "expected stripped candidate: {cands:?}"
        );
        assert!(
            cands.contains(&"search".to_string()),
            "expected 'search' from btnSearch: {cands:?}"
        );
    }

    #[test]
    fn build_candidates_includes_event_suffix_stripped() {
        let cands = build_name_candidates("btnDelete_Click");
        // Should contain "btnDelete_Click" + stripped variants
        assert!(
            cands.contains(&"delete".to_string()),
            "expected 'delete' candidate: {cands:?}"
        );
    }

    #[test]
    fn build_candidates_includes_case_variants() {
        let cands = build_name_candidates("SortColumn");
        assert!(cands.contains(&"sortcolumn".to_string()));
        // camel should be "sortColumn" lowercased
        assert!(cands.contains(&"sortcolumn".to_string()));
    }

    // ── extract helpers ───────────────────────────────────────────────────────

    #[test]
    fn extract_name_from_table_prefix() {
        assert_eq!(extract_name_from_target("table:Orders"), "Orders");
        assert_eq!(extract_name_from_target("table:Customers"), "Customers");
        assert_eq!(extract_name_from_target("ctrl:gvResults"), "gvResults");
    }

    #[test]
    fn extract_state_key_variants() {
        assert_eq!(extract_state_key("state:Session[SortColumn]"), "SortColumn");
        assert_eq!(extract_state_key("state:ViewState[EditMode]"), "EditMode");
        assert_eq!(
            extract_state_key("state:Application[SiteConfig]"),
            "SiteConfig"
        );
        assert_eq!(extract_state_key("SortColumn"), "SortColumn");
    }

    #[test]
    fn strip_control_prefix_removes_known_prefixes() {
        assert_eq!(strip_control_prefix("btnDelete"), "Delete");
        assert_eq!(strip_control_prefix("txtEmail"), "Email");
        assert_eq!(strip_control_prefix("gvResults"), "Results");
        assert_eq!(strip_control_prefix("ddlCountry"), "Country");
        assert_eq!(strip_control_prefix("lblStatus"), "Status");
        // Should not strip when remainder doesn't start with uppercase
        assert_eq!(strip_control_prefix("button"), "button");
    }

    // ── word_match ────────────────────────────────────────────────────────────

    #[test]
    fn word_match_finds_exact_word() {
        assert!(word_match("orders", "var orders = await repo.GetOrders();"));
        assert!(!word_match("order", "var orders = await repo.GetOrders();"));
    }

    // ── name_present_in_modern ────────────────────────────────────────────────

    #[test]
    fn name_present_flexible_prefix_removal() {
        let modern = "async handleSearch() { return await api.search(query); }";
        let modern_lower = modern.to_lowercase();
        // "btnSearch" → stripped to "Search" → lowercased "search" → found
        assert!(
            name_present_in_modern("btnSearch", &modern_lower),
            "btnSearch should match 'search' in modern code"
        );
    }

    #[test]
    fn name_present_state_key_matching() {
        let modern = "const [sortColumn, setSortColumn] = useState('Name');";
        let modern_lower = modern.to_lowercase();
        assert!(
            name_present_in_modern("state:Session[SortColumn]", &modern_lower),
            "state key SortColumn should be found"
        );
    }

    #[test]
    fn name_present_sql_table() {
        let modern = "await db.Customers.Where(c => c.IsActive).ToListAsync();";
        let modern_lower = modern.to_lowercase();
        assert!(name_present_in_modern("table:Customers", &modern_lower));
    }

    // ── Coverage report construction (no real graph — pure logic tests) ───────

    /// Build a minimal report by directly exercising the assessment / scoring.
    fn make_report(total: usize, covered: usize) -> MigrationCoverageReport {
        let score = if total == 0 {
            1.0
        } else {
            covered as f64 / total as f64
        };
        let missing = total - covered;
        let assessment = match score {
            s if s >= 1.0 => {
                "Complete: all graph-known elements are represented in modern code".to_string()
            }
            s if s >= 0.8 => {
                format!("Near-complete: minor gaps may need attention ({missing} item(s) missing)")
            }
            s if s >= 0.5 => format!(
                "Partial: significant functionality not yet migrated ({missing} of {total} item(s) missing)"
            ),
            _ => format!(
                "Incomplete: majority of functionality still needs migration ({missing} of {total} item(s) missing)"
            ),
        };
        MigrationCoverageReport {
            original_file: "Legacy.aspx.vb".into(),
            coverage_score: score,
            total_items: total,
            covered_items: covered,
            missing_items: missing,
            event_handlers: CoverageCategory::new("Event Handlers"),
            data_bindings: CoverageCategory::new("Data Bindings"),
            sql_queries: CoverageCategory::new("SQL Tables / Queries"),
            state_access: CoverageCategory::new("State Access"),
            api_calls: CoverageCategory::new("API Calls"),
            controls: CoverageCategory::new("Controls"),
            covered: Vec::new(),
            missing: Vec::new(),
            recommendations: Vec::new(),
            assessment,
        }
    }

    #[test]
    fn full_coverage_score_is_one() {
        let r = make_report(5, 5);
        assert!((r.coverage_score - 1.0).abs() < f64::EPSILON);
        assert!(r.assessment.contains("Complete"));
    }

    #[test]
    fn zero_coverage_empty_modern_code() {
        let r = make_report(4, 0);
        assert!((r.coverage_score - 0.0).abs() < f64::EPSILON);
        assert!(r.assessment.contains("Incomplete"));
    }

    #[test]
    fn partial_coverage_assessment() {
        let r = make_report(10, 6);
        assert!(r.coverage_score >= 0.5 && r.coverage_score < 0.8);
        assert!(r.assessment.contains("Partial"));
    }

    #[test]
    fn near_complete_coverage_assessment() {
        let r = make_report(10, 9);
        assert!(r.coverage_score >= 0.8 && r.coverage_score < 1.0);
        assert!(r.assessment.contains("Near-complete"));
    }

    // ── format_coverage_report ────────────────────────────────────────────────

    #[test]
    fn format_report_contains_key_sections() {
        let mut r = make_report(3, 2);
        r.covered.push(CoverageItem {
            category: "event_handler".into(),
            name: "btnSave_Click".into(),
            description: "found".into(),
        });
        r.missing.push(CoverageItem {
            category: "sql_table".into(),
            name: "Orders".into(),
            description: "not found".into(),
        });
        r.recommendations
            .push("Missing table reference: Orders".into());

        let md = format_coverage_report(&r);
        assert!(md.contains("# Migration Coverage Report"));
        assert!(
            md.contains("66.7%") || md.contains("66."),
            "score line: {md}"
        );
        assert!(md.contains("Missing Items"));
        assert!(md.contains("Recommendations"));
        assert!(md.contains("Covered Items"));
        assert!(md.contains("Orders"));
        assert!(md.contains("btnSave_Click"));
    }

    // ── handler_action_hint ───────────────────────────────────────────────────

    #[test]
    fn handler_action_hint_returns_useful_label() {
        assert_eq!(handler_action_hint("btnDelete_Click"), "delete");
        assert_eq!(handler_action_hint("btnSave_Click"), "save/update");
        assert_eq!(handler_action_hint("btnSearch_Click"), "search/filter");
        assert_eq!(
            handler_action_hint("Page_Load"),
            "data loading/initialization"
        );
        assert_eq!(handler_action_hint("btnCancel_Click"), "cancel");
    }
}
