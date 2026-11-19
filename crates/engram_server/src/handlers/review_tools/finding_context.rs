//! Connect selected findings to already-retained prior evidence. No retrieval,
//! gate mutation, accepted-policy inference or semantic issue matching happens here.
use crate::handlers::review_decisions::{self, ReviewDecision};
use serde_json::{Value, json};

struct Candidate {
    index: usize,
    file: Option<String>,
    events: Vec<(usize, ReviewDecision)>,
}

fn normalized(path: &str) -> String {
    let path = path.replace('\\', "/").trim_start_matches('/').to_owned();
    if cfg!(windows) {
        path.to_ascii_lowercase()
    } else {
        path
    }
}

/// Pointers resolve inside this complete report, including immutable recovery.
/// Match each event independently: a same-file summary or same review ID must
/// not turn a differently scoped event into an exception for this finding.
pub(super) fn annotate(report: &mut Value) {
    let candidates = report["prior_review_context"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, item)| Candidate {
            index,
            file: item["referenced_file"].as_str().map(normalized),
            events: item["provider_neutral_events"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .filter_map(|(index, event)| {
                    serde_json::from_value::<ReviewDecision>(event["evidence"].clone())
                        .ok()
                        .map(|event| (index, event))
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let mut linked_findings = 0usize;
    let mut links = 0usize;
    if let Some(findings) = report["findings"].as_array_mut() {
        for finding in findings {
            let Some(path) = finding["file_path"].as_str() else {
                continue;
            };
            let finding_id = finding["finding_id"].as_str();
            let selected = vec![path.to_owned()];
            let path = normalized(path);
            let mut annotations = Vec::new();
            for candidate in &candidates {
                let summary_file_match = candidate.file.as_deref() == Some(path.as_str());
                let event_matches = candidate.events.iter().filter_map(|(index, event)| {
                    if !review_decisions::relevant_to_paths(event, &selected) { return None; }
                    let pointer = format!("/prior_review_context/items/{}/provider_neutral_events/{index}", candidate.index);
                    Some(json!({"event_index":index,"event_pointer":pointer,
                        "same_finding_id":finding_id == Some(event.finding_id.as_str()),
                        "relevance_basis":if event.scope.is_some(){"declared_exact_path"}else{"legacy_complete_path_mention"},
                        "scope_assessment_pointer":format!("{pointer}/declared_scope_assessment")}))
                }).collect::<Vec<_>>();
                if summary_file_match || !event_matches.is_empty() {
                    annotations.push(json!({"item_index":candidate.index,
                        "context_pointer":format!("/prior_review_context/items/{}",candidate.index),
                        "same_file_summary":summary_file_match,"event_matches":event_matches}));
                }
            }
            if !annotations.is_empty() {
                linked_findings += 1;
                links += annotations.len();
                finding["prior_review_annotations"] = json!(annotations);
            }
        }
    }
    report["prior_review_annotation_contract"] = json!({
        "status":"path_evidence_links_only","linked_finding_count":linked_findings,"item_link_count":links,
        "automatic_finding_suppression":false,"semantic_applicability":"not_established",
        "context_status":report["prior_review_context"]["status"],
        "automatic_finding_suppression":false,"semantic_issue_matching_performed":false,
        "interpretation":"Annotations point to retained evidence in this complete report. Exact file relevance does not establish the same issue, accepted authority, semantic applicability, a current fix or permission to suppress a finding. A same-file indexed summary does not extend the scope of its separately matched events. Read complete event conditions, source/revision assessments and provenance before deciding whether prior reasoning applies. Missing links mean no admitted path match within this bounded context, not absence of relevant decisions. Finding identities, severities, gate outcomes and verdict remain unchanged."});
}

pub(super) fn markdown(report: &Value) -> String {
    if report["prior_review_annotation_contract"]["linked_finding_count"] == 0 {
        return String::new();
    }
    let findings = report["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|finding| finding.get("prior_review_annotations").is_some())
        .map(|finding| {
            json!({"finding_id":finding["finding_id"],"file_path":finding["file_path"],
            "prior_review_annotations":finding["prior_review_annotations"]})
        })
        .collect::<Vec<_>>();
    format!(
        "\n## Finding links to prior review evidence\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(
            &json!({"contract":report["prior_review_annotation_contract"],
            "findings":findings})
        )
        .expect("JSON values are serializable")
    )
}
