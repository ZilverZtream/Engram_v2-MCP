//! ranking.rs (Task 7) — the anti-anchoring core. Dedup, then score by
//! authority + directness (dominant, 0.70 of the weight) with similarity/
//! relevance kept weak (≤0.15); MMR only after the authority-driven order.
//! Never let many weak semantic hits outvote one direct source-line relation.
//! Conflicts are DETECTED and surfaced, never used to drop evidence.

use std::collections::HashMap;

use super::evidence::{Authority, EvidenceItem, EvidenceKind};
use super::plan::{EntityKind, Modality, QueryPlan};
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
/// Round-2 audit P0-4: the best raw item of each REQUESTED modality survives
/// the evidence cap (replacing the weakest selected item), so a report /
/// schema / resource question is answered from that modality whenever the
/// index has it.
/// The question's own words (>= 5 letters, lowercase): the reserves prefer a
/// candidate that carries them ("which table stores … redovisningskategorier"
/// → rk_redovisningskategorier.sql over a higher-relevance stranger).
fn question_words(question: &str) -> Vec<String> {
    question
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 5)
        .map(|t| t.to_string())
        .collect()
}

/// Rank a reserve candidate: question-word hits in its path/content first,
/// then the arm's relevance.
fn reserve_key(e: &EvidenceItem, words: &[String]) -> (usize, i64) {
    let hay = format!(
        "{} {}",
        e.path.as_deref().unwrap_or("").to_lowercase(),
        e.content.to_lowercase()
    );
    let hits = words.iter().filter(|w| hay.contains(w.as_str())).count();
    (hits, (e.relevance * 1000.0) as i64)
}

fn promote(chosen: &mut Vec<EvidenceItem>, best: &EvidenceItem) {
    if !chosen.is_empty() {
        chosen.pop();
    }
    chosen.push(best.clone());
}

