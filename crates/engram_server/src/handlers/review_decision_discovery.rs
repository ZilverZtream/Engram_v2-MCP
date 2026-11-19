//! Public navigation to existing bounded, advisory review evidence by file path.
use super::{
    DecisionView, GetReviewDecisionsRequest, discover_at, effective_status, relevant_to_paths,
    valid_iso_date,
};
use serde_json::json;

pub(super) fn validate(req: &GetReviewDecisionsRequest) -> Result<(), String> {
    if !req.review_id.is_empty()
        || req.discussion.is_some()
        || req.view != DecisionView::History
    {
        return Err("discover_for_paths selects bounded decision candidates; omit review_id, discussion and view=current. recorded_before may select earlier dated candidates; follow returned history actions for their eligible events.".into());
    }
    if req.recorded_before.as_deref().is_some_and(|date| !valid_iso_date(date)) {
        return Err("recorded_before must be an exclusive YYYY-MM-DD source-date cutoff".into());
    }
    if req.discover_for_paths.len() > 16
        || req.discover_for_paths.iter().any(|path| {
            path.len() > 512
                || path
                    .chars()
                    .any(|c| c.is_control() || "\\:*?[]".contains(c))
                || path
                    .split('/')
                    .any(|part| part.is_empty() || matches!(part, "." | ".."))
        })
    {
        return Err("discover_for_paths requires 1..16 exact repository-relative paths with forward slashes, each at most 512 bytes; no globs or traversal".into());
    }
    Ok(())
}

pub(super) fn read(
    state: &crate::state::AppState,
    project: &str,
    paths: &[String],
    recorded_before: Option<&str>,
) -> Result<String, String> {
    let found = discover_at(state, project, 24, recorded_before,
        |event| relevant_to_paths(event, paths))?;
    let candidates = found.candidates.into_iter().map(|row| {
        let status = effective_status(&row.event, "externally_attested_fix_not_checked");
        let mut candidate = json!({"review_id":row.review_id,"effective_status":status,
            "relevance_basis":if row.event.scope.is_some() {"declared_path_intersection"} else {"legacy_complete_path_mention"},
            "event":row.event,"record_identity":{"blake3":row.record_blake3,"events":row.record_events,
                "hash_basis":"Entire raw persisted review event array at this independent record read; not a hash of this projection or authentication of its contents."},
            "event_history":{"tool":"get_review_decisions","arguments":{
                "project_id":project,"review_id":row.review_id,"view":"history"}},
            "history_freshness":"The history action is a new read. It may include later appended events; this discovery is not an immutable corpus snapshot."});
        if let Some(date) = recorded_before {
            candidate["event_history"]["arguments"]["recorded_before"] = json!(date);
            candidate["history_freshness"] = json!("The history action retains the source-date cutoff. Later appended events with eligible imported dates may still appear; dates/authority are unverified and discovery is not an immutable historical corpus snapshot. Compare the selected event and its declared scope/conditions.");
        }
        candidate
    }).collect::<Vec<_>>();
    let mut output = json!({"status":"advisory_review_candidates",
        "project_id":project,"selected_paths":paths,"candidates":candidates,"coverage":found.coverage,
        "automatic_finding_suppression":false,"current_head_fix_verification_performed":false,
        "interpretation":"Latest relevant recorded events, with complete admitted rationale, declared scope, conditions and provenance. Relevance is a retrieval lead, not authenticated authority, accepted policy, semantic applicability or approval for changed code. Imported verified-fix attestations are not checked against current HEAD. Superseded relevant events are not promoted over an unrelated latest ruling; follow event_history for complete available history. Missing or omitted records establish no absence of prior decisions."});
    if let Some(date) = recorded_before {
        output["recorded_before"] = json!(date);
        output["interpretation"] = json!("Latest eligible recorded events strictly before the imported source-date cutoff, selected before path relevance. Complete admitted rationale, scope and conditions remain unverified evidence, not authenticated authority, accepted policy or approval. Historical source dates and provenance are not authenticated; current-HEAD fixes are not verified. Record identity hashes describe whole current arrays, not this historical projection. Follow cutoff-preserving event_history actions; missing/omitted records prove no absence of earlier decisions.");
    }
    serde_json::to_string_pretty(&output)
        .map_err(|error| error.to_string())
}
