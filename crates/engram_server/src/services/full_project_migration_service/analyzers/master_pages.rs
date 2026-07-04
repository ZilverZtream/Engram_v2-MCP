//! Extracted analyzer: master pages.
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

pub(crate) fn build_master_page_region_map(
    master_files: &[(String, String)],
    markup_files: &[FileContent],
) -> MasterPageRegionMap {
    let mut master_pages: Vec<MasterPageInfo> = Vec::new();
    let mut region_map: std::collections::HashMap<String, (String, Vec<String>, bool)> =
        std::collections::HashMap::new();

    // 1. Parse master pages for ContentPlaceHolder definitions
    for (path, content) in master_files {
        let mut placeholders: Vec<String> = Vec::new();

        for cap in CONTENT_PLACEHOLDER_RE.captures_iter(content) {
            let id = cap[1].to_string();
            let has_default = PLACEHOLDER_DEFAULT_RE
                .captures_iter(content)
                .any(|dc| dc[1] == *id);
            region_map
                .entry(id.clone())
                .or_insert_with(|| (path.clone(), Vec::new(), has_default));
            placeholders.push(id);
        }

        let nested_master = MASTER_PAGE_FILE_RE
            .captures(content)
            .map(|c| c[1].to_string());

        master_pages.push(MasterPageInfo {
            file_path: path.clone(),
            placeholders,
            nested_master,
        });
    }

    // 2. Scan aspx/ascx files for asp:Content fills
    for fc in markup_files {
        for cap in CONTENT_FILLS_RE.captures_iter(&fc.markup_content) {
            let region_id = cap[1].to_string();
            if let Some(entry) = region_map.get_mut(&region_id) {
                if !entry.1.contains(&fc.file_path) {
                    entry.1.push(fc.file_path.clone());
                }
            } else {
                // Region referenced but not defined in any scanned master page
                region_map.insert(
                    region_id,
                    (
                        "(unknown master)".to_string(),
                        vec![fc.file_path.clone()],
                        false,
                    ),
                );
            }
        }
    }

    // 3. Build region mappings
    let mut regions: Vec<RegionMapping> = Vec::new();
    let mut orphans: Vec<String> = Vec::new();

    for (region_name, (defined_in, filled_by, has_default)) in &region_map {
        let modern_eq = match region_name.as_str() {
            "MainContent" | "ContentPlaceHolder1" | "BodyContent" | "content" => {
                "@RenderBody()".to_string()
            }
            "head" | "HeadContent" | "HeaderContent" => {
                "@RenderSection(\"Head\", required: false)".to_string()
            }
            "ScriptsSection" | "Scripts" | "FooterScripts" => {
                "@RenderSection(\"Scripts\", required: false)".to_string()
            }
            _ => format!("@RenderSection(\"{region_name}\", required: false)"),
        };

        if filled_by.is_empty() || defined_in == "(unknown master)" {
            orphans.push(region_name.clone());
        }

        regions.push(RegionMapping {
            region_name: region_name.clone(),
            defined_in: defined_in.clone(),
            filled_by: filled_by.clone(),
            has_default_content: *has_default,
            modern_equivalent: modern_eq,
        });
    }

    regions.sort_by(|a, b| b.filled_by.len().cmp(&a.filled_by.len()));

    MasterPageRegionMap {
        master_pages,
        regions,
        orphan_regions: orphans,
    }
}
