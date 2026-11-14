//! Bound the complete natural-language report; recovery never reruns its providers.
use crate::handlers::planning_tools::change_set_delivery as snapshots;
use crate::models::AskCodebaseRequest;
use crate::services::ask_engine::report::AskReport;
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};
use std::path::Path;

fn budget(tokens: Option<usize>) -> Result<usize, McpError> {
    snapshots::budget(tokens)
}

pub(super) fn recover(data: &Path, req: &AskCodebaseRequest) -> Result<Option<String>, McpError> {
    let limit = budget(req.max_tokens)?;
    if let Some(id) = req.response_id.as_deref() {
        return snapshots::recover_ask_report(data, &req.project_id, id, req.response_offset, limit).map(Some);
    }
    if req.response_offset != 0 {
        return Err(McpError::invalid_params("response_offset requires response_id from ask_codebase recovery", None));
    }
    Ok(None)
}

pub(super) fn finish(data: &Path, req: &AskCodebaseRequest, body: String, report: &AskReport) -> Result<String, McpError> {
    let limit = budget(req.max_tokens)?;
    if body.len() <= limit { return Ok(body); }
    let policy = report.project_policy_sources.as_ref().map(|policy| json!({
        "status":policy.status, "coverage":policy.coverage,
        "source_revision":policy.source_revision, "sources_applicable":policy.sources_applicable,
        "original_sources_shown":policy.sources_shown, "sources_shown":0,
        "omissions":policy.omissions, "reference_coverage":policy.reference_coverage,
        "references_omitted":policy.references_omitted, "content_deferred":true,
        "notes":policy.notes, "instruction":policy.instruction
    }));
    let summary = json!({
        "status":report.status, "mode":report.mode, "snapshot":report.snapshot,
        "evidence_count":report.evidence.len(), "answer_member_count":report.answer_members.len(),
        "providers":report.providers, "conflicts":report.conflicts,
        "unknowns":report.unknowns, "next_best":report.next_best,
        "project_policy_sources":policy,
        "synthesis_status":report.synthesis.as_ref().map(|synthesis| synthesis.status),
        "synthesis_limitation":report.synthesis.as_ref().map(|synthesis| synthesis.limitation)
    });
    defer(data, req, body, summary, limit)
}

