//! Extracted rendering sections: finale.
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
pub(crate) fn render_section_migration_wave_plan(
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
    // ── Migration Wave Plan ───────────────────────────────────────────────
    md.push_str("## Migration Wave Plan\n\n");
    for wave in &order.waves {
        md.push_str(&format!("### Wave {} — {}\n", wave.wave_number, wave.theme));
        if !wave.prerequisites.is_empty() {
            md.push_str(&format!(
                "Prerequisites: {}\n",
                wave.prerequisites.join(", ")
            ));
        }
        for wf in &wave.files {
            md.push_str(&format!(
                "- `{}` ({}, deps:{}, dependents:{})\n",
                wf.path, wf.estimated_complexity, wf.dependency_count, wf.dependent_count
            ));
        }
        if wave.strangler_fig_checkpoint {
            md.push_str("**Integration checkpoint after this wave.**\n");
        }
        md.push('\n');
    }

    if !order.circular_dependencies.is_empty() {
        md.push_str("### Circular Dependencies\n");
        for cycle in &order.circular_dependencies {
            md.push_str(&format!("- {}\n", cycle.join(" -> ")));
        }
        md.push('\n');
    }

    if !order.bottleneck_files.is_empty() {
        md.push_str("### Bottleneck Files\n");
        for bf in &order.bottleneck_files {
            md.push_str(&format!(
                "- `{}` blocks {} downstream: {}\n",
                bf.path, bf.blocks_count, bf.suggestion
            ));
        }
        md.push('\n');
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_cross_cutting_concerns(
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
    // ── Cross-Cutting Concerns ────────────────────────────────────────────
    md.push_str("## Cross-Cutting Concerns\n\n");

    if !cross.shared_sql_tables.is_empty() {
        md.push_str("### Shared SQL Tables\n");
        for si in &cross.shared_sql_tables {
            md.push_str(&format!(
                "- **{}** used by: {}\n",
                si.name,
                si.used_by.join(", ")
            ));
        }
        md.push('\n');
    }

    if !cross.shared_state_keys.is_empty() {
        md.push_str("### Shared State Keys\n");
        for si in &cross.shared_state_keys {
            md.push_str(&format!(
                "- **{}** used by: {}\n",
                si.name,
                si.used_by.join(", ")
            ));
        }
        md.push('\n');
    }

    if !cross.shared_user_controls.is_empty() {
        md.push_str("### Shared User Controls\n");
        for si in &cross.shared_user_controls {
            md.push_str(&format!(
                "- **{}** used by: {}\n",
                si.name,
                si.used_by.join(", ")
            ));
        }
        md.push('\n');
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_page_by_page_dossiers(
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
    // ── Page-by-Page Dossiers ─────────────────────────────────────────────
    md.push_str("## Page-by-Page Dossiers\n\n");

    for d in dossiers {
        let wave_num = wave_lookup.get(&d.file_path).copied().unwrap_or(0);

        let llm_tag = if d.llm_business_purpose.is_some() || d.llm_migration_notes.is_some() {
            " — LLM-enhanced"
        } else {
            ""
        };
        md.push_str(&format!(
            "### {} (Wave {}, {}, Risk {}/10){}\n\n",
            d.file_path, wave_num, d.estimated_complexity, d.blast_radius_score, llm_tag
        ));

        if let Some(ref bp) = d.llm_business_purpose {
            md.push_str(&format!("**Business purpose**: {bp}\n\n"));
        }

        if let Some(ref cls) = d.inherits_class {
            md.push_str(&format!("**Class**: `{cls}`\n"));
        }
        if let Some(ref mp) = d.master_page {
            md.push_str(&format!("**Master**: `{mp}`\n"));
        }

        // Dependencies
        if !d.user_controls.is_empty() {
            md.push_str(&format!(
                "**User controls**: {}\n",
                d.user_controls
                    .iter()
                    .map(|uc| format!("`{}`", uc.control_path))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        // Data layer
        if !d.tables_touched.is_empty() {
            md.push_str(&format!("**Tables**: {}\n", d.tables_touched.join(", ")));
        }
        if !d.connection_strings_used.is_empty() {
            md.push_str(&format!(
                "**Connection strings**: {}\n",
                d.connection_strings_used.join(", ")
            ));
        }

        // Lifecycle
        let lc = &d.lifecycle_summary;
        if lc.lifecycle_event_count > 0 || lc.control_event_count > 0 {
            md.push_str(&format!(
                "**Lifecycle**: {} events, {} control events",
                lc.lifecycle_event_count, lc.control_event_count
            ));
            if lc.has_ispostback_logic {
                md.push_str(" (has IsPostBack)");
            }
            md.push('\n');
            if !lc.events.is_empty() {
                md.push_str(&format!("  Events: {}\n", lc.events.join(", ")));
            }
        }

        // ViewState
        let vs = &d.viewstate_summary;
        if vs.total_state_fields > 0 {
            md.push_str(&format!(
                "**ViewState**: {} explicit, {} implicit",
                vs.explicit_keys, vs.implicit_controls
            ));
            if let Some(ref hc) = vs.heaviest_control {
                md.push_str(&format!(" (heaviest: {hc})"));
            }
            md.push('\n');
        }

        // AJAX
        let aj = &d.ajax_summary;
        if aj.update_panel_count > 0 || aj.has_script_manager {
            md.push_str(&format!(
                "**AJAX**: {} UpdatePanels, {} timers, ScriptManager: {}\n",
                aj.update_panel_count, aj.timer_count, aj.has_script_manager
            ));
        }

        // Validation
        let vl = &d.validation_summary;
        if vl.validator_count > 0 || vl.custom_validator_count > 0 {
            md.push_str(&format!(
                "**Validation**: {} standard, {} custom, {} groups\n",
                vl.validator_count, vl.custom_validator_count, vl.validation_group_count
            ));
        }

        // Auth
        let au = &d.auth_summary;
        if au.has_auth_rules || au.auth_check_count > 0 || au.session_auth_count > 0 {
            md.push_str("**Auth**: ");
            if !au.required_roles.is_empty() {
                md.push_str(&format!("roles [{}] ", au.required_roles.join(", ")));
            }
            if au.auth_check_count > 0 {
                md.push_str(&format!("{} code checks ", au.auth_check_count));
            }
            if au.session_auth_count > 0 {
                md.push_str(&format!("{} session-auth patterns", au.session_auth_count));
            }
            md.push('\n');
        }

        // Phase 32: JS dependencies per page
        if let Some(js_deps) = js.page_js_dependencies.get(&d.file_path) {
            let mut dep_parts: Vec<String> = Vec::new();
            for js_file in js_deps {
                let dom_count = js
                    .dom_manipulations
                    .iter()
                    .filter(|dr| &dr.js_file == js_file)
                    .count();
                let pb_count = js
                    .postback_triggers
                    .iter()
                    .filter(|pr| &pr.js_file == js_file)
                    .count();
                let ajax_count = js
                    .ajax_calls
                    .iter()
                    .filter(|ac| &ac.js_file == js_file)
                    .count();
                let mut parts = Vec::new();
                if dom_count > 0 {
                    parts.push(format!("{dom_count} DOM refs"));
                }
                if pb_count > 0 {
                    parts.push(format!("{pb_count} postback"));
                }
                if ajax_count > 0 {
                    parts.push(format!("{ajax_count} AJAX"));
                }
                if parts.is_empty() {
                    dep_parts.push(js_file.clone());
                } else {
                    dep_parts.push(format!("{js_file} ({})", parts.join(", ")));
                }
            }
            md.push_str(&format!("**JS dependencies**: {}\n", dep_parts.join(", ")));
        }

        // Phase 32: GIS per page
        if gis.has_gis {
            let page_gis: Vec<&GisLibrarySummary> = gis
                .libraries_detected
                .iter()
                .filter(|l| l.files.iter().any(|f| f == &d.file_path))
                .collect();
            // Also check if any JS dependency of this page has GIS
            let js_has_gis = js
                .page_js_dependencies
                .get(&d.file_path)
                .map(|deps| deps.iter().any(|jf| gis.files_with_gis.contains(jf)))
                .unwrap_or(false);
            if !page_gis.is_empty() || js_has_gis {
                let lib_names: Vec<String> = page_gis
                    .iter()
                    .map(|l| {
                        let features = if l.features.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", l.features.join(", "))
                        };
                        format!("{}{features}", l.library)
                    })
                    .collect();
                let desc = if lib_names.is_empty() {
                    "via JS dependencies".to_string()
                } else {
                    lib_names.join(", ")
                };
                md.push_str(&format!(
                    "**GIS**: {} — complexity: {}\n",
                    desc, gis.migration_complexity
                ));
            }
        }

        // Phase 32: Anti-patterns per page
        let page_anti: Vec<&AntiPatternItem> = anti
            .critical_items
            .iter()
            .filter(|item| item.file_path == d.file_path)
            .collect();
        if !page_anti.is_empty() {
            let summaries: Vec<String> = page_anti
                .iter()
                .map(|a| format!("{} ({})", a.pattern_type, a.detail))
                .collect();
            md.push_str(&format!("**Anti-patterns**: {}\n", summaries.join("; ")));
        }

        // Phase 33: Method inventory per page (Gap 1)
        if let Some(inv) = method_inv.get(&d.file_path)
            && !inv.methods.is_empty()
        {
            md.push_str(&format!(
                "**Methods** ({} total: {} lifecycle, {} event handlers, {} helpers)\n\n",
                inv.total_methods, inv.lifecycle_methods, inv.event_handlers, inv.helper_methods
            ));
            md.push_str("| Method | Kind | Lines | Effects | Signature |\n");
            md.push_str("|--------|------|-------|---------|----------|\n");
            for m in &inv.methods {
                let effects = if m.effects.is_empty() {
                    "-".to_string()
                } else {
                    m.effects.join(", ")
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    m.name, m.method_kind, m.line_count, effects, m.signature
                ));
            }
            md.push('\n');
        }

        // Phase 33: Third-party controls per page (Gap 2)
        {
            let page_tp: Vec<&VendorSummary> = third_party
                .vendors_detected
                .iter()
                .filter(|v| v.files.contains(&d.file_path))
                .collect();
            if !page_tp.is_empty() {
                let parts: Vec<String> = page_tp
                    .iter()
                    .flat_map(|v| {
                        v.controls_used
                            .iter()
                            .filter(|(_, _)| true) // all controls from vendor present in this page
                            .map(|(name, count)| format!("{name} ({count})"))
                    })
                    .collect();
                md.push_str(&format!("**Third-party controls**: {}\n", parts.join(", ")));
            }
        }

        // Phase 33: VB translation flags per page (Gap 6)
        //
        // Scope the flags to the page itself, its explicit codebehind (when
        // the dossier builder detected one), and the conventional
        // `.aspx.vb` / `.aspx.cs` sibling. Previously this filter contained
        // `f.file_path.contains(cb)` where `cb` came from
        // `d.codebehind_file.as_deref().unwrap_or("")` — with no codebehind
        // detected, `cb` was the empty string and `.contains("")` is always
        // true, so the first dossier on pages without a detected codebehind
        // (e.g. OciusX `Site/AuthCallback.aspx`) dumped the project-wide
        // flag list (~50 KB) into a single page's section.
        if vb_trans.is_vb_project {
            let page_flags: Vec<&VbTranslationFlag> = vb_trans
                .translation_flags
                .iter()
                .filter(|f| {
                    analyzers::vb_translation::flag_belongs_to_page(&f.file_path, &d.file_path, d.codebehind_file.as_deref())
                })
                .collect();
            if !page_flags.is_empty() {
                let parts: Vec<String> = page_flags
                    .iter()
                    .map(|f| format!("{} ({})", f.pattern, f.count))
                    .collect();
                md.push_str(&format!("**VB translation flags**: {}\n", parts.join(", ")));
            }
        }

        // Phase 33: Caching per page (Gap 4)
        {
            let page_cache: Vec<&OutputCacheEntry> = cache_inv
                .output_cache_pages
                .iter()
                .filter(|c| c.file_path == d.file_path)
                .collect();
            if !page_cache.is_empty() {
                for oc in &page_cache {
                    let dur = oc
                        .duration_seconds
                        .map_or("-".to_string(), |d| format!("{d}s"));
                    md.push_str(&format!("**OutputCache**: Duration={dur}"));
                    if let Some(ref vbp) = oc.vary_by_param {
                        md.push_str(&format!(", VaryByParam={vbp}"));
                    }
                    md.push('\n');
                }
            }
        }

        // Risk factors
        if !d.risk_factors.is_empty() {
            md.push_str(&format!(
                "**Risk factors**: {}\n",
                d.risk_factors.join("; ")
            ));
        }

        // Migration steps
        if !d.migration_steps.is_empty() {
            md.push_str("**Migration steps**:\n");
            for (i, step) in d.migration_steps.iter().enumerate() {
                md.push_str(&format!("  {}. {step}\n", i + 1));
            }
        }

        // LLM-generated migration notes (risks + Blazor component guidance
        // that the deterministic analysis doesn't already capture). Only
        // present when `use_llm: true` and this page was within the
        // `llm_max_pages` cap.
        if let Some(ref notes) = d.llm_migration_notes {
            md.push_str("\n**Migration notes (LLM)**:\n");
            for line in notes.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Honour any bullet formatting the model already produced;
                // otherwise wrap each line in a `-` bullet.
                if trimmed.starts_with('-')
                    || trimmed.starts_with('*')
                    || trimmed.starts_with(|c: char| c.is_ascii_digit())
                {
                    md.push_str(&format!("{trimmed}\n"));
                } else {
                    md.push_str(&format!("- {trimmed}\n"));
                }
            }
        }

        md.push('\n');
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_risk_assessment(
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
    // ── Risk Assessment ───────────────────────────────────────────────────
    md.push_str("## Risk Assessment\n\n");
    md.push_str("| Risk Band | Files |\n|-----------|-------|\n");
    for (band, count) in &cross.risk_distribution {
        md.push_str(&format!("| {band} | {count} |\n"));
    }
    md.push('\n');

    if !cross.critical_risk_files.is_empty() {
        md.push_str("**Critical-risk files requiring special attention:**\n");
        for f in &cross.critical_risk_files {
            md.push_str(&format!("- `{f}`\n"));
        }
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_business_logic_summary(
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
    // ── Phase 36: Business Logic Summary ────────────────────────────────
    if !biz_logic.file_summaries.is_empty() {
        md.push_str(&crate::services::business_logic_service::render_compact_markdown(
            biz_logic,
        ));
        // Show tip only when no LLM was used (no confidence data present)
        let has_llm_data = biz_logic
            .file_summaries
            .iter()
            .any(|f| f.methods.iter().any(|m| !m.confidence.is_empty()));
        if !has_llm_data {
            md.push_str("\n> **Tip**: Run `analyze_full_project_migration` with `use_llm: true` ");
            md.push_str(
                "for LLM-powered business logic comprehension with confidence scoring.\n\n",
            );
        }
    }

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_database_intelligence(
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
    // ── Phase 37: Database Intelligence ──────────────────────────────────
    md.push_str(
        &crate::services::database_intelligence_service::render_database_intelligence_markdown(db_intel),
    );

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_session_workflows(
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
    // ── Phase 37: Session Workflows ─────────────────────────────────────
    md.push_str(&crate::services::session_workflow_service::render_session_workflows_markdown(session_wf));

}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_migration_intelligence_confidence_dashboard(
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
    // ── Phase 37: Migration Intelligence Confidence Dashboard ───────────
    md.push_str(&super::confidence_dashboard::render_confidence_dashboard(
        cross, biz_logic, db_intel, session_wf,
    ));
}
