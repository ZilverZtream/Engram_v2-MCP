#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6, slice 2 — release-20 golden run: four
//! real questions turned `unsupported` because the named-term gate only
//! looked at evidence TEXT. "What breaks if I change the signature of
//! Check_pr_id?" resolved `Check_pr_id` → `_us.accessctrl.Check_pr_id` and
//! got 50 impact relations whose content says "X calls the target" — the
//! target's name is in the plan, not in the prose. A term the index RESOLVED
//! is covered by definition; and a resolved entity found in the evidence
//! anchors support even when the question's other words are absent.

use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};
use engram_server::services::ask_engine::plan::{EntityKind, EntityMention, ResolvedEntity};
use engram_server::services::ask_engine::planner::plan_query;
use engram_server::services::ask_engine::status::{
    AnswerStatus, FreshnessSnapshot, ProviderReport, ProviderStatus, assess_status,
    has_adequate_support_with, uncovered_named_terms_with,
};

fn ev(
    id: &str,
    kind: EvidenceKind,
    provider: &str,
    path: &str,
    symbol_id: Option<&str>,
    content: &str,
) -> EvidenceItem {
    EvidenceItem {
        evidence_id: id.into(),
        kind,
        authority: Authority::CurrentCode,
        path: Some(path.into()),
        lines: Some((1, 5)),
        symbol_id: symbol_id.map(|s| s.to_string()),
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
    }
}

#[test]
fn a_resolved_entity_is_a_covered_term_even_when_the_evidence_prose_never_spells_it() {
    let q = "What breaks if I change the signature of Check_pr_id?";
    let evidence = vec![ev(
        "ev_1",
        EvidenceKind::GraphRelation,
        "impact",
        "Site/App_Code/api-v2/Services/permit/PermitService.vb",
        Some(
            "sym:function:Site/App_Code/api-v2/Services/permit/PermitService.vb:_api2.svc.PermitService.Create:34",
        ),
        "_api2.svc.PermitService.Create calls the target (weight 1)",
    )];
    let known = vec![
        "_us.accessctrl.Check_pr_id".to_string(),
        "Check_pr_id".to_string(),
    ];
    assert_eq!(
        uncovered_named_terms_with(q, &evidence, &[]),
        vec!["Check_pr_id".to_string()],
        "text alone does not cover it"
    );
    assert!(
        uncovered_named_terms_with(q, &evidence, &known).is_empty(),
        "the resolved entity covers it"
    );
    assert!(has_adequate_support_with(q, &evidence, &known));
    let mut plan = plan_query(q);
    if plan.entities.is_empty() {
        plan.entities.push(EntityMention {
            text: "Check_pr_id".into(),
            guessed_kind: EntityKind::Symbol,
            resolved: vec![],
        });
    }
    for e in plan.entities.iter_mut() {
        e.resolved = vec![ResolvedEntity {
            kind: EntityKind::Symbol,
            canonical: "_us.accessctrl.Check_pr_id".into(),
            node_id: Some("sym:_us.accessctrl.Check_pr_id".into()),
            confidence: 0.9,
        }];
    }
    let s = assess_status(
        &plan,
        &evidence,
        &[rep("impact"), rep("code")],
        &FreshnessSnapshot::default(),
        true,
    );
    assert!(
        matches!(s, AnswerStatus::Answered | AnswerStatus::Partial),
        "{s:?}"
    );
}

#[test]
fn a_resolved_entity_found_in_the_evidence_anchors_support() {
    // "Which reports (.rdl) read the rk_redovisningskategorier table?" — the
    // entity resolved (symbol + table, one canonical) and the code hits carry
    // it; the question's other words ("reports") are absent. That is found
    // evidence about the asked entity: partial/answered, not unsupported.
    let q = "Which reports (.rdl) read the rk_redovisningskategorier table?";
    let evidence = vec![ev(
        "ev_1",
        EvidenceKind::SourceCode,
        "code",
        "Site/App_Code/iFalt.designer.vb",
        None,
        "Private _rk_redovisningskategorier As EntitySet(Of rk_redovisningskategorier)\n Me.SendPropertyChanged(\"lg_lag\")",
    )];
    let known = vec!["rk_redovisningskategorier".to_string()];
    assert!(uncovered_named_terms_with(q, &evidence, &known).is_empty());
    assert!(
        has_adequate_support_with(q, &evidence, &known),
        "the asked entity was found"
    );
    assert!(
        !has_adequate_support_with("which reports read the table", &evidence, &[]),
        "without a resolved anchor the two-term rule still applies"
    );
}

#[test]
fn a_false_premise_stays_unsupported_with_known_terms_present() {
    let q = "Which Redis cluster caches the redovisningskategori list?";
    let evidence = vec![ev(
        "ev_1",
        EvidenceKind::GraphRelation,
        "usage",
        "Site/App_Code/redovisning/code/redovisningskategorier.vb",
        Some("sym:redovisningskategorier.GetByProjectId"),
        "redovisningskategorier.GetByProjectId <- api-redovisning.GetCategories",
    )];
    let known = vec!["redovisningskategorier".to_string()];
    assert_eq!(
        uncovered_named_terms_with(q, &evidence, &known),
        vec!["Redis".to_string()]
    );
    assert!(!has_adequate_support_with(q, &evidence, &known));
}
