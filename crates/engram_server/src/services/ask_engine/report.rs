//! The assembled result. In M1 `mode` is always "retrieval_only": a typed,
//! ranked, honestly-statused evidence report — never concatenated prose passed
//! off as a synthesized answer. Render fns are added in Task 10.

use std::fmt::Write as _;

use serde::Serialize;

use super::evidence::{Authority, EvidenceItem, EvidenceKind};
use super::plan::QueryPlan;
use super::status::{AnswerStatus, Conflict, FreshnessSnapshot, ProviderReport, ProviderStatus};

#[derive(Debug, Clone, Serialize)]
pub struct AskReport {
    pub question: String,
    pub plan: QueryPlan,
    pub status: AnswerStatus,
    pub mode: String,                // "retrieval_only" (M1) | "synthesized" (M2)
    pub evidence: Vec<EvidenceItem>, // ranked, deduped, bounded (high-signal)
    pub conflicts: Vec<Conflict>,
    pub unknowns: Vec<String>,  // coverage gaps
    pub next_best: Vec<String>, // suggested follow-up investigations
    pub snapshot: FreshnessSnapshot,
    pub providers: Vec<ProviderReport>,
    /// Round-4 P0-2: typed identities that ARE the answer for relation
    /// questions (empty for prose-shaped questions).
    pub answer_members: Vec<super::providers::AnswerMember>,
}

pub fn to_json(r: &AskReport) -> serde_json::Value {
    serde_json::to_value(r).unwrap_or(serde_json::Value::Null)
}

/// needed-evidence kinds that produced nothing + arms that failed/timed out.
pub fn coverage_gaps(
    plan: &QueryPlan,
    evidence: &[EvidenceItem],
    providers: &[ProviderReport],
) -> Vec<String> {
    let mut gaps = Vec::new();
    for k in &plan.needed_evidence {
        if !evidence.iter().any(|e| e.kind == *k) {
            gaps.push(format!("no {} evidence found", kind_label(*k)));
        }
    }
    // Round-2 audit P0-4: name the requested modality that no evidence covers.
    for m in &plan.modalities {
        if !evidence
            .iter()
            .any(|e| e.path.as_deref().is_some_and(|p| m.matches(p)))
        {
            gaps.push(format!(
                "no {} evidence although the question asks for it (looked for {})",
                m.label(),
                m.suffixes().join(", ")
            ));
        }
    }
    for p in providers {
        match p.status {
            ProviderStatus::Failed => gaps.push(format!(
                "the {} arm failed{}",
                p.provider,
                p.note
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            )),
            ProviderStatus::TimedOut => gaps.push(format!("the {} arm timed out", p.provider)),
            _ => {}
        }
    }
    gaps
}

/// Concrete follow-up investigations — what a good agent would do next.
pub fn next_best(plan: &QueryPlan, evidence: &[EvidenceItem], status: AnswerStatus) -> Vec<String> {
    let mut out = Vec::new();
    for e in &plan.entities {
        if e.resolved.len() > 1 {
            out.push(format!(
                "resolve_id(\"{}\") — ambiguous across {} candidates",
                e.text,
                e.resolved.len()
            ));
        }
    }
    match status {
        // An index miss is NOT proof the code is absent: the index can be behind
        // the working tree. Tell the agent to grep before it concludes — a stale
        // or empty lookup that makes an agent stop searching is worse than no
        // index at all (reserve "cannot determine" for questions the source
        // genuinely can't answer, like whether a value is user-controlled).
        AnswerStatus::Unsupported => out.push(
            "the index matched nothing — this may be an index miss, not genuine absence. \
             grep_project (or read the files directly in the working tree) to confirm \
             BEFORE concluding the code doesn't exist; do not answer \"cannot determine\" \
             from an index miss. Also try search_memory with different terms."
                .into(),
        ),
        AnswerStatus::Stale => out.push(
            "the index is BEHIND the working tree — a stale lookup is a failed lookup, not an \
             answer. grep_project / read the working tree for this query now, and \
             update_project to refresh the index."
                .into(),
        ),
        AnswerStatus::Failed => out.push(
            "a retrieval arm errored — grep_project as a fallback, and check \
             get_index_freshness / project_health."
                .into(),
        ),
        _ => {}
    }
    if let Some(top) = evidence.first() {
        if let Some(p) = &top.path {
            out.push(format!(
                "get_chunk / get_full_method_body on {p} for the full text"
            ));
        }
    }
    out
}

fn kind_label(k: EvidenceKind) -> &'static str {
    match k {
        EvidenceKind::SourceCode => "source-code",
        EvidenceKind::DocSection => "documentation",
        EvidenceKind::MemoryNote => "memory-note",
        EvidenceKind::Insight => "insight",
        EvidenceKind::BusinessRule => "business-rule",
        EvidenceKind::HistoryCommit => "history",
        EvidenceKind::GraphRelation => "graph-relation",
        EvidenceKind::ConceptGroup => "concept",
        EvidenceKind::TestRef => "test",
        EvidenceKind::Setting => "setting",
    }
}

