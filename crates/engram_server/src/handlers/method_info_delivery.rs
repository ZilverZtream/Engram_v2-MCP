//! Scoped guidance and complete immutable delivery for indexed method metadata.
use super::MethodInfoResult;
use crate::{handlers::planning_tools::change_set_delivery as snapshots,
    models::GetMethodInfoRequest, services::project_policy};
use rmcp::ErrorData as McpError;
use serde_json::json;
use std::{collections::{BTreeMap, BTreeSet}, path::Path};

pub(super) fn attach_policy(root: &Path, rows: &mut [MethodInfoResult]) {
    let mut policies = BTreeMap::new();
    for row in rows {
        let policy = policies.entry(row.file_path.clone()).or_insert_with(|| {
            let mut policy = project_policy::resolve(root, &[row.file_path.clone()], &[]);
            policy.notes.push("Guidance covers this returned method file, not its caller/callee leads or every file a future change may touch. Indexed metadata and previews are not verified current source. Authored guidance does not establish source freshness, implementation compliance or runtime behavior. Recheck guidance for the final edit scope.".into());
            policy
        });
        row.project_policy_sources = Some(policy.clone());
    }
}

pub(super) fn append_policy_markdown(rows: &[MethodInfoResult], out: &mut String) {
    let mut shown = BTreeSet::new();
    for row in rows {
        if shown.insert(&row.file_path) {
            if let Some(policy) = &row.project_policy_sources {
                out.push_str(&format!("\n## Authored guidance for `{}`\n", row.file_path));
                out.push_str(&policy.render_markdown());
            }
        }
    }
}

fn budget(req: &GetMethodInfoRequest) -> Result<usize, McpError> {
    Ok(crate::handlers::search_tools::response_budget::token_bytes(req.max_tokens)?.unwrap_or(40960))
}

pub(super) fn recover(data: &Path, req: &GetMethodInfoRequest) -> Result<Option<String>, McpError> {
    let limit = budget(req)?;
    if let Some(id) = req.response_id.as_deref() {
        return snapshots::recover_method_info(data, &req.project_id, id, req.response_offset, limit).map(Some);
    }
    if req.response_offset != 0 {
        return Err(McpError::invalid_params("response_offset requires response_id from method metadata recovery", None));
    }
    Ok(None)
}

