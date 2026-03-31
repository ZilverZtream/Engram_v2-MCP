pub mod access_layer_tools;
pub mod cognitive_tools;
pub mod git_tools;
pub mod graph_tools;
pub mod migration_tools;
pub mod project_tools;
pub mod search_tools;

pub mod runtime_observation_tools;

// ─── REG1/MCP1: Shared handler-boundary validation ───────────────────────────

/// Validate a user-supplied project_id at the MCP handler boundary.
///
/// Rejects empty strings, NUL bytes, and newline characters that would corrupt
/// composite registry keys. Calling this before any service or registry call
/// makes the validation surface explicit and prevents bypass via execution paths
/// that do not immediately reach the registry layer.
pub(super) fn validate_project_id(project_id: &str) -> Result<(), rmcp::ErrorData> {
    engram_core::security::validate_key_component("project_id", project_id)
        .map_err(|e| rmcp::ErrorData::invalid_params(e, None))
}
