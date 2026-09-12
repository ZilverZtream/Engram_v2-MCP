//! Generic controls for correlated retrieval chunks in the supporting lane.
use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};
use engram_server::services::ask_engine::ranking::{
    rank_and_select_with_terms, rank_and_select_with_terms_exempt, reserve_required_with,
};

fn item(id: &str, path: &str, line: u32, provider: &str, relevance: f32) -> EvidenceItem {
    EvidenceItem {
        evidence_id: id.into(),
        document_id: None,
        document_namespace: None,
        source_verification: None,
        kind: EvidenceKind::SourceCode,
        authority: Authority::CurrentCode,
        path: Some(path.into()),
        lines: Some((line, line + 3)),
        symbol_id: None,
        title: None,
        content: "catalog entry".into(),
        generation: None,
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance,
        extraction_method: "fts".into(),
        warnings: vec![],
        provider: provider.into(),
        score: None,
        directness: None,
    }
}

#[test]
fn same_provider_chunks_cannot_outvote_a_more_relevant_peer() {
    let peer = item("precise", "CatalogService.cs", 1, "code", 0.8);
    let mut items = vec![peer];
    for i in 0..4 {
        items.push(item(
            &format!("context{i}"),
            "CatalogSettings.cs",
            10 + i * 20,
            "code",
            0.3,
        ));
    }
    let ranked = rank_and_select_with_terms(items, 1, &["catalog".into()]);
    assert_eq!(ranked[0].evidence_id, "precise");
}

#[test]
fn already_selected_required_kind_survives_later_reservations_and_trims() {
    use engram_server::services::ask_engine::ranking::{
        retain_entity_anchored, retain_one_per_path,
    };
    let mut concept = item("concept", "Caller.vb", 2, "concept", 0.9);
    concept.kind = EvidenceKind::ConceptGroup;
    let mut business = item("business", "Caller.vb", 2, "business_logic", 0.1);
    business.kind = EvidenceKind::BusinessRule;
    let code = item("code", "Caller.vb", 1, "code", 0.8);
    let mut graph = item("graph", "Caller.vb", 2, "definition", 0.7);
    graph.kind = EvidenceKind::GraphRelation;
    let raw = vec![concept.clone(), business.clone(), code, graph];
    let mut chosen = vec![concept, business];
    let protected = reserve_required_with(
        &mut chosen,
        &raw,
        &[
            EvidenceKind::SourceCode,
            EvidenceKind::GraphRelation,
            EvidenceKind::BusinessRule,
        ],
        &[],
        &[],
        "Caller.ReadItem",
        false,
    );
    assert!(
        chosen.iter().any(|e| e.kind == EvidenceKind::BusinessRule),
        "{chosen:?}"
    );
    assert!(protected.contains("business"));
    assert!(
        !protected.contains("concept"),
        "Do not protect unrelated selected evidence"
    );
    retain_entity_anchored(&mut chosen, &["Caller.ReadItem".into()], &protected);
    retain_one_per_path(&mut chosen, &protected);
    for kind in [
        EvidenceKind::SourceCode,
        EvidenceKind::GraphRelation,
        EvidenceKind::BusinessRule,
    ] {
        assert!(chosen.iter().any(|e| e.kind == kind));
    }
    assert_eq!(
        chosen.len(),
        3,
        "Required representatives may exceed initial2 slots; do not add unrelated evidence"
    );
}

#[test]
fn reservations_protect_only_selected_representatives_and_requested_modality_slots() {
    use engram_server::services::ask_engine::plan::Modality;
    let mut low = item("low", "Rules.vb", 1, "business_logic", 0.1);
    low.kind = EvidenceKind::BusinessRule;
    let mut high = low.clone();
    high.evidence_id = "high".into();
    high.relevance = 0.2;
    let code = item("code", "Caller.vb", 1, "code", 0.8);
    let mut selected = vec![low.clone(), high.clone()];
    let raw = vec![low, high, code];
    let protected = reserve_required_with(
        &mut selected,
        &raw,
        &[EvidenceKind::BusinessRule, EvidenceKind::SourceCode],
        &[],
        &[],
        "",
        false,
    );
    assert_eq!(protected.len(), 2);
    assert!(protected.contains("high") && !protected.contains("low"));
    assert_eq!(selected.len(), 2);
    assert_eq!(
        selected
            .iter()
            .find(|e| e.evidence_id == "high")
            .unwrap()
            .relevance,
        0.2,
        "No relevance boost"
    );

    let raw: Vec<_> = (0..4)
        .map(|i| {
            item(
                &format!("report{i}"),
                &format!("Report{i}.rdl"),
                1,
                "doc",
                1.0 - i as f32 / 10.0,
            )
        })
        .collect();
    let mut selected = vec![raw[0].clone()];
    let protected = reserve_required_with(
        &mut selected,
        &raw,
        &[],
        &[Modality::Report],
        &[],
        "",
        false,
    );
    assert_eq!(selected.len(), 3);
    assert_eq!(protected.len(), 3);
    assert!(protected.contains("report0") && !protected.contains("report3"));

    let mut definition = item("definition", "Entry.vb", 1, "definition", 0.1);
    definition.kind = EvidenceKind::GraphRelation;
    let unrelated = item("unrelated", "Other.vb", 1, "concept", 0.9);
    let mut needed = item("business", "Rules.vb", 1, "business_logic", 0.8);
    needed.kind = EvidenceKind::BusinessRule;
    let raw = vec![definition.clone(), unrelated.clone(), needed];
    let mut selected = vec![definition, unrelated];
    let protected = reserve_required_with(
        &mut selected,
        &raw,
        &[EvidenceKind::BusinessRule],
        &[],
        &["Entry.vb".into()],
        "",
        true,
    );
    assert_eq!(
        protected.len(),
        2,
        "One definition satisfies both named-file and definition obligations"
    );
    assert!(protected.contains("definition") && !protected.contains("unrelated"));
}