/// Round-2 audit P0-4d: every evidence KIND the plan needs keeps one item
/// under the cap when the raw pool has one (live r39: a "when was … last
/// changed" question lost its commit documents to same-file callee items).
pub fn reserve_needed_kinds(
    chosen: &mut Vec<EvidenceItem>,
    raw: &[EvidenceItem],
    needed: &[EvidenceKind],
) {
    for k in needed {
        if chosen.iter().any(|e| e.kind == *k) {
            continue;
        }
        let Some(best) = raw.iter().filter(|e| e.kind == *k).max_by(|a, b| {
            a.relevance
                .partial_cmp(&b.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) else {
            continue;
        };
        promote(chosen, best);
    }
}

pub fn reserve_modalities(
    chosen: &mut Vec<EvidenceItem>,
    raw: &[EvidenceItem],
    modalities: &[Modality],
    question: &str,
) {
    let words = question_words(question);
    for m in modalities {
        let of_modality = |e: &EvidenceItem| e.path.as_deref().is_some_and(|p| m.matches(p));
        // A modality item already chosen that carries the question's words is
        // enough; one that does not is replaced when the pool has a better fit.
        let have = chosen
            .iter()
            .filter(|e| of_modality(e))
            .map(|e| reserve_key(e, &words))
            .max();
        let Some(best) = raw
            .iter()
            .filter(|e| of_modality(e))
            .max_by_key(|e| reserve_key(e, &words))
        else {
            continue;
        };
        let best_key = reserve_key(best, &words);
        match have {
            Some(h) if h.0 >= best_key.0 => continue,
            Some(_) => {
                chosen.retain(|e| !(of_modality(e) && reserve_key(e, &words).0 < best_key.0));
                if chosen
                    .iter()
                    .any(|e| of_modality(e) && e.evidence_id == best.evidence_id)
                {
                    continue;
                }
                chosen.push(best.clone());
            }
            None => promote(chosen, best),
        }
    }
}

/// Round-2 audit P0-4b: a FILE the question names (a resolved File entity)
/// keeps one evidence item under the cap — live, its definition item was cut
/// by ten look-alike code chunks and the named file went uncited.
pub fn reserve_entity_files(
    chosen: &mut Vec<EvidenceItem>,
    raw: &[EvidenceItem],
    plan: &QueryPlan,
) {
    let targets: Vec<String> = plan
        .entities
        .iter()
        .flat_map(|e| e.resolved.iter())
        .filter(|r| r.kind == EntityKind::File)
        .map(|r| r.canonical.replace('\\', "/").to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    for t in targets {
        let of_file = |e: &EvidenceItem| {
            e.path.as_deref().is_some_and(|p| {
                let p = p.replace('\\', "/").to_lowercase();
                p == t || p.ends_with(&format!("/{t}")) || t.ends_with(&format!("/{p}"))
            })
        };
        if chosen.iter().any(|e| of_file(e)) {
            continue;
        }
        let Some(best) = raw.iter().filter(|e| of_file(e)).max_by(|a, b| {
            a.relevance
                .partial_cmp(&b.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) else {
            continue;
        };
        if !chosen.is_empty() {
            chosen.pop();
        }
        chosen.push(best.clone());
    }
}

/// Round-2 audit P0-4e: ONE reserve pass. Everything the plan requires under
/// the cap — an item per needed KIND, per requested MODALITY (preferring the
/// candidate that carries the question's words) and per named FILE — is
/// collected first; then the weakest UNPROTECTED items make room. Live r40:
/// the needed-kind reserve evicted the .resx the modality reserve had just
/// pushed, because each reserve popped the last item blindly.
/// P0-4f: how many items of a requested modality the reserve keeps.
pub const MODALITY_SLOTS: usize = 3;
/// Distinct caller files reserved for a "who calls X" question (round-7).
pub const CALLER_FILE_SLOTS: usize = 12;

pub fn reserve_required(
    chosen: &mut Vec<EvidenceItem>,
    raw: &[EvidenceItem],
    plan: &QueryPlan,
    question: &str,
) -> std::collections::HashSet<String> {
    let files: Vec<String> = plan
        .entities
        .iter()
        .flat_map(|e| e.resolved.iter())
        .filter(|r| r.kind == EntityKind::File)
        .map(|r| r.canonical.replace('\\', "/").to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let pin_definition = plan
        .contract
        .required_facets
        .contains(&super::plan::Facet::Definition);
    let pin_callers = matches!(
        plan.contract.direction,
        super::plan::ContractDirection::Callers
    );
    reserve_required_with(
        chosen,
        raw,
        &plan.needed_evidence,
        &plan.modalities,
        &files,
        question,
        pin_definition,
        pin_callers,
    )
}

pub fn reserve_required_with(
    chosen: &mut Vec<EvidenceItem>,
    raw: &[EvidenceItem],
    needed: &[EvidenceKind],
    modalities: &[Modality],
    entity_files: &[String],
    question: &str,
    pin_definition: bool,
    pin_callers: bool,
) -> std::collections::HashSet<String> {
    let words = question_words(question);
    let mut wanted: Vec<EvidenceItem> = Vec::new();
    let has =
        |set: &[EvidenceItem], pred: &dyn Fn(&EvidenceItem) -> bool| set.iter().any(|e| pred(e));
    for m in modalities {
        let of_modality = |e: &EvidenceItem| e.path.as_deref().is_some_and(|p| m.matches(p));
        // P0-4f (live r41: one .sql among ten items = precision 0.40): a
        // question that names a modality gets up to MODALITY_SLOTS of its
        // candidates, the ones carrying the question's words first.
        let mut cands: Vec<&EvidenceItem> = raw.iter().filter(|e| of_modality(e)).collect();
        cands.sort_by_key(|e| std::cmp::Reverse(reserve_key(e, &words)));
        for c in cands.into_iter().take(MODALITY_SLOTS) {
            let present = chosen
                .iter()
                .chain(wanted.iter())
                .any(|e| e.evidence_id == c.evidence_id);
            if !present {
                wanted.push(c.clone());
            }
        }
    }
    for k in needed {
        let of_kind = |e: &EvidenceItem| e.kind == *k;
        if has(chosen, &of_kind) || has(&wanted, &of_kind) {
            continue;
        }
        if let Some(best) = raw.iter().filter(|e| of_kind(e)).max_by(|a, b| {
            a.relevance
                .partial_cmp(&b.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) && !wanted.iter().any(|w| w.evidence_id == best.evidence_id)
        {
            wanted.push(best.clone());
        }
    }
    for t in entity_files {
        let t = t.replace('\\', "/").to_lowercase();
        let of_file = |e: &EvidenceItem| {
            e.path.as_deref().is_some_and(|p| {
                let p = p.replace('\\', "/").to_lowercase();
                p == t || p.ends_with(&format!("/{t}")) || t.ends_with(&format!("/{p}"))
            })
        };
        if has(chosen, &of_file) || has(&wanted, &of_file) {
            continue;
        }
        if let Some(best) = raw
            .iter()
            .filter(|e| of_file(e))
            .max_by_key(|e| reserve_key(e, &words))
            && !wanted.iter().any(|w| w.evidence_id == best.evidence_id)
        {
            wanted.push(best.clone());
        }
    }
    // Round-7 (ox_causal_18): a required Definition facet — the queried symbol's
    // own location — must survive the cap. A symbol with many callers/usages was
    // crowding its own definition out of the answer (DeleteImage's callers
    // cited, its api-images definition dropped despite being retrieved).
    if pin_definition {
        let is_def = |e: &EvidenceItem| {
            e.provider == "definition" || e.extraction_method.contains("definition")
        };
        if !has(chosen, &is_def) && !has(&wanted, &is_def) {
            if let Some(best) = raw.iter().filter(|e| is_def(e)).max_by(|a, b| {
                a.relevance
                    .partial_cmp(&b.relevance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }) {
                wanted.push(best.clone());
            }
        }
    }
    // Round-7 (ox_causal_16): for a "who/which calls X" question the answer IS
    // the set of caller files — a symbol with many callers must not drop a
    // required caller (the ajax.ts getImage wrapper was ranked out). Reserve one
    // item per DISTINCT caller file (graph-relation evidence), bounded. Scoped
    // to the callers direction so it never touches impact/precision questions.
    if pin_callers {
        let mut seen_files: std::collections::HashSet<String> = chosen
            .iter()
            .chain(wanted.iter())
            .filter_map(|e| e.path.clone())
            .collect();
        let mut added = 0usize;
        for e in raw.iter().filter(|e| {
            e.kind == EvidenceKind::GraphRelation
                && (e.provider == "usage" || e.provider == "callee_set")
        }) {
            if added >= CALLER_FILE_SLOTS {
                break;
            }
            if let Some(p) = e.path.clone() {
                if seen_files.insert(p) {
                    wanted.push(e.clone());
                    added += 1;
                }
            }
        }
    }
    let mut protected: std::collections::HashSet<String> =
        wanted.iter().map(|w| w.evidence_id.clone()).collect();
    for w in wanted {
        if chosen.iter().any(|e| e.evidence_id == w.evidence_id) {
            continue;
        }
        // Evict the weakest item that no reserve protects.
        let victim = chosen
            .iter()
            .enumerate()
            .filter(|(_, e)| !protected.contains(&e.evidence_id))
            .min_by(|(_, a), (_, b)| {
                a.score
                    .unwrap_or(a.relevance)
                    .partial_cmp(&b.score.unwrap_or(b.relevance))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i);
        if let Some(i) = victim {
            chosen.remove(i);
        }
        protected.insert(w.evidence_id.clone());
        chosen.push(w);
    }
    protected
}

/// Batch 1 Fix A (doc 11 grind): a LOOKUP-shaped question — exactly one
/// mention, resolved unambiguously, with no Usage/Impact/History intent —
/// answers in a handful of items. Live r57: exact-fact rows cited their one
/// answer inside ten items (seven sibling DALs) and failed the 0.5 precision
/// gate. Generic: keyed on plan shape, never on project content.
pub fn lookup_cap(
    entities: &[crate::services::ask_engine::plan::EntityMention],
    intents: &[(crate::services::ask_engine::plan::Intent, f32)],
    depth: crate::services::ask_engine::retrieval::Depth,
) -> usize {
    use crate::services::ask_engine::plan::Intent;
    let full = depth.evidence_cap();
    let breadth = intents
        .iter()
        .any(|(i, _)| matches!(i, Intent::Usage | Intent::Impact | Intent::History));
    // Batch 2 (live r58): junk mentions resolve to [] — only RESOLVED
    // mentions count. Exactly one mention carries exactly one resolution,
    // and no other mention resolved at all.
    let resolved: Vec<usize> = entities
        .iter()
        .map(|e| e.resolved.len())
        .filter(|n| *n > 0)
        .collect();
    let one_clear = resolved.len() == 1 && resolved[0] == 1;
    if one_clear && !breadth {
        5.min(full)
    } else {
        full
    }
}

/// Batch 7 (doc 11, live r63 usage_5): a question that names a path scope
/// ("under ts/map") gets evidence FROM that scope — an item whose path does
/// not contain the scope as a directory infix is out. Fail-safe: never
/// empties the pool.
pub fn retain_path_scoped(items: &mut Vec<EvidenceItem>, prefixes: &[String]) {
    if prefixes.is_empty() || items.is_empty() {
        return;
    }
    let matches_scope = |path: &str| {
        let p = path.replace('\\', "/").to_lowercase();
        prefixes.iter().any(|pre| {
            p == *pre
                || p.starts_with(&format!("{pre}/"))
                || p.contains(&format!("/{pre}/"))
                || p.ends_with(&format!("/{pre}"))
        })
    };
    if !items
        .iter()
        .any(|it| it.path.as_deref().is_some_and(matches_scope))
    {
        return;
    }
    items.retain(|it| it.path.as_deref().is_some_and(matches_scope));
}

/// Batch 4 (doc 11, live r60 exact_3): under an ENGAGED lookup cap, a slot
/// belongs to evidence that actually mentions the asked entity (its text or
/// a resolved canonical); entity-seeded relation evidence and items the
/// reserve pass protects are exempt. Fail-safe: never empties the answer.
pub fn retain_entity_anchored(
    items: &mut Vec<EvidenceItem>,
    needles: &[String],
    protected: &std::collections::HashSet<String>,
) {
    if needles.is_empty() || items.is_empty() {
        return;
    }
    let keep: Vec<bool> = items
        .iter()
        .map(|it| {
            let hay = format!(
                "{} {} {}",
                it.path.as_deref().unwrap_or(""),
                it.title.as_deref().unwrap_or(""),
                it.content
            )
            .to_lowercase();
            // Batch 4c: entity-SEEDED relation evidence (the callee/graph
            // arms hop from the asked entity) is about the entity by
            // construction — its text may never repeat the name.
            protected.contains(&it.evidence_id)
                || matches!(it.kind, EvidenceKind::GraphRelation)
                || needles.iter().any(|n| hay.contains(n.as_str()))
        })
        .collect();
    if !keep.iter().any(|k| *k) {
        return;
    }
    let mut i = 0;
    items.retain(|_| {
        let k = keep[i];
        i += 1;
        k
    });
}

/// Batch 3 (doc 11, live r59 exact_6): a lookup answer never spends two of
/// its five slots on the same file — first (highest-ranked) item per path wins.
pub fn retain_one_per_path(
    items: &mut Vec<EvidenceItem>,
    protected: &std::collections::HashSet<String>,
) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    items.retain(|it| {
        if protected.contains(&it.evidence_id) {
            if let Some(p) = &it.path {
                seen.insert(p.to_lowercase());
            }
            return true;
        }
        match &it.path {
            Some(p) => seen.insert(p.to_lowercase()),
            None => true,
        }
    });
}

/// Batch 5 (doc 11, live r61): the co-occurrence term list is built from
/// RESOLVED mentions only — an unresolved junk mention ("data-access",
/// resolved to []) must not flip the ranker into co-occurrence mode and
/// hand the direct-evidence boost to chunks containing the junk word.
pub fn cooccurrence_terms(entities: &[super::plan::EntityMention]) -> Vec<String> {
    let mut t: Vec<String> = entities
        .iter()
        .filter(|e| !e.resolved.is_empty())
        .map(|e| e.text.to_lowercase())
        .filter(|t| t.len() >= 3)
        .collect();
    t.sort();
    t.dedup();
    t
}

pub fn rank_and_select(items: Vec<EvidenceItem>, cap: usize) -> Vec<EvidenceItem> {
    rank_and_select_with_terms(items, cap, &[])
}

/// Batch 4 (doc 11, live r60 usage_4): with two or more asked terms,
/// corroboration rewards TERM CO-OCCURRENCE — an item exhibiting every
/// asked term is direct evidence for the intersection — never the
/// same-subject swarm (five foreign-key rows about one column co-boosted
/// each other while the item containing both terms ranked ninth).
pub fn rank_and_select_with_terms(
    items: Vec<EvidenceItem>,
    cap: usize,
    terms: &[String],
) -> Vec<EvidenceItem> {
    rank_and_select_with_terms_exempt(items, cap, terms, None)
}

/// Phase C2 (doc-12 P0-2, r70 live): an exhaustive-set provider's items are
/// FACTS the question asked to enumerate — the anti-anchoring per-path cap
/// and the selection cap apply to the supporting-evidence lane only. Exempt
/// items always survive selection and consume no cap slot.
pub fn rank_and_select_with_terms_exempt(
    items: Vec<EvidenceItem>,
    cap: usize,
    terms: &[String],
    exempt_provider: Option<&str>,
) -> Vec<EvidenceItem> {
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
        let mut d = it.directness.unwrap_or_else(|| default_directness(it));
        let corro = if terms.len() >= 2 {
            let hay = format!(
                "{} {} {}",
                it.path.as_deref().unwrap_or(""),
                it.title.as_deref().unwrap_or(""),
                it.content
            )
            .to_lowercase();
            let present = terms.iter().filter(|t| hay.contains(t.as_str())).count();
            if present == terms.len() {
                d = d.max(0.9);
            }
            (present.saturating_sub(1) as f32 / (terms.len() - 1) as f32).clamp(0.0, 1.0)
        } else {
            ((counts.get(&subject_key(it)).copied().unwrap_or(1) as f32 - 1.0) / 3.0)
                .clamp(0.0, 1.0)
        };
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
    let mut capped = 0usize;
    for it in items {
        if exempt_provider.is_some_and(|p| it.provider == p) {
            chosen.push(it);
            continue;
        }
        if capped >= cap {
            continue;
        }
        let dup = chosen
            .iter()
            .any(|c| c.path.is_some() && c.path == it.path && near_lines(c.lines, it.lines));
        // Round-2 audit P0-4e (anti-anchoring): one file may not fill the
        // evidence set — at most two items per path (live r40: the same .vb
        // cited five times cost a lookup its precision).
        let per_file = it
            .path
            .as_ref()
            .map(|p| chosen.iter().filter(|c| c.path.as_ref() == Some(p)).count())
            .unwrap_or(0);
        if !dup && per_file < 2 {
            capped += 1;
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
