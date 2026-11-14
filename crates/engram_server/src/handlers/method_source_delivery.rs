//! Complete direct source/policy reports use the existing immutable cache.
use crate::{handlers::planning_tools::change_set_delivery as snapshots,models::GetFullMethodBodyRequest,services::project_policy::PolicyContext};
use rmcp::ErrorData as McpError;
use serde_json::json;
use std::{collections::BTreeSet,path::Path};

pub(super) fn budget(req: &GetFullMethodBodyRequest) -> Result<usize,McpError> {
    let window=req.response_id.is_some() || req.source_ranges.is_some()
        || (req.fqn.is_none() && req.file_path.is_some() && req.line_end.is_none());
    let limit=crate::handlers::search_tools::response_budget::token_bytes(req.max_tokens)?
        .unwrap_or(if window {32768} else {40960});
    Ok(if window {limit.min(32768)} else {limit})
}

pub(super) fn recover(data: &Path,req: &GetFullMethodBodyRequest) -> Result<Option<String>,McpError> {
    let limit=budget(req)?;
    if let Some(id)=req.response_id.as_deref() {
        return snapshots::recover_method_source(data,&req.project_id,id,req.response_offset,limit).map(Some);
    }
    if req.response_offset!=0 {return Err(McpError::invalid_params("response_offset requires response_id from method/source recovery",None));}
    Ok(None)
}

