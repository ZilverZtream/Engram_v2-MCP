//! Extracted analyzer: methods.
//!
//! Part of the Phase 2 refactor that split the 13k-line
//! `full_project_migration_service.rs` into focused submodules.
//! No behaviour was changed during the move; every function lives
//! here exactly as before, just under a narrower module boundary.

#![allow(unused_imports)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use engram_graph::{EdgeKind, GraphStore};
use regex::Regex;

use super::super::model::*;
// Wildcard catches parent-module `pub(super) static` / `type` /
// `pub(crate) fn` helpers that were left in the grandparent during
// the Phase 2 extraction.
use super::super::super::auth_config_service::AuthConfigMap;
use super::super::super::db_strategy_service::{self, FileDataAccessProfile};
use super::super::super::dossier_service::{self, MigrationDossier};
use super::super::super::migration_order_service::{self, MigrationOrderPlan};
use super::super::super::pattern_detection_service;
use super::super::super::state_migration_service::{self, StateMigrationReport};
use super::super::*;

/// Public wrapper for classify_method_kind, used by access_layer_tools.
pub fn classify_method_kind_pub(
    name: &str,
    effects: &[String],
    metadata: &Option<serde_json::Value>,
) -> MethodKind {
    classify_method_kind(name, effects, metadata)
}

pub(crate) fn classify_method_kind(
    name: &str,
    effects: &[String],
    metadata: &Option<serde_json::Value>,
) -> MethodKind {
    static LIFECYCLE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)^(?:Page_(?:Load|Init|PreRender|Unload|PreInit|InitComplete|LoadComplete|PreRenderComplete|SaveStateComplete|Error)|OnInit|OnLoad|OnPreRender|OnUnload)$").expect("valid regex")
    });
    static CONTROL_EVENT_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)_(?:Click|Command|RowCommand|SelectedIndexChanged|TextChanged|CheckedChanged|DataBound|RowEditing|RowUpdating|RowDeleting|RowCancelingEdit|PageIndexChanging|Sorting|ItemCommand|ItemDataBound|DataBinding|ServerClick|ServerChange|NeedDataSource|ItemCreated|Init|Load|PreRender|Unload)$").expect("valid regex")
    });

    if LIFECYCLE_RE.is_match(name) {
        return MethodKind::Lifecycle;
    }
    if CONTROL_EVENT_RE.is_match(name) {
        return MethodKind::ControlEvent;
    }

    // Check for WebMethod attribute in metadata
    if let Some(meta) = metadata {
        if let Some(sig) = meta.get("signature").and_then(|v| v.as_str())
            && sig.contains("WebMethod")
        {
            return MethodKind::WebMethod;
        }
        if let Some(eff) = meta.get("effects").and_then(|v| v.as_str())
            && eff.contains("WebMethod")
        {
            return MethodKind::WebMethod;
        }
    }

    if effects.iter().any(|e| e.contains("SQL_Access")) {
        return MethodKind::DataAccess;
    }

    MethodKind::Helper
}

