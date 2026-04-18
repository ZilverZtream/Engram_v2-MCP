//! Extracted rendering sections: operations.
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
pub(crate) fn render_section_email_notifications(
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
    // ── Phase 33: Email & Notifications (Gap 8) ────────────────────────────
    if email.has_email {
        md.push_str("## Email & Notifications\n\n");
        md.push_str(&format!(
            "**Email sending**: Yes ({} files)\n",
            email.total_email_files
        ));
        if let Some(ref cfg) = email.smtp_config {
            let host = cfg.host.as_deref().unwrap_or("unknown");
            let port = cfg.port.map_or("-".to_string(), |p| p.to_string());
            md.push_str(&format!(
                "**SMTP config**: {}:{} (SSL: {}, credentials: {})\n",
                host, port, cfg.uses_ssl, cfg.uses_credentials
            ));
        }
        md.push_str(&format!(
            "**HTML email**: {}\n",
            if email.uses_html_email { "Yes" } else { "No" }
        ));
        md.push_str(&format!(
            "**Attachments**: {}\n",
            if email.uses_attachments { "Yes" } else { "No" }
        ));
        if email.uses_legacy_cdo {
            md.push_str("**Legacy CDO**: Yes (COM interop)\n");
        }
        if email.uses_legacy_web_mail {
            md.push_str("**Legacy System.Web.Mail**: Yes (obsolete)\n");
        }
        md.push('\n');

        if !email.email_patterns.is_empty() {
            md.push_str("### Email Usage\n");
            md.push_str("| File | Pattern | Count | Modern Equivalent |\n");
            md.push_str("|------|---------|-------|-------------------|\n");
            for ep in &email.email_patterns {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ep.file_path, ep.pattern_type, ep.count, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Strategy\n");
        md.push_str(
            "- `SmtpClient` → **Obsolete in .NET 6+** — replace with `IEmailSender` abstraction\n",
        );
        md.push_str("- Register `IEmailSender` implementation: SendGrid, Mailgun, or Azure Communication Services\n");
        md.push_str("- HTML email templates → Razor templates with strongly-typed models\n");
        md.push_str("- SMTP config → `appsettings.json` service configuration\n\n");
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_background_processing(
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
    // ── Phase 33: Background Processing (Gap 8) ──────────────────────────
    if bg_jobs.has_background_jobs {
        md.push_str("## Background Processing\n\n");
        md.push_str(&format!(
            "**Background jobs**: Yes ({} files)\n",
            bg_jobs.total_background_files
        ));
        md.push_str(&format!(
            "**Fire-and-forget**: {} (HIGH RISK)\n",
            bg_jobs.fire_and_forget_count
        ));
        if bg_jobs.uses_timers {
            md.push_str("**Timers**: Yes\n");
        }
        if bg_jobs.uses_thread_pool {
            md.push_str("**ThreadPool**: Yes\n");
        }
        if bg_jobs.uses_task_run {
            md.push_str("**Task.Run**: Yes\n");
        }
        if bg_jobs.uses_hangfire {
            md.push_str("**Hangfire**: Yes (already compatible)\n");
        }
        if bg_jobs.uses_quartz {
            md.push_str("**Quartz.NET**: Yes (already compatible)\n");
        }
        md.push('\n');

        if !bg_jobs.patterns.is_empty() {
            md.push_str("### Background Job Inventory\n");
            md.push_str("| File | Pattern | Count | Risk | Modern Equivalent |\n");
            md.push_str("|------|---------|-------|------|-------------------|\n");
            for bp in &bg_jobs.patterns {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    bp.file_path, bp.pattern_type, bp.count, bp.risk_level, bp.modern_equivalent
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Strategy\n");
        md.push_str("- `ThreadPool.QueueUserWorkItem` → `BackgroundService` + `Channel<T>`\n");
        md.push_str("- `System.Timers.Timer` → `IHostedService` with `PeriodicTimer`\n");
        md.push_str("- `Task.Run()` fire-and-forget → Hangfire `BackgroundJob.Enqueue()` or `IHostedService`\n");
        md.push_str(
            "- `BackgroundWorker` → `BackgroundService` (same pattern, different base class)\n",
        );
        if bg_jobs.fire_and_forget_count > 0 {
            md.push_str(&format!(
                "- **WARNING**: {} fire-and-forget patterns will silently fail in ASP.NET Core\n",
                bg_jobs.fire_and_forget_count
            ));
        }
        md.push('\n');
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_master_page_region_map(
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
    // ── Phase 34: Master Page Region Map ──────────────────────────────────
    if !master_regions.master_pages.is_empty() {
        md.push_str("## Master Page Layout Regions\n\n");
        md.push_str(&format!(
            "**Master pages**: {} | **Content regions**: {}\n\n",
            master_regions.master_pages.len(),
            master_regions.regions.len()
        ));

        md.push_str("| Region | Defined In | Pages Filling | Has Default | Modern Equivalent |\n");
        md.push_str("|--------|-----------|---------------|-------------|-------------------|\n");
        for region in &master_regions.regions {
            md.push_str(&format!(
                "| `{}` | `{}` | {} | {} | `{}` |\n",
                region.region_name,
                region.defined_in,
                region.filled_by.len(),
                if region.has_default_content {
                    "Yes"
                } else {
                    "No"
                },
                region.modern_equivalent
            ));
        }
        md.push('\n');

        if !master_regions.orphan_regions.is_empty() {
            md.push_str("**Orphan regions** (defined but never filled):\n");
            for r in &master_regions.orphan_regions {
                md.push_str(&format!("- `{r}`\n"));
            }
            md.push('\n');
        }
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_resource_file_inventory(
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
    // ── Phase 34: Resource File Inventory ─────────────────────────────────
    if !res_inv.resource_files.is_empty() {
        md.push_str("## Resource Files (.resx)\n\n");
        md.push_str(&format!(
            "**Total files**: {} | **Total keys**: {} | **Languages**: {}\n",
            res_inv.resource_files.len(),
            res_inv.total_keys,
            if res_inv.languages_detected.is_empty() {
                "default only".to_string()
            } else {
                res_inv.languages_detected.join(", ")
            }
        ));
        if res_inv.has_global_resources {
            md.push_str("- Uses `App_GlobalResources` → migrate to `IStringLocalizer`\n");
        }
        if res_inv.has_local_resources {
            md.push_str(
                "- Uses `App_LocalResources` → migrate to page-specific `IStringLocalizer`\n",
            );
        }
        if res_inv.embedded_resource_count > 0 {
            md.push_str(&format!(
                "- {} embedded resources (images, files)\n",
                res_inv.embedded_resource_count
            ));
        }
        md.push('\n');

        md.push_str("| File | Keys | Language | Type |\n");
        md.push_str("|------|------|----------|------|\n");
        for rf in res_inv.resource_files.iter().take(30) {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                rf.file_path,
                rf.key_count,
                rf.language.as_deref().unwrap_or("default"),
                rf.resource_type
            ));
        }
        md.push('\n');
    }

}
