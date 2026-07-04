pub mod access_layer_tools;
pub mod ask_tools;
pub mod code_review_tools;
pub mod cognitive_tools;
pub mod explain_change_tools;
pub mod git_tools;
pub mod graph_tools;
pub mod grep_tools;
pub mod migration_tools;
pub mod planning_tools;
pub mod pr_history_tools;
pub mod project_tools;
pub mod quality_gate_tools;
pub mod review_tools;
pub mod search_tools;
pub mod settings_tools;

pub mod runtime_observation_tools;

// ─── REG1/MCP1: Shared handler-boundary validation ───────────────────────────

/// Validate a user-supplied project_id at the MCP handler boundary.
///
/// Delegates to the canonical strict validator in `project_service` which enforces
/// `[A-Za-z0-9_-]{1,128}`. This is the single source of truth for project_id
/// policy — both the handler boundary and every service call use identical rules,
/// closing the REG1/X1 trust-boundary gap where the weaker `validate_key_component`
/// (NUL/newline-only) allowed `/`, `..`, and shell metacharacters through the
/// handler layer before reaching filesystem-sensitive operations.
pub(super) fn validate_project_id(project_id: &str) -> Result<(), rmcp::ErrorData> {
    crate::services::project_service::validate_project_id(project_id)
        .map_err(|e| rmcp::ErrorData::invalid_params(e.to_string(), None))
}