pub(crate) fn build_method_inventories(
    graph: &Arc<GraphStore>,
    project_id: &str,
    file_contents: &[FileContent],
) -> BTreeMap<String, PageMethodInventory> {
    let mut result = BTreeMap::new();

    for fc in file_contents {
        let cb_path = fc.file_path.clone() + ".vb";
        let cb_path_cs = fc.file_path.clone() + ".cs";

        // Try both VB and CS code-behind paths
        for codebehind_path in &[&cb_path, &cb_path_cs] {
            let method_nodes = match graph.query_nodes(
                project_id,
                Some("function"),
                None,
                Some(codebehind_path),
                500,
            ) {
                Ok(nodes) => nodes,
                Err(e) => {
                    // MIG1/D2: log graph query failure so operators can see it.
                    tracing::warn!(
                        project_id,
                        path = %codebehind_path,
                        error = %e,
                        "MIG1: graph query for code-behind failed — skipping method node extraction"
                    );
                    continue;
                }
            };

            if method_nodes.is_empty() {
                // Also try without the extra extension (e.g. just "Page.aspx.vb")
                continue;
            }

            let mut methods: Vec<MethodInfo> = Vec::new();

            for node in &method_nodes {
                let effects: Vec<String> = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("effects"))
                    .and_then(|v| v.as_str())
                    .map(|s| {
                        s.split(',')
                            .map(|e| e.trim().to_string())
                            .filter(|e| !e.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();

                let signature = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("signature"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&node.name)
                    .to_string();

                let return_type = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("return_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Sub")
                    .to_string();

                let access_level = node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("access_level"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Private")
                    .to_string();

                let kind = classify_method_kind(&node.name, &effects, &node.metadata);
                let line_count = if node.end_line >= node.start_line {
                    node.end_line - node.start_line + 1
                } else {
                    1
                };

                methods.push(MethodInfo {
                    name: node.name.clone(),
                    signature,
                    return_type,
                    access_level,
                    line_range: (node.start_line, node.end_line),
                    line_count,
                    method_kind: kind,
                    effects,
                    calls_methods: vec![],
                    called_by: vec![],
                    body_preview: None, // graph nodes don't have body text
                    complexity_score: 0,
                    handles_clause: vec![],
                });
            }

            // Populate calls_methods/called_by from Dependency edges
            if let Ok(dep_edges) = graph.list_edges_by_kind(project_id, EdgeKind::Dependency, 5000)
            {
                let method_names: Vec<String> = methods.iter().map(|m| m.name.clone()).collect();
                for edge in &dep_edges {
                    for m in &mut methods {
                        if edge.source_id.ends_with(&m.name) {
                            let target_name =
                                edge.target_id.rsplit('.').next().unwrap_or(&edge.target_id);
                            if method_names.contains(&target_name.to_string()) {
                                m.calls_methods.push(target_name.to_string());
                            }
                        }
                        if edge.target_id.ends_with(&m.name) {
                            let source_name =
                                edge.source_id.rsplit('.').next().unwrap_or(&edge.source_id);
                            if method_names.contains(&source_name.to_string()) {
                                m.called_by.push(source_name.to_string());
                            }
                        }
                    }
                }
            }

            let lifecycle_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Lifecycle))
                .count();
            let event_handlers = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::ControlEvent))
                .count();
            let web_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::WebMethod))
                .count();
            let data_access_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::DataAccess))
                .count();
            let helper_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Helper))
                .count();
            let methods_with_sql = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("SQL")))
                .count();
            let methods_with_state = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("State")))
                .count();
            let largest_method = methods
                .iter()
                .max_by_key(|m| m.line_count)
                .map(|m| (m.name.clone(), m.line_count));

            let inventory = PageMethodInventory {
                file_path: fc.file_path.clone(),
                codebehind_path: codebehind_path.to_string(),
                total_methods: methods.len(),
                lifecycle_methods,
                event_handlers,
                web_methods,
                data_access_methods,
                helper_methods,
                largest_method,
                methods_with_sql,
                methods_with_state,
                methods,
            };

            result.insert(fc.file_path.clone(), inventory);
            break; // Found methods, no need to try the other extension
        }
    }

    // Fallback: if graph had no data, parse code-behind content directly
    for fc in file_contents {
        if result.contains_key(&fc.file_path) {
            continue;
        }
        if let Some(ref cb_content) = fc.codebehind_content {
            let methods = extract_methods_from_content(cb_content);
            if methods.is_empty() {
                continue;
            }
            let cb_path = fc.file_path.clone() + ".vb";
            let lifecycle_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Lifecycle))
                .count();
            let event_handlers = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::ControlEvent))
                .count();
            let web_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::WebMethod))
                .count();
            let data_access_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::DataAccess))
                .count();
            let helper_methods = methods
                .iter()
                .filter(|m| matches!(m.method_kind, MethodKind::Helper))
                .count();
            let methods_with_sql = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("SQL")))
                .count();
            let methods_with_state = methods
                .iter()
                .filter(|m| m.effects.iter().any(|e| e.contains("State")))
                .count();
            let largest_method = methods
                .iter()
                .max_by_key(|m| m.line_count)
                .map(|m| (m.name.clone(), m.line_count));

            result.insert(
                fc.file_path.clone(),
                PageMethodInventory {
                    file_path: fc.file_path.clone(),
                    codebehind_path: cb_path,
                    total_methods: methods.len(),
                    lifecycle_methods,
                    event_handlers,
                    web_methods,
                    data_access_methods,
                    helper_methods,
                    largest_method,
                    methods_with_sql,
                    methods_with_state,
                    methods,
                },
            );
        }
    }

    result
}

pub(crate) fn extract_effects_from_nearby_content(content: &str, method_name: &str) -> Vec<String> {
    // THIRD-PASS FIX: Scope effect detection to the method body when possible.
    // Previously scanned the ENTIRE file, causing every method to be tagged
    // with SQL_Access if any method in the file used SqlCommand.
    let body_text: Option<String> = {
        let is_vb = content.contains("End Sub") || content.contains("End Function");
        if is_vb {
            extract_vb_method_body(content, method_name).map(|(b, _, _, _)| b)
        } else {
            extract_cs_method_body(content, method_name).map(|(b, _, _, _)| b)
        }
    };
    // Use extracted body if available, fall back to full file content
    let scan_text = body_text.as_deref().unwrap_or(content);
    let lower = scan_text.to_lowercase();

    let mut effects = Vec::new();
    if lower.contains("sqlcommand")
        || lower.contains("sqlconnection")
        || lower.contains("sqldatareader")
        || lower.contains("sqldataadapter")
        || lower.contains("executenonquery")
        || lower.contains("executereader")
        || lower.contains("executescalar")
        || lower.contains("oledbcommand")
        || lower.contains("oledbconnection")
    {
        effects.push("SQL_Access".to_string());
    }
    if lower.contains("session(")
        || lower.contains("session[")
        || lower.contains("viewstate(")
        || lower.contains("viewstate[")
    {
        effects.push("State_Access".to_string());
    }
    if lower.contains("createobject") {
        effects.push("COM_Interop".to_string());
    }
    if lower.contains("response.redirect")
        || lower.contains("server.transfer")
        || lower.contains("response.write")
    {
        effects.push("HTTP_Response".to_string());
    }
    if lower.contains("smtpclient")
        || lower.contains("mailmessage")
        || lower.contains("cdo.message")
    {
        effects.push("Email_Send".to_string());
    }
    effects
}

