//! Extracted analyzer: classic asp.
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


pub(crate) fn build_classic_asp_summary(
    graph: &Arc<GraphStore>,
    project_id: &str,
    asp_files: &[(String, String)],
) -> ClassicAspSummary {
    if asp_files.is_empty() {
        // Check graph for any existing classic ASP insights
        let asp_insights = nodes_or_warn(
            graph.query_nodes(project_id, Some("insight"), None, None, 1_000),
            "asp_insights",
        )
        .into_iter()
        .filter(|n| n.name.to_lowercase().contains("classic_asp"))
        .count();
        if asp_insights == 0 {
            return ClassicAspSummary {
                total_asp_files: 0,
                com_objects: vec![],
                ado_connections: 0,
                sql_statements: 0,
                includes: vec![],
                state_accesses: 0,
                migration_effort_hours: 0.0,
            };
        }
    }

    let include_edges = edges_or_warn(
        graph.list_edges_by_kind(project_id, EdgeKind::IncludesFile, 5_000),
        "IncludesFile",
    );

    let mut com_objects = Vec::new();
    let mut ado_connections = 0usize;
    let mut sql_statements = 0usize;
    let mut state_accesses = 0usize;
    let mut includes = Vec::new();

    // Scan ASP file contents for patterns
    for (path, content) in asp_files {
        for cap in ASP_CREATE_OBJ_RE.captures_iter(content) {
            let prog_id = cap[1].to_string();
            if prog_id.to_lowercase().contains("adodb") {
                ado_connections += 1;
            }
            com_objects.push(ComObjectRef {
                file_path: path.clone(),
                prog_id,
            });
        }
        sql_statements += ASP_SQL_RE.find_iter(content).count();
        state_accesses += ASP_STATE_RE.find_iter(content).count();
        for cap in ASP_INCLUDE_RE.captures_iter(content) {
            includes.push(IncludeRef {
                source_file: path.clone(),
                included_file: cap[1].to_string(),
            });
        }
    }

    // Also gather includes from graph edges for .asp files
    for e in &include_edges {
        let src = super::common::extract_file_from_node_id(&e.source_id);
        if src.to_lowercase().ends_with(".asp") {
            let inc = IncludeRef {
                source_file: src,
                included_file: e.target_id.clone(),
            };
            if !includes
                .iter()
                .any(|i| i.source_file == inc.source_file && i.included_file == inc.included_file)
            {
                includes.push(inc);
            }
        }
    }

    // Estimate effort: ~2h per ASP file + 0.5h per COM object + 0.25h per SQL statement
    let effort = (asp_files.len() as f64 * 2.0)
        + (com_objects.len() as f64 * 0.5)
        + (sql_statements as f64 * 0.25);

    ClassicAspSummary {
        total_asp_files: asp_files.len(),
        com_objects,
        ado_connections,
        sql_statements,
        includes,
        state_accesses,
        migration_effort_hours: effort,
    }
}
