//! Extracted analyzer: cross layer.
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

/// Build cross-layer traces from JS AJAX calls → handlers → database.
pub(crate) fn build_cross_layer_traces(
    js_analysis: &JsAnalysisSummary,
    sp_catalog: &StoredProcedureCatalog,
    service_endpoints: &ServiceEndpointSummary,
    code_files: &[(&str, &str)],
) -> CrossLayerTraceSummary {
    // 1. Build URL→handler file map from service endpoints and code files
    let mut url_to_handler: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // Map from service_endpoints
    for ep in &service_endpoints.web_services {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }
    for ep in &service_endpoints.http_handlers {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }
    for ep in &service_endpoints.wcf_services {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }
    for ep in &service_endpoints.route_handlers {
        let base = extract_filename_from_path(&ep.file_path);
        url_to_handler.insert(base.to_lowercase(), ep.file_path.clone());
    }

    // Also map from code files by filename
    for &(path, _) in code_files {
        let lower = path.to_lowercase();
        if lower.ends_with(".ashx")
            || lower.ends_with(".ashx.cs")
            || lower.ends_with(".ashx.vb")
            || lower.ends_with(".asmx")
            || lower.ends_with(".asmx.cs")
            || lower.ends_with(".asmx.vb")
        {
            let base = extract_filename_from_path(path);
            // Strip .cs / .vb suffix for matching
            let base_lower = base.to_lowercase().replace(".cs", "").replace(".vb", "");
            url_to_handler.insert(base_lower, path.to_string());
        }
    }

    // Build code_file content map
    let content_map: std::collections::HashMap<&str, &str> =
        code_files.iter().map(|&(p, c)| (p, c)).collect();

    // SP name → tables map from catalog
    let mut sp_tables: std::collections::HashMap<String, (Vec<String>, Vec<String>)> =
        std::collections::HashMap::new();
    for sp in &sp_catalog.procedures {
        sp_tables.insert(
            sp.name.to_lowercase(),
            (sp.tables_read.clone(), sp.tables_written.clone()),
        );
    }

    let mut chains: Vec<DataFlowChain> = Vec::new();
    let mut unresolved_urls: Vec<String> = Vec::new();
    let mut resolved_handlers: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 2. For each AJAX call, try to resolve the chain
    for ajax_call in &js_analysis.ajax_calls {
        let url = &ajax_call.target_url;
        let url_parts = extract_url_parts(url);
        let handler_file_lower = url_parts
            .file_part
            .to_lowercase()
            .replace(".cs", "")
            .replace(".vb", "");

        // Try to find handler
        let handler_path = url_to_handler.get(&handler_file_lower).cloned();

        if handler_path.is_none() {
            if !unresolved_urls.contains(url) {
                unresolved_urls.push(url.clone());
            }
            continue;
        }

        let handler_path = handler_path.expect("checked above");
        resolved_handlers.insert(handler_path.clone());

        // Build steps
        let mut steps: Vec<DataFlowStep> = Vec::new();
        let mut tables_touched: Vec<String> = Vec::new();
        let mut risk_notes: Vec<String> = Vec::new();

        // Step 1: Client AJAX call
        steps.push(DataFlowStep {
            layer: "client".to_string(),
            file_path: ajax_call.js_file.clone(),
            action: format!(
                "{} {} to {}",
                ajax_call.transport,
                url_parts.method_part.as_deref().unwrap_or(""),
                url
            ),
            params: Vec::new(),
        });

        // Step 2: Handler processing
        let handler_content = find_handler_content(&handler_path, &content_map);
        let mut sp_names: Vec<String> = Vec::new();

        if let Some(content) = handler_content {
            // Find SP calls in handler
            for cap in HANDLER_SP_NAME_RE.captures_iter(content) {
                sp_names.push(cap[1].to_string());
            }

            // Find direct table access
            for cap in HANDLER_TABLE_RE.captures_iter(content) {
                let table = cap[1].to_string();
                if !tables_touched.contains(&table) {
                    tables_touched.push(table);
                }
            }

            let sp_desc = if !sp_names.is_empty() {
                format!("calls {}", sp_names.join(", "))
            } else if !tables_touched.is_empty() {
                format!("direct SQL on: {}", tables_touched.join(", "))
            } else {
                "processes request (no SQL detected)".to_string()
            };

            steps.push(DataFlowStep {
                layer: "handler".to_string(),
                file_path: handler_path.clone(),
                action: sp_desc,
                params: sp_names.clone(),
            });
        } else {
            steps.push(DataFlowStep {
                layer: "handler".to_string(),
                file_path: handler_path.clone(),
                action: "handler file (code not available for analysis)".to_string(),
                params: Vec::new(),
            });
            risk_notes.push("Handler code-behind not found — cannot trace data layer".into());
        }

        // Step 3: Database layer (from SP catalog)
        for sp_name in &sp_names {
            if let Some((reads, writes)) = sp_tables.get(&sp_name.to_lowercase()) {
                for t in reads {
                    if !tables_touched.contains(t) {
                        tables_touched.push(t.clone());
                    }
                }
                for t in writes {
                    if !tables_touched.contains(t) {
                        tables_touched.push(t.clone());
                    }
                }

                steps.push(DataFlowStep {
                    layer: "database".to_string(),
                    file_path: sp_name.clone(),
                    action: format!(
                        "reads: [{}], writes: [{}]",
                        reads.join(", "),
                        writes.join(", ")
                    ),
                    params: Vec::new(),
                });
            }
        }

        let feature_name = url_parts
            .method_part
            .unwrap_or_else(|| url_parts.file_part.clone());

        chains.push(DataFlowChain {
            feature_name,
            trigger_file: ajax_call.js_file.clone(),
            steps,
            tables_touched,
            risk_notes,
        });
    }

    // Find handlers without callers
    let all_handler_paths: Vec<String> = url_to_handler.values().cloned().collect();
    let handlers_without_ajax_callers: Vec<String> = all_handler_paths
        .into_iter()
        .filter(|h| !resolved_handlers.contains(h))
        .collect();

    let total_chains = chains.len();

    CrossLayerTraceSummary {
        chains,
        total_chains,
        unresolved_urls,
        handlers_without_ajax_callers,
    }
}

