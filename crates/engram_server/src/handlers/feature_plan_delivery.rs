//! Bound complete feature reports while preserving the original JSON for recovery.
use crate::handlers::planning_tools::change_set_delivery as snapshots;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use serde_json::{Value, json};
use std::path::Path;

const TOOL: &str = "one_shot_feature_plan";
const CAPTURE_LIMIT: usize = 1024 * 1024;

pub(super) fn budget(tokens: Option<usize>) -> Result<usize, McpError> {
    Ok(crate::handlers::search_tools::response_budget::token_bytes(tokens)?.unwrap_or(40 * 1024))
}

pub(super) async fn recover(
    data: &Path,
    project: &str,
    id: &str,
    offset: usize,
    limit: usize,
) -> Result<CallToolResult, McpError> {
    let (data, project, id) = (data.to_path_buf(), project.to_owned(), id.to_owned());
    let text = tokio::task::spawn_blocking(move || {
        snapshots::recover_read_report(&data, &project, &id, offset, limit, TOOL)
    })
    .await
    .map_err(|_| McpError::internal_error("Feature response recovery worker failed", None))??;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

pub(super) async fn finish(
    data: &Path,
    project: &str,
    limit: usize,
    report: Value,
) -> Result<CallToolResult, McpError> {
    let body = serde_json::to_string(&report)
        .map_err(|_| McpError::internal_error("Cannot render feature plan", None))?;
    if body.len() > CAPTURE_LIMIT {
        return Err(McpError::internal_error(
            "Feature plan exceeds 1 MiB response bound; narrow the feature or area",
            None,
        ));
    }
    if body.len() <= limit {
        return Ok(CallToolResult::success(vec![Content::text(body)]));
    }
    let id = blake3::hash(body.as_bytes()).to_hex().to_string();
    let preview = json!({
        "status":report["status"], "reason":report["reason"], "project_id":project,
        "detail_level":report["detail_level"], "active_generation":report["active_generation"],
        "provider_calls":report["provenance"]["provider_calls"],
        "response_delivery":{
            "status":"DEFERRED_FEATURE_PLAN", "complete":false, "response_id":id,
            "original_bytes":body.len(), "response_budget_bytes":limit,
            "instruction":"Recover the full original before relying on its plan, source evidence, coverage, or scoped guidance. Concatenate content by byte offset and verify the BLAKE3 response ID. Recovery never reruns retrieval, generation or source freshness checks.",
            "retention":"Up to 24 hours or 32 newer feature-plan snapshots for this project.",
            "next_call":snapshots::read_report_continuation(TOOL, project, &id, 0, limit)
        }
    })
    .to_string();
    if preview.len() > limit {
        return Err(McpError::invalid_params(
            format!(
                "max_tokens is too small for feature-plan recovery metadata; at least {} estimated tokens required",
                preview.len().div_ceil(4)
            ),
            None,
        ));
    }
    let (data, project) = (data.to_path_buf(), project.to_owned());
    tokio::task::spawn_blocking(move || snapshots::save_read_report(&data, &project, &body, TOOL))
        .await
        .map_err(|_| McpError::internal_error("Feature response capture worker failed", None))??;
    Ok(CallToolResult::success(vec![Content::text(preview)]))
}
