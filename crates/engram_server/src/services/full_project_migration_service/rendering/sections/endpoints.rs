//! Extracted rendering sections: endpoints.
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
pub(crate) fn render_section_service_endpoints(
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
    // ── Service Endpoints (Phase 32) ─────────────────────────────────────
    if endpoints.total_endpoints > 0 {
        md.push_str("## Service Endpoints\n\n");
        if !endpoints.web_services.is_empty() {
            md.push_str(&format!(
                "**Web Services (ASMX)**: {}\n",
                endpoints.web_services.len()
            ));
            md.push_str("| File | Service | Methods | Called By |\n");
            md.push_str("|------|---------|---------|----------|\n");
            for ep in &endpoints.web_services {
                let methods = if ep.methods.is_empty() {
                    "(see code)".into()
                } else {
                    ep.methods.join(", ")
                };
                let callers = if ep.called_by.is_empty() {
                    "(none detected)".into()
                } else {
                    ep.called_by.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    ep.file_path, ep.service_name, methods, callers
                ));
            }
            md.push('\n');
        }
        if !endpoints.http_handlers.is_empty() {
            md.push_str(&format!(
                "**HTTP Handlers (ASHX)**: {}\n",
                endpoints.http_handlers.len()
            ));
            md.push_str("| File | Handler | Modern Equivalent |\n");
            md.push_str("|------|---------|-------------------|\n");
            for ep in &endpoints.http_handlers {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ep.file_path, ep.service_name, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }
        if !endpoints.wcf_services.is_empty() {
            md.push_str(&format!(
                "**WCF Services (SVC)**: {}\n",
                endpoints.wcf_services.len()
            ));
            md.push_str("| File | Service | Modern Equivalent |\n");
            md.push_str("|------|---------|-------------------|\n");
            for ep in &endpoints.wcf_services {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ep.file_path, ep.service_name, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }
        if !endpoints.http_modules.is_empty() {
            md.push_str(&format!(
                "**HTTP Modules**: {}\n",
                endpoints.http_modules.len()
            ));
            md.push_str("| Module | Type | Modern Equivalent |\n");
            md.push_str("|--------|------|-------------------|\n");
            for ep in &endpoints.http_modules {
                md.push_str(&format!(
                    "| {} | {} | {} |\n",
                    ep.file_path, ep.service_name, ep.modern_equivalent
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Impact\n");
        if !endpoints.web_services.is_empty() {
            md.push_str(&format!(
                "- {} ASMX services → Web API / Minimal API controllers\n",
                endpoints.web_services.len()
            ));
        }
        if !endpoints.http_handlers.is_empty() {
            md.push_str(&format!(
                "- {} ASHX handlers → Middleware or endpoint routes\n",
                endpoints.http_handlers.len()
            ));
        }
        if !endpoints.wcf_services.is_empty() {
            md.push_str(&format!(
                "- {} WCF services → gRPC or REST API\n",
                endpoints.wcf_services.len()
            ));
        }
        if !endpoints.http_modules.is_empty() {
            md.push_str(&format!(
                "- {} HTTP modules → ASP.NET Core middleware pipeline\n",
                endpoints.http_modules.len()
            ));
        }
        md.push('\n');
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_javascript_typescript_client_side_dependencies(
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
    // ── JavaScript/TypeScript & Client-Side Dependencies (Phase 32) ──────
    if js.total_script_files > 0 || !js.dom_manipulations.is_empty() || !js.ajax_calls.is_empty() {
        md.push_str("## JavaScript/TypeScript & Client-Side Dependencies\n\n");
        md.push_str(&format!(
            "**Client script files (.js/.ts/.tsx/.jsx)**: {} ({} with server-side dependencies)\n",
            js.total_script_files, js.script_files_with_server_deps
        ));
        if !js.dom_manipulations.is_empty() {
            let jquery_count = js
                .dom_manipulations
                .iter()
                .filter(|d| d.selector_type.contains("jquery"))
                .count();
            let getbyid_count = js
                .dom_manipulations
                .iter()
                .filter(|d| {
                    d.selector_type.contains("getelementbyid")
                        || d.selector_type.contains("getElementById")
                })
                .count();
            let clientid_count = js
                .dom_manipulations
                .iter()
                .filter(|d| {
                    d.selector_type.contains("client_id") || d.selector_type.contains("asp_client")
                })
                .count();
            md.push_str(&format!(
                "**DOM manipulations**: {} (jQuery: {}, getElementById: {}, ASP ClientID: {})\n",
                js.dom_manipulations.len(),
                jquery_count,
                getbyid_count,
                clientid_count
            ));
        }
        if !js.postback_triggers.is_empty() {
            md.push_str(&format!(
                "**Postback triggers**: {} __doPostBack calls from JS\n",
                js.postback_triggers.len()
            ));
        }
        if !js.ajax_calls.is_empty() {
            // Transport breakdown
            let mut transport_counts: BTreeMap<String, usize> = BTreeMap::new();
            for ac in &js.ajax_calls {
                *transport_counts.entry(ac.transport.clone()).or_insert(0) += 1;
            }
            let breakdown: Vec<String> = transport_counts
                .iter()
                .map(|(t, c)| format!("{t}: {c}"))
                .collect();
            md.push_str(&format!(
                "**AJAX calls**: {} ({})\n",
                js.ajax_calls.len(),
                breakdown.join(", ")
            ));
        }
        if let Some(ref jq) = js.jquery_version_hint {
            md.push_str(&format!("**jQuery version**: {jq}\n"));
        }
        md.push('\n');

        if !js.ajax_calls.is_empty() {
            md.push_str("### AJAX Endpoint Inventory\n");
            md.push_str("| JS File | Target URL | Transport | Method | Target Type |\n");
            md.push_str("|---------|-----------|-----------|--------|-------------|\n");
            for ac in &js.ajax_calls {
                let method = ac.target_method.as_deref().unwrap_or("(N/A)");
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    ac.js_file, ac.target_url, ac.transport, method, ac.target_type
                ));
            }
            md.push('\n');
        }

        if !js.page_js_dependencies.is_empty() {
            md.push_str("### Page ↔ JS Dependencies\n");
            md.push_str("| Page | JS Files | DOM Refs | Postbacks | AJAX Calls |\n");
            md.push_str("|------|----------|----------|-----------|------------|\n");
            for (page, js_files_list) in &js.page_js_dependencies {
                let dom_count = js
                    .dom_manipulations
                    .iter()
                    .filter(|d| js_files_list.contains(&d.js_file))
                    .count();
                let pb_count = js
                    .postback_triggers
                    .iter()
                    .filter(|p| js_files_list.contains(&p.js_file))
                    .count();
                let ajax_count = js
                    .ajax_calls
                    .iter()
                    .filter(|a| js_files_list.contains(&a.js_file))
                    .count();
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    page,
                    js_files_list.join(", "),
                    dom_count,
                    pb_count,
                    ajax_count
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Impact\n");
        if !js.dom_manipulations.is_empty() {
            md.push_str(&format!("- {} JS files manipulate server control IDs → must update to modern component selectors\n",
                js.script_files_with_server_deps));
        }
        if !js.postback_triggers.is_empty() {
            md.push_str(&format!("- {} `__doPostBack` calls → must replace with component event handlers / SignalR\n",
                js.postback_triggers.len()));
        }
        let asmx_ajax = js
            .ajax_calls
            .iter()
            .filter(|a| a.target_url.contains(".asmx"))
            .count();
        if asmx_ajax > 0 {
            md.push_str(&format!("- {asmx_ajax} AJAX calls to .asmx → must migrate to Web API / Minimal API endpoints\n"));
        }
        let page_methods = js
            .ajax_calls
            .iter()
            .filter(|a| a.transport == "page_methods")
            .count();
        if page_methods > 0 {
            md.push_str(&format!("- {page_methods} PageMethods calls → must migrate to Blazor JS interop / API calls\n"));
        }
        if !js.inline_script_files.is_empty() {
            md.push_str(&format!(
                "- {} files have inline `<script>` blocks → extract to separate JS modules\n",
                js.inline_script_files.len()
            ));
        }
        md.push('\n');
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_gis_spatial_analysis(
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
    // ── GIS / Spatial Analysis (Phase 32) ────────────────────────────────
    if gis.has_gis {
        md.push_str("## GIS / Spatial Analysis\n\n");
        let lib_summary: Vec<String> = gis
            .libraries_detected
            .iter()
            .map(|l| format!("{} ({} files)", l.library, l.files.len()))
            .collect();
        md.push_str(&format!("**Libraries**: {}\n", lib_summary.join(", ")));
        md.push_str(&format!(
            "**Total spatial calls**: {}\n",
            gis.total_spatial_calls
        ));
        md.push_str(&format!(
            "**Migration complexity**: {}\n\n",
            gis.migration_complexity
        ));

        for lib in &gis.libraries_detected {
            md.push_str(&format!("### {}\n", lib.library));
            md.push_str(&format!("- **Files**: {}\n", lib.files.join(", ")));
            if !lib.features.is_empty() {
                md.push_str(&format!("- **Features**: {}\n", lib.features.join(", ")));
            }
            if let Some(ref style) = lib.api_style {
                md.push_str(&format!("- **API style**: {style}\n"));
            }
            if lib.has_3d {
                md.push_str("- **3D support**: Yes\n");
            }
            if lib.api_keys_detected > 0 {
                md.push_str(&format!(
                    "- **API keys detected**: {}\n",
                    lib.api_keys_detected
                ));
            }
            // Show modern target based on target_stack
            if !gis.modern_targets.blazor.is_empty() {
                md.push_str(&format!(
                    "- **Modern target ({target_stack})**: {}\n",
                    gis.modern_targets.blazor.join(", ")
                ));
            } else if !gis.modern_targets.react.is_empty() {
                md.push_str(&format!(
                    "- **Modern target ({target_stack})**: {}\n",
                    gis.modern_targets.react.join(", ")
                ));
            } else if !gis.modern_targets.angular.is_empty() {
                md.push_str(&format!(
                    "- **Modern target ({target_stack})**: {}\n",
                    gis.modern_targets.angular.join(", ")
                ));
            }
            md.push('\n');
        }

        md.push_str("### Migration Considerations\n");
        for lib in &gis.libraries_detected {
            match lib.library.to_lowercase().as_str() {
                "google_maps" | "google maps" => {
                    md.push_str("- Google Maps JS API → wrapper component needed (direct DOM → component binding)\n");
                }
                "esri_arcgis" | "esri" | "arcgis" => {
                    md.push_str(
                        "- Esri AMD → ES module migration required (Dojo → modern bundler)\n",
                    );
                }
                "leaflet" => {
                    md.push_str("- Leaflet → wrapper component with proper lifecycle management\n");
                }
                "openlayers" => {
                    md.push_str(
                        "- OpenLayers → wrapper component with proper lifecycle management\n",
                    );
                }
                _ => {}
            }
        }
        let wms_count = gis.libraries_detected.iter().filter(|l| l.has_wms).count();
        if wms_count > 0 {
            md.push_str(&format!(
                "- {wms_count} WMS layer endpoint(s) must be preserved\n"
            ));
        }
        let key_count: usize = gis
            .libraries_detected
            .iter()
            .map(|l| l.api_keys_detected)
            .sum();
        if key_count > 0 {
            md.push_str(&format!(
                "- {key_count} API key(s) must be migrated to server-side configuration\n"
            ));
        }
        md.push('\n');
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_jquery_ecosystem_inventory(
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
    // ── Phase 35: jQuery Ecosystem Inventory ─────────────────────────────
    if jquery_inv.total_usages > 0 || jquery_inv.core_version.is_some() {
        md.push_str("## jQuery Plugin Ecosystem\n\n");
        if let Some(ref ver) = jquery_inv.core_version {
            let vuln_badge = if jquery_inv.core_vulnerable {
                " **VULNERABLE**"
            } else {
                ""
            };
            md.push_str(&format!("**jQuery Core**: v{ver}{vuln_badge}\n\n"));
            for note in &jquery_inv.vulnerability_notes {
                md.push_str(&format!("- {note}\n"));
            }
            if !jquery_inv.vulnerability_notes.is_empty() {
                md.push('\n');
            }
        }
        md.push_str(&format!(
            "**Total plugin usages**: {} | **Files analyzed**: {}\n\n",
            jquery_inv.total_usages, jquery_inv.files_analyzed,
        ));

        if !jquery_inv.ui_widgets.is_empty() {
            md.push_str("### jQuery UI Widgets\n\n");
            md.push_str("| Widget | File | Line | Modern Equivalent | Complexity |\n");
            md.push_str("|--------|------|------|-------------------|------------|\n");
            for w in &jquery_inv.ui_widgets {
                md.push_str(&format!(
                    "| {} | `{}` | {} | {} | {} |\n",
                    w.name, w.file_path, w.line_number, w.modern_equivalent, w.migration_complexity
                ));
            }
            md.push('\n');
        }

        if !jquery_inv.third_party_plugins.is_empty() {
            md.push_str("### Third-Party Plugins\n\n");
            md.push_str("| Plugin | File | Line | Modern Equivalent | Complexity |\n");
            md.push_str("|--------|------|------|-------------------|------------|\n");
            for p in &jquery_inv.third_party_plugins {
                md.push_str(&format!(
                    "| {} | `{}` | {} | {} | {} |\n",
                    p.name, p.file_path, p.line_number, p.modern_equivalent, p.migration_complexity
                ));
            }
            md.push('\n');
        }

        if !jquery_inv.custom_plugins.is_empty() {
            md.push_str("### Custom Plugins ($.fn.*)\n\n");
            md.push_str("| Plugin | File | Line |\n");
            md.push_str("|--------|------|------|\n");
            for p in &jquery_inv.custom_plugins {
                md.push_str(&format!(
                    "| {} | `{}` | {} |\n",
                    p.name, p.file_path, p.line_number
                ));
            }
            md.push('\n');
        }

        if !jquery_inv.deprecated_patterns.is_empty() {
            md.push_str("### Deprecated Patterns\n\n");
            md.push_str("| Pattern | File | Line | Recommendation |\n");
            md.push_str("|---------|------|------|----------------|\n");
            for d in &jquery_inv.deprecated_patterns {
                md.push_str(&format!(
                    "| {} | `{}` | {} | {} |\n",
                    d.name, d.file_path, d.line_number, d.modern_equivalent
                ));
            }
            md.push('\n');
        }
    }

}