pub(crate) fn extract_url_parts(url: &str) -> UrlParts {
    // Strip query string and fragment
    let clean = url.split('?').next().unwrap_or(url);
    let clean = clean.split('#').next().unwrap_or(clean);

    // Split on last / to separate method from file
    // e.g. "Services/MapData.asmx/GetPolygons" → file="MapData.asmx", method="GetPolygons"
    let parts: Vec<&str> = clean.rsplitn(2, '/').collect();
    if parts.len() == 2 {
        let maybe_method = parts[0];
        let path_part = parts[1];

        // If the path part contains a file extension, the right side is a method name
        if path_part.contains('.') && !maybe_method.contains('.') {
            let file = extract_filename_from_path(path_part);
            return UrlParts {
                file_part: file.to_string(),
                method_part: Some(maybe_method.to_string()),
            };
        }
    }

    // No method part, just extract filename
    let file = extract_filename_from_path(clean);
    UrlParts {
        file_part: file.to_string(),
        method_part: None,
    }
}

pub(crate) fn extract_filename_from_path(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

pub(crate) fn find_handler_content<'a>(
    handler_path: &str,
    content_map: &std::collections::HashMap<&str, &'a str>,
) -> Option<&'a str> {
    // Direct match
    if let Some(&c) = content_map.get(handler_path) {
        return Some(c);
    }
    // Try with .cs or .vb suffix
    let with_cs = format!("{handler_path}.cs");
    if let Some(&c) = content_map.get(with_cs.as_str()) {
        return Some(c);
    }
    let with_vb = format!("{handler_path}.vb");
    if let Some(&c) = content_map.get(with_vb.as_str()) {
        return Some(c);
    }
    // Partial match by filename
    let filename = extract_filename_from_path(handler_path).to_lowercase();
    for (&path, &content) in content_map {
        let pf = extract_filename_from_path(path).to_lowercase();
        if pf == filename || pf.starts_with(&filename) {
            return Some(content);
        }
    }
    None
}
