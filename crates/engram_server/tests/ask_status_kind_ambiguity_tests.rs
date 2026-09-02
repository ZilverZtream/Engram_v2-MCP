#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6 (owner: keep looping) — golden miss
//! `ox_exact_5`: "Where is CheckIfAdminOrArbetsledare defined?" came back
//! AMBIGUOUS because the name resolved to the function AND to a Session
//! setting key that merely reuses the function's name. A setting is not a
//! competing definition of a symbol: ambiguity is two candidates of the
//! SAME kind with different canonical names.

use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};
use engram_server::services::ask_engine::plan::{EntityKind, EntityMention, ResolvedEntity};
use engram_server::services::ask_engine::planner::plan_query;
use engram_server::services::ask_engine::status::{
    AnswerStatus, FreshnessSnapshot, ProviderReport, ProviderStatus, assess_status,
};

fn ev(content: &str) -> EvidenceItem {
    EvidenceItem {
        evidence_id: "ev_1".into(),
        kind: EvidenceKind::SourceCode,
        authority: Authority::CurrentCode,
        path: Some("Site/App_Code/users-security/code/aspnetUsers.vb".into()),
        lines: Some((120, 140)),
        symbol_id: Some("sym:function:Site/App_Code/users-security/code/aspnetUsers.vb:_us.aspnetUsers.CheckIfAdminOrArbetsledare:120".into()),
        title: None,
        content: content.into(),
        generation: Some(1),
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: 0.8,
        extraction_method: "graph".into(),
        warnings: vec![],
        provider: "code".into(),
        score: None,
        directness: None,
    }
}

fn re(kind: EntityKind, canonical: &str) -> ResolvedEntity {
    ResolvedEntity {
        kind,
        canonical: canonical.into(),
        node_id: Some(format!("id:{canonical}")),
        confidence: 0.8,
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

#[test]
fn a_setting_that_reuses_a_symbols_name_is_not_a_competing_definition() {
    let q = "Where is CheckIfAdminOrArbetsledare defined?";
    let mut plan = plan_query(q);
    if plan.entities.is_empty() {
        plan.entities.push(EntityMention {
            text: "CheckIfAdminOrArbetsledare".into(),
            guessed_kind: EntityKind::Symbol,
            resolved: vec![],
        });
    }
    for e in plan.entities.iter_mut() {
        e.resolved = vec![
            re(
                EntityKind::Setting,
                "Session:aspnetUser.CheckIfAdminOrArbetsledare",
            ),
            re(
                EntityKind::Symbol,
                "_us.aspnetUsers.CheckIfAdminOrArbetsledare",
            ),
        ];
    }
    let evidence = vec![ev(
        "Public Function CheckIfAdminOrArbetsledare(pr_id As Integer) As Boolean",
    )];
    let s = assess_status(
        &plan,
        &evidence,
        &[rep("code"), rep("usage")],
        &FreshnessSnapshot::default(),
        true,
    );
    assert!(
        matches!(s, AnswerStatus::Answered | AnswerStatus::Partial),
        "different kinds are one thing, got {s:?}"
    );
}

#[test]
fn two_symbols_with_different_names_stay_ambiguous() {
    let q = "Where is GetByID defined?";
    let mut plan = plan_query(q);
    if plan.entities.is_empty() {
        plan.entities.push(EntityMention {
            text: "GetByID".into(),
            guessed_kind: EntityKind::Symbol,
            resolved: vec![],
        });
    }
    for e in plan.entities.iter_mut() {
        e.resolved = vec![
            re(EntityKind::Symbol, "_ata.atalista.GetByID"),
            re(EntityKind::Symbol, "_ata.huvud.GetByID"),
        ];
    }
    let evidence = vec![ev("Public Function GetByID(id As Integer) As Object")];
    let s = assess_status(
        &plan,
        &evidence,
        &[rep("code")],
        &FreshnessSnapshot::default(),
        true,
    );
    assert_eq!(s, AnswerStatus::Ambiguous);
}
