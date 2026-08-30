//! Deterministic multi-intent / multi-entity planner. Pure string logic — no
//! substrate deps. Replaces the old single-intent `classify()`: a question maps
//! to a weighted SET of intents and a list of entity mentions + qualifiers.
//! Entity RESOLUTION (graph lookups) happens in `resolver`, not here.

use super::evidence::EvidenceKind;
use super::plan::*;

/// Classify a question into a weighted intent set, entities, and qualifiers.
pub fn plan_query(question: &str) -> QueryPlan {
    let q = question.trim();
    let lower = q.to_lowercase();

    let mut intents: Vec<(Intent, f32)> = Vec::new();
    let add = |it: Intent, w: f32, acc: &mut Vec<(Intent, f32)>| {
        if let Some(e) = acc.iter_mut().find(|(i, _)| *i == it) {
            e.1 = e.1.max(w);
        } else {
            acc.push((it, w));
        }
    };

    // ── Feature ──
    for (p, w) in [
        ("as a ", 0.9f32),
        ("as an ", 0.9),
        ("add ", 0.7),
        ("implement ", 0.8),
        ("create a ", 0.7),
        ("build ", 0.6),
        ("i want to ", 0.6),
    ] {
        if lower.starts_with(p) {
            add(Intent::Feature, w, &mut intents);
        }
    }
    if lower.contains("i want") || lower.contains("we need to add") {
        add(Intent::Feature, 0.6, &mut intents);
    }

    // ── Impact ──
    for kw in [
        "what breaks",
        "blast radius",
        "impact of",
        "safe to",
        "affected by",
        "what depends on",
        "consequences of",
        "ripple",
    ] {
        if lower.contains(kw) {
            add(Intent::Impact, 0.85, &mut intents);
        }
    }
    if (lower.contains("change")
        || lower.contains("remove")
        || lower.contains("rename")
        || lower.contains("delete")
        || lower.contains("edit")
        || lower.contains("modify"))
        && (lower.contains("break")
            || lower.contains("risk")
            || lower.contains("what happens")
            || lower.contains("affect"))
    {
        add(Intent::Impact, 0.7, &mut intents);
    }

    // ── Usage ──
    for kw in [
        "where is",
        "where are",
        "who calls",
        "who uses",
        "used where",
        "references to",
        "callers of",
        "call sites",
        "consumed by",
        "read from",
        "written to",
    ] {
        if lower.contains(kw) {
            add(Intent::Usage, 0.8, &mut intents);
        }
    }

    // ── History (WHEN) vs Rationale (WHY) — distinct intents ──
    for kw in [
        "when did",
        "when was",
        "history of",
        "last changed",
        "recently changed",
        "who changed",
        "which commit",
        "which pr",
    ] {
        if lower.contains(kw) {
            add(Intent::History, 0.8, &mut intents);
        }
    }
    for kw in [
        "why is",
        "why does",
        "why was",
        "why did",
        "rationale",
        "reason for",
        "reason behind",
        "the point of",
        "designed this way",
        "intended to",
    ] {
        if lower.contains(kw) {
            add(Intent::Rationale, 0.8, &mut intents);
        }
    }

    // ── BugDiagnosis ──
    for kw in [
        "fail",
        "fails",
        "failing",
        "error",
        "exception",
        "crash",
        "broken",
        "doesn't work",
        "does not work",
        "why can't",
        "why cannot",
        "not working",
        "returns null",
        "throws",
        "stack trace",
        "root cause",
        "regression",
    ] {
        if lower.contains(kw) {
            add(Intent::BugDiagnosis, 0.75, &mut intents);
        }
    }

    // ── Requirements ──
    for kw in [
        "requirement",
        "supposed to",
        "should it",
        "is it correct",
        "expected behavior",
        "acceptance criteria",
        "spec says",
        "which requirement",
        "meant to",
    ] {
        if lower.contains(kw) {
            add(Intent::Requirements, 0.7, &mut intents);
        }
    }

    // ── Compare ──
    for kw in [
        "compare",
        "difference between",
        "vs ",
        "versus",
        "old and new",
        "before and after",
        "instead of",
    ] {
        if lower.contains(kw) {
            add(Intent::Compare, 0.7, &mut intents);
        }
    }

    // ── Test ──
    for kw in [
        "how should this be tested",
        "how do i test",
        "test coverage",
        "unit test",
        "how to test",
        "what tests",
        "which tests",
    ] {
        if lower.contains(kw) {
            add(Intent::Test, 0.7, &mut intents);
        }
    }

    // ── Unknowns ──
    for kw in [
        "what remains unknown",
        "what don't we know",
        "what do we not know",
        "coverage gap",
        "what's missing",
        "unresolved",
    ] {
        if lower.contains(kw) {
            add(Intent::Unknowns, 0.7, &mut intents);
        }
    }

    // ── Explain: default/backstop; also positively signalled ──
    for kw in [
        "how does",
        "how do",
        "how is",
        "what is",
        "what does",
        "explain",
        "walk me through",
        "tell me about",
        "what's the",
        "overview of",
    ] {
        if lower.contains(kw) {
            add(Intent::Explain, 0.6, &mut intents);
        }
    }
    if intents.is_empty() {
        add(Intent::Explain, 0.5, &mut intents);
    }

    // Sort strongest-first; keep meaningful arms (weight >= 60% of top, floor 0.5).
    intents.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top = intents.first().map(|(_, w)| *w).unwrap_or(0.0);
    intents.retain(|(_, w)| *w >= (top * 0.6).max(0.5));

    let entities = extract_entities(q);
    let qualifiers = extract_qualifiers(q, &lower);
    let answer_type = primary_answer_type(intents.first().map(|(i, _)| *i));
    let needed_evidence = needed_evidence_for(&intents);
    let modalities = detect_modalities(&lower);

    QueryPlan {
        intents,
        entities,
        qualifiers,
        needed_evidence,
        answer_type,
        modalities,
    }
}

