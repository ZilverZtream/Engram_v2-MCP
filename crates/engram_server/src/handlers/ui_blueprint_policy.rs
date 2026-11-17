//! Current authored guidance accompanies the requested indexed layout.
use crate::{models::GetUiBlueprintRequest, services::project_policy, tools::Engram};
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
        let record = self.ensure_project_record(&project).await?;
        let policy = tokio::task::spawn_blocking(move || {
            let mut policy = project_policy::resolve(Path::new(&record.directory), &[path], &[])
                .with_source_recovery(&project);
            policy.notes.push("Guidance applies to the requested layout path. Recheck instructions for the final edit scope. Layout data remains an indexed observation; current guidance does not verify layout freshness, UI compliance or runtime behavior.".into());
            policy
        }).await.map_err(|error| McpError::internal_error(error.to_string(), None))?;
        report["project_policy_sources"] = serde_json::to_value(policy)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        result.content[0] = Content::text(
            serde_json::to_string(&report)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?,
        );
        Ok(result)
    }
}
