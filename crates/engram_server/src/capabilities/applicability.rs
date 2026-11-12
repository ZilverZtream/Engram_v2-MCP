//! Per-request applicability for legacy-only public tools. Discovery is a union
//! across projects; it cannot establish applicability to the requested project.
use crate::tools::Engram;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

/// Scope only concrete target paths, never substring/glob filters. A mixed
/// project's WebForms evidence must not turn a TypeScript file into a page.
fn accepts_target(tool: &str, path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let extension = lower.rsplit_once('.').map_or("", |(_, ext)| ext);
    match tool {
        "get_page_context" | "map_validation_controls" | "map_ajax_regions" => {
            matches!(extension, "aspx" | "ascx" | "master")
        }
        "get_vb_translation_traps" => extension == "vb",
        "analyze_sync_hazards"
        | "generate_characterization_tests"
        | "generate_instrumentation_code" => matches!(extension, "cs" | "vb"),
        "trace_ui_event"
        | "get_ui_blueprint"
        | "map_page_lifecycle"
        | "analyze_viewstate_deps"
        | "trace_data_flow"
        | "generate_migration_scaffold"
        | "get_migration_dossier"
        | "check_migration_coverage" => {
            matches!(extension, "aspx" | "ascx" | "master" | "asp" | "cs" | "vb")
        }
        // Migration status can track supporting JS/config assets too.
        _ => true,
    }
}

impl Engram {
    /// Called after typed argument deserialization and before dispatch to a
    /// legacy service. Returning a result means no analysis or mutation ran.
    pub(crate) async fn legacy_tool_applicability(
        &self,
        tool: &str,
        project_id: &str,
        targets: &[&str],
    ) -> Result<Option<CallToolResult>, McpError> {
        let runtime = self.ensure_project_runtime(project_id).await?;
        for path in targets {
            engram_core::safe_join(std::path::Path::new(&runtime.info.directory), path)
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        }
        let (generation, view) = self
            .pin_active_source_generation(project_id, &runtime.search)
            .await?;
        let (capabilities, metadata) =
            crate::services::project_service::selected_project_capabilities(
                &runtime.info.project_type,
                generation,
                &view,
            );
        let rejection = if !capabilities.allows_legacy_tool(tool) {
            if view.selected_file_revisions().is_none() {
                Some(("applicability_unknown", "Source capability coverage is unavailable. Update the project before requesting specialized analysis.".to_owned()))
            } else if tool == "benchmark_retrieval_defaults" {
                Some(("not_applicable", "Not applicable to this project type: built-in benchmark queries target WebForms. Supply nonempty custom_queries with authored relevant_paths for this project's retrieval benchmark.".to_owned()))
            } else {
                Some(("not_applicable", "Not applicable to this project type: the selected source has no supported legacy capability for this tool.".to_owned()))
            }
        } else {
            targets.iter().find(|path| !accepts_target(tool, path)).map(|path| {
                ("not_applicable", format!("Not applicable to this target file: {path}. Its source language/format is outside this tool's supported legacy inputs."))
            })
        };
        Ok(rejection.map(|(status, message)| {
            CallToolResult::success(vec![Content::text(
                serde_json::json!({
                    "format": "engram-tool-applicability-v1",
                    "status": status,
                    "tool": tool,
                    "project_id": project_id,
                    "message": message,
                    "analysis_performed": false,
                    "project_capabilities": metadata
                })
                .to_string(),
            )])
        }))
    }
}