/// Round-2 audit P0-4: the evidence modality the question asks for, from
/// WHOLE-WORD cues — "reporting" is not a report request, "table" is.
pub fn detect_modalities(lower: &str) -> Vec<Modality> {
    let toks: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();
    let has = |words: &[&str]| toks.iter().any(|t| words.contains(t));
    let mut out = Vec::new();
    if has(&["report", "reports", "rdl", "rdlc", "ssrs"]) {
        out.push(Modality::Report);
    }
    if has(&[
        "table",
        "tables",
        "schema",
        "column",
        "columns",
        "migration",
        "migrations",
        "sql",
        "dbml",
        "procedure",
        "procedures",
    ]) {
        out.push(Modality::Sql);
    }
    if has(&[
        "resx",
        "resource",
        "resources",
        "translation",
        "translations",
        "localized",
        "localization",
    ]) {
        out.push(Modality::Resource);
    }
    if has(&["aspx", "ascx", "markup", "page", "pages"]) {
        out.push(Modality::Markup);
    }
    if has(&["typescript", "javascript", "ts", "js", "tsx"]) {
        out.push(Modality::Script);
    }
    out
}

/// Extract candidate entity mentions from surface form. Resolution is separate.
pub fn extract_entities(q: &str) -> Vec<EntityMention> {
    let mut out: Vec<EntityMention> = Vec::new();
    let push = |text: String, kind: EntityKind, out: &mut Vec<EntityMention>| {
        if text.len() < 2 {
            return;
        }
        if out.iter().any(|m| m.text.eq_ignore_ascii_case(&text)) {
            return;
        }
        out.push(EntityMention {
            text,
            guessed_kind: kind,
            resolved: Vec::new(),
        });
    };

    // 1. Quoted strings — highest-signal literal entities/symptoms.
    let bytes = q.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '"' || c == '\'' || c == '`' {
            if let Some(rel) = q[i + 1..].find(c) {
                let inner = &q[i + 1..i + 1 + rel];
                if !inner.trim().is_empty() {
                    push(inner.trim().to_string(), EntityKind::Unknown, &mut out);
                }
                i = i + 1 + rel + 1;
                continue;
            }
        }
        i += 1;
    }

    // 2. Token scan for code-shaped identifiers.
    for raw in
        q.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | '?' | '!' | ';' | ':'))
    {
        let t = raw.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '.' | ','));
        if t.len() < 2 {
            continue;
        }
        // dotted path: a.b.c / Resources.text.Key / ImportService.vb
        // Round-2 audit P0-4e: a lowercase hyphenated token of 8+ chars is a
        // file mention ("api-installationsobjektprojekt"); the resolver maps
        // it to the file by stem.
        if t.len() >= 8
            && t.contains('-')
            && !t.starts_with('-')
            && !t.ends_with('-')
            && t.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            push(t.to_string(), EntityKind::File, &mut out);
            continue;
        }
        if t.contains('.') && !t.starts_with('.') && !t.ends_with('.') {
            let tl = t.to_lowercase();
            let looks_file = [
                ".cs", ".vb", ".ts", ".js", ".aspx", ".ascx", ".sql", ".resx", ".vbhtml", ".razor",
                ".json", ".xml", ".config",
            ]
            .iter()
            .any(|e| tl.ends_with(e));
            let kind = if looks_file {
                EntityKind::File
            } else {
                EntityKind::Symbol
            };
            push(t.to_string(), kind, &mut out);
            continue;
        }
        // path with slashes
        if t.contains('/') || t.contains('\\') {
            push(t.replace('\\', "/"), EntityKind::File, &mut out);
            continue;
        }
        // CamelCase or snake_case → identifier (table/setting kind left to resolver)
        let has_upper_inner = t.chars().skip(1).any(|c| c.is_ascii_uppercase());
        let has_underscore = t.contains('_');
        if has_upper_inner || has_underscore {
            let snakey = has_underscore
                && t.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit());
            let kind = if t.starts_with("ss_") || t.starts_with("tbl") || snakey {
                EntityKind::Unknown
            } else {
                EntityKind::Symbol
            };
            push(t.to_string(), kind, &mut out);
            continue;
        }
        // Single-token PascalCase identifier (Authenticate, Login, Run) — the
        // common case a naive "needs inner caps" rule drops. Excluded when it is
        // a question/grammar word (How, What, Where, ...).
        if t.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && t.chars().all(|c| c.is_ascii_alphanumeric())
            && t.len() >= 3
            && !is_stopword(&t.to_lowercase())
        {
            push(t.to_string(), EntityKind::Symbol, &mut out);
        }
    }
    out
}

