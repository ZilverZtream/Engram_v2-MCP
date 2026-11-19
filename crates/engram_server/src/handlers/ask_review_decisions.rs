//! Explicit prior-ruling questions compose the existing advisory decision reader.
use crate::{
    handlers::review_decisions::{DecisionView, GetReviewDecisionsRequest},
    services::ask_engine::{
        evidence::{Authority, EvidenceItem, EvidenceKind},
        resolver::scope_full_prefix,
        status::{ProviderReport, ProviderStatus},
    },
    tools::Engram,
};
use serde_json::Value;
use std::{collections::BTreeSet, sync::OnceLock, time::Duration};

pub(super) struct ReviewEvidence {
    pub item: Option<EvidenceItem>,
    pub provider: ProviderReport,
}

fn paths(question: &str, scopes: &[String]) -> (Vec<String>, bool, bool) {
    let lower = question.to_ascii_lowercase();
    if !["review decision", "review ruling", "prior review", "accepted exception"].iter().any(|term| lower.contains(term)) {
        return (Vec::new(), false, false);
    }
    let mut selected = BTreeSet::new();
    let mut scope_excluded = false;
    for token in question.split_whitespace() {
        let token = token.trim_matches(['`', '"', '\'', '(', ')', '[', ']', ',', ';', '?', '!']);
        if token.len() > 4096 || token.contains(['*', '?', '{', '}']) { continue; }
        let Some(path) = crate::services::project_policy::normalized_relative(token) else { continue; };
        let lower = path.to_ascii_lowercase();
        if ![".vb", ".aspx", ".ascx", ".master", ".ashx", ".asmx", ".js", ".ts", ".sql"].iter().any(|extension| lower.ends_with(extension)) { continue; }
        if !scopes.is_empty() && !scopes.iter().any(|scope| scope_full_prefix(&path, scope).is_some()) {
            scope_excluded = true;
            continue;
        }
        if selected.len() == 16 && !selected.contains(&path) {
            return (selected.into_iter().collect(), true, scope_excluded);
        }
        selected.insert(path);
    }
    (selected.into_iter().collect(), false, scope_excluded)
}

fn recorded_before(question: &str) -> Result<Option<String>, &'static str> {
    static BEFORE: OnceLock<regex::Regex> = OnceLock::new();
    let expression = BEFORE.get_or_init(|| regex::Regex::new(r"(?i)\bbefore\s+`?([0-9]{4}-[0-9]{2}-[0-9]{2})\b").expect("literal review-date expression"));
    let mut dates = expression.captures_iter(question);
    let Some(first) = dates.next() else { return Ok(None); };
    let date = first[1].to_owned();
    if !crate::handlers::review_decisions::valid_iso_date(&date)
        || dates.any(|capture| capture[1] != date)
    {
        return Err("Prior-review questions require one valid exclusive 'before YYYY-MM-DD' source-date cutoff; use get_review_decisions with recorded_before for an explicit dated read. No current ruling is substituted.");
    }
    Ok(Some(date))
}

pub(super) async fn gather(
    engram: &Engram,
    project: &str,
    question: &str,
    scopes: &[String],
    deadline: Duration,
) -> Option<ReviewEvidence> {
    let (selected, paths_truncated, scope_excluded) = paths(question, scopes);
    if selected.is_empty() && !scope_excluded { return None; }
    let mut out = ReviewEvidence { item: None, provider: ProviderReport {
        provider: "review_decisions".into(), status: ProviderStatus::Empty, count: 0,
        note: Some("Recorded review evidence is advisory; no current approval, semantic applicability or fix verification is established.".into()),
        examined: 0, available: None, truncated: paths_truncated, proof: None,
    }};
    if selected.is_empty() {
        out.provider.note = Some("None of the named review files match the requested path scope. No review discovery was run.".into());
        return Some(out);
    }
    let recorded_before = match recorded_before(question) {
        Ok(value) => value,
        Err(note) => {
            out.provider.status = ProviderStatus::Failed;
            out.provider.note = Some(note.into());
            return Some(out);
        }
    };
    let request = GetReviewDecisionsRequest {
        project_id: project.into(), review_id: String::new(), discover_for_paths: selected,
        view: DecisionView::History, recorded_before, discussion: None,
    };
    let result = tokio::time::timeout(deadline, engram.handle_get_review_decisions(request)).await;
    let parsed = match result {
        Ok(Ok(result)) => result.content.first().and_then(|content| content.as_text())
            .ok_or_else(|| "Review reader returned no text".to_string())
            .and_then(|text| serde_json::from_str::<Value>(&text.text).map_err(|error| error.to_string())),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => {
            out.provider.status = ProviderStatus::TimedOut;
            out.provider.note = Some("Review metadata deadline reached. An owned bounded metadata reader may still finish; no unfinished result or absence of decisions is claimed.".into());
            return Some(out);
        }
    };
    match parsed {
        Ok(value) => {
            let count = value["candidates"].as_array().map_or(0, Vec::len);
            out.provider.count = count;
            out.provider.examined = value["coverage"]["identity_records_inspected"].as_u64().unwrap_or(0) as usize;
            out.provider.truncated |= value["coverage"]["identity_scan_has_more"].as_bool().unwrap_or(false)
                || ["identity_values_omitted", "candidate_or_event_byte_omissions"].iter().any(|key| value["coverage"][*key].as_u64().unwrap_or(0) > 0);
            out.provider.note = Some(format!("{} Discovery coverage: {}{}", value["interpretation"].as_str().unwrap_or("Advisory recorded review evidence"), value["coverage"], if paths_truncated { " Explicit path selection stopped at16; further files require another question." } else { "" }));
            if scope_excluded && let Some(note) = &mut out.provider.note {
                note.push_str(" Named paths outside the requested scope were omitted from review discovery.");
            }
            if count > 0 {
                out.provider.status = ProviderStatus::Hit;
                let content = value.to_string();
                out.item = Some(EvidenceItem {
                    evidence_id: "recorded_review_decisions".into(), memory_record: None,
                    document_id: None, document_namespace: None, source_verification: Some("Recorded current metadata observations; not authenticated policy or current-HEAD fix verification".into()),
                    kind: EvidenceKind::MemoryNote, authority: Authority::AgentMemory,
                    path: None, lines: None, symbol_id: Some(format!("review_decisions:{}", blake3::hash(content.as_bytes()).to_hex())),
                    title: Some("Recorded scoped review decisions and complete conditions".into()), content,
                    generation: None, commit: None, timestamp: None,
                    confidence: 0.45, relevance: 1.0, extraction_method: "recorded_review_path_discovery".into(),
                    warnings: vec!["Latest admitted events are unverified recorded context. Missing or omitted records prove no absence; scope/conditions require reassessment before a current edit. History actions are fresh reads, not immutable recovery.".into()],
                    provider: "review_decisions".into(), score: None, directness: Some(0.5),
                });
            }
        }
        Err(detail) => {
            out.provider.status = ProviderStatus::Failed;
            out.provider.note = Some(format!("Review metadata unavailable: {detail}; no absence of decisions is established."));
        }
    }
    Some(out)
}
