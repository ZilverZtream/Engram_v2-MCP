//! ranking.rs (Task 7) — the anti-anchoring core. Dedup, then score by
//! authority + directness (dominant, 0.70 of the weight) with similarity/
//! relevance kept weak (≤0.15); MMR only after the authority-driven order.
//! Never let many weak semantic hits outvote one direct source-line relation.
//! Conflicts are DETECTED and surfaced, never used to drop evidence.

use std::collections::HashMap;

use super::evidence::{Authority, EvidenceItem, EvidenceKind};
use super::status::Conflict;

const HALFLIFE_MS: f64 = 30.0 * 24.0 * 3600.0 * 1000.0;

fn subject_key(ev: &EvidenceItem) -> String {
    ev.symbol_id
        .clone()
        .or_else(|| ev.path.clone())
        .or_else(|| ev.title.clone())
        .unwrap_or_else(|| ev.evidence_id.clone())
}

fn default_directness(ev: &EvidenceItem) -> f32 {
    let base: f32 = match ev.kind {
        EvidenceKind::GraphRelation => 0.9,
        EvidenceKind::SourceCode => 0.7,
        EvidenceKind::BusinessRule | EvidenceKind::Setting => 0.6,
        EvidenceKind::MemoryNote | EvidenceKind::DocSection | EvidenceKind::TestRef => 0.5,
        EvidenceKind::HistoryCommit | EvidenceKind::ConceptGroup => 0.45,
        EvidenceKind::Insight => 0.35,
    };
    // Similarity-only evidence is capped low regardless of kind.
    if ev.authority == Authority::SemanticSimilarity {
        base.min(0.2)
    } else {
        base
    }
}

fn recency(ts: Option<u64>, now_ms: u64) -> f32 {
    match ts {
        Some(t) if t > 0 && t <= now_ms => {
            (0.5f64.powf((now_ms - t) as f64 / HALFLIFE_MS)) as f32
        }
        _ => 0.0,
    }
}

fn score(ev: &EvidenceItem, directness: f32, corroboration: f32, now_ms: u64) -> f32 {
    0.40 * ev.authority.weight()
        + 0.30 * directness
        + 0.15 * ev.relevance.clamp(0.0, 1.0)
        + 0.10 * corroboration
        + 0.05 * recency(ev.timestamp, now_ms)
}

/// Collapse exact duplicates: same symbol_id, else same (path, lines). Keep the
/// higher-authority (lower enum), then higher-relevance instance.
fn dedup(items: Vec<EvidenceItem>) -> Vec<EvidenceItem> {
    let mut best: Vec<EvidenceItem> = Vec::new();
    'outer: for it in items {
        for existing in best.iter_mut() {
            let same = match (&it.symbol_id, &existing.symbol_id) {
                (Some(a), Some(b)) => a == b,
                _ => it.path.is_some() && it.path == existing.path && it.lines == existing.lines,
            };
            if same {
                let better = it.authority < existing.authority
                    || (it.authority == existing.authority && it.relevance > existing.relevance);
                if better {
                    *existing = it;
                }
                continue 'outer;
            }
        }
        best.push(it);
    }
    best
}

/// Rank by authority/directness and select a small, source-diverse, high-signal
/// set (MMR after the authority order). Fills each kept item's score/directness.
pub fn rank_and_select(items: Vec<EvidenceItem>, cap: usize) -> Vec<EvidenceItem> {
    let now_ms = crate::utils::now_ms();
    let mut items = dedup(items);

    // Corroboration: how many items share a subject.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for it in &items {
        *counts.entry(subject_key(it)).or_insert(0) += 1;
    }

    // Score.
    for it in items.iter_mut() {
        let d = default_directness(it);
        let corro = ((counts.get(&subject_key(it)).copied().unwrap_or(1) as f32 - 1.0) / 3.0)
            .clamp(0.0, 1.0);
        it.directness = Some(d);
        it.score = Some(score(it, d, corro, now_ms));
    }
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // MMR-lite: greedily add, skipping near-duplicates of an already-chosen item
    // (same file within 5 lines, or same title).
    let mut chosen: Vec<EvidenceItem> = Vec::new();
    for it in items {
        if chosen.len() >= cap {
            break;
        }
        let dup = chosen.iter().any(|c| {
            (c.path.is_some() && c.path == it.path && near_lines(c.lines, it.lines))
                || (c.title.is_some() && c.title == it.title && it.title.is_some())
        });
        if !dup {
            chosen.push(it);
        }
    }
    chosen
}