/// Common question / grammar words that are never code entities. Deliberately
/// omits verb-ish words (run/save/get/set/login) — those can be real symbols,
/// and the resolver drops them harmlessly if they don't resolve.
fn is_stopword(w: &str) -> bool {
    matches!(
        w,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "into"
            | "this"
            | "that"
            | "these"
            | "those"
            | "what"
            | "when"
            | "where"
            | "which"
            | "why"
            | "who"
            | "whom"
            | "whose"
            | "how"
            | "does"
            | "did"
            | "are"
            | "was"
            | "were"
            | "has"
            | "have"
            | "had"
            | "will"
            | "would"
            | "should"
            | "could"
            | "can"
            | "shall"
            | "may"
            | "might"
            | "not"
            | "but"
            | "our"
            | "your"
            | "their"
            | "its"
            | "his"
            | "her"
            | "any"
            | "all"
            | "some"
            | "each"
    )
}

/// Roles, change verbs, and scope words that qualify the retrieval.
pub fn extract_qualifiers(q: &str, lower: &str) -> Qualifiers {
    let mut ql = Qualifiers::default();
    for role in [
        "administrator",
        "admin",
        "tenant admin",
        "tenant administrator",
        "foreman",
        "superuser",
        "guest",
        "anonymous",
        "operator",
        "manager",
        "user",
    ] {
        if lower.contains(role) {
            ql.roles.push(role.to_string());
        }
    }
    // "from X to Y" — slice the ORIGINAL string to preserve case (XML, not xml).
    // Use `.get()` throughout: `pos`/`tp` are byte offsets into a lowercased copy
    // and lowercasing is not always length-preserving, so a raw index could land
    // off a char boundary and panic. `.get()` yields None there instead.
    if let Some(pos) = lower.find(" from ") {
        if let Some(rest) = q.get(pos + 6..) {
            if let Some(tp) = rest.to_lowercase().find(" to ") {
                if let (Some(from_s), Some(to_s)) = (rest.get(..tp), rest.get(tp + 4..)) {
                    let from = from_s.trim().to_string();
                    let to = to_s
                        .split(|c: char| c == ' ' || c == '?' || c == '.')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !from.is_empty() && !to.is_empty() {
                        ql.change = Some((from, to));
                    }
                }
            }
        }
    }
    // Arrow forms "X -> Y" / "X → Y". Compute arrow byte length so the RHS slice
    // starts on a char boundary ("→" is 3 bytes, "->" is 2).
    let arrow = ["->", "→", "-->"]
        .iter()
        .filter_map(|a| q.find(a).map(|i| (i, a.len())))
        .min_by_key(|(i, _)| *i);
    if let Some((ar, alen)) = arrow {
        let from = q[..ar].split_whitespace().last().unwrap_or("").to_string();
        let to = q[ar + alen..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_end_matches('?')
            .to_string();
        if !from.is_empty() && !to.is_empty() && ql.change.is_none() {
            ql.change = Some((from, to));
        }
    }
    for scope in [
        "import", "export", "upload", "download", "login", "logout", "print", "report", "sync",
        "offline", "mobile", "api",
    ] {
        if lower.contains(scope) {
            ql.scopes.push(scope.to_string());
        }
    }
    ql
}

fn primary_answer_type(top: Option<Intent>) -> AnswerType {
    match top {
        Some(Intent::Impact) => AnswerType::ImpactSet,
        Some(Intent::Usage) => AnswerType::UsageSites,
        Some(Intent::History) => AnswerType::Timeline,
        Some(Intent::Rationale) => AnswerType::Rationale,
        Some(Intent::Feature) => AnswerType::Plan,
        Some(Intent::BugDiagnosis) => AnswerType::RootCause,
        Some(Intent::Requirements) => AnswerType::RequirementRef,
        Some(Intent::Compare) => AnswerType::Comparison,
        Some(Intent::Test) => AnswerType::TestGuidance,
        Some(Intent::Unknowns) => AnswerType::CoverageGaps,
        _ => AnswerType::Explanation,
    }
}

fn needed_evidence_for(intents: &[(Intent, f32)]) -> Vec<EvidenceKind> {
    use EvidenceKind::*;
    let mut set: Vec<EvidenceKind> = Vec::new();
    let want = |k: EvidenceKind, s: &mut Vec<EvidenceKind>| {
        if !s.contains(&k) {
            s.push(k);
        }
    };
    for (it, _) in intents {
        match it {
            Intent::Explain => {
                for k in [DocSection, SourceCode, GraphRelation, BusinessRule] {
                    want(k, &mut set)
                }
            }
            Intent::Impact => {
                for k in [
                    GraphRelation,
                    SourceCode,
                    TestRef,
                    Setting,
                    BusinessRule,
                    HistoryCommit,
                ] {
                    want(k, &mut set)
                }
            }
            Intent::Usage => {
                for k in [GraphRelation, SourceCode, ConceptGroup] {
                    want(k, &mut set)
                }
            }
            Intent::History => {
                for k in [HistoryCommit, SourceCode] {
                    want(k, &mut set)
                }
            }
            Intent::Rationale => {
                for k in [MemoryNote, HistoryCommit, SourceCode, DocSection] {
                    want(k, &mut set)
                }
            }
            Intent::Feature => {
                for k in [MemoryNote, ConceptGroup, SourceCode, TestRef] {
                    want(k, &mut set)
                }
            }
            Intent::BugDiagnosis => {
                for k in [SourceCode, GraphRelation, BusinessRule, HistoryCommit] {
                    want(k, &mut set)
                }
            }
            Intent::Requirements => {
                for k in [MemoryNote, DocSection] {
                    want(k, &mut set)
                }
            }
            Intent::Compare => {
                for k in [SourceCode, HistoryCommit, DocSection] {
                    want(k, &mut set)
                }
            }
            Intent::Test => {
                for k in [TestRef, SourceCode] {
                    want(k, &mut set)
                }
            }
            Intent::Unknowns => {
                for k in [MemoryNote, DocSection, SourceCode] {
                    want(k, &mut set)
                }
            }
        }
    }
    set
}