fn authority_label(a: Authority) -> &'static str {
    match a {
        Authority::RuntimeEvidence => "runtime",
        Authority::CurrentCode => "current_code",
        Authority::ApprovedRequirement => "requirement",
        Authority::CurrentDocs => "docs",
        Authority::MergedHistory => "history",
        Authority::DerivedBusinessLogic => "business_logic",
        Authority::AgentMemory => "memory",
        Authority::DreamerInsight => "insight",
        Authority::SemanticSimilarity => "similar",
    }
}

fn status_reason(r: &AskReport) -> &'static str {
    match r.status {
        AnswerStatus::Answered => "direct, adequately-authoritative evidence found",
        AnswerStatus::Partial => "some evidence, but coverage gaps remain",
        AnswerStatus::Ambiguous => "an entity resolves to multiple candidates — disambiguate first",
        AnswerStatus::Stale => "evidence is behind the current snapshot",
        AnswerStatus::Unsupported => {
            "no evidence of adequate authority was found — not asserting an answer"
        }
        AnswerStatus::Failed => "retrieval arms errored; results are incomplete",
    }
}

/// Human-facing report: leads with the answer's epistemic state, then the ranked
/// KEY evidence with citations. Labelled retrieval_only — never concatenation.
pub fn render_markdown(r: &AskReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# ask_codebase — {} report", r.mode);
    let _ = writeln!(s, "\n**question:** {}", r.question);

    let intents: Vec<String> = r
        .plan
        .intents
        .iter()
        .map(|(i, w)| format!("{i:?}({w:.2})"))
        .collect();
    let ents: Vec<String> = r
        .plan
        .entities
        .iter()
        .map(|e| {
            if e.resolved.is_empty() {
                format!("{}(?)", e.text)
            } else {
                format!("{}→{}", e.text, e.resolved.len())
            }
        })
        .collect();
    let _ = writeln!(s, "**understanding:** intents [{}]", intents.join(", "));
    if !ents.is_empty() {
        let _ = writeln!(s, "**entities:** {}", ents.join(", "));
    }
    let _ = writeln!(s, "**status:** {:?} — {}", r.status, status_reason(r));

    let mut snap = format!(
        "generation {:?} · tier {}",
        r.snapshot.project_generation, r.snapshot.semantic_tier
    );
    if let Some(c) = &r.snapshot.git_commit {
        let short: String = c.chars().take(10).collect();
        let _ = write!(snap, " · commit {short}");
    }
    if r.snapshot.reindex_required {
        snap.push_str(" · REINDEX-REQUIRED");
    }
    if r.snapshot.incompatible {
        snap.push_str(" · SNAPSHOT-MISMATCH");
    }
    let _ = writeln!(s, "**snapshot:** {snap}");

    if !r.answer_members.is_empty() {
        let complete = r
            .providers
            .iter()
            .filter_map(|p| p.proof.as_ref())
            .all(|p| p.complete());
        let _ = writeln!(
            s,
            "\n## Answer members ({}, coverage {})",
            r.answer_members.len(),
            if complete { "complete" } else { "INCOMPLETE" }
        );
        for m in &r.answer_members {
            let _ = writeln!(
                s,
                "- {} [{}]{}",
                m.display_name,
                m.relation,
                m.path
                    .as_deref()
                    .map(|p| format!(" — {p}"))
                    .unwrap_or_default()
            );
        }
    }
    let _ = writeln!(s, "\n## Key evidence");
    if r.evidence.is_empty() {
        let _ = writeln!(s, "_(none of adequate authority)_");
    }
    for e in &r.evidence {
        let loc = match (&e.path, e.lines) {
            (Some(p), Some((a, b))) => format!("{p}:{a}-{b}"),
            (Some(p), None) => p.clone(),
            _ => e.title.clone().unwrap_or_else(|| e.evidence_id.clone()),
        };
        let _ = writeln!(
            s,
            "- **[{}]** {} _(via {}, score {:.2})_",
            authority_label(e.authority),
            loc,
            e.provider,
            e.score.unwrap_or(0.0)
        );
        let snippet: String = e.content.chars().take(200).collect();
        let snippet = snippet.replace('\n', " ");
        if !snippet.trim().is_empty() {
            let _ = writeln!(s, "  {}", snippet.trim());
        }
    }

    if !r.conflicts.is_empty() {
        let _ = writeln!(s, "\n## Conflicts (shown, not resolved)");
        for c in &r.conflicts {
            let _ = writeln!(s, "- {} [{} vs {}]", c.summary, c.left, c.right);
        }
    }
    if !r.unknowns.is_empty() {
        let _ = writeln!(s, "\n## Unknowns / coverage gaps");
        for u in &r.unknowns {
            let _ = writeln!(s, "- {u}");
        }
    }
    if !r.next_best.is_empty() {
        let _ = writeln!(s, "\n## Next best investigation");
        for n in &r.next_best {
            let _ = writeln!(s, "- {n}");
        }
    }
    let provs: Vec<String> = r
        .providers
        .iter()
        .map(|p| format!("{}({:?},{})", p.provider, p.status, p.count))
        .collect();
    let _ = writeln!(s, "\n_providers: {}_", provs.join(", "));
    s
}