fn near_lines(a: Option<(u32, u32)>, b: Option<(u32, u32)>) -> bool {
    match (a, b) {
        (Some((a0, _)), Some((b0, _))) => a0.abs_diff(b0) <= 5,
        _ => false,
    }
}

const DENY: &[&str] = &[
    "reject", "deny", "denied", "forbid", "forbidden", "never", "cannot", "not allowed",
    "block", "blocked", "prevent", "disallow", "must not",
];
const ALLOW: &[&str] = &[
    "allow", "allowed", "permit", "permitted", "enable", "enabled", "grant", "granted",
    "can ", "is allowed", "may ",
];

fn has_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// Shared significant token (len ≥ 4) between two contents — a cheap "same
/// subject" test for conflict detection.
fn shares_token(a: &str, b: &str) -> Option<String> {
    let bl = b.to_lowercase();
    a.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 4)
        .find(|w| bl.contains(*w))
        .map(|w| w.to_string())
}

/// Detect conflicts to SURFACE (never to silently resolve): (1) an approved
/// requirement / runtime observation that contradicts current code on the same
/// subject; (2) evidence drawn from a stale generation next to current evidence.
pub fn detect_conflicts(items: &[EvidenceItem], active_generation: u64) -> Vec<Conflict> {
    let mut out = Vec::new();

    // (1) authority disagreement — requirement/runtime vs current code, opposing polarity.
    for hi in items.iter().filter(|e| {
        matches!(e.authority, Authority::ApprovedRequirement | Authority::RuntimeEvidence)
    }) {
        for code in items.iter().filter(|e| e.authority == Authority::CurrentCode) {
            if let Some(tok) = shares_token(&hi.content, &code.content) {
                let hi_l = hi.content.to_lowercase();
                let code_l = code.content.to_lowercase();
                let opposed = (has_any(&hi_l, DENY) && has_any(&code_l, ALLOW))
                    || (has_any(&hi_l, ALLOW) && has_any(&code_l, DENY));
                if opposed {
                    out.push(Conflict {
                        summary: format!(
                            "'{}' evidence and current code disagree on '{tok}'",
                            hi.provider
                        ),
                        left: hi.evidence_id.clone(),
                        right: code.evidence_id.clone(),
                        kind: "authority_disagreement".into(),
                    });
                }
            }
        }
    }

    // (2) snapshot mismatch — a stale-generation item beside current evidence on the same subject.
    for stale in items
        .iter()
        .filter(|e| matches!(e.generation, Some(g) if g != active_generation))
    {
        for cur in items.iter().filter(|e| {
            e.generation == Some(active_generation) || (e.generation.is_none() && e.path.is_some())
        }) {
            let same = match (&stale.symbol_id, &cur.symbol_id) {
                (Some(a), Some(b)) => a == b,
                _ => stale.path.is_some() && stale.path == cur.path,
            };
            if same {
                out.push(Conflict {
                    summary: format!(
                        "evidence '{}' is from generation {:?}, not the active {active_generation}",
                        stale.evidence_id, stale.generation
                    ),
                    left: stale.evidence_id.clone(),
                    right: cur.evidence_id.clone(),
                    kind: "snapshot_mismatch".into(),
                });
                break;
            }
        }
    }
    out
}
