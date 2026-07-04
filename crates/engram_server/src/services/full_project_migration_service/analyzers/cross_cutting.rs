//! Extracted analyzer: cross cutting.
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_cross_cutting_summary(
    dossiers: &[MigrationDossier],
    state_report: &StateMigrationReport,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    ap: &AntiPatternSummary,
    se: &ServiceEndpointSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
    method_inv: &BTreeMap<String, PageMethodInventory>,
    dep_inv: &DependencyInventory,
    cache_inv: &CachingInventory,
    email: &EmailPatternReport,
    bg_jobs: &BackgroundJobReport,
    sp_cat: &StoredProcedureCatalog,
    inherit: &InheritanceChainReport,
    cfg_transforms: &ConfigTransformReport,
    res_inv: &ResourceInventory,
    master_regions: &MasterPageRegionMap,
    vb_translation: &VbTranslationReport,
) -> CrossCuttingSummary {
    let mut complexity_distribution: BTreeMap<String, usize> = BTreeMap::new();
    let mut risk_distribution: BTreeMap<String, usize> = BTreeMap::new();
    let mut sql_table_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut control_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut critical_risk_files = Vec::new();
    let mut total_validators = 0usize;
    let mut total_update_panels = 0usize;
    let mut total_lifecycle_events = 0usize;
    let mut files_with_ispostback = 0usize;

    for d in dossiers {
        // Complexity distribution
        *complexity_distribution
            .entry(d.estimated_complexity.clone())
            .or_insert(0) += 1;

        // Risk distribution
        let risk_band = match d.blast_radius_score {
            0..=3 => "Low",
            4..=6 => "Medium",
            7..=8 => "High",
            _ => "Critical",
        };
        *risk_distribution.entry(risk_band.to_string()).or_insert(0) += 1;

        if d.blast_radius_score >= 9 {
            critical_risk_files.push(d.file_path.clone());
        }

        // Shared SQL tables
        for table in &d.tables_touched {
            sql_table_map
                .entry(table.clone())
                .or_default()
                .push(d.file_path.clone());
        }

        // Shared user controls
        for uc in &d.user_controls {
            control_map
                .entry(uc.control_path.clone())
                .or_default()
                .push(d.file_path.clone());
        }

        // Validators
        total_validators +=
            d.validation_summary.validator_count + d.validation_summary.custom_validator_count;

        // UpdatePanels
        total_update_panels += d.ajax_summary.update_panel_count;

        // Lifecycle events
        total_lifecycle_events +=
            d.lifecycle_summary.lifecycle_event_count + d.lifecycle_summary.control_event_count;

        if d.lifecycle_summary.has_ispostback_logic {
            files_with_ispostback += 1;
        }
    }

    // Shared state keys from project-wide state_migration report
    let mut state_key_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for rec in &state_report.recommendations {
        let mut all_files: Vec<String> = rec.readers.clone();
        all_files.extend(rec.writers.iter().cloned());
        all_files.sort();
        all_files.dedup();
        if !all_files.is_empty() {
            state_key_map.insert(rec.state_key.clone(), all_files);
        }
    }

    // Filter to only items shared by 2+ files
    let shared_sql_tables = sql_table_map
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(name, mut used_by)| {
            used_by.sort();
            used_by.dedup();
            SharedItem { name, used_by }
        })
        .collect();

    let shared_state_keys = state_key_map
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(name, used_by)| SharedItem { name, used_by })
        .collect();

    let shared_user_controls = control_map
        .into_iter()
        .filter(|(_, files)| files.len() >= 2)
        .map(|(name, mut used_by)| {
            used_by.sort();
            used_by.dedup();
            SharedItem { name, used_by }
        })
        .collect();

    // Phase 33 method aggregation
    let mut total_methods = 0usize;
    let mut total_event_handlers = 0usize;
    let mut total_web_methods = 0usize;
    let mut largest_file_by_methods: Option<(String, usize)> = None;
    for (path, inv) in method_inv {
        total_methods += inv.total_methods;
        total_event_handlers += inv.event_handlers;
        total_web_methods += inv.web_methods;
        if largest_file_by_methods
            .as_ref()
            .is_none_or(|(_, c)| inv.total_methods > *c)
            && inv.total_methods > 0
        {
            largest_file_by_methods = Some((path.clone(), inv.total_methods));
        }
    }

    CrossCuttingSummary {
        total_pages_analyzed: dossiers.len(),
        complexity_distribution,
        shared_sql_tables,
        shared_state_keys,
        shared_user_controls,
        risk_distribution,
        critical_risk_files,
        total_validators,
        total_update_panels,
        total_lifecycle_events,
        files_with_ispostback,
        total_script_files: js.total_script_files,
        legacy_total_js_files: js.total_script_files,
        total_gis_libraries: gis.libraries_detected.len(),
        total_anti_patterns: ap.total_anti_patterns,
        total_service_endpoints: se.total_endpoints,
        total_classic_asp_files: asp.total_asp_files,
        total_reports: rpt.total_reports,
        // Phase 33
        total_methods,
        total_event_handlers,
        total_web_methods,
        largest_file_by_methods,
        total_nuget_packages: dep_inv.total_packages,
        target_framework: dep_inv
            .target_frameworks
            .first()
            .cloned()
            .unwrap_or_default(),
        total_cached_pages: cache_inv.total_cached_pages,
        total_cache_keys: cache_inv.total_cache_keys,
        has_email: email.has_email,
        has_background_jobs: bg_jobs.has_background_jobs,
        // Phase 34 aggregation
        total_stored_procedures: sp_cat.total_procedures,
        total_sp_called_from_code: sp_cat.procedures_called_from_code,
        deepest_inheritance_chain: inherit.deepest_chain_depth,
        total_base_classes: inherit.base_classes.len(),
        total_config_environments: cfg_transforms.environments.len(),
        total_resource_files: res_inv.resource_files.len(),
        total_resource_languages: res_inv.languages_detected.len(),
        total_master_page_regions: master_regions.regions.len(),
        total_legacy_packages: dep_inv.legacy_packages.len(),
        option_strict_on_files: vb_translation.dynamic_dispatch.option_strict_on_files,
        option_strict_off_files: vb_translation.dynamic_dispatch.option_strict_off_files,
        dynamic_dispatch_methods: vb_translation
            .dynamic_dispatch
            .methods_with_dynamic_dispatch,
        dynamic_dispatch_risk_tier: vb_translation
            .dynamic_dispatch
            .dynamic_dispatch_risk_tier
            .clone(),
    }
}
