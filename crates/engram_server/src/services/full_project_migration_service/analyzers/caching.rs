//! Extracted analyzer: caching.
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

pub(crate) fn build_caching_inventory(
    markup_files: &[FileContent],
    code_refs: &[(&str, &str)],
    code_files: &[(String, String)],
) -> CachingInventory {
    static OUTPUT_CACHE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<%@\s*OutputCache\s+([^%]+?)%>"#).expect("valid regex")
    });
    static CACHE_ATTR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(\w+)\s*=\s*"([^"]*)""#).expect("valid regex")
    });
    static CACHE_API_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)(?:HttpRuntime\.Cache|HttpContext\.Current\.Cache|\bCache)\.(Insert|Add|Get|Remove)\s*\(\s*"([^"]+)""#).expect("valid regex")
    });
    static RESPONSE_CACHE_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)Response\.Cache\.Set(?:Expires|Cacheability|MaxAge|ValidUntilExpires|NoStore|NoTransforms|SlidingExpiration|Revalidation|ETag|LastModified|VaryByCustom|OmitVaryStar)\s*\(").expect("valid regex")
    });
    static SQL_CACHE_DEP_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r#"(?i)new\s+SqlCacheDependency\s*\(\s*"?([^",)]*)"?\s*(?:,\s*"?([^",)]*)"?)?\s*\)"#,
        )
        .expect("valid regex")
    });

    let mut output_cache_pages = Vec::new();
    let mut programmatic_keys: BTreeMap<String, (Vec<String>, String)> = BTreeMap::new();
    let mut response_cache_files: Vec<String> = Vec::new();
    let mut sql_cache_deps = Vec::new();

    // Scan markup files for OutputCache directives
    for fc in markup_files {
        for cap in OUTPUT_CACHE_RE.captures_iter(&fc.markup_content) {
            let attrs_str = &cap[1];
            let mut duration: Option<u32> = None;
            let mut vary_by_param: Option<String> = None;
            let mut vary_by_control: Option<String> = None;
            let mut vary_by_custom: Option<String> = None;
            let mut location: Option<String> = None;
            let mut cache_profile: Option<String> = None;
            let mut sql_dependency: Option<String> = None;

            for attr_cap in CACHE_ATTR_RE.captures_iter(attrs_str) {
                let key = &attr_cap[1];
                let val = attr_cap[2].to_string();
                match key.to_lowercase().as_str() {
                    "duration" => duration = val.parse().ok(),
                    "varybyparam" => vary_by_param = Some(val),
                    "varybycontrol" => vary_by_control = Some(val),
                    "varybycustom" => vary_by_custom = Some(val),
                    "location" => location = Some(val),
                    "cacheprofile" => cache_profile = Some(val),
                    "sqldependency" => sql_dependency = Some(val),
                    _ => {}
                }
            }

            let mut modern_parts = Vec::new();
            if let Some(d) = duration {
                modern_parts.push(format!("Duration = {d}"));
            }
            if let Some(ref vbp) = vary_by_param
                && vbp != "none"
                && vbp != "*"
            {
                modern_parts.push(format!("VaryByQueryKeys = new[] {{ \"{vbp}\" }}"));
            }
            let modern_equivalent = if modern_parts.is_empty() {
                "[ResponseCache]".to_string()
            } else {
                format!("[ResponseCache({})]", modern_parts.join(", "))
            };

            output_cache_pages.push(OutputCacheEntry {
                file_path: fc.file_path.clone(),
                duration_seconds: duration,
                vary_by_param,
                vary_by_control,
                vary_by_custom,
                location,
                cache_profile,
                sql_dependency,
                modern_equivalent,
            });
        }
    }

    // Scan code files for programmatic cache patterns
    let all_code: Vec<(&str, &str)> = code_refs
        .iter()
        .copied()
        .chain(code_files.iter().map(|(p, c)| (p.as_str(), c.as_str())))
        .collect();

    for (path, content) in &all_code {
        for cap in CACHE_API_RE.captures_iter(content) {
            let operation = cap[1].to_string();
            let cache_key = cap[2].to_string();
            programmatic_keys
                .entry(cache_key)
                .or_insert_with(|| (Vec::new(), operation.clone()))
                .0
                .push(path.to_string());
        }

        if RESPONSE_CACHE_RE.is_match(content) && !response_cache_files.contains(&path.to_string())
        {
            response_cache_files.push(path.to_string());
        }

        for cap in SQL_CACHE_DEP_RE.captures_iter(content) {
            let db = cap.get(1).map(|m| m.as_str().to_string());
            let table = cap.get(2).map(|m| m.as_str().to_string());
            sql_cache_deps.push(SqlCacheDependencyEntry {
                file_path: path.to_string(),
                database_name: db,
                table_name: table,
                modern_note: "No direct .NET Core equivalent — use EF Change Tracker + cache invalidation or message bus".to_string(),
            });
        }
    }

    let programmatic_cache_keys: Vec<ProgrammaticCacheEntry> = programmatic_keys
        .into_iter()
        .map(|(key, (mut files, operation))| {
            files.sort();
            files.dedup();
            let modern = if files.len() > 1 {
                "IDistributedCache (shared across instances)".to_string()
            } else {
                "IMemoryCache with SlidingExpiration".to_string()
            };
            ProgrammaticCacheEntry {
                cache_key: key,
                operation,
                has_expiration: false,
                has_dependency: false,
                modern_equivalent: modern,
                files,
            }
        })
        .collect();

    let total_cached = output_cache_pages.len();
    let total_keys = programmatic_cache_keys.len();
    let has_resp = !response_cache_files.is_empty();
    let has_sql = !sql_cache_deps.is_empty();

    CachingInventory {
        output_cache_pages,
        programmatic_cache_keys,
        response_cache_files,
        sql_cache_dependencies: sql_cache_deps,
        total_cached_pages: total_cached,
        total_cache_keys: total_keys,
        has_response_caching: has_resp,
        has_sql_dependencies: has_sql,
    }
}
