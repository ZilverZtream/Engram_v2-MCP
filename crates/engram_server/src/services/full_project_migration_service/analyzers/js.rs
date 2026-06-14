//! Extracted analyzer: js.
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
use super::super::*;
use super::super::super::auth_config_service::AuthConfigMap;
use super::super::super::db_strategy_service::{self, FileDataAccessProfile};
use super::super::super::dossier_service::{self, MigrationDossier};
use super::super::super::migration_order_service::{self, MigrationOrderPlan};
use super::super::super::pattern_detection_service;
use super::super::super::state_migration_service::{self, StateMigrationReport};


pub(crate) fn build_js_analysis(
    graph: &Arc<GraphStore>,
    project_id: &str,
    markup_files: &[FileContent],
    script_files: &[(String, String)],
) -> JsAnalysisSummary {
    let dom_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ManipulatesDom, 10_000),
        "ManipulatesDom",
    );
    let postback_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::TriggersPostback, 10_000),
        "TriggersPostback",
    );
    let api_call_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::ApiCall, 10_000),
        "ApiCall/js",
    );
    let contains_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::Contains, 50_000),
        "Contains",
    );

    // Build DOM manipulation refs
    let dom_manipulations: Vec<JsDomRef> = dom_edges
        .iter()
        .map(|e| {
            let selector_type = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("selector_type").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string();
            JsDomRef {
                js_file: super::common::extract_file_from_node_id(&e.source_id),
                target_control: e.target_id.clone(),
                selector_type,
            }
        })
        .collect();

    // Build postback trigger refs
    let postback_triggers: Vec<JsPostbackRef> = postback_edges
        .iter()
        .map(|e| {
            let unique_id = e
                .metadata
                .as_ref()
                .and_then(|m| m.get("unique_id").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            JsPostbackRef {
                js_file: super::common::extract_file_from_node_id(&e.source_id),
                target_control: e.target_id.clone(),
                unique_id,
            }
        })
        .collect();

    // Build AJAX call refs
    let ajax_calls: Vec<JsAjaxCall> = api_call_edges
        .iter()
        .map(|e| {
            let meta = e.metadata.as_ref();
            // Keys MUST match what js_extractor emits: `ajax_transport` /
            // `ajax_target_method` (the old `transport`/`method` lookups always
            // resolved to unknown/None despite the extractor populating them).
            let transport = meta
                .and_then(|m| m.get("ajax_transport").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string();
            let target_method = meta
                .and_then(|m| m.get("ajax_target_method").and_then(|v| v.as_str()))
                .map(String::from);
            let target_type = meta
                .and_then(|m| m.get("target_type").and_then(|v| v.as_str()))
                .unwrap_or("unknown")
                .to_string();
            JsAjaxCall {
                js_file: super::common::extract_file_from_node_id(&e.source_id),
                target_url: e.target_id.clone(),
                transport,
                target_method,
                target_type,
            }
        })
        .collect();

    // Build page→control ownership map from Contains edges
    let mut control_to_page: BTreeMap<String, String> = BTreeMap::new();
    for e in &contains_edges {
        let source_file = super::common::extract_file_from_node_id(&e.source_id);
        if source_file.to_lowercase().ends_with(".aspx")
            || source_file.to_lowercase().ends_with(".ascx")
            || source_file.to_lowercase().ends_with(".master")
        {
            control_to_page.insert(e.target_id.clone(), source_file);
        }
    }

    // Build page↔JS dependency map
    let mut page_js_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // From graph edges: which JS files reference controls owned by which pages
    for dom_ref in &dom_manipulations {
        if let Some(page) = control_to_page.get(&dom_ref.target_control) {
            let js_list = page_js_deps.entry(page.clone()).or_default();
            if !js_list.contains(&dom_ref.js_file) {
                js_list.push(dom_ref.js_file.clone());
            }
        }
    }
    for pb_ref in &postback_triggers {
        if let Some(page) = control_to_page.get(&pb_ref.target_control) {
            let js_list = page_js_deps.entry(page.clone()).or_default();
            if !js_list.contains(&pb_ref.js_file) {
                js_list.push(pb_ref.js_file.clone());
            }
        }
    }

    // From markup: scan <script src="..."> tags
    for fc in markup_files {
        for cap in JS_SCRIPT_SRC_RE.captures_iter(&fc.markup_content) {
            let js_ref = cap[1].to_string();
            let js_list = page_js_deps.entry(fc.file_path.clone()).or_default();
            if !js_list.contains(&js_ref) {
                js_list.push(js_ref);
            }
        }
    }

    // Detect inline <script> blocks (not src= external files)
    let mut inline_script_files = Vec::new();
    for fc in markup_files {
        if JS_INLINE_RE
            .find_iter(&fc.markup_content)
            .any(|m| !JS_SRC_ATTR_RE.is_match(m.as_str()))
        {
            inline_script_files.push(fc.file_path.clone());
        }
    }

    // Detect jQuery version hint from JS files
    let mut jquery_version_hint = None;
    for (path, _content) in script_files {
        if let Some(cap) = JS_JQUERY_RE.captures(&path.to_lowercase()) {
            jquery_version_hint = Some(cap[1].to_string());
            break;
        }
    }

    // Count JS files with server-side dependencies
    let mut js_files_with_deps: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for dr in &dom_manipulations {
        js_files_with_deps.insert(dr.js_file.clone());
    }
    for pr in &postback_triggers {
        js_files_with_deps.insert(pr.js_file.clone());
    }
    for ac in &ajax_calls {
        js_files_with_deps.insert(ac.js_file.clone());
    }

    JsAnalysisSummary {
        total_script_files: script_files.len(),
        legacy_total_js_files: script_files.len(),
        script_files_with_server_deps: js_files_with_deps.len(),
        legacy_js_files_with_server_deps: js_files_with_deps.len(),
        dom_manipulations,
        postback_triggers,
        ajax_calls,
        page_js_dependencies: page_js_deps,
        inline_script_files,
        jquery_version_hint,
    }
}
