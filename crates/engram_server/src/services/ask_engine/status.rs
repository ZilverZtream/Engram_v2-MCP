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

/// Is the evidence a REAL answer, or just coincidental keyword matches? On a
/// large codebase loose FTS finds *something* for any question, so "evidence is
/// non-empty" is not enough to claim support (the live OciusX eval showed
/// nonsense questions returning partial instead of abstaining). Adequate support
/// = a graph relation (a resolved-entity structural link), OR — for a
/// multi-term question — a hit whose text covers ≥2 distinct query terms (a lone
/// coincidental keyword is not an answer). A single-term question is satisfied
/// by any lexical hit.
/// The question's NAMED terms that no evidence mentions (external audit
/// 2026-08-29 row 6). A named term is identifier-like (`Check_pr_id`,
/// `api.GetAll`), CamelCase, or a capitalised word that is not sentence-
/// initial ("Which Redis cluster …"). Such a term is the question's premise:
/// when nothing in the evidence set (path, title, content) contains it, the
/// answer must not be asserted from the OTHER terms — that is anchoring on a
/// false premise. Returned in question order, original spelling, deduped.
pub fn uncovered_named_terms(question: &str, evidence: &[EvidenceItem]) -> Vec<String> {
    uncovered_named_terms_with(question, evidence, &[])
}

/// Everything the evidence set says, lowercased: path, title, symbol id,
/// content — plus the terms the planner RESOLVED (`known`), because a name
/// the index resolved is covered by definition even when a relation's prose
/// only says "X calls the target" (row 6 slice 2).
fn evidence_haystack(evidence: &[EvidenceItem], known: &[String]) -> String {
    let mut hay: String = evidence
        .iter()
        .map(|e| {
            format!(
                "{} {} {} {}\n",
                e.path.clone().unwrap_or_default(),
                e.title.clone().unwrap_or_default(),
                e.symbol_id.clone().unwrap_or_default(),
                e.content
            )
        })
        .collect();
    for k in known {
        hay.push_str(k);
        hay.push('\n');
    }
    hay.to_lowercase()
}

/// `uncovered_named_terms` with the planner's resolved terms counted as covered.
pub fn uncovered_named_terms_with(
    question: &str,
    evidence: &[EvidenceItem],
    known: &[String],
) -> Vec<String> {
    let hay = evidence_haystack(evidence, known);
    let mut out: Vec<String> = Vec::new();
    for (i, raw) in question.split_whitespace().enumerate() {
        let tok = raw
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.')
            .trim_matches('.');
        if tok.len() < 3 || !tok.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        let upper_start = tok.chars().next().is_some_and(|c| c.is_ascii_uppercase());
        let inner_upper = tok.chars().skip(1).any(|c| c.is_ascii_uppercase());
        let identifier = tok.contains('_') || tok.contains('.');
        let named = identifier || inner_upper || (upper_start && i > 0);
        if !named {
            continue;
        }
        let lower = tok.to_lowercase();
        if is_filler_term(&lower) || hay.contains(&lower) {
            continue;
        }
        if !out.iter().any(|o| o.eq_ignore_ascii_case(tok)) {
            out.push(tok.to_string());
        }
    }
    out
}

pub fn has_adequate_support(question: &str, evidence: &[EvidenceItem]) -> bool {
    has_adequate_support_with(question, evidence, &[])
}

/// `has_adequate_support` with the planner's resolved terms: a resolved
/// entity that the evidence actually contains anchors support on its own —
/// the index found the thing that was asked about (row 6 slice 2).
pub fn has_adequate_support_with(
    question: &str,
    evidence: &[EvidenceItem],
    known: &[String],
) -> bool {
    use super::evidence::EvidenceKind::GraphRelation;
    // A named premise nobody has evidence for is not supported by evidence
    // for the question's other terms (row 6: "Which Redis cluster caches the
    // redovisningskategori list?" was answered from the real term alone).
    if !uncovered_named_terms_with(question, evidence, known).is_empty() {
        return false;
    }
    // Strong structural support = a graph relation from a RESOLVED-entity arm
    // (impact / usage / companion). NOT the concept arm: it matches a node on a
    // single name-stem, so a lone "…Policy" class would falsely support a
    // nonsense multi-term question.
    if evidence.iter().any(|e| {
        e.kind == GraphRelation && matches!(e.provider.as_str(), "impact" | "usage" | "companion")
    }) {
        return true;
    }
    // A resolved entity present in the evidence (text, path or symbol id) is
    // structural support: the index found the asked-about thing.
    {
        let hay = evidence_haystack(evidence, &[]);
        if known
            .iter()
            .filter(|k| k.len() >= 4)
            .any(|k| hay.contains(&k.to_lowercase()))
        {
            return true;
        }
    }
    // Distinctive terms only: len >= 5 (drops "work"/"does"/"what"/"page"/…,
    // which as short substrings would spuriously match code like "framework")
    // and no filler words. A distinctive term substring-matching code is real
    // signal; two of them in one hit means the hit is on-topic.
    let terms: Vec<String> = question
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 5 && !is_filler_term(w))
        .map(|s| s.to_string())
        .collect();
    // Concept matches (single-stem) are too weak to establish support alone.
    let considered = || evidence.iter().filter(|e| e.provider != "concept");
    if terms.len() < 2 {
        return considered().next().is_some();
    }
    // Aggregate term coverage across the evidence SET, not per-hit: a compound
    // question's distinctive terms ("authentication" + "changed") legitimately
    // live in different files, so requiring both in one hit falsely abstains.
    let mut covered: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for e in considered() {
        let hay = format!("{} {}", e.title.clone().unwrap_or_default(), e.content).to_lowercase();
        for t in &terms {
            if hay.contains(t.as_str()) {
                covered.insert(t.as_str());
            }
        }
    }
    covered.len() >= 2
}

