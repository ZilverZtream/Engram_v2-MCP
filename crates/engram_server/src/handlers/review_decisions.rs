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
}

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

pub(crate) fn read(
    state: &crate::state::AppState,
    pid: &str,
    review: &str,
) -> Result<Vec<ReviewDecision>, String> {
    state
        .registry
        .get_meta(pid, &key(review)?)
        .map_err(|e| e.to_string())?
        .map(|s| {
            serde_json::from_str(&s).map_err(|e| format!("invalid stored decision evidence: {e}"))
        })
        .transpose()
        .map(|v| v.unwrap_or_default())
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
    let clean = repo
        .as_ref()
        .and_then(|r| r.statuses(None).ok())
        .map(|s| s.is_empty());
    let mut latest = std::collections::BTreeMap::new();
    for event in &events {
        latest.insert(&event.finding_id, event);
    }
    let current: Vec<_> = latest.values().map(|event| {
        let state = match event.kind {
            DecisionKind::Open => "open",
            DecisionKind::AcceptedException => "accepted_exception",
            DecisionKind::ClaimedFix => "claimed_fix",
            DecisionKind::VerifiedFix if head.as_deref() == event.verification.as_ref().map(|v| v.commit.as_str()) && clean == Some(true) => "externally_verified_fix_at_current_clean_head",
            DecisionKind::VerifiedFix => "verification_not_current",
        };
        serde_json::json!({"finding_id":event.finding_id,"event_id":event.event_id,"effective_status":state,"evidence":event})
    }).collect();
    Ok(
        serde_json::json!({"review_id":review,"head_commit":head,"working_tree_clean":clean,
        "coverage":if events.is_empty(){"not_imported"}else{"imported_events_only"},
        "verification_origin":"external_attestation; Engram did not run checks or authenticate authors, URLs, or artifact digests",
        "automatic_finding_suppression":false,"current":current,"events":events}),
    )
}

impl Engram {
    pub async fn handle_record_review_decisions(
        &self,
        req: RecordReviewDecisionsRequest,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_project_record(&req.project_id).await?;
        let update = || -> Result<usize, String> {
            let _guard = WRITES
                .lock()
                .map_err(|_| "decision writer lock unavailable")?;
            let mut events = read(&self.state, &req.project_id, &req.review_id)?;
            for event in &req.decisions {
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
            }
            let encoded = serde_json::to_string(&events).map_err(|e| e.to_string())?;
            if events.len() > 500 || encoded.len() > 1024 * 1024 {
                return Err("review evidence exceeds 500 events or 1 MiB".into());
            }
            self.state
                .registry
                .set_meta(&req.project_id, &key(&req.review_id)?, &encoded)
                .map_err(|e| e.to_string())?;
            Ok(events.len())
        };
        let count = update().map_err(|e| McpError::invalid_params(e, None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Stored {count} immutable review events. External attestations only; no checks were executed and no findings suppressed."
        ))]))
    }

    pub async fn handle_get_review_decisions(
        &self,
        req: GetReviewDecisionsRequest,
    ) -> Result<CallToolResult, McpError> {
        let rec = self.ensure_project_record(&req.project_id).await?;
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
