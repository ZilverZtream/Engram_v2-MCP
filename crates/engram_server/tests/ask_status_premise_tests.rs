#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6 — the OciusX golden baseline (35 questions)
//! exposed two status-engine defects:
//! 1. "Which Redis cluster caches the redovisningskategori list?" was ANSWERED
//!    from the real term (a graph relation on `redovisningskategori`) while the
//!    question's premise — Redis, cluster — has no evidence at all. A named term
//!    of the question that no evidence mentions makes the answer unsupported,
//!    and the report must say which term.
//! 2. Seven real questions came back `ambiguous` because an entity resolved to
//!    the same name under two node kinds (a table and its .sql file). Ambiguity
//!    means genuinely DIFFERENT symbols.

use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};
use engram_server::services::ask_engine::plan::{EntityKind, EntityMention, ResolvedEntity};
use engram_server::services::ask_engine::planner::plan_query;
use engram_server::services::ask_engine::status::{
    AnswerStatus, FreshnessSnapshot, ProviderReport, ProviderStatus, assess_status,
    has_adequate_support, uncovered_named_terms,
};

fn ev(id: &str, kind: EvidenceKind, provider: &str, path: &str, content: &str) -> EvidenceItem {
    EvidenceItem {
        evidence_id: id.into(),
        kind,
        authority: Authority::CurrentCode,
        path: Some(path.into()),
        lines: Some((1, 5)),
        symbol_id: None,
        title: None,
        content: content.into(),
        generation: Some(1),
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: 0.8,
        extraction_method: "graph".into(),
        warnings: vec![],
        provider: provider.into(),
        score: None,
        directness: None,
    }
}

fn rep(p: &str) -> ProviderReport {
    ProviderReport {
        provider: p.into(),
        status: ProviderStatus::Hit,
        count: 1,
        note: None,
        examined: 0,
        available: None,
        truncated: false,
        proof: None,
    }
}

fn re(kind: EntityKind, canonical: &str, node_id: &str) -> ResolvedEntity {
    ResolvedEntity {
        kind,
        canonical: canonical.into(),
        node_id: Some(node_id.into()),
        confidence: 0.8,
    }
}

#[test]
fn a_named_term_without_any_evidence_makes_the_answer_unsupported() {
    let q = "Which Redis cluster caches the redovisningskategori list?";
    // What the live engine had: a resolved-entity graph relation on the real
    // term plus source code mentioning it — and nothing about Redis.
    let evidence = vec![
        ev(
            "ev_1",
            EvidenceKind::GraphRelation,
            "usage",
            "Site/App_Code/redovisning/code/redovisningskategorier.vb",
            "redovisningskategorier.GetByProjectId <- api-redovisning.GetCategories",
        ),
        ev(
            "ev_2",
            EvidenceKind::SourceCode,
            "code",
            "Site/App_Code/redovisning/code/redovisningskategorier.vb",
            "Public Class redovisningskategorier ... list of categories",
        ),
    ];
    assert_eq!(
        uncovered_named_terms(q, &evidence),
        vec!["Redis".to_string()],
        "the premise term nobody has evidence for is named"
    );
    assert!(
        !has_adequate_support(q, &evidence),
        "a graph relation on one term does not support a question whose named premise is absent"
    );
    let plan = plan_query(q);
    let s = assess_status(
        &plan,
        &evidence,
        &[rep("usage"), rep("code")],
        &FreshnessSnapshot::default(),
        has_adequate_support(q, &evidence),
    );
    assert_eq!(s, AnswerStatus::Unsupported);
}

#[test]
fn named_terms_present_in_the_evidence_keep_the_answer() {
    let q = "Where is Check_pr_id defined and who calls it?";
    let evidence = vec![ev(
        "ev_1",
        EvidenceKind::GraphRelation,
        "usage",
        "Site/App_Code/us/accessctrl.vb",
        "accessctrl.Check_pr_id <- api.GetAll (calls)",
    )];
    assert!(
        uncovered_named_terms(q, &evidence).is_empty(),
        "{:?}",
        uncovered_named_terms(q, &evidence)
    );
    assert!(has_adequate_support(q, &evidence));
    let plan = plan_query(q);
    let s = assess_status(
        &plan,
        &evidence,
        &[rep("usage")],
        &FreshnessSnapshot::default(),
        true,
    );
    assert!(
        matches!(s, AnswerStatus::Answered | AnswerStatus::Partial),
        "a fully covered question is answered/partial, got {s:?}"
    );
}

#[test]
fn the_same_symbol_under_two_node_kinds_is_not_ambiguous() {
    let q = "What depends on the rk_redovisningskategorier table?";
    let mut plan = plan_query(q);
    if plan.entities.is_empty() {
        plan.entities.push(EntityMention {
            text: "rk_redovisningskategorier".into(),
            guessed_kind: EntityKind::Table,
            resolved: vec![],
        });
    }
    for e in plan.entities.iter_mut() {
        e.resolved = vec![
            re(
                EntityKind::Table,
                "rk_redovisningskategorier",
                "table:rk_redovisningskategorier",
            ),
            re(
                EntityKind::File,
                "rk_redovisningskategorier",
                "file:db-x.sql/dbo/Tables/rk_redovisningskategorier.sql",
            ),
        ];
    }
    let evidence = vec![ev(
        "ev_1",
        EvidenceKind::GraphRelation,
        "impact",
        "db-x.sql/dbo/Tables/rk_redovisningskategorier.sql",
        "rk_redovisningskategorier <- redovisningskategorier.GetByProjectId (queries_table)",
    )];
    let s = assess_status(
        &plan,
        &evidence,
        &[rep("impact")],
        &FreshnessSnapshot::default(),
        true,
    );
    assert!(
        !matches!(s, AnswerStatus::Ambiguous),
        "one name resolved as a table AND its file is one thing, not two branches: {s:?}"
    );

    // Two DIFFERENT symbols stay ambiguous.
    for e in plan.entities.iter_mut() {
        e.resolved = vec![
            re(EntityKind::Symbol, "projekt.GetByID", "sym:a"),
            re(EntityKind::Symbol, "marker.GetByID", "sym:b"),
        ];
    }
    let s2 = assess_status(
        &plan,
        &evidence,
        &[rep("impact")],
        &FreshnessSnapshot::default(),
        true,
    );
    assert_eq!(s2, AnswerStatus::Ambiguous);
}