/// Common 5+ letter words that are not distinctive query subjects.
fn is_filler_term(w: &str) -> bool {
    matches!(
        w,
        "which"
            | "there"
            | "where"
            | "their"
            | "these"
            | "those"
            | "would"
            | "could"
            | "should"
            | "about"
            | "after"
            | "before"
            | "using"
            | "based"
            | "other"
            | "every"
            | "while"
            | "doing"
            | "being"
            | "still"
            | "again"
            | "some"
            | "thing"
            | "things"
            | "something"
            | "anything"
            | "everything"
            | "please"
            | "system"
            | "systems"
    )
}

/// The one evidence kind that most directly answers a given answer type. Used to
/// decide Answered vs Partial without penalizing a project that simply doesn't
/// index docs/business-rules/history (e.g. OciusX is code-only).
fn primary_kind(t: super::plan::AnswerType) -> super::evidence::EvidenceKind {
    use super::evidence::EvidenceKind as K;
    use super::plan::AnswerType as A;
    match t {
        A::ImpactSet | A::UsageSites => K::GraphRelation,
        A::Timeline => K::HistoryCommit,
        A::RequirementRef => K::MemoryNote,
        A::TestGuidance => K::TestRef,
        // Explanation, Rationale, Plan, RootCause, Comparison, CoverageGaps
        _ => K::SourceCode,
    }
}

/// Calibrate the honest status from what the arms actually returned. Distinguishes
/// unsupported (no real answer) from failed (arms errored) from ambiguous
/// (unresolved entity) from stale (behind snapshot) from partial/answered.
/// `adequate_support` is computed by `has_adequate_support` (the orchestrator has
/// the question text).
pub fn assess_status(
    plan: &QueryPlan,
    evidence: &[EvidenceItem],
    providers: &[ProviderReport],
    snapshot: &FreshnessSnapshot,
    adequate_support: bool,
) -> AnswerStatus {
    let ran: Vec<&ProviderReport> = providers
        .iter()
        .filter(|p| p.status != ProviderStatus::Absent)
        .collect();
    let all_failed = !ran.is_empty()
        && ran
            .iter()
            .all(|p| matches!(p.status, ProviderStatus::Failed | ProviderStatus::TimedOut));
    // Abstain honestly: no evidence, only failed arms, or only coincidental
    // keyword matches all mean "no answer of adequate support".
    if evidence.is_empty() {
        return if all_failed {
            AnswerStatus::Failed
        } else {
            AnswerStatus::Unsupported
        };
    }
    // Ambiguous means genuinely DIFFERENT symbols of the SAME kind: one name
    // resolved under two node kinds (a table and its .sql file; a function and
    // a session key named after it — golden `ox_exact_5`) is one thing, not
    // two branches.
    if plan.entities.iter().any(|e| {
        let mut by_kind: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for r in &e.resolved {
            by_kind
                .entry(format!("{:?}", r.kind))
                .or_default()
                .insert(r.canonical.to_lowercase());
        }
        by_kind.values().any(|names| names.len() > 1)
    }) {
        return AnswerStatus::Ambiguous;
    }
    let has_authority = evidence
        .iter()
        .any(|e| e.authority <= Authority::AgentMemory);
    if !adequate_support || !has_authority {
        return AnswerStatus::Unsupported;
    }
    if snapshot.reindex_required || snapshot.incompatible {
        return AnswerStatus::Stale;
    }
    // Answered when the answer type's PRIMARY evidence kind is present; otherwise
    // there is adequate support but the ideal evidence is thin → partial.
    // Round-2 audit P0-4: evidence that never touches a requested modality
    // cannot be a full answer ("which reports …" answered from .vb only).
    if plan.modalities.iter().any(|m| {
        !evidence
            .iter()
            .any(|e| e.path.as_deref().is_some_and(|p| m.matches(p)))
    }) {
        return AnswerStatus::Partial;
    }
    let primary = primary_kind(plan.answer_type);
    if evidence.iter().any(|e| e.kind == primary) {
        AnswerStatus::Answered
    } else {
        AnswerStatus::Partial
    }
}
