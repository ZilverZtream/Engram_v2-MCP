//! Whole-report delivery. Gate execution and finding selection remain in the parent.
use crate::handlers::planning_tools::change_set_delivery as snapshots;
use crate::handlers::search_tools::response_budget::token_bytes;
use crate::models::requests::PreCommitReviewRequest;
use rmcp::{ErrorData as McpError, model::{CallToolResult, Content}};
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn budget(tokens: Option<usize>) -> Result<usize, McpError> {
    Ok(token_bytes(tokens)?.unwrap_or(40 * 1024))
}

pub(super) fn finish(req: &PreCommitReviewRequest, data: &Path, body: String, report: &Value) -> Result<CallToolResult, McpError> {
    let limit = budget(req.max_tokens)?;
    let deferred_maintenance = req.quick && report.get("instruction_maintenance_context").is_some();
    if body.len() <= limit && !deferred_maintenance {
        return Ok(CallToolResult::success(vec![Content::text(body)]));
    }
    let id = blake3::hash(body.as_bytes()).to_hex().to_string();
    let ledger: Vec<_> = report["findings"].as_array().into_iter().flatten().enumerate().map(|(index, f)| {
        let mut row = json!({"finding_index":index,"finding_id":f["finding_id"],"severity":f["severity"],"gate":f["gate"],
            "file_path":f["file_path"],"lines":f.get("lines").cloned().unwrap_or(json!([])),"title":f["title"]});
        if let Some(annotations) = f["prior_review_annotations"].as_array() {
            row["prior_review_annotation_count"] = json!(annotations.len());
        }
        row
    }).collect();
    let coverage = &report["coverage"];
    let count = |key: &str| coverage[key].as_array().map(Vec::len);
    let mut manifest = json!({
        "verdict":report["verdict"],"summary":report["summary"],"finding_ledger":ledger,"mode":report["mode"],"file_observation_count":report["summary"]["file_observations"],
        "gate_status":report["gate_status"],
        "instruction_maintenance_context":super::maintenance_context::summary(&report["instruction_maintenance_context"]),
        "coverage":{
            "path_prefixes":coverage["path_prefixes"],
            "git_diff_source":coverage["git_diff_source"],
            "submitted_files":count("submitted_files"),"textual_diff_files":count("textual_diff_files"),
            "unexamined_files":count("unexamined_files"),"unindexed_files":count("unindexed_files"),
            "static_analysis":coverage["static_analysis"],"compilation":"not_run","test_execution":"not_run",
            "changed_during_review":coverage["changed_during_review"],"source_snapshot_complete":coverage["source_snapshot_complete"],
            "head_before":coverage["head_before"],"head_after":coverage["head_after"],"diff_blake3":coverage["diff_blake3"],
            "review_decisions":coverage["review_decisions"],
            "scope":"Static gate coverage only. Counts refer to the submitted scope. Null is unknown/not assessed. Recover the original coverage, policy and decision evidence before acting."
        },
        "response_delivery":{
            "status":"DEFERRED_REVIEW","complete":false,"response_id":id,"original_bytes":body.len(),
            "original_format":if req.output_json {"json"} else {"markdown"},"response_budget_bytes":limit,
            "estimate":"ceil(UTF-8 complete reply-text bytes / 4); not provider tokens or transport envelope bytes",
            "finding_scope":"Every finding selected by max_findings/min_severity remains in this ledger. The original requested compact/full view and its evidence limits are preserved; this is not proof all possible findings were found.",
            "instruction":"Recover the complete original report before resolving findings or applying prior decisions. Concatenate content by byte offset and verify response_id as BLAKE3. Findings remain visible; prior decisions do not suppress them. Recovery does not rerun gates or certify current source.",
            "retention":"At most 24 hours or 32 newer review snapshots for this project; unavailable snapshots require a fresh review.",
            "next_call":snapshots::review_continuation(&req.project_id,&id,0,limit)
        }
    });
    if let Some(patch) = report.get("reviewed_patch") {
        manifest["reviewed_patch"] = patch.clone();
    }
    let render = |page: &Value| -> Result<String, McpError> {
        let json = serde_json::to_string(page).map_err(|e| McpError::internal_error(e.to_string(),None))?;
        Ok(if req.output_json { json } else { format!("# Pre-commit review\n\n```json\n{json}\n```\n") })
    };
    let mut text = render(&manifest)?;
    if text.len() > limit {
        manifest["gate_status"] = json!({"detail":"available in the saved report"});
        manifest["response_delivery"]["finding_scope"] = json!("First page of the selected line findings. Recover the saved report for the full ledger, gate evidence and file observations.");
        manifest["response_delivery"]["total_line_findings"] = json!(manifest["finding_ledger"].as_array().map(Vec::len));
        text = render(&manifest)?;
        while text.len() > limit && manifest["finding_ledger"].as_array_mut().and_then(Vec::pop).is_some() {
            text = render(&manifest)?;
        }
    }
    if text.len() > limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for the review summary and recovery pointer: at least {} estimated tokens required.",text.len().div_ceil(4)),None));
    }
    snapshots::save_review(data,&req.project_id,&body)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}
