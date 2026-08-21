#![allow(clippy::unwrap_used)]
//! ask_engine (ask_codebase brain, Milestone 1) — unit + integration tests.
//! Tasks append their own tests below their section marker.

// ─── Task 1: typed model ─────────────────────────────────────────────────────
use engram_server::services::ask_engine::evidence::{Authority, EvidenceItem, EvidenceKind};

#[test]
fn authority_orders_strongest_first_and_weight_is_monotonic() {
    // Declaration order = Ord order: RuntimeEvidence is the smallest (strongest).
    assert!(Authority::RuntimeEvidence < Authority::CurrentCode);
    assert!(Authority::CurrentCode < Authority::SemanticSimilarity);
    // weight() is the inverse: strongest authority => highest weight.
    assert!(Authority::CurrentCode.weight() > Authority::SemanticSimilarity.weight());
    assert!(Authority::RuntimeEvidence.weight() >= Authority::CurrentCode.weight());
}

#[test]
fn evidence_item_serializes_with_snake_case_kind() {
    let ev = EvidenceItem {
        evidence_id: "ev_1".into(),
        kind: EvidenceKind::SourceCode,
        authority: Authority::CurrentCode,
        path: Some("a.vb".into()),
        lines: Some((10, 20)),
        symbol_id: None,
        title: None,
        content: "x".into(),
        generation: Some(3),
        commit: None,
        timestamp: None,
        confidence: 0.9,
        relevance: 0.8,
        extraction_method: "fts".into(),
        warnings: vec![],
        provider: "code".into(),
        score: None,
        directness: None,
    };
    let j = serde_json::to_value(&ev).unwrap();
    assert_eq!(j["kind"], "source_code");
    assert_eq!(j["authority"], "current_code");
    assert_eq!(j["evidence_id"], "ev_1");
    // skip_serializing_if hides empty warnings + None score.
    assert!(j.get("warnings").is_none());
    assert!(j.get("score").is_none());
}

// ─── Task 2: deterministic multi-intent planner ──────────────────────────────
use engram_server::services::ask_engine::plan::{EntityKind, Intent};
use engram_server::services::ask_engine::planner::{extract_entities, plan_query};

#[test]
fn compound_question_yields_multiple_intents() {
    let p = plan_query("How does authentication work, and what would break if we changed it?");
    let intents: Vec<Intent> = p.intents.iter().map(|(i, _)| *i).collect();
    assert!(intents.contains(&Intent::Explain), "{intents:?}");
    assert!(intents.contains(&Intent::Impact), "{intents:?}");
}

#[test]
fn extracts_file_entity_with_kind() {
    let ents = extract_entities(
        "What breaks if we change marker serialization in ImportService.vb from XML to JSON?",
    );
    assert!(
        ents.iter()
            .any(|e| e.text == "ImportService.vb" && e.guessed_kind == EntityKind::File),
        "{ents:?}"
    );
}

#[test]
fn why_is_rationale_not_history() {
    let p = plan_query("Why is customer status enforced on the server?");
    assert_eq!(p.intents.first().unwrap().0, Intent::Rationale);
}

#[test]
fn bare_topic_defaults_to_explain() {
    let p = plan_query("marker clustering");
    assert_eq!(p.intents.first().unwrap().0, Intent::Explain);
}

#[test]
fn change_verb_qualifier_from_x_to_y() {
    let p = plan_query("change serialization from XML to JSON");
    assert_eq!(p.qualifiers.change, Some(("XML".into(), "JSON".into())));
}

#[test]
fn captures_single_pascalcase_symbol_but_not_question_words() {
    let ents = extract_entities("How does Authenticate work and where is Run used?");
    assert!(ents.iter().any(|e| e.text == "Authenticate"), "{ents:?}");
    assert!(ents.iter().any(|e| e.text == "Run"), "{ents:?}");
    assert!(!ents.iter().any(|e| e.text.eq_ignore_ascii_case("How")), "{ents:?}");
}
