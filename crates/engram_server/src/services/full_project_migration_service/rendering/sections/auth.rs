//! Extracted rendering sections: auth.
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
pub(crate) fn render_section_authentication_authorization(
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
    // ── Authentication & Authorization ────────────────────────────────────
    md.push_str("## Authentication & Authorization\n\n");
    md.push_str(&format!(
        "**Auth mode**: {} | **Complexity**: {}\n\n",
        auth.auth_mode, auth.migration_complexity
    ));
    if let Some(ref fa) = auth.forms_auth {
        md.push_str(&format!(
            "- Forms Auth: login=`{}`, timeout={}min, cookie=`{}`\n",
            fa.login_url, fa.timeout_minutes, fa.cookie_name
        ));
    }
    if auth.windows_auth.is_some() {
        md.push_str("- Windows Authentication detected\n");
    }
    if !auth.location_rules.is_empty() {
        md.push_str(&format!(
            "- {} location authorization rules\n",
            auth.location_rules.len()
        ));
        for lr in &auth.location_rules {
            md.push_str(&format!("  - `{}`: ", lr.path));
            if !lr.allow_roles.is_empty() {
                md.push_str(&format!("allow [{}] ", lr.allow_roles.join(", ")));
            }
            if !lr.deny_users.is_empty() {
                md.push_str(&format!("deny [{}]", lr.deny_users.join(", ")));
            }
            md.push('\n');
        }
    }
    if !auth.code_auth_checks.is_empty() {
        md.push_str(&format!(
            "- {} code-level auth checks across {} files\n",
            auth.code_auth_checks.len(),
            {
                let mut files: Vec<&str> = auth
                    .code_auth_checks
                    .iter()
                    .map(|c| c.file_path.as_str())
                    .collect();
                files.sort();
                files.dedup();
                files.len()
            }
        ));
    }
    if !auth.session_auth_patterns.is_empty() {
        md.push_str(&format!(
            "- **{} session-based auth anti-patterns** (must migrate to Identity)\n",
            auth.session_auth_patterns.len()
        ));
    }
    if !auth.recommendations.is_empty() {
        md.push_str("\n**Recommendations:**\n");
        for r in &auth.recommendations {
            md.push_str(&format!(
                "- [{}] {}: {}\n",
                r.severity, r.category, r.recommendation
            ));
        }
    }
    md.push('\n');

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_multi_tenancy_analysis(
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
    // ── Phase 33: Multi-Tenancy Analysis (Gap 7) ──────────────────────────
    if multi_tenant.is_multi_tenant {
        md.push_str("## Multi-Tenancy Analysis\n\n");
        md.push_str(&format!(
            "**Multi-tenant**: Yes (confidence: {})\n",
            multi_tenant.confidence
        ));
        if let Some(ref col) = multi_tenant.tenant_id_column_name {
            md.push_str(&format!("**Tenant ID column**: `{col}`\n"));
        }
        if let Some(ref strat) = multi_tenant.isolation_strategy {
            md.push_str(&format!("**Isolation strategy**: {strat}\n"));
        }
        if let Some(ref res) = multi_tenant.tenant_resolution {
            md.push_str(&format!(
                "**Tenant resolution**: {} via `{}`\n",
                res.mechanism, res.file_path
            ));
        }
        md.push_str(&format!(
            "**Tenant-filtered queries**: {}\n",
            multi_tenant.tenant_filtered_queries
        ));
        md.push_str(&format!(
            "**Files with tenant logic**: {}\n\n",
            multi_tenant.files_with_tenant_logic.len()
        ));

        if !multi_tenant.detection_evidence.is_empty() {
            md.push_str("### Detection Evidence\n");
            md.push_str("| Type | File | Detail |\n|------|------|--------|\n");
            for ev in &multi_tenant.detection_evidence {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ev.evidence_type, ev.file_path, ev.detail
                ));
            }
            md.push('\n');
        }

        if !multi_tenant.migration_recommendations.is_empty() {
            md.push_str("### Modern Migration Strategy\n");
            for (i, rec) in multi_tenant.migration_recommendations.iter().enumerate() {
                md.push_str(&format!("{}. {rec}\n", i + 1));
            }
            md.push('\n');
        }
    }

}
