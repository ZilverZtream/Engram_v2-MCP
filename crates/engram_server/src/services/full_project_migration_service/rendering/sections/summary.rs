//! Extracted rendering sections: summary.
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
pub(crate) fn render_section_prelude(
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
    // No-op: the original render_markdown allocated `md` here. The
    // orchestrator now owns that allocation and passes `md` in.
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_header(
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
    // ── Header ────────────────────────────────────────────────────────────
    md.push_str(&format!(
        "# Full Migration Analysis — {project_id}\n\n\
         Generated: {generated_at} | Target: **{target_stack}**\n\n"
    ));

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_executive_summary(
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
    // ── Executive Summary ─────────────────────────────────────────────────
    md.push_str("## Executive Summary\n\n");
    md.push_str(&format!(
        "- **Total pages analyzed**: {}\n",
        cross.total_pages_analyzed
    ));
    for (complexity, count) in &cross.complexity_distribution {
        md.push_str(&format!("- {complexity} complexity: {count} files\n"));
    }
    md.push_str(&format!("- **Migration waves**: {}\n", order.waves.len()));
    md.push_str(&format!(
        "- **Circular dependencies**: {}\n",
        order.circular_dependencies.len()
    ));
    md.push_str(&format!(
        "- **Bottleneck files**: {}\n",
        order.bottleneck_files.len()
    ));
    md.push_str(&format!(
        "- **Total state keys**: {}\n",
        state.summary.total_state_keys
    ));
    md.push_str(&format!(
        "- **High-risk state keys**: {}\n",
        state.summary.high_risk_keys.len()
    ));
    md.push_str(&format!(
        "- **Total validators**: {}\n",
        cross.total_validators
    ));
    md.push_str(&format!(
        "- **Total UpdatePanels**: {}\n",
        cross.total_update_panels
    ));
    md.push_str(&format!(
        "- **Files with IsPostBack branching**: {}\n",
        cross.files_with_ispostback
    ));
    if cross.total_script_files > 0 {
        md.push_str(&format!(
            "- **Client script files (.js/.ts/.tsx/.jsx)**: {} ({} with server-side dependencies)\n",
            cross.total_script_files, js.script_files_with_server_deps
        ));
    }
    if cross.total_gis_libraries > 0 {
        md.push_str(&format!(
            "- **GIS libraries**: {}\n",
            cross.total_gis_libraries
        ));
    }
    if cross.total_anti_patterns > 0 {
        md.push_str(&format!(
            "- **Design anti-patterns**: {}\n",
            cross.total_anti_patterns
        ));
    }
    if cross.total_service_endpoints > 0 {
        md.push_str(&format!(
            "- **Service endpoints**: {} (ASMX/ASHX/WCF/Modules)\n",
            cross.total_service_endpoints
        ));
    }
    if cross.total_classic_asp_files > 0 {
        md.push_str(&format!(
            "- **Classic ASP files**: {}\n",
            cross.total_classic_asp_files
        ));
    }
    if cross.total_reports > 0 {
        md.push_str(&format!(
            "- **Reports (SSRS/Crystal)**: {}\n",
            cross.total_reports
        ));
    }
    if !cross.critical_risk_files.is_empty() {
        md.push_str(&format!(
            "- **Critical-risk files**: {}\n",
            cross.critical_risk_files.join(", ")
        ));
    }
    if vb_trans.vb_file_count > 0 {
        md.push_str(&format!(
            "- **Dynamic-dispatch risk tier**: {} (Option Strict Off files: {}, dynamic methods: {})\n",
            cross.dynamic_dispatch_risk_tier,
            cross.option_strict_off_files,
            cross.dynamic_dispatch_methods
        ));
    }
    md.push('\n');

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_project_dependencies(
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
    // ── Phase 33: Project Dependencies (Gap 3) ───────────────────────────
    if dep_inv.total_packages > 0 || dep_inv.total_assemblies > 0 {
        md.push_str("## Project Dependencies\n\n");
        if let Some(tf) = dep_inv.target_frameworks.first() {
            md.push_str(&format!("**Target Framework**: {tf}\n"));
        }
        md.push_str(&format!(
            "**NuGet Packages**: {} ({} have modern replacements, {} need manual evaluation)\n",
            dep_inv.total_packages,
            dep_inv.packages_with_known_replacement,
            dep_inv.packages_without_replacement
        ));
        md.push_str(&format!(
            "**Assembly References**: {} ({} framework, {} third-party)\n",
            dep_inv.total_assemblies,
            dep_inv.framework_assemblies.len(),
            dep_inv.third_party_assemblies.len()
        ));
        md.push_str(&format!(
            "**Project References**: {}\n\n",
            dep_inv.project_references.len()
        ));

        if !dep_inv.nuget_packages.is_empty() {
            md.push_str("### NuGet Packages\n");
            md.push_str("| Package | Version | Modern Replacement | Category | Notes |\n");
            md.push_str("|---------|---------|-------------------|----------|-------|\n");
            for pkg in &dep_inv.nuget_packages {
                let ver = pkg.version.as_deref().unwrap_or("-");
                let modern = pkg.modern_replacement.as_deref().unwrap_or("(evaluate)");
                let notes = pkg.migration_notes.as_deref().unwrap_or("");
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    pkg.name, ver, modern, pkg.category, notes
                ));
            }
            md.push('\n');
        }

        let removable: Vec<&AssemblyRefInfo> = dep_inv
            .assembly_references
            .iter()
            .filter(|a| a.removal_reason.is_some())
            .collect();
        if !removable.is_empty() {
            md.push_str("### Framework Assemblies Requiring Replacement\n");
            md.push_str("| Assembly | Status in .NET Core | Migration Path |\n");
            md.push_str("|----------|--------------------|--------------|\n");
            for asm in &removable {
                let reason = asm.removal_reason.as_deref().unwrap_or("");
                let modern = asm.modern_equivalent.as_deref().unwrap_or("(none)");
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    asm.assembly_name, reason, modern
                ));
            }
            md.push('\n');
        }

        let compatible: Vec<&NuGetPackageInfo> = dep_inv
            .nuget_packages
            .iter()
            .filter(|p| {
                p.modern_replacement
                    .as_deref()
                    .is_some_and(|m| m.contains("compatible") || m == p.name)
            })
            .collect();
        if !compatible.is_empty() {
            let names: Vec<&str> = compatible.iter().map(|p| p.name.as_str()).collect();
            md.push_str(&format!(
                "### Compatible Packages (no action needed)\n{}\n\n",
                names.join(", ")
            ));
        }
        md.push('\n');
    }

}
