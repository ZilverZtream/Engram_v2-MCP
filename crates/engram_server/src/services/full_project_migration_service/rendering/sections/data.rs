//! Extracted rendering sections: data.
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
pub(crate) fn render_section_data_access_patterns(
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
    // ── Data Access Patterns ──────────────────────────────────────────────
    if !data_access.is_empty() {
        md.push_str("## Data Access Patterns\n\n");
        let mut pattern_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut total_tables = 0usize;
        let mut injection_risk_files = Vec::new();
        for dap in data_access {
            *pattern_counts
                .entry(format!("{:?}", dap.primary_pattern))
                .or_insert(0) += 1;
            total_tables += dap.table_count;
            if dap.has_concatenated_sql {
                injection_risk_files.push(dap.file_path.clone());
            }
        }
        md.push_str(&format!(
            "**Files with data access**: {} | **Total tables**: {}\n\n",
            data_access.len(),
            total_tables
        ));
        md.push_str("| Pattern | Files |\n|---------|-------|\n");
        for (pattern, count) in &pattern_counts {
            md.push_str(&format!("| {pattern} | {count} |\n"));
        }
        md.push('\n');
        if !injection_risk_files.is_empty() {
            md.push_str(&format!(
                "**SQL injection risk** (concatenated SQL): {}\n\n",
                injection_risk_files.join(", ")
            ));
        }
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_code_behind_method_inventory(
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
    // ── Phase 33: Code-Behind Method Inventory (Gap 1) ─────────────────────
    if !method_inv.is_empty() {
        let total_m: usize = method_inv.values().map(|i| i.total_methods).sum();
        let total_eh: usize = method_inv.values().map(|i| i.event_handlers).sum();
        let total_wm: usize = method_inv.values().map(|i| i.web_methods).sum();
        let total_helpers: usize = method_inv.values().map(|i| i.helper_methods).sum();
        let total_lc: usize = method_inv.values().map(|i| i.lifecycle_methods).sum();

        md.push_str("## Code-Behind Method Inventory\n\n");
        md.push_str(&format!(
            "**Total methods**: {} across {} code-behind files\n",
            total_m,
            method_inv.len()
        ));
        md.push_str(&format!(
            "**Lifecycle handlers**: {} | **Event handlers**: {} | **WebMethods**: {} | **Helpers**: {}\n",
            total_lc, total_eh, total_wm, total_helpers
        ));
        if let Some((ref path, count)) = cross.largest_file_by_methods {
            md.push_str(&format!(
                "**Largest code-behind**: {} ({} methods)\n",
                path, count
            ));
        }
        md.push('\n');

        // Top 10 files by method count
        let mut sorted_files: Vec<(&String, &PageMethodInventory)> = method_inv.iter().collect();
        sorted_files.sort_by(|a, b| b.1.total_methods.cmp(&a.1.total_methods));
        sorted_files.truncate(10);

        if !sorted_files.is_empty() {
            md.push_str("### Files by Method Count (top 10)\n");
            md.push_str("| File | Methods | Events | SQL Methods | Largest Method |\n");
            md.push_str("|------|---------|--------|-------------|----------------|\n");
            for (path, inv) in &sorted_files {
                let largest = inv
                    .largest_method
                    .as_ref()
                    .map(|(n, lc)| format!("{n} ({lc} lines)"))
                    .unwrap_or_else(|| "-".to_string());
                md.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    path, inv.total_methods, inv.event_handlers, inv.methods_with_sql, largest
                ));
            }
            md.push('\n');
        }

        // Complexity indicators
        let big_methods: usize = method_inv
            .values()
            .flat_map(|i| i.methods.iter())
            .filter(|m| m.line_count > 50)
            .count();
        let sql_methods: usize = method_inv.values().map(|i| i.methods_with_sql).sum();
        let com_methods: usize = method_inv
            .values()
            .flat_map(|i| i.methods.iter())
            .filter(|m| m.effects.iter().any(|e| e.contains("COM")))
            .count();
        md.push_str("### Migration Complexity Indicators\n");
        if big_methods > 0 {
            md.push_str(&format!(
                "- {} methods > 50 lines → candidates for decomposition\n",
                big_methods
            ));
        }
        if sql_methods > 0 {
            md.push_str(&format!(
                "- {} methods with SQL_Access → need repository extraction\n",
                sql_methods
            ));
        }
        if com_methods > 0 {
            md.push_str(&format!(
                "- {} methods with COM_Interop → need modern library replacement\n",
                com_methods
            ));
        }
        if total_wm > 0 {
            md.push_str(&format!(
                "- {} WebMethods → must become API endpoints\n",
                total_wm
            ));
        }
        md.push('\n');
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_stored_procedure_catalog(
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
    // ── Phase 34: Stored Procedure Catalog ──────────────────────────────
    if sp_cat.total_procedures > 0 {
        md.push_str("## Stored Procedure Catalog\n\n");
        md.push_str(&format!(
            "**Total**: {} procedures | **Called from code**: {} | **Uncalled (dead?)**: {}\n\n",
            sp_cat.total_procedures,
            sp_cat.procedures_called_from_code,
            sp_cat.uncalled_procedures.len()
        ));

        md.push_str("| Procedure | Params | Tables Read | Tables Written | Lines | Dynamic SQL | Cursor | Modern Equivalent |\n");
        md.push_str("|-----------|--------|-------------|----------------|-------|-------------|--------|-------------------|\n");
        for sp in sp_cat.procedures.iter().take(50) {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} | {} | {} |\n",
                sp.name,
                sp.parameters.len(),
                sp.tables_read.join(", "),
                sp.tables_written.join(", "),
                sp.line_count,
                if sp.has_dynamic_sql { "Yes" } else { "No" },
                if sp.has_cursor { "Yes" } else { "No" },
                sp.modern_equivalent
            ));
        }
        md.push('\n');

        // Parameter details for top SPs
        let top_sps: Vec<_> = sp_cat
            .procedures
            .iter()
            .filter(|sp| !sp.parameters.is_empty() && !sp.called_from.is_empty())
            .take(10)
            .collect();
        if !top_sps.is_empty() {
            md.push_str("### Stored Procedure Parameters (Top Called)\n\n");
            for sp in top_sps {
                md.push_str(&format!(
                    "**{}** — called from: {}\n\n",
                    sp.name,
                    sp.called_from.join(", ")
                ));
                md.push_str("| Parameter | SQL Type | C# Type | Direction | Default |\n");
                md.push_str("|-----------|----------|---------|-----------|--------|\n");
                for p in &sp.parameters {
                    md.push_str(&format!(
                        "| `{}` | {} | `{}` | {} | {} |\n",
                        p.name,
                        p.sql_type,
                        p.csharp_type,
                        p.direction,
                        p.default_value.as_deref().unwrap_or("-")
                    ));
                }
                md.push('\n');
            }
        }

        if !sp_cat.uncalled_procedures.is_empty() {
            md.push_str("### Potentially Dead Procedures\n\n");
            md.push_str("These SPs were found in `.sql` files but are not called from any scanned code-behind:\n\n");
            for name in sp_cat.uncalled_procedures.iter().take(30) {
                md.push_str(&format!("- `{name}`\n"));
            }
            md.push('\n');
        }
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_inheritance_chain_report(
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
    // ── Phase 34: Inheritance Chain Report ────────────────────────────────
    if !inherit.chains.is_empty() {
        md.push_str("## Base Class Inheritance Chains\n\n");
        md.push_str(&format!(
            "**Deepest chain**: {} levels | **Shared base classes**: {}\n\n",
            inherit.deepest_chain_depth,
            inherit.base_classes.len()
        ));

        // Base class summary
        if !inherit.base_classes.is_empty() {
            md.push_str("### Shared Base Classes\n\n");
            md.push_str("| Base Class | File | Derived Pages | Lifecycle Methods | Session Keys Initialized |\n");
            md.push_str("|------------|------|---------------|-------------------|-------------------------|\n");
            for bc in &inherit.base_classes {
                md.push_str(&format!(
                    "| `{}` | `{}` | {} | {} | {} |\n",
                    bc.class_name,
                    bc.file_path,
                    bc.derived_count,
                    bc.lifecycle_methods.join(", "),
                    if bc.state_keys_initialized.is_empty() {
                        "-".to_string()
                    } else {
                        bc.state_keys_initialized.join(", ")
                    }
                ));
            }
            md.push('\n');
        }

        // Shared lifecycle methods
        if !inherit.shared_lifecycle_methods.is_empty() {
            md.push_str("### Shared Lifecycle Methods\n\n");
            for slm in &inherit.shared_lifecycle_methods {
                md.push_str(&format!(
                    "- **{}** defined in `{}`, overridden in: {} {}\n",
                    slm.method_name,
                    slm.defining_class,
                    slm.overridden_in.join(", "),
                    if slm.calls_base {
                        "(calls base)"
                    } else {
                        "(does NOT call base)"
                    }
                ));
            }
            md.push('\n');
        }

        // Per-page chain diagrams (top 20)
        md.push_str("### Inheritance Chains per Page\n\n");
        for chain in inherit.chains.iter().take(20) {
            md.push_str(&format!(
                "**{}**: `{}`\n",
                chain.page_file,
                chain.chain.join(" → ")
            ));
            if !chain.inherited_state_writes.is_empty() {
                md.push_str(&format!(
                    "  - Inherited Session keys: {}\n",
                    chain.inherited_state_writes.join(", ")
                ));
            }
            if !chain.inherited_lifecycle_methods.is_empty() {
                let parts: Vec<String> = chain
                    .inherited_lifecycle_methods
                    .iter()
                    .map(|(m, c)| format!("{m} ({c})"))
                    .collect();
                md.push_str(&format!("  - Inherited lifecycle: {}\n", parts.join(", ")));
            }
        }
        md.push('\n');
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_cross_layer_data_flow_chains(
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
    // ── Phase 35: Cross-Layer Data Flow Chains ───────────────────────────
    if !cross_traces.chains.is_empty() {
        md.push_str("## Cross-Layer Data Flow Chains\n\n");
        md.push_str(&format!(
            "**Total chains**: {} | **Unresolved URLs**: {}\n\n",
            cross_traces.total_chains,
            cross_traces.unresolved_urls.len(),
        ));

        for chain in cross_traces.chains.iter().take(20) {
            md.push_str(&format!("### Feature: {}\n\n", chain.feature_name));
            md.push_str("| Layer | File | Action |\n");
            md.push_str("|-------|------|--------|\n");
            for step in &chain.steps {
                md.push_str(&format!(
                    "| {} | `{}` | {} |\n",
                    step.layer, step.file_path, step.action
                ));
            }
            if !chain.tables_touched.is_empty() {
                md.push_str(&format!(
                    "\n**Tables**: {}\n",
                    chain.tables_touched.join(", ")
                ));
            }
            for note in &chain.risk_notes {
                md.push_str(&format!("- {note}\n"));
            }
            md.push('\n');
        }

        if !cross_traces.unresolved_urls.is_empty() {
            md.push_str("### Unresolved AJAX URLs\n\n");
            for url in &cross_traces.unresolved_urls {
                md.push_str(&format!("- `{url}`\n"));
            }
            md.push('\n');
        }
    }
}

#[allow(clippy::too_many_arguments, unused_variables)]
pub(crate) fn render_section_inherited_effects(
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
    // ── Phase 35: Inherited Effects ──────────────────────────────────────
    if !inherit.inherited_effects.is_empty() {
        md.push_str("## Inherited Effects (Base Class Propagation)\n\n");
        md.push_str("| Derived Class | Inherited From | Method | Effects |\n");
        md.push_str("|---------------|----------------|--------|--------|\n");
        for eff in inherit.inherited_effects.iter().take(50) {
            let effects_str = eff.effects.join(", ").replace('|', "\\|");
            md.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                eff.class, eff.inherited_from, eff.method, effects_str
            ));
        }
        md.push('\n');
    }
}