fn defer(data: &Path, req: &AskCodebaseRequest, body: String, summary: Value, limit: usize) -> Result<String, McpError> {
    let id = blake3::hash(body.as_bytes()).to_hex().to_string();
    let format = match req.output_format.to_ascii_lowercase().as_str() {
        "json" => "json", "both" => "both", _ => "markdown"
    };
    let manifest = json!({"summary":summary, "response_delivery":{
        "status":"DEFERRED_ASK_REPORT", "complete":false, "response_id":id,
        "original_bytes":body.len(), "original_format":format, "response_budget_bytes":limit,
        "estimate":"ceil(UTF-8 complete reply-text bytes / 4); not provider tokens or transport envelope bytes",
        "instruction":"Evidence, answer members, synthesized text and complete authored policy are deferred, not absent. Recover the original report before answering from its details. Summary status describes retrieval, not completeness of this reply. Concatenate content by byte offset and verify response_id as BLAKE3. Recovery never reruns retrieval, routing or synthesis and does not recheck current source or policy freshness.",
        "retention":"At most 24 hours or 32 newer ask-report snapshots for this project; unavailable snapshots require a fresh query.",
        "next_call":snapshots::ask_report_continuation(&req.project_id, &id, 0, limit)
    }});
    let encoded = serde_json::to_string(&manifest).map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let output = if format == "json" { encoded } else {
        format!("# ask_codebase: deferred evidence\n\nRecover the original report before answering from its details.\n\n```json\n{encoded}\n```\n")
    };
    if output.len() > limit {
        return Err(McpError::invalid_params(format!("max_tokens is too small for ask_codebase status, coverage and recovery metadata; at least {} estimated tokens ({} UTF-8 bytes) required. Increase max_tokens or narrow the question. No evidence or qualifications were silently removed.", output.len().div_ceil(4), output.len()), None));
    }
    snapshots::save_ask_report(data, &req.project_id, &body)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::ask_engine::{evidence::*, planner, report, status::*, synthesis};

    fn original_report(root: &Path) -> AskReport {
        AskReport {
            question:"How does SaveOrder work?".into(), plan:planner::plan_query("SaveOrder"),
            status:AnswerStatus::Partial, mode:"synthesized".into(),
            synthesis:Some(synthesis::Synthesis {
                status:"generated", reason:None, provider:Some("fixture".into()), requested_model:None,
                provider_requests:1, provider_requests_semantics:"One owned test answer; no model invoked",
                answer:Some(synthesis::Answer {statements:vec![synthesis::Statement {
                    text:"The shown method preserves manual approval.".into(),
                    citations:vec![synthesis::Citation {evidence_id:"save_order".into(),quote:"manual approval".into()}],
                }],unknowns:vec!["Runtime binding is unverified.".into()]}), limitation:synthesis::LIMITATION,
            }),
            project_policy_sources:Some(crate::services::project_policy::resolve(root,&["Site/Order.vb".into()],&[])),
            evidence:vec![EvidenceItem {
                evidence_id:"save_order".into(), document_id:None, document_namespace:None, source_verification:None,
                kind:EvidenceKind::SourceCode, authority:Authority::CurrentCode,
                path:Some("Site/Order.vb".into()), lines:Some((1,2001)), symbol_id:None,title:None,
                content:"' Preserve manual approval: Å quoted \"evidence\".\n".repeat(2000),
                generation:Some(7),commit:None,timestamp:None,confidence:0.9,relevance:1.0,
                extraction_method:"ast".into(),warnings:vec!["Callers not retrieved".into()],provider:"definition".into(),score:None,directness:None,
            }],
            conflicts:vec![Conflict {summary:"Conflicting historical guidance".into(),left:"save_order".into(),right:"old_rule".into(),kind:"authority_disagreement".into()}],
            unknowns:vec!["Runtime binding is unverified.".into()],next_best:vec!["Inspect actual callers before editing".into()],
            snapshot:FreshnessSnapshot {project_generation:Some(7),semantic_tier:"off".into(),..Default::default()},
            providers:vec![ProviderReport {provider:"history".into(),status:ProviderStatus::TimedOut,count:0,note:Some("Budget expired".into()),examined:0,available:None,truncated:true,proof:None}],
            answer_members:vec![],
        }
    }

    #[test]
    fn ask_delivery_preserves_qualified_report_and_exact_recovery_in_all_formats_after_policy_changes() {
        let temporary=tempfile::tempdir().unwrap();
        let guide="Preserve the manual approval exception.\n";
        std::fs::write(temporary.path().join("AGENTS.md"),guide).unwrap();
        let report=original_report(temporary.path());
        for format in ["json","markdown","both"] {
            let mut req:AskCodebaseRequest=serde_json::from_value(json!({"project_id":"project","question":"SaveOrder","output_format":format,"max_tokens":2048})).unwrap();
            let json=serde_json::to_string_pretty(&report::to_json(&report)).unwrap();
            let markdown=report::render_markdown(&report);
            let body=match format {"json"=>json,"both"=>format!("{markdown}\n\n```json\n{json}\n```"),_=>markdown};
            assert!(body.len()>40*1024);
            let first=finish(temporary.path(),&req,body.clone(),&report).unwrap();
            assert!(first.len()<=8192);
            let encoded=if format=="json" {first.as_str()} else {first.split("```json\n").nth(1).unwrap().split("\n```").next().unwrap()};
            let manifest:Value=serde_json::from_str(encoded).unwrap();
            assert_eq!(manifest["summary"]["status"],"partial");
            assert_eq!(manifest["summary"]["providers"][0]["status"],"timed_out");
            assert_eq!(manifest["summary"]["unknowns"],json!(["Runtime binding is unverified."]));
            assert_eq!(manifest["summary"]["conflicts"][0]["summary"],"Conflicting historical guidance");
            assert_eq!(manifest["summary"]["project_policy_sources"]["sources_shown"],0);
            assert_eq!(manifest["response_delivery"]["original_format"],format);
            std::fs::write(temporary.path().join("AGENTS.md"),"Changed current guidance\n").unwrap();
            let mut next=manifest["response_delivery"]["next_call"].clone();
            let id=manifest["response_delivery"]["response_id"].as_str().unwrap();
            let mut recovered=String::new();
            loop {
                assert_eq!(next["tool"],"ask_codebase");
                assert_eq!(next["arguments"]["max_tokens"],2048);
                req=serde_json::from_value(next["arguments"].clone()).unwrap();
                assert!(req.question.is_empty());
                let raw=recover(temporary.path(),&req).unwrap().unwrap();
                assert!(raw.len()<=8192);
                let page:Value=serde_json::from_str(&raw).unwrap();
                assert_eq!(page["start_byte"],recovered.len());
                recovered.push_str(page["content"].as_str().unwrap());
                if page["complete"]==true {break;}
                next=page["next_call"].clone();
            }
            assert_eq!(recovered,body);
            assert_eq!(blake3::hash(recovered.as_bytes()).to_hex().as_str(),id);
            assert!(recovered.contains("manual approval exception"));
            assert!(recovered.contains("Runtime binding is unverified."));
            assert!(snapshots::recover_database_report(temporary.path(),"project",id,0,8192).is_err());
            req.response_id=None;req.response_offset=0;req.max_tokens=Some(100_000);
            assert_eq!(finish(temporary.path(),&req,body.clone(),&report).unwrap(),body);
            assert_eq!(finish(temporary.path(),&req,"small unchanged report".into(),&report).unwrap(),"small unchanged report");
            req.max_tokens=None;
            assert!(finish(temporary.path(),&req,body.clone(),&report).unwrap().len()<=40*1024);
        }
    }

    #[test]
    fn ask_delivery_rejects_invalid_controls_and_minimum_metadata_before_creating_snapshots() {
        let temporary=tempfile::tempdir().unwrap();let report=original_report(temporary.path());
        let mut req:AskCodebaseRequest=serde_json::from_value(json!({"project_id":"project","question":"SaveOrder","max_tokens":0})).unwrap();
        assert!(recover(temporary.path(),&req).is_err());
        req.max_tokens=Some(usize::MAX);assert!(recover(temporary.path(),&req).is_err());
        req.max_tokens=Some(2048);req.response_offset=1;assert!(recover(temporary.path(),&req).is_err());
        req.response_offset=0;req.max_tokens=Some(1);
        assert!(finish(temporary.path(),&req,"full report".repeat(100),&report).is_err());
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(),0);
    }
}
