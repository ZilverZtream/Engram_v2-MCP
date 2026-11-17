//! Current authored guidance accompanies explicit-file UI inspection reports.
use crate::{
    models::{
        AnalyzeViewStateDepsRequest, GetUiBlueprintRequest, MapAjaxRegionsRequest,
        MapPageLifecycleRequest, MapValidationControlsRequest,
    },
    services::project_policy,
    tools::Engram,
};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};
use std::path::Path;

impl Engram {
    pub async fn handle_get_ui_blueprint_with_policy(
        &self,
        req: GetUiBlueprintRequest,
    ) -> Result<CallToolResult, McpError> {
        let project = req.project_id.clone();
        let path = req.file_path.replace('\\', "/");
        let mut result = self.handle_get_ui_blueprint(req).await?;
        if result.is_error == Some(true) {
            return Ok(result);
        }
        let Some(text) = result.content.first().and_then(|part| part.as_text()) else {
            return Ok(result);
        };
        let mut report = match serde_json::from_str::<Value>(&text.text) {
            Ok(value) if value.is_object() => value,
            _ => json!({"status":"layout_unavailable","file":path,"message":text.text}),
        };
        let policy = self.file_ui_policy_context(project, path).await?;
        report["project_policy_sources"] = serde_json::to_value(policy)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        result.content[0] = Content::text(
            serde_json::to_string(&report)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?,
        );
        Ok(result)
    }

    async fn file_ui_policy_context(
        &self,
        project: String,
        path: String,
    ) -> Result<project_policy::PolicyContext, McpError> {
        let record = self.ensure_project_record(&project).await?;
        tokio::task::spawn_blocking(move || {
            let mut policy = project_policy::resolve(Path::new(&record.directory), &[path.replace('\\', "/")], &[])
                .with_source_recovery(&project);
            policy.notes.push("Guidance applies to the explicitly requested file only. Recheck instructions for companions and the final edit scope. Current guidance does not verify report freshness, inferred recommendations, UI compliance or runtime behavior.".into());
            policy
        }).await.map_err(|error| McpError::internal_error(error.to_string(), None))
    }

    async fn file_ui_report_policy(
        &self,
        project: String,
        path: String,
        mut result: CallToolResult,
        output_json: bool,
    ) -> Result<CallToolResult, McpError> {
        if result.is_error == Some(true) {
            return Ok(result);
        }
        let Some(text) = result.content.first().and_then(|part| part.as_text()) else {
            return Ok(result);
        };
        let original = text.text.clone();
        let policy = self.file_ui_policy_context(project, path).await?;
        let text = if output_json {
            let mut report = match serde_json::from_str::<Value>(&original) {
                Ok(value) if value.is_object() => value,
                // The legacy AJAX mapper formats Markdown even for output_json.
                // Retain that report explicitly rather than discarding its data.
                _ => json!({"report_format":"markdown","report":original}),
            };
            report["project_policy_sources"] = serde_json::to_value(policy)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            serde_json::to_string(&report)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?
        } else {
            format!("{original}{}", policy.render_markdown())
        };
        result.content[0] = Content::text(text);
        Ok(result)
    }

    pub async fn handle_map_validation_controls_with_policy(
        &self,
        req: MapValidationControlsRequest,
    ) -> Result<CallToolResult, McpError> {
        let (project, path, output_json) = (
            req.project_id.clone(),
            req.file_path.clone(),
            req.output_json,
        );
        let result = self.handle_map_validation_controls(req).await?;
        self.file_ui_report_policy(project, path, result, output_json)
            .await
    }
    pub async fn handle_map_page_lifecycle_with_policy(
        &self,
        req: MapPageLifecycleRequest,
    ) -> Result<CallToolResult, McpError> {
        let (project, path, output_json) = (
            req.project_id.clone(),
            req.file_path.clone(),
            req.output_json,
        );
        let result = self.handle_map_page_lifecycle(req).await?;
        self.file_ui_report_policy(project, path, result, output_json)
            .await
    }
    pub async fn handle_analyze_viewstate_deps_with_policy(
        &self,
        req: AnalyzeViewStateDepsRequest,
    ) -> Result<CallToolResult, McpError> {
        let (project, path, output_json) = (
            req.project_id.clone(),
            req.file_path.clone(),
            req.output_json,
        );
        let result = self.handle_analyze_viewstate_deps(req).await?;
        self.file_ui_report_policy(project, path, result, output_json)
            .await
    }
    pub async fn handle_map_ajax_regions_with_policy(
        &self,
        req: MapAjaxRegionsRequest,
    ) -> Result<CallToolResult, McpError> {
        let (project, path, output_json) = (
            req.project_id.clone(),
            req.file_path.clone(),
            req.output_json,
        );
        let result = self.handle_map_ajax_regions(req).await?;
        self.file_ui_report_policy(project, path, result, output_json)
            .await
    }
}
