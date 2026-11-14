//! Whole-report delivery for standalone pattern retrieval; composed planners keep full evidence.
use super::{PatternJson,change_set_delivery as snapshots};
use crate::models::FindImplementationPatternRequest;
use rmcp::ErrorData as McpError;
use serde_json::json;
use std::path::Path;

fn budget(tokens: Option<usize>) -> Result<usize,McpError> {
    Ok(crate::handlers::search_tools::response_budget::token_bytes(tokens)?.unwrap_or(40*1024))
}

pub(super) fn recover(data: &Path,req: &FindImplementationPatternRequest) -> Result<Option<String>,McpError> {
    let limit=budget(req.max_tokens)?;
    if let Some(id)=req.response_id.as_deref() {
        return snapshots::recover_pattern_report(data,&req.project_id,id,req.response_offset,limit).map(Some);
    }
    if req.response_offset!=0 {
        return Err(McpError::invalid_params("response_offset requires response_id from pattern-report recovery",None));
    }
    Ok(None)
}

pub(super) fn finish(data: &Path,req: &FindImplementationPatternRequest,body: String,report: &PatternJson,public_delivery: bool) -> Result<String,McpError> {
    if !public_delivery {return Ok(body);}
    let limit=budget(req.max_tokens)?;
    if body.len()<=limit {return Ok(body);}
    let id=blake3::hash(body.as_bytes()).to_hex().to_string();let policy=&report.project_policy_sources;
    let manifest=json!({"summary":{
        "query":report.query,"inferred_kind":report.inferred_kind,
        "exemplars_returned":report.exemplars.len(),"common_shapes_returned":report.common_shapes.len(),
        "coverage":report.coverage,
        "scope":"The selected capped report, not exhaustive repository patterns or proof that the examples implement the requested behavior.",
        "project_policy_sources":{"status":policy.status,"coverage":policy.coverage,"source_revision":policy.source_revision,
            "sources_applicable":policy.sources_applicable,"original_sources_shown":policy.sources_shown,"sources_shown":0,
            "omissions":policy.omissions,"reference_coverage":policy.reference_coverage,"references_omitted":policy.references_omitted,
            "notes":policy.notes,"instruction":policy.instruction,"content_deferred":true}
        },"response_delivery":{
        "status":"DEFERRED_PATTERN_REPORT","complete":false,"response_id":id,"original_bytes":body.len(),
        "original_format":if req.output_json {"json"} else {"markdown"},"response_budget_bytes":limit,
        "estimate":"ceil(UTF-8 complete reply-text bytes / 4); not provider tokens or transport envelope bytes",
        "instruction":"Exemplars, shapes, snippets, source identities and complete authored policy are deferred, not absent. Recover the original report before imitating an example or choosing edit scope. Concatenate content by byte offset and verify response_id as BLAKE3. Recovery never repeats retrieval or rechecks current source/policy freshness.",
        "retention":"At most 24 hours or 32 newer pattern-report snapshots for this project; unavailable snapshots require a fresh query.",
        "next_call":snapshots::pattern_report_continuation(&req.project_id,&id,0,limit)
    }});
    let encoded=serde_json::to_string(&manifest).map_err(|error| McpError::internal_error(error.to_string(),None))?;
    let output=if req.output_json {encoded} else {format!("# Implementation patterns: deferred evidence\n\nRecover the original report before acting on its details.\n\n```json\n{encoded}\n```\n")};
    if output.len()>limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for pattern coverage, policy and recovery metadata; at least {} estimated tokens ({} UTF-8 bytes) required. Increase max_tokens or narrow the query. No evidence or qualifications were silently removed.",output.len().div_ceil(4),output.len()),None));
    }
    snapshots::save_pattern_report(data,&req.project_id,&body)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::planning_tools::{PatternCoverage,PatternExemplar,ExemplarShape};
    use serde_json::Value;

    fn report(root: &Path) -> PatternJson {
        PatternJson {
            query:"SaveOrder helper".into(),inferred_kind:"class".into(),common_shapes:vec![],
            exemplars:vec![PatternExemplar {path:"Site/Order.vb".into(),rank:1,kind_match:true,hits:1,score:4.0,structural:0,shape:ExemplarShape::default(),coupled:vec![],snippet:"Å quoted \"evidence\"\n".repeat(4000),line:1}],
            coverage:PatternCoverage {lexical_hits:200,lexical_cap:200,lexical_status:"truncated".into(),lexical_files:20,kind_filter:"class".into(),kind_filter_applied:true,kind_matched_files:12,candidates_considered:12,candidates_cap:15,exemplar_cap:1,handlers_cap:12,controls_cap:30,data_cap:12,chain_depth_cap:3,failures:vec!["One unavailable declaration".into()]},
            project_policy_sources:crate::services::project_policy::resolve(root,&["Site/Order.vb".into()],&[]),
        }
    }

    #[test]
    fn pattern_delivery_retains_coverage_and_exact_original_json_markdown_after_policy_edits() {
        let owned=tempfile::tempdir().unwrap();let guide="Preserve the manual approval exception.\n";
        std::fs::write(owned.path().join("AGENTS.md"),guide).unwrap();let report=report(owned.path());
        for output_json in [false,true] {
            let req:FindImplementationPatternRequest=serde_json::from_value(json!({"project_id":"project","pattern_query":"SaveOrder helper","max_tokens":2048,"output_json":output_json})).unwrap();
            let original=if output_json {serde_json::to_string_pretty(&report).unwrap()} else {format!("# Pattern report\n{}\n{}",report.exemplars[0].snippet,report.project_policy_sources.render_markdown())};
            let raw=finish(owned.path(),&req,original.clone(),&report,true).unwrap();assert!(raw.len()<=8192);
            let encoded=if output_json {raw.as_str()} else {raw.split("```json\n").nth(1).unwrap().split("\n```").next().unwrap()};
            let manifest:Value=serde_json::from_str(encoded).unwrap();
            assert_eq!(manifest["summary"]["coverage"],serde_json::to_value(&report.coverage).unwrap());
            assert_eq!(manifest["summary"]["exemplars_returned"],1);
            assert_eq!(manifest["summary"]["project_policy_sources"]["content_deferred"],true);
            std::fs::write(owned.path().join("AGENTS.md"),"Changed current guidance\n").unwrap();
            let mut next=manifest["response_delivery"]["next_call"].clone();let mut recovered=String::new();
            loop {
                assert_eq!(next["tool"],"find_implementation_pattern");assert_eq!(next["arguments"]["max_tokens"],2048);
                let req:FindImplementationPatternRequest=serde_json::from_value(next["arguments"].clone()).unwrap();assert!(req.pattern_query.is_empty());
                let raw=recover(owned.path(),&req).unwrap().unwrap();assert!(raw.len()<=8192);
                let page:Value=serde_json::from_str(&raw).unwrap();assert_eq!(page["start_byte"],recovered.len());
                recovered.push_str(page["content"].as_str().unwrap());if page["complete"]==true {break;}next=page["next_call"].clone();
            }
            assert_eq!(recovered,original);
            assert_eq!(blake3::hash(recovered.as_bytes()).to_hex().as_str(),manifest["response_delivery"]["response_id"].as_str().unwrap());
            assert!(recovered.contains("manual approval exception"));
            assert!(snapshots::recover_concept_footprint(owned.path(),"project",manifest["response_delivery"]["response_id"].as_str().unwrap(),0,8192).is_err());
            let mut large=req.clone();large.max_tokens=Some(1_000_000);
            assert_eq!(finish(owned.path(),&large,original.clone(),&report,true).unwrap(),original);
            large.max_tokens=None;assert!(finish(owned.path(),&large,original,&report,true).unwrap().len()<=40*1024);
            assert_eq!(finish(owned.path(),&req,"small original report".into(),&report,true).unwrap(),"small original report");
        }
    }

    #[test]
    fn pattern_internal_evidence_bypasses_public_delivery_and_invalid_controls_write_no_snapshot() {
        let owned=tempfile::tempdir().unwrap();let report=report(owned.path());
        let mut req:FindImplementationPatternRequest=serde_json::from_value(json!({"project_id":"project","pattern_query":"SaveOrder","max_tokens":0})).unwrap();
        assert!(recover(owned.path(),&req).is_err());
        let original="Full composed-planner evidence\n".repeat(3000);
        assert_eq!(finish(owned.path(),&req,original.clone(),&report,false).unwrap(),original);
        req.max_tokens=Some(usize::MAX);assert!(recover(owned.path(),&req).is_err());
        req.max_tokens=Some(2048);req.response_offset=1;assert!(recover(owned.path(),&req).is_err());
        req.response_offset=0;req.max_tokens=Some(1);assert!(finish(owned.path(),&req,original,&report,true).is_err());
        assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(),0);
    }
}
