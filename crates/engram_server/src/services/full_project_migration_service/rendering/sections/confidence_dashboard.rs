//! Extracted rendering sections: confidence dashboard.
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

pub(crate) fn render_confidence_dashboard(
    cross: &CrossCuttingSummary,
    biz_logic: &crate::services::business_logic_service::ProjectBusinessLogicReport,
    db_intel: &crate::services::database_intelligence_service::DatabaseIntelligence,
    session_wf: &crate::services::session_workflow_service::SessionWorkflowReport,
) -> String {
    let mut md = String::with_capacity(2_000);
    md.push_str("## Migration Intelligence Confidence\n\n");
    md.push_str("| Dimension | Coverage | Confidence |\n|---|---|---|\n");

    // Code Structure
    md.push_str(&format!(
        "| Code Structure | {} pages analyzed | {} |\n",
        cross.total_pages_analyzed,
        if cross.total_pages_analyzed > 0 {
            "✅ High"
        } else {
            "❌ Low"
        }
    ));

    // Business Logic — single pass for confidence counts
    let total_methods: usize = biz_logic
        .file_summaries
        .iter()
        .map(|f| f.methods.len())
        .sum();
    let (mut llm_methods, mut high_conf, mut med_conf, mut low_conf) =
        (0usize, 0usize, 0usize, 0usize);
    for m in biz_logic.file_summaries.iter().flat_map(|f| &f.methods) {
        if !m.confidence.is_empty() {
            llm_methods += 1;
            match m.confidence.as_str() {
                "High" => high_conf += 1,
                "Medium" => med_conf += 1,
                "Low" => low_conf += 1,
                _ => {}
            }
        }
    }

    if llm_methods > 0 {
        md.push_str(&format!(
            "| Business Logic | {llm_methods}/{total_methods} methods analyzed by LLM | ✅ High ({high_conf}), ⚠️ Medium ({med_conf}), ❌ Low ({low_conf}) |\n"
        ));
    } else {
        md.push_str(&format!(
            "| Business Logic | {total_methods} methods (deterministic only) | ⚠️ Medium (no LLM) |\n"
        ));
    }

    // Database
    let sp_count = db_intel.sp_logic.len();
    let trigger_count = db_intel.triggers.len();
    let table_count = db_intel.schema.tables.len();
    let db_confidence = if sp_count > 0 && table_count > 0 {
        "✅ High"
    } else if sp_count > 0 || table_count > 0 {
        "⚠️ Medium"
    } else {
        "ℹ️ No SQL files"
    };
    md.push_str(&format!(
        "| Database | {table_count} tables in schema, {sp_count} SPs analyzed, {trigger_count} triggers | {db_confidence} |\n"
    ));

    // Session Workflows
    let wf_count = session_wf.cross_page_chains;
    let wf_confidence = if session_wf.total_keys > 0 {
        if session_wf.warnings.is_empty() {
            "✅ High"
        } else {
            "⚠️ Medium"
        }
    } else {
        "ℹ️ No state detected"
    };
    md.push_str(&format!(
        "| Session Workflows | {} keys, {wf_count} cross-page flows | {wf_confidence} |\n",
        session_wf.total_keys
    ));

    // Data Access
    md.push_str(&format!(
        "| Data Access | {} SPs, {} called from code | {} |\n",
        cross.total_stored_procedures,
        cross.total_sp_called_from_code,
        if cross.total_sp_called_from_code > 0 {
            "✅ High"
        } else if cross.total_stored_procedures > 0 {
            "⚠️ Medium"
        } else {
            "ℹ️ No SPs"
        }
    ));

    // External Integrations
    let ext_count = cross.total_service_endpoints;
    md.push_str(&format!(
        "| External Integrations | {} service endpoints | {} |\n",
        ext_count,
        if ext_count > 0 {
            "⚠️ Medium (contracts not parsed)"
        } else {
            "ℹ️ None detected"
        }
    ));

    md.push('\n');
    md
}
