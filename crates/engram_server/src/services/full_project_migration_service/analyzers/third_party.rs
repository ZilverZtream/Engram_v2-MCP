//! Extracted analyzer: third party.
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


pub(crate) fn build_third_party_control_summary(markup_files: &[FileContent]) -> ThirdPartyControlSummary {
    static THIRD_PARTY_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#"(?i)<(telerik|rad|dx|ig|igtbl|igmisc|igsch|ComponentArt|kendo|obout|eo|FarPoint|Dart|cwc|ntx):(\w+)\b"#).expect("valid regex")
    });

    let mut vendor_controls: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut all_files: Vec<String> = Vec::new();

    for fc in markup_files {
        let mut found_in_file = false;
        for cap in THIRD_PARTY_RE.captures_iter(&fc.markup_content) {
            let prefix = cap[1].to_string();
            let control_name = cap[2].to_string();
            let vendor = classify_vendor_from_prefix(&prefix);
            vendor_controls
                .entry(vendor)
                .or_default()
                .entry(format!("{prefix}:{control_name}"))
                .or_default()
                .push(fc.file_path.clone());
            found_in_file = true;
        }
        if found_in_file {
            all_files.push(fc.file_path.clone());
        }
    }
    all_files.sort();
    all_files.dedup();

    let mut vendors_detected = Vec::new();
    let mut total_third_party = 0usize;
    let mut unmapped_controls = Vec::new();

    for (vendor, controls_map) in &vendor_controls {
        let (suite, modern_suite, license) = vendor_suite_info(vendor);
        let mut controls_used: Vec<(String, usize)> = Vec::new();
        let mut vendor_files: Vec<String> = Vec::new();
        let mut vendor_count = 0usize;

        for (tag_name, files) in controls_map {
            let usage = files.len();
            vendor_count += usage;
            controls_used.push((tag_name.clone(), usage));

            let control_short = tag_name.split(':').nth(1).unwrap_or(tag_name);
            if engram_index::control_mapping::lookup(control_short).is_none() {
                let first_file = files.first().cloned().unwrap_or_default();
                unmapped_controls.push(UnmappedControl {
                    tag_name: tag_name.clone(),
                    vendor: vendor.clone(),
                    file_path: first_file,
                    note: format!(
                        "No automatic mapping — evaluate {modern_suite} or manual implementation"
                    ),
                });
            }

            vendor_files.extend(files.iter().cloned());
        }

        vendor_files.sort();
        vendor_files.dedup();
        controls_used.sort_by(|a, b| b.1.cmp(&a.1));
        total_third_party += vendor_count;

        vendors_detected.push(VendorSummary {
            vendor: vendor.clone(),
            suite: suite.to_string(),
            control_count: vendor_count,
            controls_used,
            files: vendor_files,
            modern_replacement_suite: modern_suite.to_string(),
            license_note: license.to_string(),
        });
    }

    vendors_detected.sort_by(|a, b| b.control_count.cmp(&a.control_count));

    ThirdPartyControlSummary {
        vendors_detected,
        total_third_party_controls: total_third_party,
        files_with_third_party: all_files,
        unmapped_controls,
    }
}

pub(crate) fn classify_vendor_from_prefix(prefix: &str) -> String {
    match prefix.to_lowercase().as_str() {
        "telerik" | "rad" | "kendo" => "Telerik".to_string(),
        "dx" => "DevExpress".to_string(),
        "ig" | "igtbl" | "igmisc" | "igsch" | "ntx" => "Infragistics".to_string(),
        "componentart" => "ComponentArt".to_string(),
        "obout" => "Obout".to_string(),
        "eo" => "EO.WebControls".to_string(),
        "farpoint" => "FarPoint".to_string(),
        "dart" => "Dart".to_string(),
        "cwc" => "CustomWebControls".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn vendor_suite_info(vendor: &str) -> (&'static str, &'static str, &'static str) {
    match vendor {
        "Telerik" => (
            "UI for ASP.NET AJAX",
            "Telerik UI for Blazor or MudBlazor",
            "Commercial for Telerik Blazor; MudBlazor is MIT",
        ),
        "DevExpress" => (
            "ASP.NET Controls",
            "DevExpress Blazor Components or MudBlazor",
            "Commercial license required",
        ),
        "Infragistics" => (
            "Ultimate UI for ASP.NET",
            "IgniteUI for Blazor or MudBlazor",
            "Commercial license required",
        ),
        "ComponentArt" => (
            "Web.UI",
            "MudBlazor or Radzen",
            "ComponentArt discontinued; use open-source alternative",
        ),
        "Obout" => (
            "Suite for ASP.NET",
            "MudBlazor",
            "Obout discontinued; migrate to open-source",
        ),
        "EO.WebControls" => ("EO.Web", "MudBlazor", "Commercial"),
        "FarPoint" => (
            "Spread for ASP.NET",
            "SpreadJS or AG Grid",
            "Commercial license required",
        ),
        _ => (
            "Unknown Suite",
            "MudBlazor (open-source)",
            "Evaluate licensing",
        ),
    }
}
