//! Extracted rendering sections: config.
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
pub(crate) fn render_section_web_config_inventory(
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
    // ── web.config Inventory (Phase 32) ──────────────────────────────────
    if !webconfig.connection_strings.is_empty()
        || !webconfig.app_settings.is_empty()
        || webconfig.session_state.is_some()
        || !webconfig.http_handlers.is_empty()
        || !webconfig.http_modules.is_empty()
    {
        md.push_str("## Configuration (web.config)\n\n");

        if !webconfig.connection_strings.is_empty() {
            md.push_str("### Connection Strings\n");
            md.push_str("| Name | Provider | Integrated Auth | Used By |\n");
            md.push_str("|------|----------|-----------------|--------|\n");
            for cs in &webconfig.connection_strings {
                let used = if cs.used_by.is_empty() {
                    "(none found)".to_string()
                } else {
                    cs.used_by.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    cs.name, cs.provider, cs.has_integrated_security, used
                ));
            }
            md.push('\n');
        }

        if !webconfig.app_settings.is_empty() {
            md.push_str(&format!(
                "### App Settings ({} keys)\n",
                webconfig.app_settings.len()
            ));
            md.push_str("| Key | Preview | Used By |\n");
            md.push_str("|-----|---------|--------|\n");
            for a in &webconfig.app_settings {
                let used = if a.used_by.is_empty() {
                    "(none found)".to_string()
                } else {
                    a.used_by.join(", ")
                };
                md.push_str(&format!("| {} | {} | {} |\n", a.key, a.value_preview, used));
            }
            md.push('\n');
        }

        if let Some(ref ss) = webconfig.session_state {
            md.push_str(&format!("### Session State\n**Mode**: {}", ss.mode));
            if let Some(t) = ss.timeout_minutes {
                md.push_str(&format!(" | **Timeout**: {}min", t));
            }
            md.push('\n');
            let migration_hint = match ss.mode.as_str() {
                "InProc" => "Replace with IDistributedCache (Redis or SQL Server)",
                "StateServer" => "Replace with Redis-backed IDistributedCache",
                "SQLServer" => "Replace with distributed cache (Redis/IDistributedCache)",
                "Custom" => "Evaluate custom provider → IDistributedCache adapter",
                _ => "Replace with IDistributedCache",
            };
            md.push_str(&format!("→ Migration: {migration_hint}\n\n"));
        }

        if !webconfig.http_handlers.is_empty() {
            md.push_str(&format!(
                "### HTTP Handlers ({})\n",
                webconfig.http_handlers.len()
            ));
            md.push_str("| Verb | Path | Type |\n|------|------|------|\n");
            for h in &webconfig.http_handlers {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    h.verb, h.path, h.handler_type
                ));
            }
            md.push_str("→ Migration: Replace with Minimal API / Controller endpoints\n\n");
        }

        if !webconfig.http_modules.is_empty() {
            md.push_str(&format!(
                "### HTTP Modules ({})\n",
                webconfig.http_modules.len()
            ));
            md.push_str("| Name | Type |\n|------|------|\n");
            for m in &webconfig.http_modules {
                md.push_str(&format!("| {} | {} |\n", m.name, m.module_type));
            }
            md.push_str("→ Migration: Replace with ASP.NET Core middleware\n\n");
        }

        if let Some(ref ce) = webconfig.custom_errors {
            md.push_str(&format!("### Custom Errors\n**Mode**: {}", ce.mode));
            if let Some(ref dr) = ce.default_redirect {
                md.push_str(&format!(" | Default: {dr}"));
            }
            md.push('\n');
            for (code, redirect) in &ce.status_redirects {
                md.push_str(&format!("- {code} → {redirect}\n"));
            }
            md.push_str("→ Migration: Replace with UseExceptionHandler + UseStatusCodePagesWithReExecute\n\n");
        }

        if let Some(ref comp) = webconfig.compilation {
            md.push_str(&format!("### Compilation\n**Debug**: {}", comp.debug));
            if let Some(ref tf) = comp.target_framework {
                md.push_str(&format!(" | **Target Framework**: {tf}"));
            }
            md.push('\n');
            if !comp.assemblies.is_empty() {
                md.push_str(&format!(
                    "**Referenced assemblies**: {}\n",
                    comp.assemblies.len()
                ));
            }
            md.push('\n');
        }
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_url_routing_rewriting(
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
    // ── Phase 33: URL Routing & Rewriting (Gap 5) ─────────────────────────
    if url_routing.total_url_patterns > 0
        || !url_routing.rewrite_path_calls.is_empty()
        || !url_routing.redirects.is_empty()
        || !url_routing.server_transfers.is_empty()
    {
        md.push_str("## URL Routing & Rewriting\n\n");
        md.push_str(&format!(
            "**URL patterns**: {} ({} rewrite rules, {} page routes, {} URL mappings)\n",
            url_routing.total_url_patterns,
            url_routing.rewrite_rules.len(),
            url_routing.page_routes.len(),
            url_routing.url_mappings.len()
        ));
        md.push_str(&format!(
            "**RewritePath calls**: {}\n",
            url_routing.rewrite_path_calls.len()
        ));
        md.push_str(&format!("**Redirects**: {}\n", url_routing.redirects.len()));
        md.push_str(&format!(
            "**Server.Transfer calls**: {}\n",
            url_routing.server_transfers.len()
        ));
        md.push_str(&format!(
            "**Friendly URLs**: {}\n\n",
            if url_routing.has_friendly_urls {
                "enabled"
            } else {
                "disabled"
            }
        ));

        if !url_routing.rewrite_rules.is_empty() {
            md.push_str("### IIS Rewrite Rules\n");
            md.push_str("| Rule | Match Pattern | Action | Target | Modern Equivalent |\n");
            md.push_str("|------|--------------|--------|--------|-------------------|\n");
            for rule in &url_routing.rewrite_rules {
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    rule.rule_name,
                    rule.match_pattern,
                    rule.action_type,
                    rule.target_url,
                    rule.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !url_routing.page_routes.is_empty() {
            md.push_str("### Page Routes (Global.asax)\n");
            md.push_str("| Route Name | URL Pattern | Physical Page | Modern Equivalent |\n");
            md.push_str("|-----------|-------------|---------------|-------------------|\n");
            for route in &url_routing.page_routes {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    route.route_name,
                    route.url_pattern,
                    route.physical_page,
                    route.modern_equivalent
                ));
            }
            md.push('\n');
        }

        if !url_routing.server_transfers.is_empty() {
            md.push_str("### Code-Based URL Manipulation\n");
            md.push_str("| File | Type | Target |\n|------|------|--------|\n");
            for st in &url_routing.server_transfers {
                md.push_str(&format!(
                    "| {} | Server.Transfer | {} |\n",
                    st.file_path, st.target_page
                ));
            }
            for rp in &url_routing.rewrite_path_calls {
                md.push_str(&format!(
                    "| {} | RewritePath | {} |\n",
                    rp.file_path, rp.target_path
                ));
            }
            md.push('\n');
            md.push_str(&format!("**WARNING**: {} Server.Transfer calls must be refactored — this pattern does not exist in ASP.NET Core\n\n", url_routing.server_transfers.len()));
        }

        md.push_str("### Migration Strategy\n");
        md.push_str(
            "- IIS Rewrite Rules → ASP.NET Core URL Rewriting Middleware (`app.UseRewriter()`)\n",
        );
        md.push_str("- `MapPageRoute` → `app.MapGet()` / `@page` directives\n");
        md.push_str("- `HttpContext.RewritePath` → Middleware pipeline or endpoint routing\n");
        md.push_str(
            "- `Server.Transfer` → **No equivalent** — refactor to redirect or shared component\n",
        );
        md.push_str(
            "- `Response.Redirect` → `Results.Redirect()` / `NavigationManager.NavigateTo()`\n\n",
        );
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_config_transforms(
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
    // ── Phase 34: Config Transforms ───────────────────────────────────────
    if !cfg_transforms.environments.is_empty() {
        md.push_str("## Configuration Transforms\n\n");
        md.push_str(&format!(
            "**Environments**: {} | **Total transforms**: {}\n\n",
            cfg_transforms.environments.len(),
            cfg_transforms.total_transforms
        ));
        md.push_str("Modern equivalent: `appsettings.{Environment}.json` with environment-specific overrides.\n\n");

        for env in &cfg_transforms.environments {
            md.push_str(&format!("### {} (`{}`)\n\n", env.name, env.file_path));
            md.push_str("| XPath | Operation | Key | Value |\n");
            md.push_str("|-------|-----------|-----|-------|\n");
            for t in &env.transforms {
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    t.xpath_hint,
                    t.operation,
                    t.key.as_deref().unwrap_or("-"),
                    t.value_preview.as_deref().unwrap_or("-")
                ));
            }
            md.push('\n');
        }

        if !cfg_transforms.connection_string_overrides.is_empty() {
            md.push_str("**Connection string overrides by environment:**\n");
            for (env, cs) in &cfg_transforms.connection_string_overrides {
                md.push_str(&format!("- `{env}` → `{cs}`\n"));
            }
            md.push('\n');
        }

        if !cfg_transforms.debug_flag_overrides.is_empty() {
            md.push_str("**Debug flag by environment:**\n");
            for (env, debug) in &cfg_transforms.debug_flag_overrides {
                md.push_str(&format!("- `{env}` → debug={debug}\n"));
            }
            md.push('\n');
        }
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_binding_redirects(
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
    // ── Phase 34: Binding Redirects ───────────────────────────────────────
    if !dep_inv.binding_redirects.is_empty() {
        md.push_str("## Assembly Binding Redirects\n\n");
        md.push_str(&format!(
            "**{}** binding redirects found — these indicate version conflicts to resolve.\n\n",
            dep_inv.binding_redirects.len()
        ));
        md.push_str("| Assembly | Old Version | New Version | Known Replacement |\n");
        md.push_str("|----------|-------------|-------------|-------------------|\n");
        for br in &dep_inv.binding_redirects {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                br.assembly_name,
                br.old_version_range,
                br.new_version,
                if br.has_known_replacement {
                    "Yes"
                } else {
                    "No"
                }
            ));
        }
        md.push('\n');
    }
}
