//! Extracted analyzer: sp catalog.
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

pub(crate) fn build_sp_catalog(
    sql_files: &[(String, String)],
    code_files: &[(&str, &str)],
) -> StoredProcedureCatalog {
    use engram_index::sp_extractor;

    let mut all_procs: Vec<StoredProcedureInfo> = Vec::new();
    let mut code_calls: Vec<(String, String)> = Vec::new(); // (sp_name, calling_file)

    // 1. Parse SQL files for SP definitions
    for (_path, content) in sql_files {
        let defs = sp_extractor::parse_sp_definitions(content);
        for sp in defs {
            let modern_eq = if sp.has_dynamic_sql {
                "raw SQL (review for SQL injection)".to_string()
            } else if sp.has_cursor {
                "LINQ query or Dapper (cursor refactoring needed)".to_string()
            } else if sp.tables_read.len() > 3 || sp.tables_written.len() > 2 {
                "EF Core with repository pattern (complex joins)".to_string()
            } else if sp.tables_written.is_empty() {
                "EF Core query or Dapper".to_string()
            } else {
                "EF Core SaveChanges or Dapper Execute".to_string()
            };

            all_procs.push(StoredProcedureInfo {
                name: sp.name.clone(),
                parameters: sp
                    .parameters
                    .iter()
                    .map(|p| SpParameterInfo {
                        name: p.name.clone(),
                        sql_type: p.sql_type.clone(),
                        direction: p.direction.clone(),
                        default_value: p.default_value.clone(),
                        csharp_type: p.csharp_type.clone(),
                    })
                    .collect(),
                tables_read: sp.tables_read,
                tables_written: sp.tables_written,
                called_from: Vec::new(), // filled below
                line_count: sp.line_count,
                has_dynamic_sql: sp.has_dynamic_sql,
                has_cursor: sp.has_cursor,
                modern_equivalent: modern_eq,
            });
        }
    }

    // 2. Scan code files for SP calls
    for (path, content) in code_files {
        let rel = engram_core::RelPath::new(path);
        let (_, edges) = sp_extractor::extract_code_side_sp_calls(&rel, content);
        for edge in edges {
            if edge.kind == "calls_stored_procedure" {
                code_calls.push((edge.target_name.clone(), path.to_string()));
            }
        }
    }

    // 3. Cross-reference: mark which SPs are called from code
    for (sp_name, calling_file) in &code_calls {
        for proc in &mut all_procs {
            if proc.name.eq_ignore_ascii_case(sp_name) && !proc.called_from.contains(calling_file) {
                proc.called_from.push(calling_file.clone());
            }
        }
    }

    let total = all_procs.len();
    let with_params = all_procs
        .iter()
        .filter(|p| !p.parameters.is_empty())
        .count();
    let called_from_code = all_procs
        .iter()
        .filter(|p| !p.called_from.is_empty())
        .count();
    let uncalled: Vec<String> = all_procs
        .iter()
        .filter(|p| p.called_from.is_empty())
        .map(|p| p.name.clone())
        .collect();

    StoredProcedureCatalog {
        procedures: all_procs,
        total_procedures: total,
        procedures_with_params: with_params,
        procedures_called_from_code: called_from_code,
        uncalled_procedures: uncalled,
    }
}

/// Public wrapper for building a stored procedure catalog from SQL + code files.
/// Used by standalone tools (e.g., `analyze_database_intelligence`) that need the catalog
/// without running the full project migration analysis.
/// `sp_limit` caps the number of procedures to include (0 = unlimited).
pub fn build_sp_catalog_public(
    sql_files: &[(String, String)],
    code_files: &[(&str, &str)],
    sp_limit: usize,
) -> StoredProcedureCatalog {
    let mut catalog = build_sp_catalog(sql_files, code_files);
    // Sort so that procs called from application code appear first — when
    // `sp_limit` truncates, we want business-critical SPs to survive. On
    // real projects the tail tends to be framework procs (e.g. `aspnet_*`
    // from Membership) that no application code actually references, so
    // pushing them to the back via a descending `called_from.len()` sort
    // (with alphabetical tiebreaker for determinism) is the right shape.
    catalog.procedures.sort_by(|a, b| {
        b.called_from
            .len()
            .cmp(&a.called_from.len())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    if sp_limit > 0 && catalog.procedures.len() > sp_limit {
        catalog.procedures.truncate(sp_limit);
    }
    // Keep `total_procedures` in sync with what's actually returned so the
    // downstream renderer and JSON consumers agree on the count.
    catalog.total_procedures = catalog.procedures.len();
    catalog
}
