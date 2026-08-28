//! Handler for the flagship `pre_commit_review` tool.
//!
//! The handler orchestrates the 10 gates in `pre_commit_review_service`
//! and formats the result as either markdown (human-facing) or JSON
//! (CI-facing). All the heavy lifting lives in the service module — this
//! is deliberately thin.

use std::path::PathBuf;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::handlers::validate_project_id;
use crate::models::requests::PreCommitReviewRequest;
use crate::services::pre_commit_review_service::{
    ReviewConfig, Severity, render_json, render_markdown, resolve_diff_source,
    run_pre_commit_review,
};
use crate::services::project_service::{ensure_project_record, get_active_generation};
use crate::tools::Engram;

impl Engram {
    pub async fn handle_pre_commit_review(
        &self,
        req: PreCommitReviewRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = ensure_project_record(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let generation = get_active_generation(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let project_dir = PathBuf::from(rec.directory.clone());

        let start = std::time::Instant::now();

        // Resolve the diff input into raw unified-diff text.
        let diff_text = resolve_diff_source(&project_dir, &req.diff)
            .map_err(|e| McpError::invalid_params(format!("diff resolution failed: {e}"), None))?;

        if diff_text.trim().is_empty() {
            let body = "No changes detected in the requested diff. Nothing to review.";
            return Ok(CallToolResult::success(vec![Content::text(
                body.to_string(),
            )]));
        }

        let config = ReviewConfig {
            max_findings: req.max_findings.clamp(1, 200),
            min_severity: Severity::from_str(&req.min_severity).unwrap_or(Severity::Style),
            skip_gates: req.skip_gates.iter().cloned().collect(),
            output_json: req.output_json,
        };

        let (findings, gates_run, files_analysed, outcomes) = run_pre_commit_review(
            &self.state,
            &req.project_id,
            &project_dir,
            generation,
            &diff_text,
            &config,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let elapsed_ms = start.elapsed().as_millis();

        tracing::info!(
            project_id = %req.project_id,
            files = files_analysed,
            findings = findings.len(),
            gates_run,
            elapsed_ms,
            "pre_commit_review complete"
        );

        let body = if config.output_json {
            let payload = render_json(findings, files_analysed, gates_run, elapsed_ms, &outcomes);
            serde_json::to_string_pretty(&payload)
                .map_err(|e| McpError::internal_error(format!("json render: {e}"), None))?
        } else {
            render_markdown(&findings, files_analysed, gates_run, elapsed_ms, &outcomes)
        };

        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}