#[test]
fn more_chunks_from_one_provider_leave_existing_score_unchanged() {
    let one = item("a_first", "Large.cs", 10, "code", 0.5);
    let single = rank_and_select_with_terms(vec![one.clone()], 10, &[]);
    let mut many = vec![one];
    for i in 1..5 {
        many.push(item(
            &format!("chunk{i}"),
            "Large.cs",
            10 + i * 20,
            "code",
            0.5,
        ));
    }
    let ranked = rank_and_select_with_terms(many, 10, &[]);
    let original = ranked.iter().find(|e| e.evidence_id == "a_first").unwrap();
    assert_eq!(original.score, single[0].score);
}

#[test]
fn provider_diversity_retains_its_bounded_bonus() {
    let one = item("a_first", "Shared.cs", 10, "code", 0.5);
    let solo = rank_and_select_with_terms(vec![one.clone()], 10, &[])[0]
        .score
        .unwrap();
    let ranked = rank_and_select_with_terms(
        vec![one, item("second", "Shared.cs", 30, "definition", 0.5)],
        10,
        &[],
    );
    let score = ranked
        .iter()
        .find(|e| e.evidence_id == "a_first")
        .unwrap()
        .score
        .unwrap();
    assert!((score - solo - 0.1 / 3.0).abs() < 0.00001);
}

#[test]
fn direct_calls_and_exempt_members_keep_their_priority() {
    let mut call = item("call", "Caller.cs", 10, "usage", 0.5);
    call.kind = EvidenceKind::GraphRelation;
    call.directness = Some(0.85);
    call.content = "Wrapper calls the target".into();
    let mut source = item("source", "Impl.cs", 10, "code", 0.5);
    source.content = "catalog entry create rename uniqueness".into();
    let mut member = item("member", "Member.cs", 10, "callee_set", 0.0);
    member.directness = Some(0.1);
    let ranked = rank_and_select_with_terms_exempt(
        vec![source, member, call],
        1,
        &["catalog".into()],
        Some("callee_set"),
    );
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].evidence_id, "call");
    assert!(ranked.iter().any(|e| e.evidence_id == "member"));
}

#[test]
fn tied_scores_remain_deterministic_and_definition_reserve_survives() {
    let a = item("a", "A.cs", 10, "code", 0.5);
    let b = item("b", "B.cs", 10, "code", 0.5);
    for pool in [vec![a.clone(), b.clone()], vec![b, a]] {
        assert_eq!(rank_and_select_with_terms(pool, 1, &[])[0].evidence_id, "a");
    }
    let def = item("definition", "Definition.cs", 10, "definition", 0.0);
    let mut selected = vec![item("context", "Context.cs", 10, "code", 1.0)];
    let protected = reserve_required_with(&mut selected, &[def], &[], &[], &[], "", true);
    assert_eq!(selected[0].evidence_id, "definition");
    assert!(protected.contains("definition"));
}

#[test]
fn adjacent_identified_definition_and_caller_are_distinct_evidence() {
    let mut definition = item("definition", "Service.cs", 6, "definition", 0.9);
    definition.symbol_id = Some("sym:Service.Create".into());
    let mut caller = item("caller", "Service.cs", 2, "usage", 0.5);
    caller.symbol_id = Some("sym:Service.Wrapper".into());
    caller.kind = EvidenceKind::GraphRelation;
    caller.directness = Some(0.85);
    let ranked = rank_and_select_with_terms(vec![definition, caller], 10, &[]);
    assert_eq!(ranked.len(), 2);
    assert!(ranked.iter().any(|e| e.evidence_id == "caller"));
    assert!(ranked.iter().any(|e| e.evidence_id == "definition"));
}

#[test]
fn explicit_identity_does_not_relax_per_file_cap_or_exact_dedup() {
    let mut items = Vec::new();
    for i in 0..3 {
        let mut e = item(&format!("symbol{i}"), "Service.cs", 1 + i, "code", 0.5);
        e.symbol_id = Some(format!("sym:Service.Method{i}"));
        items.push(e);
    }
    let mut duplicate = items[0].clone();
    duplicate.evidence_id = "duplicate".into();
    duplicate.lines = Some((100, 105));
    duplicate.relevance = 0.2;
    items.push(duplicate);
    let ranked = rank_and_select_with_terms(items, 10, &[]);
    assert_eq!(ranked.len(), 2, "at most two identified symbols per file");
    assert!(!ranked.iter().any(|e| e.evidence_id == "duplicate"));
}

#[test]
fn unidentified_nearby_chunks_still_collapse_including_mixed_pairs() {
    for identify_first in [false, true] {
        let mut first = item("first", "Service.cs", 2, "code", 0.9);
        if identify_first {
            first.symbol_id = Some("sym:Service.Method".into());
        }
        let second = item("second", "Service.cs", 6, "code", 0.5);
        let ranked = rank_and_select_with_terms(vec![first, second], 10, &[]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].evidence_id, "first");
    }
}