pub(super) fn finish(data: &Path,req: &GetFullMethodBodyRequest,body: String,paths: &[String],policy: &PolicyContext) -> Result<String,McpError> {
    let limit=budget(req)?;
    if body.len()<=limit {return Ok(body);}
    let id=blake3::hash(body.as_bytes()).to_hex().to_string();let recovery_limit=limit.min(32768);
    let paths=paths.iter().collect::<BTreeSet<_>>();
    let manifest=json!({"summary":{
        "source_paths":paths,"source_evidence":"DEFERRED_NOT_SUPPLIED",
        "original_qualifications":"Original source hashes, bounds, caller expansion, freshness and boundary qualifications remain in the immutable report and must be recovered before interpreting it. They are not re-evaluated here.",
        "project_policy_sources":{"status":policy.status,"coverage":policy.coverage,"source_revision":policy.source_revision,
            "sources_applicable":policy.sources_applicable,"original_sources_shown":policy.sources_shown,"sources_shown":0,
            "omissions":policy.omissions,"reference_coverage":policy.reference_coverage,"references_omitted":policy.references_omitted,
            "notes":policy.notes,"instruction":policy.instruction,"content_deferred":true}
        },"response_delivery":{
        "status":"DEFERRED_METHOD_SOURCE","complete":false,"response_id":id,"original_bytes":body.len(),
        "original_format":if req.output_json {"json"} else {"markdown"},"response_budget_bytes":limit,"recovery_budget_bytes":recovery_limit,
        "estimate":"ceil(UTF-8 complete reply-text bytes / 4); not provider tokens or transport envelope bytes",
        "instruction":"No source is supplied in this reply. Recover the complete original report before answering from source or editing. Concatenate content by byte offset and verify response_id as BLAKE3. Source windows remain their original complete windows after reconstruction. Recovery performs no new source, policy, project or graph lookup and cannot certify current freshness.",
        "retention":"At most 24 hours or 32 newer method-source snapshots for this project; unavailable snapshots require a fresh source request.",
        "next_call":snapshots::method_source_continuation(&req.project_id,&id,0,recovery_limit)
    }});
    let encoded=serde_json::to_string(&manifest).map_err(|error|McpError::internal_error(error.to_string(),None))?;
    let output=if req.output_json {encoded} else {format!("# Method/source: deferred evidence\n\nRecover the complete original report before using its source.\n\n```json\n{encoded}\n```\n")};
    if output.len()>limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for method/source identities, policy and recovery metadata; at least {} estimated tokens ({} UTF-8 bytes) required. Increase max_tokens or narrow the source request. No source or qualifications were silently removed.",output.len().div_ceil(4),output.len()),None));
    }
    snapshots::save_method_source(data,&req.project_id,&body)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn method_source_recovery_preserves_exact_source_policy_and_qualifications_in_both_formats() {
        let owned=tempfile::tempdir().unwrap();let guide="Preserve the accepted manual approval exception.\n";
        std::fs::write(owned.path().join("AGENTS.md"),guide).unwrap();
        let paths=vec!["Site/Method.vb".into(),"Site/Caller.vb".into()];let policy=crate::services::project_policy::resolve(owned.path(),&paths,&[]);
        let code="' Å quoted \"source\"\r\n".repeat(4000);
        for output_json in [false,true] {
            let req:GetFullMethodBodyRequest=serde_json::from_value(json!({"project_id":"project","fqn":"SaveOrder","max_tokens":2048,"output_json":output_json})).unwrap();
            let original=if output_json {json!({"source_code":code,"source_file_hash":"original-file-hash","source_verification":"UNVERIFIED_BOUNDARIES","caller_expansion":{"complete":false},"freshness":{"warning":"INDEX_BEHIND"},"project_policy_sources":policy}).to_string()}
                else {format!("# Method\n{code}\nSource verification: UNVERIFIED_BOUNDARIES\nFreshness: INDEX_BEHIND\n{}",policy.render_markdown())};
            let raw=finish(owned.path(),&req,original.clone(),&paths,&policy).unwrap();assert!(raw.len()<=8192);
            let encoded=if output_json {raw.as_str()} else {raw.split("```json\n").nth(1).unwrap().split("\n```").next().unwrap()};
            let manifest:Value=serde_json::from_str(encoded).unwrap();assert_eq!(manifest["summary"]["source_evidence"],"DEFERRED_NOT_SUPPLIED");
            std::fs::write(owned.path().join("AGENTS.md"),"Changed live policy\n").unwrap();
            let mut next=manifest["response_delivery"]["next_call"].clone();let mut restored=String::new();
            loop {
                assert_eq!(next["tool"],"get_full_method_body");assert_eq!(next["arguments"]["max_tokens"],2048);
                let req:GetFullMethodBodyRequest=serde_json::from_value(next["arguments"].clone()).unwrap();assert!(req.fqn.is_none() && req.file_path.is_none());
                let raw=recover(owned.path(),&req).unwrap().unwrap();assert!(raw.len()<=8192);
                let page:Value=serde_json::from_str(&raw).unwrap();assert_eq!(page["start_byte"],restored.len());
                restored.push_str(page["content"].as_str().unwrap());if page["complete"]==true {break;}next=page["next_call"].clone();
            }
            assert_eq!(restored,original);assert_eq!(blake3::hash(restored.as_bytes()).to_hex().as_str(),manifest["response_delivery"]["response_id"].as_str().unwrap());
            assert!(restored.contains("UNVERIFIED_BOUNDARIES") && restored.contains("INDEX_BEHIND") && restored.contains("manual approval exception"));
            assert!(snapshots::recover_pattern_report(owned.path(),"project",manifest["response_delivery"]["response_id"].as_str().unwrap(),0,8192).is_err());
            let mut large=req.clone();large.max_tokens=Some(1_000_000);assert_eq!(finish(owned.path(),&large,original.clone(),&paths,&policy).unwrap(),original);
            large.max_tokens=None;assert!(finish(owned.path(),&large,original,&paths,&policy).unwrap().len()<=40960);
            assert_eq!(finish(owned.path(),&req,"small unchanged report".into(),&paths,&policy).unwrap(),"small unchanged report");
        }
    }

    #[test]
    fn source_window_and_recovery_ceilings_and_invalid_controls_remain_bounded_without_writes() {
        let owned=tempfile::tempdir().unwrap();let policy=crate::services::project_policy::resolve(owned.path(),&[],&[]);
        let mut req:GetFullMethodBodyRequest=serde_json::from_value(json!({"project_id":"project","file_path":"Site/Method.vb"})).unwrap();
        assert_eq!(budget(&req).unwrap(),32768);req.max_tokens=Some(1_000_000);assert_eq!(budget(&req).unwrap(),32768);
        req.file_path=None;req.fqn=Some("SaveOrder".into());req.max_tokens=None;assert_eq!(budget(&req).unwrap(),40960);
        req.response_id=Some("a".repeat(64));req.max_tokens=Some(1_000_000);assert_eq!(budget(&req).unwrap(),32768);
        req.response_id=None;req.max_tokens=Some(0);assert!(recover(owned.path(),&req).is_err());
        req.max_tokens=Some(usize::MAX);assert!(recover(owned.path(),&req).is_err());
        req.max_tokens=Some(2048);req.response_offset=1;assert!(recover(owned.path(),&req).is_err());
        req.response_offset=0;req.max_tokens=Some(1);assert!(finish(owned.path(),&req,"large source".repeat(500),&[],&policy).is_err());
        assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(),0);
    }
}
