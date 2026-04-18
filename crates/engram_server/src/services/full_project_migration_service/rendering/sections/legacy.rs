//! Extracted rendering sections: legacy.
//!
//! Phase 3 of the full_project_migration_service refactor split
//! `render_markdown` (2,524 lines) into per-section functions grouped
//! by topic. Each `render_section_*` function takes `md: &mut String`
//! plus every parameter of the original `render_markdown` - no
//! identifier rewriting happened during the move, so the rendered
//! bytes are identical to before.

#![allow(unused_imports, clippy::too_many_arguments, clippy::collapsible_else_if)]

use std::collections::BTreeMap;

use super::super::super::model::*;
use super::super::super::super::auth_config_service::AuthConfigMap;
use super::super::super::super::db_strategy_service::FileDataAccessProfile;
use super::super::super::super::dossier_service::MigrationDossier;
use super::super::super::super::migration_order_service::MigrationOrderPlan;
use super::super::super::super::state_migration_service::StateMigrationReport;
// Wildcard pulls in parent-level `pub(super)` helpers.
use super::super::super::*;


#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_third_party_control_libraries(
    md: &mut String,
    project_id: &str,
    target_stack: &str,
    generated_at: &str,
    order: &MigrationOrderPlan,
    state: &StateMigrationReport,
    auth: &AuthConfigMap,
    data_access: &[FileDataAccessProfile],
    dossiers: &[MigrationDossier],
    cross: &CrossCuttingSummary,
    wave_lookup: &BTreeMap<String, u32>,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    webconfig: &WebConfigInventory,
    endpoints: &ServiceEndpointSummary,
    global: &GlobalAsaxSummary,
    anti: &AntiPatternSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
    method_inv: &BTreeMap<String, PageMethodInventory>,
    third_party: &ThirdPartyControlSummary,
    dep_inv: &DependencyInventory,
    cache_inv: &CachingInventory,
    url_routing: &UrlRoutingInventory,
    vb_trans: &VbTranslationReport,
    multi_tenant: &MultiTenancyReport,
    email: &EmailPatternReport,
    bg_jobs: &BackgroundJobReport,
    sp_cat: &StoredProcedureCatalog,
    inherit: &InheritanceChainReport,
    cfg_transforms: &ConfigTransformReport,
    master_regions: &MasterPageRegionMap,
    res_inv: &ResourceInventory,
    vb_traps: &engram_index::vb_translation_traps::VbTranslationTrapReport,
    jquery_inv: &engram_index::jquery_inventory::JQueryInventory,
    cross_traces: &CrossLayerTraceSummary,
    biz_logic: &crate::services::business_logic_service::ProjectBusinessLogicReport,
    db_intel: &crate::services::database_intelligence_service::DatabaseIntelligence,
    session_wf: &crate::services::session_workflow_service::SessionWorkflowReport,
) {
    // ── Phase 33: Third-Party Control Libraries (Gap 2) ────────────────────
    if third_party.total_third_party_controls > 0 {
        md.push_str("## Third-Party Control Libraries\n\n");
        md.push_str(&format!(
            "**Vendors detected**: {}\n",
            third_party.vendors_detected.len()
        ));
        md.push_str(&format!(
            "**Total third-party controls**: {} across {} files\n\n",
            third_party.total_third_party_controls,
            third_party.files_with_third_party.len()
        ));

        for vendor in &third_party.vendors_detected {
            md.push_str(&format!("### {} {}\n", vendor.vendor, vendor.suite));
            let control_list: Vec<String> = vendor
                .controls_used
                .iter()
                .map(|(name, count)| format!("{name} ({count})"))
                .collect();
            md.push_str(&format!(
                "- **Controls used**: {}\n",
                control_list.join(", ")
            ));
            md.push_str(&format!(
                "- **Modern replacement ({target_stack})**: {}\n",
                vendor.modern_replacement_suite
            ));
            md.push_str(&format!("- **License**: {}\n\n", vendor.license_note));
        }

        if !third_party.unmapped_controls.is_empty() {
            md.push_str("### Unmapped Controls (no automatic mapping)\n");
            md.push_str("| Control | Vendor | File | Note |\n|---------|--------|------|------|\n");
            for uc in &third_party.unmapped_controls {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    uc.tag_name, uc.vendor, uc.file_path, uc.note
                ));
            }
            md.push('\n');
        }
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_design_anti_patterns(
    md: &mut String,
    project_id: &str,
    target_stack: &str,
    generated_at: &str,
    order: &MigrationOrderPlan,
    state: &StateMigrationReport,
    auth: &AuthConfigMap,
    data_access: &[FileDataAccessProfile],
    dossiers: &[MigrationDossier],
    cross: &CrossCuttingSummary,
    wave_lookup: &BTreeMap<String, u32>,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    webconfig: &WebConfigInventory,
    endpoints: &ServiceEndpointSummary,
    global: &GlobalAsaxSummary,
    anti: &AntiPatternSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
    method_inv: &BTreeMap<String, PageMethodInventory>,
    third_party: &ThirdPartyControlSummary,
    dep_inv: &DependencyInventory,
    cache_inv: &CachingInventory,
    url_routing: &UrlRoutingInventory,
    vb_trans: &VbTranslationReport,
    multi_tenant: &MultiTenancyReport,
    email: &EmailPatternReport,
    bg_jobs: &BackgroundJobReport,
    sp_cat: &StoredProcedureCatalog,
    inherit: &InheritanceChainReport,
    cfg_transforms: &ConfigTransformReport,
    master_regions: &MasterPageRegionMap,
    res_inv: &ResourceInventory,
    vb_traps: &engram_index::vb_translation_traps::VbTranslationTrapReport,
    jquery_inv: &engram_index::jquery_inventory::JQueryInventory,
    cross_traces: &CrossLayerTraceSummary,
    biz_logic: &crate::services::business_logic_service::ProjectBusinessLogicReport,
    db_intel: &crate::services::database_intelligence_service::DatabaseIntelligence,
    session_wf: &crate::services::session_workflow_service::SessionWorkflowReport,
) {
    // ── Design Anti-Patterns (Phase 32) ──────────────────────────────────
    if anti.total_anti_patterns > 0 {
        md.push_str("## Design Anti-Patterns\n\n");
        md.push_str(&format!(
            "**Total detected**: {}\n\n",
            anti.total_anti_patterns
        ));
        md.push_str("| Type | Count | Impact |\n|------|-------|--------|\n");
        let impact_map: std::collections::HashMap<&str, &str> = [
            (
                "God Object",
                "Must split before migration — too many responsibilities",
            ),
            (
                "Session Soup",
                "Blocks parallel migration — shared mutable state",
            ),
            (
                "Spaghetti Events",
                "Cross-file event chains — map dependencies carefully",
            ),
            (
                "SqlDataSource Coupling",
                "Inline SQL + data binding — extract to repository",
            ),
            (
                "Tight GIS Coupling",
                "GIS tightly bound to data — extract map service",
            ),
            (
                "Windows Service",
                "Background processing — migrate to IHostedService",
            ),
        ]
        .into_iter()
        .collect();
        for (name, count) in &anti.by_type {
            let impact = impact_map
                .get(name.as_str())
                .unwrap_or(&"Review before migration");
            md.push_str(&format!("| {name} | {count} | {impact} |\n"));
        }
        md.push('\n');

        if !anti.critical_items.is_empty() {
            md.push_str("### Critical Items\n");
            for item in &anti.critical_items {
                md.push_str(&format!(
                    "- **{}**: `{}` — {} → {}\n",
                    item.pattern_type, item.file_path, item.detail, item.recommendation
                ));
            }
            md.push('\n');
        }

        if !anti.migration_impact.is_empty() {
            md.push_str("### Migration Impact\n");
            for impact in &anti.migration_impact {
                md.push_str(&format!("- {impact}\n"));
            }
            md.push('\n');
        }
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_classic_asp(
    md: &mut String,
    project_id: &str,
    target_stack: &str,
    generated_at: &str,
    order: &MigrationOrderPlan,
    state: &StateMigrationReport,
    auth: &AuthConfigMap,
    data_access: &[FileDataAccessProfile],
    dossiers: &[MigrationDossier],
    cross: &CrossCuttingSummary,
    wave_lookup: &BTreeMap<String, u32>,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    webconfig: &WebConfigInventory,
    endpoints: &ServiceEndpointSummary,
    global: &GlobalAsaxSummary,
    anti: &AntiPatternSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
    method_inv: &BTreeMap<String, PageMethodInventory>,
    third_party: &ThirdPartyControlSummary,
    dep_inv: &DependencyInventory,
    cache_inv: &CachingInventory,
    url_routing: &UrlRoutingInventory,
    vb_trans: &VbTranslationReport,
    multi_tenant: &MultiTenancyReport,
    email: &EmailPatternReport,
    bg_jobs: &BackgroundJobReport,
    sp_cat: &StoredProcedureCatalog,
    inherit: &InheritanceChainReport,
    cfg_transforms: &ConfigTransformReport,
    master_regions: &MasterPageRegionMap,
    res_inv: &ResourceInventory,
    vb_traps: &engram_index::vb_translation_traps::VbTranslationTrapReport,
    jquery_inv: &engram_index::jquery_inventory::JQueryInventory,
    cross_traces: &CrossLayerTraceSummary,
    biz_logic: &crate::services::business_logic_service::ProjectBusinessLogicReport,
    db_intel: &crate::services::database_intelligence_service::DatabaseIntelligence,
    session_wf: &crate::services::session_workflow_service::SessionWorkflowReport,
) {
    // ── Classic ASP (Phase 32) ───────────────────────────────────────────
    if asp.total_asp_files > 0 {
        md.push_str("## Classic ASP Files\n\n");
        md.push_str(&format!(
            "**Files**: {} | **Estimated effort**: {:.0}h\n\n",
            asp.total_asp_files, asp.migration_effort_hours
        ));

        // Group COM objects by file
        let mut asp_by_file: BTreeMap<String, (Vec<String>, usize, Vec<String>, usize)> =
            BTreeMap::new();
        for co in &asp.com_objects {
            asp_by_file
                .entry(co.file_path.clone())
                .or_default()
                .0
                .push(co.prog_id.clone());
        }
        for inc in &asp.includes {
            asp_by_file
                .entry(inc.source_file.clone())
                .or_default()
                .2
                .push(inc.included_file.clone());
        }

        if !asp_by_file.is_empty() {
            md.push_str("| File | COM Objects | Includes |\n|------|-------------|----------|\n");
            for (file, (coms, _, incs, _)) in &asp_by_file {
                let com_list = if coms.is_empty() {
                    "(none)".into()
                } else {
                    coms.join(", ")
                };
                let inc_list = if incs.is_empty() {
                    "(none)".into()
                } else {
                    incs.join(", ")
                };
                md.push_str(&format!("| {file} | {com_list} | {inc_list} |\n"));
            }
            md.push('\n');
        }

        md.push_str("### Migration Path\n");
        md.push_str("- Classic ASP → ASP.NET Core Razor Pages or Blazor\n");
        md.push_str("- COM objects (ADODB) → Entity Framework Core / Dapper\n");
        md.push_str("- Server-side includes → Partial views / Razor components\n");
        md.push_str("- `Response.Write` → Razor template syntax\n\n");
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_reports(
    md: &mut String,
    project_id: &str,
    target_stack: &str,
    generated_at: &str,
    order: &MigrationOrderPlan,
    state: &StateMigrationReport,
    auth: &AuthConfigMap,
    data_access: &[FileDataAccessProfile],
    dossiers: &[MigrationDossier],
    cross: &CrossCuttingSummary,
    wave_lookup: &BTreeMap<String, u32>,
    js: &JsAnalysisSummary,
    gis: &GisAnalysisSummary,
    webconfig: &WebConfigInventory,
    endpoints: &ServiceEndpointSummary,
    global: &GlobalAsaxSummary,
    anti: &AntiPatternSummary,
    asp: &ClassicAspSummary,
    rpt: &ReportSummary,
    method_inv: &BTreeMap<String, PageMethodInventory>,
    third_party: &ThirdPartyControlSummary,
    dep_inv: &DependencyInventory,
    cache_inv: &CachingInventory,
    url_routing: &UrlRoutingInventory,
    vb_trans: &VbTranslationReport,
    multi_tenant: &MultiTenancyReport,
    email: &EmailPatternReport,
    bg_jobs: &BackgroundJobReport,
    sp_cat: &StoredProcedureCatalog,
    inherit: &InheritanceChainReport,
    cfg_transforms: &ConfigTransformReport,
    master_regions: &MasterPageRegionMap,
    res_inv: &ResourceInventory,
    vb_traps: &engram_index::vb_translation_traps::VbTranslationTrapReport,
    jquery_inv: &engram_index::jquery_inventory::JQueryInventory,
    cross_traces: &CrossLayerTraceSummary,
    biz_logic: &crate::services::business_logic_service::ProjectBusinessLogicReport,
    db_intel: &crate::services::database_intelligence_service::DatabaseIntelligence,
    session_wf: &crate::services::session_workflow_service::SessionWorkflowReport,
) {
    // ── Reports (Phase 32) ──────────────────────────────────────────────
    if rpt.total_reports > 0 {
        md.push_str("## Reports (SSRS / Crystal)\n\n");
        md.push_str(&format!(
            "**SSRS reports**: {} | **Crystal Reports**: {}\n\n",
            rpt.ssrs_reports.len(),
            rpt.crystal_reports.len()
        ));

        if !rpt.ssrs_reports.is_empty() {
            md.push_str("### SSRS Reports\n");
            md.push_str("| File | Datasets | Parameters | Subreports | Target |\n");
            md.push_str("|------|----------|------------|------------|--------|\n");
            for r in &rpt.ssrs_reports {
                let ds = if r.datasets.is_empty() {
                    "(none)".into()
                } else {
                    r.datasets.join(", ")
                };
                let sub = if r.subreports.is_empty() {
                    "(none)".into()
                } else {
                    r.subreports.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    r.file_path, ds, r.parameters, sub, r.migration_target
                ));
            }
            md.push('\n');
        }

        if !rpt.crystal_reports.is_empty() {
            md.push_str("### Crystal Reports\n");
            md.push_str("| File | Report (.rpt) | Binary | Modern Equivalent |\n");
            md.push_str("|------|--------------|--------|-------------------|\n");
            for cr in &rpt.crystal_reports {
                let rpt_name = if cr.report_file.is_empty() {
                    "(embedded)".into()
                } else {
                    cr.report_file.clone()
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    cr.file_path, rpt_name, cr.is_binary, cr.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if rpt.has_binary_rpt_files {
            md.push_str(&format!("**Warning**: {} binary .rpt files cannot be automatically migrated — manual recreation required\n\n",
                rpt.crystal_reports.len()));
        }

        if !rpt.shared_data_sources.is_empty() {
            md.push_str(&format!(
                "**Shared data sources**: {}\n\n",
                rpt.shared_data_sources.join(", ")
            ));
        }
    }

}
