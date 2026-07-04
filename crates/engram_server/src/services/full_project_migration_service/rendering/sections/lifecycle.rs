//! Extracted rendering sections: lifecycle.
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
pub(crate) fn render_section_global_asax(
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
    // ── Global.asax (Phase 32) ───────────────────────────────────────────
    if global.has_global_asax {
        md.push_str("## Application Lifecycle (Global.asax)\n\n");
        if let Some(ref cls) = global.codebehind_class {
            md.push_str(&format!("**Class**: `{cls}`\n\n"));
        }
        if !global.lifecycle_events.is_empty() {
            md.push_str("### Lifecycle Events\n");
            md.push_str("| Event | Lines | Key Actions | Modern Equivalent |\n");
            md.push_str("|-------|-------|-------------|-------------------|\n");
            for ev in &global.lifecycle_events {
                let actions = if ev.key_actions.is_empty() {
                    "(none detected)".to_string()
                } else {
                    ev.key_actions.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ev.event_name, ev.line_count, actions, ev.modern_equivalent
                ));
            }
            md.push('\n');
        }
        if !global.startup_registrations.is_empty() {
            md.push_str("### Startup Registrations (→ Program.cs)\n");
            for reg in &global.startup_registrations {
                md.push_str(&format!(
                    "- **{}**: {}\n",
                    reg.registration_type, reg.detail
                ));
            }
            md.push('\n');
        }
        if !global.modern_mapping.is_empty() {
            md.push_str("### Migration Notes\n");
            for mm in &global.modern_mapping {
                md.push_str(&format!("- {} → {}\n", mm.legacy, mm.modern));
            }
            md.push('\n');
        }
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_state_management(
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
    // ── State Management ──────────────────────────────────────────────────
    md.push_str("## State Management (Project-Wide)\n\n");
    md.push_str(&format!(
        "**Total state keys**: {}\n\n",
        state.summary.total_state_keys
    ));
    if !state.summary.by_store.is_empty() {
        md.push_str("| Store | Keys |\n|-------|------|\n");
        for (store, count) in &state.summary.by_store {
            md.push_str(&format!("| {store} | {count} |\n"));
        }
        md.push('\n');
    }
    if !state.summary.by_target.is_empty() {
        md.push_str("**Migration targets:**\n");
        for (target, count) in &state.summary.by_target {
            md.push_str(&format!("- {target}: {count} keys\n"));
        }
        md.push('\n');
    }
    if !state.summary.high_risk_keys.is_empty() {
        md.push_str("**High-risk keys:**\n");
        for k in &state.summary.high_risk_keys {
            md.push_str(&format!("- `{k}`\n"));
        }
        md.push('\n');
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_caching_strategy(
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
    // ── Phase 33: Caching Strategy (Gap 4) ─────────────────────────────────
    if cache_inv.total_cached_pages > 0
        || cache_inv.total_cache_keys > 0
        || cache_inv.has_response_caching
    {
        md.push_str("## Caching Strategy\n\n");
        md.push_str(&format!(
            "**Output-cached pages**: {}\n",
            cache_inv.total_cached_pages
        ));
        md.push_str(&format!(
            "**Programmatic cache keys**: {}\n",
            cache_inv.total_cache_keys
        ));
        md.push_str(&format!(
            "**Response-cached files**: {}\n",
            cache_inv.response_cache_files.len()
        ));
        md.push_str(&format!(
            "**SQL cache dependencies**: {}\n\n",
            cache_inv.sql_cache_dependencies.len()
        ));

        if !cache_inv.output_cache_pages.is_empty() {
            md.push_str("### Page/Control Output Caching\n");
            md.push_str("| Page | Duration | VaryByParam | Location | Modern Equivalent |\n");
            md.push_str("|------|----------|-------------|----------|-------------------|\n");
            for oc in &cache_inv.output_cache_pages {
                let dur = oc
                    .duration_seconds
                    .map_or("-".to_string(), |d| format!("{d}s"));
                let vbp = oc.vary_by_param.as_deref().unwrap_or("-");
                let loc = oc.location.as_deref().unwrap_or("-");
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    oc.file_path, dur, vbp, loc, oc.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !cache_inv.programmatic_cache_keys.is_empty() {
            md.push_str("### Programmatic Cache Keys\n");
            md.push_str("| Key | Operations | Used By | Modern Equivalent |\n");
            md.push_str("|-----|-----------|---------|-------------------|\n");
            for ck in &cache_inv.programmatic_cache_keys {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ck.cache_key,
                    ck.operation,
                    ck.files.join(", "),
                    ck.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !cache_inv.sql_cache_dependencies.is_empty() {
            md.push_str("### SQL Cache Dependencies\n");
            md.push_str("| File | Database | Table | Note |\n|------|----------|-------|------|\n");
            for sd in &cache_inv.sql_cache_dependencies {
                let db = sd.database_name.as_deref().unwrap_or("-");
                let tbl = sd.table_name.as_deref().unwrap_or("-");
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    sd.file_path, db, tbl, sd.modern_note
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Strategy\n");
        md.push_str("- `HttpRuntime.Cache` → `IMemoryCache` (single-server) or `IDistributedCache` (Redis, multi-server)\n");
        md.push_str("- `<%@ OutputCache %>` → `[ResponseCache]` attribute + `services.AddResponseCaching()`\n");
        md.push_str("- `Response.Cache.*` → `Response.Headers` or `[ResponseCache]` attribute\n");
        md.push_str("- `SqlCacheDependency` → Manual invalidation via Change Tracking, SignalR, or message bus\n\n");
    }
}
