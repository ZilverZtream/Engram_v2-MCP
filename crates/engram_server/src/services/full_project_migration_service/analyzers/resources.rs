//! Extracted analyzer: resources.
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

pub(crate) fn build_resource_inventory(resx_files: &[(String, String)]) -> ResourceInventory {
    let mut files: Vec<ResourceFileInfo> = Vec::new();
    let mut total_keys = 0usize;
    let mut languages: Vec<String> = Vec::new();
    let mut has_global = false;
    let mut has_local = false;
    let mut embedded_count = 0usize;

    for (path, content) in resx_files {
        let key_count = RESX_DATA_RE.captures_iter(content).count();
        total_keys += key_count;

        // Detect embedded resources (file refs)
        let file_ref_count = RESX_FILE_REF_RE.captures_iter(content).count();
        embedded_count += file_ref_count;

        // Detect language from filename
        let language = RESX_LANG_RE.captures(path).map(|c| c[1].to_string());
        if let Some(ref lang) = language
            && !languages.contains(lang)
        {
            languages.push(lang.clone());
        }

        // Classify: App_GlobalResources vs App_LocalResources
        let resource_type =
            if path.contains("App_GlobalResources") || path.contains("app_globalresources") {
                has_global = true;
                "global".to_string()
            } else if path.contains("App_LocalResources") || path.contains("app_localresources") {
                has_local = true;
                "local".to_string()
            } else {
                "embedded".to_string()
            };

        files.push(ResourceFileInfo {
            file_path: path.clone(),
            key_count,
            language,
            resource_type,
        });
    }

    files.sort_by(|a, b| b.key_count.cmp(&a.key_count));

    ResourceInventory {
        resource_files: files,
        total_keys,
        languages_detected: languages,
        has_global_resources: has_global,
        has_local_resources: has_local,
        embedded_resource_count: embedded_count,
    }
}
