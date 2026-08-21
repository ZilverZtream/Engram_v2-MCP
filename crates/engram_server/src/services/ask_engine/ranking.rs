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
        Some(t) if t > 0 && t <= now_ms => (0.5f64.powf((now_ms - t) as f64 / HALFLIFE_MS)) as f32,
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
        // Preserve a provider-set directness (companion 0.5, usage 0.85, …); only
        // fall back to the kind default when the provider left it unset. Without
        // this, a co-change companion (kind GraphRelation) would be scored as a
        // direct 0.9 relation and outrank real dependency edges.
        let d = it.directness.unwrap_or_else(|| default_directness(it));
        let corro = ((counts.get(&subject_key(it)).copied().unwrap_or(1) as f32 - 1.0) / 3.0)
            .clamp(0.0, 1.0);
        it.directness = Some(d);
        it.score = Some(score(it, d, corro, now_ms));
    }
    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Deterministic tie-break: identical-score items order stably by id
            // (ids are assigned in a deterministic order upstream).
            .then_with(|| a.evidence_id.cmp(&b.evidence_id))
    });

    // MMR-lite: greedily add, skipping near-duplicates of an already-chosen item
    // (same file within 5 lines). NOT same title — distinct symbols routinely
    // share a name (Page_Load, Execute, .ctor) across files, and collapsing on
    // title would silently discard real, direct call-site evidence.
    let mut chosen: Vec<EvidenceItem> = Vec::new();
    for it in items {
        if chosen.len() >= cap {
            break;
        }
        let dup = chosen
            .iter()
            .any(|c| c.path.is_some() && c.path == it.path && near_lines(c.lines, it.lines));
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
    "reject",
    "deny",
    "denied",
    "forbid",
    "forbidden",
    "never",
    "cannot",
    "not allowed",
    "block",
    "blocked",
    "prevent",
    "disallow",
    "must not",
];
const ALLOW: &[&str] = &[
    "allow",
    "allowed",
    "permit",
    "permitted",
    "enable",
    "enabled",
    "grant",
    "granted",
    "can ",
    "is allowed",
    "may ",
];

fn has_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// Common words that carry no subject identity — excluded as shared tokens so a
/// generic word never anchors a spurious conflict.
fn is_common_word(w: &str) -> bool {
    matches!(
        w,
        "which"
            | "there"
            | "their"
            | "these"
            | "those"
            | "where"
            | "while"
            | "would"
            | "could"
            | "should"
            | "about"
            | "after"
            | "before"
            | "value"
            | "class"
            | "using"
            | "return"
            | "function"
            | "method"
            | "public"
            | "private"
            | "static"
            | "string"
            | "object"
    )
}

/// The ±`radius`-char window around the first occurrence of `token` in `s`,
/// snapped to char boundaries so multibyte content never panics.
fn window_around<'a>(s: &'a str, token: &str, radius: usize) -> Option<&'a str> {
    let at = s.find(token)?;
    let mut lo = at.saturating_sub(radius);
    while lo > 0 && !s.is_char_boundary(lo) {
        lo -= 1;
    }
    let mut hi = (at + token.len() + radius).min(s.len());
    while hi < s.len() && !s.is_char_boundary(hi) {
        hi += 1;
    }
    Some(&s[lo..hi])
}

/// Significant tokens (len ≥ 5, not common words) shared between two contents.
fn shared_tokens(a: &str, b: &str) -> Vec<String> {
    let bl = b.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for w in a
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 5 && !is_common_word(w))
    {
        if bl.contains(w) && !out.iter().any(|x| x == w) {
            out.push(w.to_string());
        }
    }
    out
}

/// Detect conflicts to SURFACE (never silently resolve): an approved requirement
/// / runtime observation that contradicts current code about the SAME subject —
/// opposing polarity words must appear near the shared token in BOTH contents,
/// not merely somewhere in the snippet (keeps false positives down). The
/// snapshot/generation heuristic was removed: `Node.generation` is a per-node
/// last-extracted marker (unchanged files legitimately keep old generations
/// after incremental updates), so it cannot distinguish stale from fresh —
/// reindex_required is the honest staleness signal instead.
pub fn detect_conflicts(items: &[EvidenceItem], _active_generation: u64) -> Vec<Conflict> {
    let mut out = Vec::new();
    for hi in items.iter().filter(|e| {
        matches!(
            e.authority,
            Authority::ApprovedRequirement | Authority::RuntimeEvidence
        )
    }) {
        let hi_l = hi.content.to_lowercase();
        for code in items
            .iter()
            .filter(|e| e.authority == Authority::CurrentCode)
        {
            let code_l = code.content.to_lowercase();
            let Some(tok) = shared_tokens(&hi.content, &code.content)
                .into_iter()
                .find(|t| {
                    let hw = window_around(&hi_l, t, 90).unwrap_or(&hi_l);
                    let cw = window_around(&code_l, t, 90).unwrap_or(&code_l);
                    (has_any(hw, DENY) && has_any(cw, ALLOW))
                        || (has_any(hw, ALLOW) && has_any(cw, DENY))
                })
            else {
                continue;
            };
            out.push(Conflict {
                summary: format!(
                    "'{}' evidence and current code disagree about '{tok}'",
                    hi.provider
                ),
                left: hi.evidence_id.clone(),
                right: code.evidence_id.clone(),
                kind: "authority_disagreement".into(),
            });
        }
    }
    out
}
