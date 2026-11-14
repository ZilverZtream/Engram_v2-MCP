//! Public footprint delivery. Internal planning keeps its original FootprintEvidence.
use super::change_set_delivery as snapshots;
use crate::{models::GetConceptFootprintRequest, services::project_policy::PolicyContext};
use rmcp::ErrorData as McpError;
use serde_json::json;
use std::path::Path;

fn budget(tokens: Option<usize>) -> Result<usize,McpError> {
    Ok(crate::handlers::search_tools::response_budget::token_bytes(tokens)?.unwrap_or(40*1024))
}

pub(super) fn recover(data: &Path,req: &GetConceptFootprintRequest) -> Result<Option<String>,McpError> {
    let limit=budget(req.max_tokens)?;
    if let Some(id)=req.response_id.as_deref() {
        return snapshots::recover_concept_footprint(data,&req.project_id,id,req.response_offset,limit).map(Some);
    }
    if req.response_offset!=0 {
        return Err(McpError::invalid_params("response_offset requires response_id from concept-footprint recovery",None));
    }
    Ok(None)
}

pub(super) fn finish(data: &Path,req: &GetConceptFootprintRequest,text: String,paths: Vec<String>,complete: bool,policy: PolicyContext) -> Result<String,McpError> {
    let limit=budget(req.max_tokens)?;
    let body=if req.output_json {
        serde_json::to_string_pretty(&json!({"concept":req.concept,"footprint_markdown":text,
            "source_paths":paths,"source_paths_complete":complete,"project_policy_sources":policy}))
            .map_err(|error| McpError::internal_error(error.to_string(),None))?
    } else {
        format!("{text}{}",policy.render_markdown())
    };
    if body.len()<=limit {return Ok(body);}
    let id=blake3::hash(body.as_bytes()).to_hex().to_string();
    let manifest=json!({"summary":{
        "concept":req.concept,"source_paths_returned":paths.len(),"source_paths_complete":complete,
        "scope":"Counts describe the original capped footprint, not exhaustive repository coverage. Recover its graph, lexical, literal and group-cap qualifications before acting.",
        "project_policy_sources":{"status":policy.status,"coverage":policy.coverage,"source_revision":policy.source_revision,
            "sources_applicable":policy.sources_applicable,"original_sources_shown":policy.sources_shown,"sources_shown":0,
            "omissions":policy.omissions,"reference_coverage":policy.reference_coverage,"references_omitted":policy.references_omitted,
            "notes":policy.notes,"instruction":policy.instruction,"content_deferred":true}
        },"response_delivery":{
        "status":"DEFERRED_CONCEPT_FOOTPRINT","complete":false,"response_id":id,"original_bytes":body.len(),
        "original_format":if req.output_json {"json"} else {"markdown"},"response_budget_bytes":limit,
        "estimate":"ceil(UTF-8 complete reply-text bytes / 4); not provider tokens or transport envelope bytes",
        "instruction":"The original footprint, source identities and complete authored policy are deferred, not absent. Recover the original before choosing edit scope or acting on its details. Concatenate content by byte offset and verify response_id as BLAKE3. Recovery never repeats graph/source retrieval or rechecks current policy/source freshness.",
        "retention":"At most 24 hours or 32 newer concept-footprint snapshots for this project; unavailable snapshots require a fresh query.",
        "next_call":snapshots::concept_footprint_continuation(&req.project_id,&id,0,limit)
    }});
    let encoded=serde_json::to_string(&manifest).map_err(|error| McpError::internal_error(error.to_string(),None))?;
    let output=if req.output_json {encoded} else {format!("# Concept footprint: deferred evidence\n\nRecover the original footprint before acting on its details.\n\n```json\n{encoded}\n```\n")};
    if output.len()>limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for footprint coverage, policy and recovery metadata; at least {} estimated tokens ({} UTF-8 bytes) required. Increase max_tokens or narrow the concept. No evidence or qualifications were silently removed.",output.len().div_ceil(4),output.len()),None));
    }
    snapshots::save_concept_footprint(data,&req.project_id,&body)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn footprint_delivery_recovers_original_capped_evidence_paths_and_policy_in_both_formats() {
        let owned=tempfile::tempdir().unwrap();let guide="Preserve the manual approval exception.\n";
        std::fs::write(owned.path().join("AGENTS.md"),guide).unwrap();
        let paths=vec!["Site/Orders.vb".to_owned(),"db/Orders.sql".to_owned()];
        let policy=crate::services::project_policy::resolve(owned.path(),&paths,&[]);
        let text="# Orders footprint\n- source node: Å quoted \"evidence\"\n... and 6 more\ncoverage: incomplete; runtime binding unverified\n".repeat(700);
        for output_json in [false,true] {
            let req:GetConceptFootprintRequest=serde_json::from_value(json!({"project_id":"project","concept":"Orders","max_tokens":2048,"output_json":output_json})).unwrap();
            let expected=if output_json {serde_json::to_string_pretty(&json!({"concept":"Orders","footprint_markdown":text,"source_paths":paths,"source_paths_complete":false,"project_policy_sources":policy})).unwrap()} else {format!("{text}{}",policy.render_markdown())};
            let initial=finish(owned.path(),&req,text.clone(),paths.clone(),false,policy.clone()).unwrap();assert!(initial.len()<=8192);
            let encoded=if output_json {initial.as_str()} else {initial.split("```json\n").nth(1).unwrap().split("\n```").next().unwrap()};
            let manifest:Value=serde_json::from_str(encoded).unwrap();
            assert_eq!(manifest["summary"]["source_paths_returned"],2);
            assert_eq!(manifest["summary"]["source_paths_complete"],false);
            assert_eq!(manifest["summary"]["project_policy_sources"]["content_deferred"],true);
            std::fs::write(owned.path().join("AGENTS.md"),"Changed current policy\n").unwrap();
            let mut next=manifest["response_delivery"]["next_call"].clone();let mut original=String::new();
            loop {
                assert_eq!(next["tool"],"get_concept_footprint");assert_eq!(next["arguments"]["max_tokens"],2048);
                let recovery:GetConceptFootprintRequest=serde_json::from_value(next["arguments"].clone()).unwrap();assert!(recovery.concept.is_empty());
                let raw=recover(owned.path(),&recovery).unwrap().unwrap();assert!(raw.len()<=8192);
                let page:Value=serde_json::from_str(&raw).unwrap();assert_eq!(page["start_byte"],original.len());
                original.push_str(page["content"].as_str().unwrap());
                if page["complete"]==true {break;}next=page["next_call"].clone();
            }
            assert_eq!(original,expected);
            assert_eq!(blake3::hash(original.as_bytes()).to_hex().as_str(),manifest["response_delivery"]["response_id"].as_str().unwrap());
            assert!(original.contains("manual approval exception") && original.contains("... and 6 more"));
            assert!(snapshots::recover_ask_report(owned.path(),"project",manifest["response_delivery"]["response_id"].as_str().unwrap(),0,8192).is_err());
            let mut large=req.clone();large.max_tokens=Some(1_000_000);
            assert_eq!(finish(owned.path(),&large,text.clone(),paths.clone(),false,policy.clone()).unwrap(),expected);
            large.max_tokens=None;assert!(finish(owned.path(),&large,text.clone(),paths.clone(),false,policy.clone()).unwrap().len()<=40*1024);
            if !output_json {
                let small=finish(owned.path(),&req,"small original footprint\n".into(),paths.clone(),true,policy.clone()).unwrap();
                assert!(small.starts_with("small original footprint\n") && small.contains(guide));
            }
        }
    }

    #[test]
    fn invalid_footprint_controls_and_minimum_metadata_refusal_write_no_snapshot() {
        let owned=tempfile::tempdir().unwrap();let policy=crate::services::project_policy::resolve(owned.path(),&[],&[]);
        let mut req:GetConceptFootprintRequest=serde_json::from_value(json!({"project_id":"project","concept":"Orders","max_tokens":0})).unwrap();
        assert!(recover(owned.path(),&req).is_err());
        req.max_tokens=Some(usize::MAX);assert!(recover(owned.path(),&req).is_err());
        req.max_tokens=Some(2048);req.response_offset=1;assert!(recover(owned.path(),&req).is_err());
        req.response_offset=0;req.max_tokens=Some(1);
        assert!(finish(owned.path(),&req,"large footprint".repeat(100),vec![],false,policy).is_err());
        assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(),0);
    }
}
