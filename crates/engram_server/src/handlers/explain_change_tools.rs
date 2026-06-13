//! Handler for `explain_change` — thin wrapper that resolves project
//! metadata, invokes the service, and renders either markdown or JSON.

use std::path::PathBuf;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::handlers::validate_project_id;
use crate::models::requests::ExplainChangeRequest;
use crate::services::explain_change_service::{
    ExplainChangeConfig, SubjectStyle, explain_change as svc_explain,
};
use crate::services::project_service::{ensure_project_record, get_active_generation};
use crate::tools::Engram;

impl Engram {
    pub async fn handle_explain_change(
        &self,
        req: ExplainChangeRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = ensure_project_record(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let generation = get_active_generation(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let project_dir = PathBuf::from(rec.directory.clone());

        let style = match req.subject_style.as_str() {
            "plain" => SubjectStyle::Plain,
            _ => SubjectStyle::Conventional,
        };
        let config = ExplainChangeConfig {
            style,
            include_changelog: req.include_changelog,
            use_llm: req.use_llm,
        };

        let result = svc_explain(
            &self.state,
            &req.project_id,
            &project_dir,
            generation,
            &req.diff,
            &config,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some((narrative, rendered)) = result else {
            return Ok(CallToolResult::success(vec![Content::text(
                "No changes detected in the requested diff. Nothing to explain.".to_string(),
            )]));
        };

        let body = match req.output_format.as_str() {
            "json" => {
                // Emit the structured narrative + the three rendered
                // strings so CI pipelines can pick whichever they
                // need without re-running.
                let payload = serde_json::json!({
                    "narrative": &narrative,
                    "commit_message": rendered.commit_message,
                    "pr_description": rendered.pr_description,
                    "changelog_entry": rendered.changelog_entry,
                });
                serde_json::to_string_pretty(&payload)
                    .map_err(|e| McpError::internal_error(format!("json render: {e}"), None))?
            }
            _ => {
                // Markdown bundle — commit message, PR description,
                // and optional changelog entry concatenated with
                // clear fences so a human reader can copy any slice.
                let mut out = String::with_capacity(2048 + rendered.pr_description.len());
                out.push_str("## Commit message\n\n```\n");
                out.push_str(rendered.commit_message.trim_end());
                out.push_str("\n```\n\n");
                out.push_str("## PR description\n\n");
                out.push_str(&rendered.pr_description);
                if let Some(cl) = &rendered.changelog_entry {
                    out.push_str("\n## Changelog entry\n\n```markdown\n");
                    out.push_str(cl.trim_end());
                    out.push_str("\n```\n");
                }
                out
            }
        };

        tracing::info!(
            project_id = %req.project_id,
            kind = ?narrative.kind,
            scope = ?narrative.scope,
            files = narrative.affected_files.len(),
            alignments = narrative.rule_alignments.len(),
            coupling_notes = narrative.coupling_notes.len(),
            risk = %narrative.risk_badge,
            "explain_change complete"
        );

        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}