/// Produce a body preview: full for ≤30 lines, truncated otherwise.
pub(crate) fn make_body_preview(body: &str, line_count: u32) -> String {
    let lines: Vec<&str> = body.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    // Dedent: find minimum leading whitespace
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let dedent = |line: &str| -> String {
        if line.len() >= min_indent {
            line[min_indent..].to_string()
        } else {
            line.trim_start().to_string()
        }
    };

    if line_count <= 30 {
        lines
            .iter()
            .map(|l| dedent(l))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let first_10: Vec<String> = lines.iter().take(10).map(|l| dedent(l)).collect();
        let last_5: Vec<String> = lines
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|l| dedent(l))
            .collect();
        // Use the actual number of shown lines so the count is correct even if
        // take(10)/take(5) yielded fewer lines than expected.
        let shown = first_10.len() + last_5.len();
        let remaining = (line_count as usize).saturating_sub(shown);
        format!(
            "{}\n    ... ({remaining} more lines) ...\n{}",
            first_10.join("\n"),
            last_5.join("\n")
        )
    }
}

/// Compute a heuristic complexity score for a method body.
/// Uses pre-compiled LazyLock regexes to avoid per-call compilation overhead.
///
/// THIRD-PASS FIX: Subtract overlap counts to prevent double-counting.
/// `else if` matches both `\bif\b` and `\belse\s+if\b`.
/// `select case` matches both `\bcase\b` and `\bselect\s+case\b`.
/// `do while` matches both `\bwhile\b` and `\bdo\s+while\b`.
/// `for each` (VB) matches both `\bfor\s` and `\bfor\s+each\b`.
/// `foreach` (C#) matches `\bfor\s` because of the word boundary + space.
pub(crate) fn compute_complexity_score(body: &str) -> u32 {
    let mut score: u32 = 0;

    // Branches (1 point each), with overlap subtraction
    let if_count = CX_IF_RE.find_iter(body).count() as u32;
    let else_if_count = CX_ELSE_IF_RE.find_iter(body).count() as u32;
    let elseif_count = CX_ELSEIF_RE.find_iter(body).count() as u32;
    // `else if` and `elseif` also match `\bif\b`, so subtract them
    score += if_count
        .saturating_sub(else_if_count)
        .saturating_sub(elseif_count);
    score += else_if_count;
    score += elseif_count;

    score += CX_SWITCH_RE.find_iter(body).count() as u32;
    let case_count = CX_CASE_RE.find_iter(body).count() as u32;
    let select_case_count = CX_SELECT_CASE_RE.find_iter(body).count() as u32;
    // `select case` also matches `\bcase\b`, subtract overlap
    score += case_count.saturating_sub(select_case_count);
    score += select_case_count;

    // Loops (1 point each), with overlap subtraction
    let for_count = CX_FOR_RE.find_iter(body).count() as u32;
    let foreach_count = CX_FOREACH_RE.find_iter(body).count() as u32;
    let for_each_count = CX_FOR_EACH_RE.find_iter(body).count() as u32;
    // `for each` (VB) matches `\bfor\s`, and C# `foreach` does NOT match `\bfor\s`
    // (because `foreach` has no space after `for`). So only subtract VB for_each.
    score += for_count.saturating_sub(for_each_count);
    score += foreach_count;
    score += for_each_count;

    let while_count = CX_WHILE_RE.find_iter(body).count() as u32;
    let do_while_count = CX_DO_WHILE_RE.find_iter(body).count() as u32;
    // `do while` also matches `\bwhile\b`, subtract overlap
    score += while_count.saturating_sub(do_while_count);
    score += do_while_count;
    score += CX_DO_RE.find_iter(body).count() as u32;

    // Error handlers (2 points each)
    score += CX_TRY_BRACE_RE.find_iter(body).count() as u32 * 2;
    score += CX_TRY_EOL_RE.find_iter(body).count() as u32 * 2;
    score += CX_CATCH_RE.find_iter(body).count() as u32 * 2;
    score += CX_ON_ERROR_RE.find_iter(body).count() as u32 * 2;

    // SQL strings (3 points each)
    score += CX_SQL_SELECT_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_INSERT_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_UPDATE_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_DELETE_RE.find_iter(body).count() as u32 * 3;
    score += CX_CMD_TEXT_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_CMD_RE.find_iter(body).count() as u32 * 3;
    score += CX_SQL_ADAPTER_RE.find_iter(body).count() as u32 * 3;

    // Session access (1 point each)
    score += CX_SESSION_RE.find_iter(body).count() as u32;

    score
}
