//! Complete database reports use the existing immutable response cache.
use crate::handlers::planning_tools::change_set_delivery as snapshots;
use crate::handlers::search_tools::response_budget::token_bytes;
use crate::models::AnalyzeDatabaseIntelligenceRequest;
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn budget(tokens: Option<usize>) -> Result<usize, McpError> {
    Ok(token_bytes(tokens)?.unwrap_or(65_536))
}

pub(super) fn recover(data: &Path, req: &AnalyzeDatabaseIntelligenceRequest) -> Result<Option<String>, McpError> {
    let limit = budget(req.max_tokens)?;
    if let Some(id) = req.response_id.as_deref() {
        return snapshots::recover_database_report(data, &req.project_id, id, req.response_offset, limit).map(Some);
    }
    if req.response_offset != 0 {
        return Err(McpError::invalid_params("response_offset requires response_id from database report recovery", None));
    }
    Ok(None)
}

pub(super) fn finish(data: &Path, req: &AnalyzeDatabaseIntelligenceRequest, body: String, summary: Value) -> Result<String, McpError> {
    let limit = budget(req.max_tokens)?;
    if body.len() <= limit { return Ok(body); }
    let id = blake3::hash(body.as_bytes()).to_hex().to_string();
    let manifest = json!({"summary":summary, "response_delivery":{
        "status":"DEFERRED_DATABASE_REPORT", "complete":false, "response_id":id,
        "original_bytes":body.len(), "original_format":if req.output_json {"json"} else {"markdown"},
        "response_budget_bytes":limit,
        "estimate":"ceil(UTF-8 complete reply-text bytes / 4); not provider tokens or transport envelope bytes",
        "scope":"Summary counts describe the selected original report within existing SQL discovery and sp_limit bounds, not every database object or every possible finding.",
        "instruction":"Recover the complete original before acting on procedure, schema or trigger details. Concatenate content by byte offset and verify response_id as BLAKE3. Recovery does not rerun analysis or certify current SQL/database state.",
        "retention":"At most 24 hours or 32 newer database-report snapshots for this project; unavailable snapshots require a fresh analysis.",
        "next_call":snapshots::database_report_continuation(&req.project_id, &id, 0, limit)
    }});
    let encoded = serde_json::to_string(&manifest).map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let output = if req.output_json { encoded } else {
        format!("# Database report: deferred evidence\n\nRecover the complete report before acting on its details.\n\n```json\n{encoded}\n```\n")
    };
    if output.len() > limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for database report metadata and warnings; at least {} estimated tokens ({} UTF-8 bytes) required. No evidence was silently removed.", output.len().div_ceil(4), output.len()), None));
    }
    snapshots::save_database_report(data, &req.project_id, &body)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_report_formats_recover_exact_original_with_warnings_and_small_reply_compatibility() {
        let temporary = tempfile::tempdir().unwrap();
        for output_json in [false, true] {
            let mut req: AnalyzeDatabaseIntelligenceRequest = serde_json::from_value(json!({
                "project_id":"project", "output_json":output_json, "max_tokens":1024})).unwrap();
            let body = if output_json { json!({"sp_logic":[{"purpose":"Å table \"quotation\"\n".repeat(600)}]}).to_string() }
                else { "# Database report\n\nÅ table `quotation`\n".repeat(600) };
            let summary = json!({"stored_procedures":1, "warnings":["Static source analysis only"]});
            let first = finish(temporary.path(), &req, body.clone(), summary.clone()).unwrap();
            assert!(first.len() <= 4096);
            let encoded = if output_json { first.as_str() } else { first.split("```json\n").nth(1).unwrap().split("\n```").next().unwrap() };
            let manifest: Value = serde_json::from_str(encoded).unwrap();
            assert_eq!(manifest["summary"], summary);
            assert_eq!(manifest["response_delivery"]["original_format"], if output_json {"json"} else {"markdown"});
            let next = &manifest["response_delivery"]["next_call"];
            assert_eq!(next["tool"], "analyze_database_intelligence");
            req.response_id = Some(next["arguments"]["response_id"].as_str().unwrap().into());
            let mut original = String::new();
            loop {
                req.response_offset = original.len();
                let raw = recover(temporary.path(), &req).unwrap().unwrap();
                assert!(raw.len() <= 4096);
                let page: Value = serde_json::from_str(&raw).unwrap();
                original.push_str(page["content"].as_str().unwrap());
                if page["complete"] == true { break; }
                assert_eq!(page["next_call"]["tool"], "analyze_database_intelligence");
            }
            assert_eq!(original, body);
            assert_eq!(blake3::hash(original.as_bytes()).to_hex().as_str(), req.response_id.as_deref().unwrap());
            assert!(snapshots::recover_request_trace(temporary.path(), "project", req.response_id.as_deref().unwrap(), 0, 4096).is_err());
            req.response_id = None;
            req.response_offset = 0;
            assert_eq!(finish(temporary.path(), &req, "unchanged small report".into(), summary).unwrap(), "unchanged small report");
        }
    }

    #[test]
    fn invalid_controls_and_too_small_budget_create_no_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let mut req: AnalyzeDatabaseIntelligenceRequest = serde_json::from_value(json!({"project_id":"project", "max_tokens":0})).unwrap();
        assert!(recover(temporary.path(), &req).is_err());
        req.max_tokens = Some(1);
        assert!(finish(temporary.path(), &req, "large report".into(), json!({})).is_err());
        req.response_offset = 1;
        assert!(recover(temporary.path(), &req).is_err());
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 0);
    }
}
