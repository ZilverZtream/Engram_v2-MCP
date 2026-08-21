//! Honest epistemic status + freshness snapshot. The engine must say what it
//! actually knows: empty ≠ stale ≠ failed ≠ absent.

use engram_core::ProjectRecord;
use serde::Serialize;

use super::evidence::{Authority, EvidenceItem};
use super::plan::QueryPlan;
use super::retrieval::RetrievalCtx;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStatus {
    Answered,    // strong, direct, corroborated evidence
    Partial,     // some evidence, coverage gaps
    Ambiguous,   // entity resolution has >1 material branch
    Stale,       // evidence is behind the requested/current snapshot
    Unsupported, // nothing of adequate authority found
    Failed,      // retrieval engine error(s) dominated
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Hit,           // returned evidence
    Empty,         // ran cleanly, namespace/graph had nothing
    Stale,         // returned, but behind snapshot
    Ambiguous,     // multiple candidate branches
    Misunderstood, // query could not be formed (e.g. no resolvable entity)
    Failed,        // engine error
    Absent,        // provider not applicable to this plan (not run)
    TimedOut,      // dropped on deadline
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub provider: String,
    pub status: ProviderStatus,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The single evidence snapshot the report is pinned to. Only fields that are
/// actually trackable in the index/registry (verified) — there is no separate
/// business-logic generation, and no git branch is stored.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FreshnessSnapshot {
    pub project_generation: Option<u64>,
    pub git_commit: Option<String>,     // registry meta "last_git_oid"
    pub git_branch: Option<String>,     // from as_of.branch only (not indexed)
    pub history_watermark: Option<u64>, // meta "pr_ingest_watermark" | "total_commits"
    pub last_index_ms: Option<u64>,     // meta "last_index_completed_ms"
    pub semantic_tier: String,          // "semantic" | "degraded_trigram" | "off"
    pub reindex_required: bool,         // ProjectRecord.reindex_required_since_ms.is_some()
    pub incompatible: bool,             // reserved: cross-snapshot incompatibility (unused in M1)
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub summary: String,
    pub left: String,  // evidence_id
    pub right: String, // evidence_id
    pub kind: String,  // "authority_disagreement" | "snapshot_mismatch"
}

/// Assemble the one snapshot the report is pinned to, from the trackable sources.
pub async fn build_snapshot(
    ctx: &RetrievalCtx,
    rec: &ProjectRecord,
    as_of_branch: Option<&str>,
) -> FreshnessSnapshot {
    let reg = &ctx.registry;
    let pid = &ctx.project_id;
    let git_commit = reg.get_meta(pid, "last_git_oid").ok().flatten();
    let history_watermark = reg
        .get_meta(pid, "pr_ingest_watermark")
        .ok()
        .flatten()
        .or_else(|| reg.get_meta(pid, "total_commits").ok().flatten())
        .and_then(|s| s.parse::<u64>().ok());
    let last_index_ms = reg
        .get_meta(pid, "last_index_completed_ms")
        .ok()
        .flatten()
        .and_then(|s| s.parse::<u64>().ok());
    let semantic_tier = match ctx.search.semantic_quality() {
        engram_index::SemanticQuality::Semantic => "semantic",
        engram_index::SemanticQuality::DegradedTrigram => "degraded_trigram",
        engram_index::SemanticQuality::Off => "off",
    }
    .to_string();
    FreshnessSnapshot {
        project_generation: Some(ctx.generation),
        git_commit,
        git_branch: as_of_branch.map(|s| s.to_string()),
        history_watermark,
        last_index_ms,
        semantic_tier,
        reindex_required: rec.reindex_required_since_ms.is_some(),
        incompatible: false, // set by the orchestrator from snapshot_mismatch conflicts
    }
}

/// Calibrate the honest status from what the arms actually returned. Distinguishes
/// unsupported (nothing of adequate authority) from failed (arms errored) from
/// ambiguous (unresolved entity) from stale (behind snapshot) from partial.
pub fn assess_status(
    plan: &QueryPlan,
    evidence: &[EvidenceItem],
    providers: &[ProviderReport],
    snapshot: &FreshnessSnapshot,
) -> AnswerStatus {
    let ran: Vec<&ProviderReport> = providers
        .iter()
        .filter(|p| p.status != ProviderStatus::Absent)
        .collect();
    let all_failed = !ran.is_empty()
        && ran
            .iter()
            .all(|p| matches!(p.status, ProviderStatus::Failed | ProviderStatus::TimedOut));
    if evidence.is_empty() {
        return if all_failed {
            AnswerStatus::Failed
        } else {
            AnswerStatus::Unsupported
        };
    }
    if plan.entities.iter().any(|e| e.resolved.len() > 1) {
        return AnswerStatus::Ambiguous;
    }
    // Adequate authority = AgentMemory or stronger (excludes insight/semantic-only).
    let has_adequate = evidence
        .iter()
        .any(|e| e.authority <= Authority::AgentMemory);
    if !has_adequate {
        return AnswerStatus::Unsupported;
    }
    if snapshot.reindex_required || snapshot.incompatible {
        return AnswerStatus::Stale;
    }
    let gap = plan
        .needed_evidence
        .iter()
        .any(|k| !evidence.iter().any(|e| e.kind == *k));
    if gap {
        return AnswerStatus::Partial;
    }
    AnswerStatus::Answered
}
