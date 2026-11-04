//! Provider-neutral, append-only review attestations. Imported text is evidence,
//! never an instruction, and never causes findings to be silently suppressed.
use crate::tools::Engram;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

static WRITES: Mutex<()> = Mutex::new(());
const IDENTITY_PREFIX: &str = "review_decision_identity:";
const DISCOVERY_RECORDS: usize = 64;
const DISCOVERY_RECORD_BYTES: usize = 64 * 1024;
const DISCOVERY_TOTAL_BYTES: usize = 256 * 1024;

fn identity_key(review: &str) -> Result<String, String> {
    key(review)?;
    Ok(format!("{IDENTITY_PREFIX}{}", blake3::hash(review.as_bytes()).to_hex()))
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Open,
    AcceptedException,
    ClaimedFix,
    VerifiedFix,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    /// Full commit object ID against which the external check passed.
    pub commit: String,
    /// Reproducible check description; Engram does not execute imported commands.
    pub check: String,
    pub evidence_url: String,
    /// SHA-256 of the externally retained check artifact.
    pub artifact_sha256: String,
    pub verifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecision {
    /// Immutable event ID, unique within this review.
    pub event_id: String,
    /// Stable identity of the finding/thread, shared by its successive events.
    pub finding_id: String,
    /// Previous event for this finding; required for a transition, absent for its first event.
    pub supersedes: Option<String>,
    pub kind: DecisionKind,
    pub source_url: String,
    pub author: String,
    /// Source timestamp, retained verbatim as provenance (not used to order events).
    pub recorded_at: String,
    pub rationale: String,
    pub verification: Option<Verification>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordReviewDecisionsRequest {
    pub project_id: String,
    /// Use PR-123 for merged-work integration, or another stable review identity.
    pub review_id: String,
    pub decisions: Vec<ReviewDecision>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetReviewDecisionsRequest {
    pub project_id: String,
    pub review_id: String,
    /// Omit for the unchanged immutable decision-event history. discussion:{}
    /// lists retained imported discussion captures; a thread/hash selects bounded
    /// hash-bound UTF-8 pages. Captures are evidence, never authenticated policy.
    #[serde(default)]
    pub discussion: Option<DiscussionSelector>,
}
pub use super::review_discussion::DiscussionSelector;

fn key(review: &str) -> Result<String, String> {
    if review.trim().is_empty() || review.len() > 256 || review.trim() != review {
        return Err("review_id must be nonblank, trimmed and at most 256 bytes".into());
    }
    Ok(format!(
        "review_decisions:{}",
        blake3::hash(review.as_bytes()).to_hex()
    ))
}

fn valid_hex(s: &str, lengths: &[usize]) -> bool {
    lengths.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_iso_date(value: &str) -> bool {
    if value.len() != 10
        || value.as_bytes()[4] != b'-'
        || value.as_bytes()[7] != b'-'
        || !value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return false;
    }
    let year = value[..4].parse::<u32>().unwrap_or(0);
    let month = value[5..7].parse::<usize>().unwrap_or(0);
    let day = value[8..10].parse::<u32>().unwrap_or(0);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        0,
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    month > 0 && month < days.len() && day > 0 && day <= days[month]
}

fn validate(event: &ReviewDecision) -> Result<(), String> {
    for value in [
        &event.event_id,
        &event.finding_id,
        &event.source_url,
        &event.author,
        &event.recorded_at,
        &event.rationale,
    ] {
        if value.trim().is_empty() || value.len() > 16000 {
            return Err(
                "decision provenance fields must be nonblank and at most 16000 bytes".into(),
            );
        }
    }
    if !event.source_url.starts_with("https://") {
        return Err("source_url must use https".into());
    }
    if event.kind == DecisionKind::VerifiedFix {
        let v = event
            .verification
            .as_ref()
            .ok_or("verified_fix requires an external verification attestation")?;
        if !valid_hex(&v.commit, &[40, 64])
            || !valid_hex(&v.artifact_sha256, &[64])
            || v.check.trim().is_empty()
            || v.verifier.trim().is_empty()
            || !v.evidence_url.starts_with("https://")
        {
            return Err("verification requires full commit, artifact SHA-256, check, verifier and HTTPS evidence URL".into());
        }
    } else if event.verification.is_some() {
        return Err("verification is only allowed on verified_fix events".into());
    }
    Ok(())
}

fn decode_events(raw: &str) -> Result<Vec<ReviewDecision>, String> {
    let events: Vec<ReviewDecision> = serde_json::from_str(raw)
        .map_err(|e| format!("invalid stored decision evidence: {e}"))?;
    if events.len() > 500 {
        return Err("stored review evidence exceeds 500 events".into());
    }
    for event in &events { validate(event)?; }
    Ok(events)
}

pub(crate) fn read(
    state: &crate::state::AppState, pid: &str, review: &str,
) -> Result<Vec<ReviewDecision>, String> {
    state.registry.get_meta_bounded(pid, &key(review)?, 1024 * 1024)
        .map_err(|e| e.to_string())?
        .map(|raw| decode_events(&raw)).transpose().map(|events| events.unwrap_or_default())
}

pub(crate) struct DiscoveredDecision {
    pub review_id: String,
    pub event: ReviewDecision,
    pub record_blake3: String,
    pub record_events: usize,
}

pub(crate) struct DecisionDiscovery {
    pub candidates: Vec<DiscoveredDecision>,
    pub coverage: serde_json::Value,
}

/// Discover only identities explicitly recorded with their event arrays. Old
/// BLAKE3 keys contain no recoverable review ID: never infer one from a URL or
/// rationale. Those rows remain recoverable by exact ID through the public tool.
pub(crate) fn discover(
    state: &crate::state::AppState, pid: &str, max_candidates: usize,
    mut relevant: impl FnMut(&ReviewDecision) -> bool,
) -> Result<DecisionDiscovery, String> {
    let page = state.registry.list_meta_prefix_bounded(
        pid, IDENTITY_PREFIX, DISCOVERY_RECORDS, 256, 32 * 1024,
    ).map_err(|e| e.to_string())?;
    let mut candidates = Vec::new();
    let mut invalid_identities = 0usize;
    let mut unavailable_records = 0usize;
    let mut unrelated_current_events = 0usize;
    let mut candidate_omissions = 0usize;
    let mut record_bytes_read = 0usize;
    let mut issue_ids = Vec::new();
    for (stored_key, review_id) in page.entries {
        if identity_key(&review_id).as_deref() != Ok(stored_key.as_str()) {
            invalid_identities += 1;
            continue;
        }
        let limit = DISCOVERY_RECORD_BYTES.min(DISCOVERY_TOTAL_BYTES - record_bytes_read);
        let events_key = key(&review_id)?;
        let observation = state.registry.map_meta_bounded(pid, &events_key, limit, |bytes| {
            // Charge every admitted raw body, including one whose decoder fails.
            record_bytes_read += bytes.len();
            let events = decode_events(std::str::from_utf8(bytes)?)
                .map_err(anyhow::Error::msg)?;
            Ok((events, blake3::hash(bytes).to_hex().to_string()))
        });
        let (events, record_hash) = match observation {
            Ok(Some(value)) => value,
            Ok(None) | Err(_) => {
                unavailable_records += 1;
                if issue_ids.len() < 3 { issue_ids.push(review_id); }
                continue;
            }
        };
        let mut latest = std::collections::BTreeMap::new();
        for event in &events { latest.insert(&event.finding_id, event); }
        for event in latest.values() {
            if !relevant(event) {
                unrelated_current_events += 1;
                continue;
            }
            // Preserve complete small evidence; disclose larger evidence rather
            // than clipping rationale or presenting a synthetic shortened event.
            if candidates.len() >= max_candidates.min(24)
                || serde_json::to_vec(event).map_err(|e| e.to_string())?.len() > 8 * 1024 {
                candidate_omissions += 1;
                continue;
            }
            candidates.push(DiscoveredDecision {
                review_id: review_id.clone(), event: (**event).clone(),
                record_blake3: record_hash.clone(), record_events: events.len(),
            });
        }
    }
    Ok(DecisionDiscovery { candidates, coverage: serde_json::json!({
        "status":"partial", "identity_records_inspected":page.inspected,
        "identity_record_scan_cap":DISCOVERY_RECORDS, "identity_scan_has_more":page.has_more,
        "identity_values_omitted":page.omitted_values, "invalid_identity_records":invalid_identities,
        "unavailable_or_over_budget_records":unavailable_records,
        "unavailable_review_id_examples":issue_ids,
        "unrelated_current_events":unrelated_current_events,
        "candidate_or_event_byte_omissions":candidate_omissions,
        "serialized_record_bytes_read":record_bytes_read,
        "per_record_byte_cap":DISCOVERY_RECORD_BYTES,"total_record_byte_cap":DISCOVERY_TOTAL_BYTES,
        "legacy_identity_coverage":"Unknown: older hashed rows without identity companions cannot be discovered. Read by the original review_id, or record the unchanged original events with that exact ID to add a companion. No IDs are inferred and no old arrays are migrated by discovery.",
        "relevance":"A complete changed relative path must be mentioned in the current event's rationale or finding ID. This is text relevance only, not authenticated authority, accepted scope, runtime applicability, or complete semantic discovery.",
        "observation_scope":"Independent current metadata observations, not an atomic corpus snapshot or historical replay. Missing, malformed, oversized and uninspected records establish no absence of decisions."
    }) })
}

/// Older imports used AcceptedException for a workflow/LLM disposition. Keep
/// the immutable original event, but never expose that classification as policy
/// acceptance. Explicit externally supplied decisions retain their own identity.
pub(crate) fn workflow_disposition_only(event: &ReviewDecision) -> bool {
    event.event_id.starts_with("review-history:")
        && [
            "Imported review disposition=wont_fix;",
            "Imported review disposition=llm_classified_dismissed;",
        ]
        .iter()
        .any(|prefix| event.rationale.starts_with(prefix))
        && matches!(
            event.kind,
            DecisionKind::Open | DecisionKind::AcceptedException
        )
}

pub(crate) fn effective_status(
    event: &ReviewDecision,
    verified_fix_status: &'static str,
) -> &'static str {
    if workflow_disposition_only(event) {
        return "unverified_review_disposition";
    }
    match event.kind {
        DecisionKind::Open => "open",
        DecisionKind::AcceptedException => "accepted_exception",
        DecisionKind::ClaimedFix => "claimed_fix",
        DecisionKind::VerifiedFix => verified_fix_status,
    }
}

/// Append provider-imported decision evidence through the same immutable event
/// checks as the public tool. The caller remains responsible for provenance;
/// imported status is evidence and never suppresses a finding automatically.
pub(crate) fn append(
    state: &crate::state::AppState,
    pid: &str,
    review: &str,
    decisions: &[ReviewDecision],
) -> Result<usize, String> {
    let _guard = WRITES
        .lock()
        .map_err(|_| "decision writer lock unavailable")?;
    let events_key = key(review)?;
    let original = state.registry.get_meta_bounded(pid, &events_key, 1024 * 1024)
        .map_err(|e| e.to_string())?;
    let mut events = original.as_deref().map(decode_events).transpose()?.unwrap_or_default();
    let mut changed = false;
    for event in decisions {
        validate(event)?;
        if let Some(existing) = events.iter().find(|old| old.event_id == event.event_id) {
            if existing != event {
                return Err("event IDs are immutable; append a superseding event".into());
            }
            continue;
        }
        let previous = events
            .iter()
            .rev()
            .find(|old| old.finding_id == event.finding_id);
        if previous.map(|e| e.event_id.as_str()) != event.supersedes.as_deref() {
            return Err("supersedes must identify the latest event for this finding".into());
        }
        events.push(event.clone());
        changed = true;
    }
    // Idempotent identity backfill preserves even a legacy array's exact bytes.
    let encoded = if changed { serde_json::to_string(&events).map_err(|e| e.to_string())? }
        else { original.as_deref().unwrap_or("[]").to_owned() };
    if events.len() > 500 || encoded.len() > 1024 * 1024 {
        return Err("review evidence exceeds 500 events or 1 MiB".into());
    }
    let published = state.registry.compare_exchange_meta_with_companion(
        pid, &events_key, original.as_deref(), &encoded, &identity_key(review)?, review,
    ).map_err(|e| e.to_string())?;
    if !published {
        return Err("review evidence changed or its identity companion conflicts; reread the exact review_id and retry without replacing existing events".into());
    }
    Ok(events.len())
}

pub(crate) fn snapshot(
    state: &crate::state::AppState,
    pid: &str,
    review: &str,
    root: &std::path::Path,
) -> Result<serde_json::Value, String> {
    let events = read(state, pid, review)?;
    let repo = git2::Repository::open(root).ok();
    let head = repo
        .as_ref()
        .and_then(|r| r.head().ok())
        .and_then(|h| h.target())
        .map(|id| id.to_string());
    // Ignored build output is not a working-tree change; untracked files are.
    let clean = repo
        .as_ref()
        .and_then(|r| {
            let mut options = git2::StatusOptions::new();
            options.include_ignored(false).include_untracked(true);
            r.statuses(Some(&mut options)).ok()
        })
        .map(|s| s.is_empty());
    let mut latest = std::collections::BTreeMap::new();
    for event in &events {
        latest.insert(&event.finding_id, event);
    }
    let current: Vec<_> = latest.values().map(|event| {
        let verified = if head.is_some() && event.verification.as_ref().is_some_and(|v|
            head.as_deref() == Some(v.commit.as_str())) && clean == Some(true) {
            "externally_verified_fix_at_current_clean_head"
        } else { "verification_not_current" };
        let state = effective_status(event, verified);
        serde_json::json!({"finding_id":event.finding_id,"event_id":event.event_id,"effective_status":state,"evidence":event})
    }).collect();
    Ok(
        serde_json::json!({"review_id":review,"head_commit":head,"working_tree_clean":clean,
        "coverage":if events.is_empty(){"not_imported"}else{"imported_events_only"},
        "verification_origin":"imported observations and external attestation declarations; workflow/LLM disposition establishes no accepted exception. Engram did not run checks or authenticate authors, URLs, or artifact digests",
        "automatic_finding_suppression":false,"current":current,"events":events}),
    )
}

/// Historical view containing only events whose source date is strictly before
/// an ISO YYYY-MM-DD cutoff. Invalid or missing source dates are counted and
/// excluded rather than allowed to leak future review outcomes into a replay.
pub(crate) fn snapshot_before(
    state: &crate::state::AppState,
    pid: &str,
    review: &str,
    exclusive_date: &str,
) -> Result<serde_json::Value, String> {
    if !valid_iso_date(exclusive_date) {
        return Err("historical review cutoff must be YYYY-MM-DD".into());
    }
    let all = read(state, pid, review)?;
    let mut invalid_or_future = 0usize;
    let events = all
        .into_iter()
        .filter(|event| {
            let date = event.recorded_at.get(..10);
            let included = date.is_some_and(|date| valid_iso_date(date) && date < exclusive_date);
            if !included {
                invalid_or_future += 1;
            }
            included
        })
        .collect::<Vec<_>>();
    let mut latest = std::collections::BTreeMap::new();
    for event in &events {
        latest.insert(&event.finding_id, event);
    }
    let current = latest
        .values()
        .map(|event| {
            let status = effective_status(event, "externally_attested_fix_before_cutoff");
            serde_json::json!({
                "finding_id": event.finding_id,
                "event_id": event.event_id,
                "effective_status": status,
                "evidence": event,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "review_id": review,
        "exclusive_cutoff": exclusive_date,
        "coverage": if events.is_empty() { "no_dated_events_before_cutoff" } else { "imported_events_before_cutoff" },
        "excluded_future_or_unparseable_events": invalid_or_future,
        "verification_origin": "imported source dates and external attestations; Engram did not authenticate them",
        "automatic_finding_suppression": false,
        "current": current,
        "events": events,
    }))
}

impl Engram {
    pub async fn handle_record_review_decisions(
        &self,
        req: RecordReviewDecisionsRequest,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_project_record(&req.project_id).await?;
        let count = append(&self.state, &req.project_id, &req.review_id, &req.decisions)
            .map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Stored {count} immutable review events. External attestations only; no checks were executed and no findings suppressed."
        ))]))
    }

    pub async fn handle_get_review_decisions(
        &self,
        req: GetReviewDecisionsRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
        if let Some(selector)=req.discussion {
            let guard=engram_core::memory::AllocationGuard::try_new(&self.state.memory_budget,
                8*1024*1024,engram_core::Subsystem::Misc,"imported review discussion recovery")
                .map_err(|error|McpError::internal_error(error.to_string(),None))?;
            let state=self.state.clone();
            let project=req.project_id;
            let review=req.review_id;
            let recovered=tokio::task::spawn_blocking(move || {
                let _guard=guard;
                super::review_discussion::recover(&state,&project,&review,selector)
            }).await.map_err(|error|McpError::internal_error(format!("discussion recovery worker failed: {error}"),None))?
                .map_err(|error|McpError::invalid_params(error.to_string(),None))?;
            return Ok(CallToolResult::success(vec![Content::text(recovered)]));
        }
        let value = snapshot(
            &self.state,
            &req.project_id,
            &req.review_id,
            std::path::Path::new(&rec.directory),
        )
        .map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        )]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::config::Config;

    #[test]
    fn historical_snapshot_excludes_future_and_unparseable_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = Config {
            data_dir: tmp.path().join("data"),
            allowed_roots: vec![project.clone()],
            embedding_backend: "fts_only".into(),
            max_concurrent_jobs: 1,
            ..Default::default()
        };
        let (state, _rx) = crate::state::AppState::new(cfg).unwrap();
        state
            .registry
            .put_project(&engram_core::ProjectRecord {
                project_id: "p".into(),
                project_name: "p".into(),
                directory: project.to_string_lossy().into_owned(),
                project_type: "general".into(),
                created_at_ms: 0,
                updated_at_ms: 0,
                reindex_required_since_ms: None,
            })
            .unwrap();
        let event = |id: &str, date: &str| ReviewDecision {
            event_id: id.into(),
            finding_id: id.into(),
            supersedes: None,
            kind: DecisionKind::ClaimedFix,
            source_url: "https://example.test/review".into(),
            author: "reviewer".into(),
            recorded_at: date.into(),
            rationale: "imported finding".into(),
            verification: None,
        };
        let mut legacy = event("review-history:legacy", "2026-01-01T00:00:00Z");
        legacy.kind = DecisionKind::AcceptedException;
        legacy.rationale = "Imported review disposition=llm_classified_dismissed; file=App.cs; finding=review observation".into();
        append(
            &state,
            "p",
            "PR-1",
            &[
                event("old", "2025-12-31T23:59:59Z"),
                event("future", "2026-02-01T00:00:00Z"),
                event("invalid", "unknown"),
                event("invalid-calendar", "2025-02-30T00:00:00Z"),
                legacy,
            ],
        )
        .unwrap();

        let view = snapshot_before(&state, "p", "PR-1", "2026-01-15").unwrap();
        let current = view["current"].as_array().unwrap();
        assert_eq!(current.len(), 2, "{view}");
        assert!(current.iter().any(|event| event["event_id"] == "old"));
        assert_eq!(
            current
                .iter()
                .find(|event| event["event_id"] == "review-history:legacy")
                .unwrap()["effective_status"],
            "unverified_review_disposition"
        );
        assert_eq!(view["excluded_future_or_unparseable_events"], 3, "{view}");
    }

    #[test]
    fn ignored_build_output_does_not_make_the_working_tree_dirty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join("bin")).unwrap();
        std::fs::write(project.join(".gitignore"), "bin/\n").unwrap();
        std::fs::write(project.join("App.cs"), "class App {}\n").unwrap();
        let repo = git2::Repository::init(&project).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(".gitignore")).unwrap();
        index.add_path(std::path::Path::new("App.cs")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
        std::fs::write(project.join("bin").join("App.dll"), [0u8; 4]).unwrap();
        let cfg = Config {
            data_dir: tmp.path().join("data"),
            allowed_roots: vec![project.clone()],
            embedding_backend: "fts_only".into(),
            max_concurrent_jobs: 1,
            ..Default::default()
        };
        let (state, _rx) = crate::state::AppState::new(cfg).unwrap();

        let view = snapshot(&state, "p", "PR-1", &project).unwrap();
        assert_eq!(view["working_tree_clean"], true, "{view}");

        std::fs::write(project.join("App.cs"), "class App { int x; }\n").unwrap();
        let view = snapshot(&state, "p", "PR-1", &project).unwrap();
        assert_eq!(view["working_tree_clean"], false, "{view}");
    }
}
