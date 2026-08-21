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
