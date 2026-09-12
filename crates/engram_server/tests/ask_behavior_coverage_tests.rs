use engram_server::services::ask_engine::{
    evidence::{Authority, EvidenceItem, EvidenceKind},
    planner::plan_query,
    report::{coverage_gaps, next_best},
    status::{AnswerStatus, FreshnessSnapshot, assess_status},
};

fn evidence() -> Vec<EvidenceItem> {
    let mut source = EvidenceItem {
        evidence_id: "source".into(),
        document_id: None,
        document_namespace: None,
        source_verification: None,
        kind: EvidenceKind::SourceCode,
        authority: Authority::CurrentCode,
        path: Some("Service.cs".into()),
        lines: Some((1, 30)),
        symbol_id: Some("sym:Service".into()),
        title: None,
        content: "// records create add rename retitle shared tenant validation\nvoid Run() {}"
            .into(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: 0.9,
        extraction_method: "definition".into(),
        warnings: vec![],
        provider: "definition".into(),
        score: None,
        directness: None,
    };
    let mut relation = source.clone();
    relation.evidence_id = "usage".into();
    relation.kind = EvidenceKind::GraphRelation;
    relation.provider = "usage".into();
    relation.content = "Wrapper calls Service.Run".into();
    source
        .content
        .push_str("\n// create and rename implemented here in shared and tenant scopes");
    vec![source, relation]
}

fn status(question: &str) -> AnswerStatus {
    assess_status(
        &plan_query(question),
        &evidence(),
        &[],
        &FreshnessSnapshot::default(),
        true,
    )
}

#[test]
fn coordinated_operations_across_scopes_need_verified_coverage() {
    assert_eq!(
        status("Where are records created and renamed across shared and tenant scopes?"),
        AnswerStatus::Partial
    );
}

#[test]
fn paraphrased_coordinated_operations_do_not_gain_completeness_from_keywords() {
    assert_eq!(
        status("Where can records be added and retitled across shared and tenant scopes?"),
        AnswerStatus::Partial
    );
}

#[test]
fn behavior_gap_is_unverified_not_absent_and_has_actionable_followup() {
    let plan = plan_query("How are records created and renamed within shared and tenant scopes?");
    let gaps = coverage_gaps(&plan, &evidence(), &[]);
    assert!(
        gaps.iter()
            .any(|g| g.contains("unverified") && g.contains("create") && g.contains("rename")),
        "{gaps:?}"
    );
    let next = next_best(&plan, &evidence(), AnswerStatus::Partial);
    assert!(
        next.iter()
            .any(|n| n.contains("get_full_method_body") && n.contains("operation")),
        "{next:?}"
    );
}

#[test]
fn ordinary_explanations_and_compound_identifier_definitions_keep_existing_status() {
    assert_eq!(
        status("How does request validation work in the service?"),
        AnswerStatus::Answered
    );
    assert_eq!(
        status("Where is CreateAndRenameService defined?"),
        AnswerStatus::Answered
    );
    assert_eq!(
        status("Where is `create and rename` defined?"),
        AnswerStatus::Answered
    );
    // Caller completeness is already Partial without an exhaustive caller proof.
    assert_eq!(
        status("Who calls CreateAndRenameService?"),
        AnswerStatus::Partial
    );
}

#[test]
fn prose_operations_are_distinct_and_scopes_are_preserved_as_unresolved_context() {
    use engram_server::services::ask_engine::plan::BehaviorOperation;
    let plan =
        plan_query("Where are widgets added and renamed across regional and account scopes?");
    assert_eq!(
        plan.contract.behavior_requirements,
        vec![BehaviorOperation::Create, BehaviorOperation::Rename]
    );
    assert_eq!(
        plan.contract.behavior_scope_context.as_deref(),
        Some("across regional and account scopes")
    );
    for question in [
        "How do create and add commands work?",
        "How does CreateAndRenameService work?",
        "How does `create and rename` work?",
        "How do I create a record?",
    ] {
        assert!(
            plan_query(question)
                .contract
                .behavior_requirements
                .is_empty(),
            "{question}"
        );
        assert_eq!(status(question), AnswerStatus::Answered, "{question}");
    }
}

#[test]
fn behavioral_contract_does_not_hide_empty_or_stale_evidence() {
    let plan = plan_query("Where are records created and renamed across shared and tenant scopes?");
    assert_eq!(
        assess_status(&plan, &[], &[], &FreshnessSnapshot::default(), true),
        AnswerStatus::Unsupported
    );
    let snapshot = FreshnessSnapshot {
        reindex_required: true,
        ..FreshnessSnapshot::default()
    };
    assert_eq!(
        assess_status(&plan, &evidence(), &[], &snapshot, true),
        AnswerStatus::Stale
    );
}
