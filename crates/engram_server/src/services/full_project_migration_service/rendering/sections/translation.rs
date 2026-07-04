//! Extracted rendering sections: translation.
//!
//! Phase 3 of the full_project_migration_service refactor split
//! `render_markdown` (2,524 lines) into per-section functions grouped
//! by topic. Each `render_section_*` function takes `md: &mut String`
//! plus every parameter of the original `render_markdown` - no
//! identifier rewriting happened during the move, so the rendered
//! bytes are identical to before.

#![allow(
    unused_imports,
    clippy::too_many_arguments,
    clippy::collapsible_else_if
)]

use std::collections::BTreeMap;

use super::super::super::super::auth_config_service::AuthConfigMap;
use super::super::super::super::db_strategy_service::FileDataAccessProfile;
use super::super::super::super::dossier_service::MigrationDossier;
use super::super::super::super::migration_order_service::MigrationOrderPlan;
use super::super::super::super::state_migration_service::StateMigrationReport;
use super::super::super::model::*;
// Wildcard pulls in parent-level `pub(super)` helpers.
use super::super::super::*;

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_language_translation_analysis(
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
    // ── Phase 33: Language & Translation Analysis (Gap 6) ────────────────
    if vb_trans.vb_file_count > 0 || vb_trans.cs_file_count > 0 {
        md.push_str("## Language & Translation Analysis\n\n");
        let primary = if vb_trans.is_vb_project {
            "VB.NET"
        } else {
            "C#"
        };
        md.push_str(&format!(
            "**Primary language**: {primary} ({} files)\n",
            if vb_trans.is_vb_project {
                vb_trans.vb_file_count
            } else {
                vb_trans.cs_file_count
            }
        ));
        if vb_trans.mixed_language {
            let secondary = if vb_trans.is_vb_project {
                "C#"
            } else {
                "VB.NET"
            };
            let sec_count = if vb_trans.is_vb_project {
                vb_trans.cs_file_count
            } else {
                vb_trans.vb_file_count
            };
            md.push_str(&format!(
                "**Secondary language**: {secondary} ({sec_count} files)\n"
            ));
        }
        md.push_str(&format!(
            "**Translation flags**: {} across files\n\n",
            vb_trans.total_flags
        ));
        md.push_str(&format!(
            "**Dynamic-dispatch risk tier**: {}\n",
            vb_trans.dynamic_dispatch.dynamic_dispatch_risk_tier
        ));
        md.push_str(&format!(
            "**Option Strict**: On in {} file(s), Off in {} file(s)\n",
            vb_trans.dynamic_dispatch.option_strict_on_files,
            vb_trans.dynamic_dispatch.option_strict_off_files
        ));
        md.push_str(&format!(
            "**Dynamic-dispatch counters**: {} late-bound call(s), {} `As Object` declaration(s), {} `CallByName` call(s) across {} method(s)\n\n",
            vb_trans.dynamic_dispatch.late_binding_call_count,
            vb_trans.dynamic_dispatch.object_var_count,
            vb_trans.dynamic_dispatch.callbyname_count,
            vb_trans.dynamic_dispatch.methods_with_dynamic_dispatch
        ));

        if !vb_trans.flags_by_category.is_empty() {
            md.push_str("### Translation Risk Summary\n");
            md.push_str("| Category | Count | Risk | Auto-Translatable |\n");
            md.push_str("|----------|-------|------|-------------------|\n");
            for (cat, count) in &vb_trans.flags_by_category {
                let risk = vb_trans
                    .translation_flags
                    .iter()
                    .find(|f| &f.category == cat)
                    .map(|f| f.risk_level.as_str())
                    .unwrap_or("low");
                let auto_val = vb_trans
                    .translation_flags
                    .iter()
                    .find(|f| &f.category == cat)
                    .map(|f| if f.auto_translatable { "Yes" } else { "No" })
                    .unwrap_or("No");
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    cat, count, risk, auto_val
                ));
            }
            md.push('\n');
        }

        if !vb_trans.highest_risk_files.is_empty() {
            md.push_str("### Highest-Risk Files (most translation flags)\n");
            md.push_str("| File | Flags |\n|------|-------|\n");
            for (path, count) in &vb_trans.highest_risk_files {
                md.push_str(&format!("| {} | {} |\n", path, count));
            }
            md.push('\n');
        }

        if vb_trans.is_vb_project {
            md.push_str("### Migration Strategy\n");
            md.push_str("1. Run automated VB→C# converter (dotnet-vb2cs or Instant C#) for mechanical translations\n");
            let on_error = vb_trans
                .flags_by_category
                .get("ErrorHandling")
                .copied()
                .unwrap_or(0);
            if on_error > 0 {
                md.push_str(&format!("2. Manually fix {on_error} `On Error Resume Next` patterns → proper try-catch\n"));
            }
            let late = vb_trans
                .flags_by_category
                .get("LateBind")
                .copied()
                .unwrap_or(0);
            if late > 0 {
                md.push_str(&format!("3. Convert {late} `Dim x As Object` late bindings → `dynamic` or typed interfaces\n"));
            }
            md.push('\n');
        }
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_vb_translation_traps(
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
    // ── Phase 35: VB Translation Traps ──────────────────────────────────
    if vb_traps.total_traps > 0 {
        md.push_str("## VB.NET Translation Traps\n\n");
        md.push_str(&format!(
            "**Total traps**: {} | **Silent bugs**: {} | **Compile errors**: {} | **Files analyzed**: {}\n\n",
            vb_traps.total_traps,
            vb_traps.silent_bug_count,
            vb_traps.compile_error_count,
            vb_traps.files_analyzed,
        ));
        md.push_str("| Trap | Location | Risk | VB Code | Guidance |\n");
        md.push_str("|------|----------|------|---------|----------|\n");
        for trap in vb_traps.traps.iter().take(50) {
            let code_escaped = trap.vb_code.replace('|', "\\|");
            let guidance_escaped = trap.guidance.replace('|', "\\|");
            let guidance_short = if guidance_escaped.len() > 80 {
                // Truncate at a safe char boundary
                let end = guidance_escaped
                    .char_indices()
                    .nth(80)
                    .map(|(i, _)| i)
                    .unwrap_or(guidance_escaped.len());
                format!("{}...", &guidance_escaped[..end])
            } else {
                guidance_escaped
            };
            md.push_str(&format!(
                "| {} | `{}` | {} | `{}` | {} |\n",
                trap.trap, trap.location, trap.risk, code_escaped, guidance_short
            ));
        }
        if vb_traps.total_traps > 50 {
            md.push_str(&format!(
                "\n*... and {} more traps (see JSON output for full list)*\n",
                vb_traps.total_traps - 50
            ));
        }
        md.push('\n');
    }
}
