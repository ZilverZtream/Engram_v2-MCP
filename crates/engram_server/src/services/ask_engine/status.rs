//! Honest epistemic status + freshness snapshot. The engine must say what it
//! actually knows: empty ≠ stale ≠ failed ≠ absent.

use serde::Serialize;

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
    pub incompatible: bool,             // an evidence item's generation != active gen
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub summary: String,
    pub left: String,  // evidence_id
    pub right: String, // evidence_id
    pub kind: String,  // "authority_disagreement" | "snapshot_mismatch"
}