pub(super) fn finish(data: &Path, req: &GetMethodInfoRequest, body: String) -> Result<String, McpError> {
    let limit = budget(req)?;
    if body.len() <= limit { return Ok(body); }
    let id = blake3::hash(body.as_bytes()).to_hex().to_string();
    let manifest = json!({"summary":{
        "metadata_evidence":"DEFERRED_NOT_SUPPLIED", "project_policy_sources_deferred":true,
        "qualifications":"Indexed metadata and previews are not verified current source. Original method identities, coverage limitations, freshness and complete authored guidance remain in the immutable report; none are re-evaluated here."
    },"response_delivery":{
        "status":"DEFERRED_METHOD_INFO", "complete":false, "response_id":id,
        "original_bytes":body.len(), "original_format":if req.output_json {"json"} else {"markdown"},
        "response_budget_bytes":limit,
        "estimate":"ceil(UTF-8 complete reply-text bytes / 4); not provider tokens or transport envelope bytes",
        "instruction":"Recover the complete original report before interpreting metadata or editing. Concatenate content by byte offset and verify response_id as BLAKE3. Recovery performs no project, graph, source or policy lookup and cannot certify current freshness.",
        "retention":"At most 24 hours or 32 newer method-info snapshots for this project; unavailable snapshots require a fresh metadata request.",
        "next_call":snapshots::method_info_continuation(&req.project_id,&id,0,limit)
    }});
    let encoded = serde_json::to_string(&manifest).map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let output = if req.output_json {encoded} else {format!("# Method metadata: deferred evidence\n\n```json\n{encoded}\n```\n")};
    if output.len() > limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for complete method metadata recovery controls; at least {} estimated tokens ({} UTF-8 bytes) required. Increase max_tokens or narrow the request. No metadata or guidance was silently clipped.",output.len().div_ceil(4),output.len()),None));
    }
    snapshots::save_method_info(data,&req.project_id,&body)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn method_info_recovery_preserves_array_rows_and_complete_original_guidance_in_both_formats() {
        let owned = tempfile::tempdir().unwrap();
        let guide = "Preserve accepted manual approval exceptions. Å\n".repeat(400);
        std::fs::write(owned.path().join("AGENTS.md"), &guide).unwrap();
        let policy = project_policy::resolve(owned.path(), &["Site/Save.vb".into()], &[]);
        for output_json in [true, false] {
            let req: GetMethodInfoRequest = serde_json::from_value(json!({"project_id":"project","fqn_or_name":"Save","output_json":output_json,"max_tokens":1024})).unwrap();
            let original = if output_json {json!([{"fqn":"Save","file_path":"Site/Save.vb","body_preview":"Indexed preview, not current source","project_policy_sources":policy}]).to_string()}
                else {format!("# Save\nIndexed preview, not current source\n{}", policy.render_markdown())};
            let raw = finish(owned.path(), &req, original.clone()).unwrap(); assert!(raw.len() <= 4096);
            let encoded = if output_json {raw.as_str()} else {raw.split("```json\n").nth(1).unwrap().split("\n```").next().unwrap()};
            let manifest: Value = serde_json::from_str(encoded).unwrap();
            assert_eq!(manifest["summary"]["metadata_evidence"],"DEFERRED_NOT_SUPPLIED");
            std::fs::write(owned.path().join("AGENTS.md"), "Changed guidance\n").unwrap();
            let mut restored = String::new(); let mut next = manifest["response_delivery"]["next_call"].clone();
            loop {
                assert_eq!(next["tool"],"get_method_info"); assert_eq!(next["arguments"]["max_tokens"],1024);
                let request: GetMethodInfoRequest = serde_json::from_value(next["arguments"].clone()).unwrap(); assert!(request.fqn_or_name.is_empty());
                let raw = recover(owned.path(), &request).unwrap().unwrap(); assert!(raw.len() <= 4096);
                let page: Value = serde_json::from_str(&raw).unwrap(); assert_eq!(page["start_byte"],restored.len());
                restored.push_str(page["content"].as_str().unwrap()); if page["complete"] == true {break;} next = page["next_call"].clone();
            }
            assert_eq!(restored,original); assert_eq!(blake3::hash(restored.as_bytes()).to_hex().as_str(),manifest["response_delivery"]["response_id"].as_str().unwrap());
            if output_json {assert!(serde_json::from_str::<Value>(&restored).unwrap().is_array());}
            assert!(snapshots::recover_method_source(owned.path(),"project",manifest["response_delivery"]["response_id"].as_str().unwrap(),0,4096).is_err());
            let mut large = req.clone(); large.max_tokens = Some(1_000_000); assert_eq!(finish(owned.path(), &large, original.clone()).unwrap(),original);
            large.max_tokens = None; assert!(finish(owned.path(), &large, original).unwrap().len() <= 40960);
        }
    }

    #[test]
    fn invalid_method_info_controls_and_minimum_metadata_refusal_write_no_snapshot() {
        let owned = tempfile::tempdir().unwrap();
        let mut req: GetMethodInfoRequest = serde_json::from_value(json!({"project_id":"project","fqn_or_name":"Save","max_tokens":0})).unwrap();
        assert!(recover(owned.path(), &req).is_err()); req.max_tokens = Some(usize::MAX); assert!(recover(owned.path(), &req).is_err());
        req.max_tokens = Some(1024); req.response_offset = 1; assert!(recover(owned.path(), &req).is_err());
        req.response_offset = 0; req.max_tokens = Some(1); assert!(finish(owned.path(), &req, "metadata".repeat(100)).is_err());
        assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(),0);
    }
}
