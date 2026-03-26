#![allow(clippy::unwrap_used)]
//! Integrity canary test — synthetic drift injection (Ticket 8).
//!
//! Tests that the integrity checker detects various forms of cross-store
//! drift and reports them correctly. Uses synthetic mismatches injected
//! directly into the `build_integrity_mismatches` function (unit-level)
//! to verify detection precision without requiring full indexing.

use engram_server::services::integrity_service::*;

// Re-verify the mismatch detection logic with canary scenarios.

/// Canary 1: Perfect alignment — no mismatches.
#[test]
fn canary_no_drift_when_stores_aligned() {
    let tantivy_docs = vec![
        search_doc("memory", "doc1", "src/a.cs"),
        search_doc("memory", "doc2", "src/b.cs"),
        search_doc("memory", "doc3", "src/c.cs"),
    ];
    let docstore_docs = vec![
        docstore_doc("memory", "doc1", "src/a.cs"),
        docstore_doc("memory", "doc2", "src/b.cs"),
        docstore_doc("memory", "doc3", "src/c.cs"),
    ];

    let result = build_test_mismatches(3, 3, 3, &tantivy_docs, &docstore_docs);
    assert!(
        result.is_empty(),
        "Aligned stores should have zero mismatches"
    );
}

/// Canary 2: Single Tantivy orphan — detects accurately.
#[test]
fn canary_detects_single_tantivy_orphan() {
    let tantivy_docs = vec![
        search_doc("memory", "doc1", "src/a.cs"),
        search_doc("memory", "doc2", "src/b.cs"),
        search_doc("memory", "orphan1", "src/orphan.cs"),
    ];
    let docstore_docs = vec![
        docstore_doc("memory", "doc1", "src/a.cs"),
        docstore_doc("memory", "doc2", "src/b.cs"),
    ];

    let result = build_test_mismatches(3, 2, 2, &tantivy_docs, &docstore_docs);

    let tantivy_orphan = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::TantivyOrphan));
    assert!(tantivy_orphan.is_some(), "Should detect Tantivy orphan");
    assert_eq!(tantivy_orphan.unwrap().actual, 1);
}

/// Canary 3: Single docstore orphan.
#[test]
fn canary_detects_single_docstore_orphan() {
    let tantivy_docs = vec![search_doc("memory", "doc1", "src/a.cs")];
    let docstore_docs = vec![
        docstore_doc("memory", "doc1", "src/a.cs"),
        docstore_doc("memory", "orphan1", "src/orphan.cs"),
    ];

    let result = build_test_mismatches(1, 2, 1, &tantivy_docs, &docstore_docs);

    let docstore_orphan = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::DocstoreOrphan));
    assert!(docstore_orphan.is_some(), "Should detect Docstore orphan");
}

/// Canary 4: Vector store bloat (vectors > tantivy docs).
#[test]
fn canary_detects_vector_bloat() {
    let tantivy_docs = vec![search_doc("memory", "doc1", "src/a.cs")];
    let docstore_docs = vec![docstore_doc("memory", "doc1", "src/a.cs")];

    // vector_count=200 >> tantivy_count=1 (threshold is +100)
    let result = build_test_mismatches(1, 1, 200, &tantivy_docs, &docstore_docs);

    let vector_orphan = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::VectorOrphan));
    assert!(vector_orphan.is_some(), "Should detect vector orphan bloat");
}

/// Canary 5: Count divergence beyond 5% threshold.
#[test]
fn canary_detects_count_divergence() {
    // Create 20 tantivy docs but only 10 docstore docs
    let tantivy_docs: Vec<_> = (0..20)
        .map(|i| search_doc("memory", &format!("d{i}"), &format!("src/{i}.cs")))
        .collect();
    let docstore_docs: Vec<_> = (0..10)
        .map(|i| docstore_doc("memory", &format!("d{i}"), &format!("src/{i}.cs")))
        .collect();

    let result = build_test_mismatches(20, 10, 10, &tantivy_docs, &docstore_docs);

    let divergence = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::CountDivergence));
    assert!(
        divergence.is_some(),
        "Should detect count divergence (20 vs 10, diff > 5%)"
    );
}

/// Canary 6: No false alarms when vector count is slightly under tantivy + tolerance.
#[test]
fn canary_no_false_alarm_vector_within_tolerance() {
    let tantivy_docs = vec![search_doc("memory", "doc1", "src/a.cs")];
    let docstore_docs = vec![docstore_doc("memory", "doc1", "src/a.cs")];

    // vector_count=50 < tantivy_count(1)+100 → no VectorOrphan
    let result = build_test_mismatches(1, 1, 50, &tantivy_docs, &docstore_docs);

    let vector_orphan = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::VectorOrphan));
    assert!(
        vector_orphan.is_none(),
        "Vector count within tolerance should not trigger VectorOrphan"
    );
}

/// Canary 7: Namespace skew — same doc_id in different namespaces.
#[test]
fn canary_namespace_skew_detected_as_orphan() {
    let tantivy_docs = vec![search_doc("memory", "doc1", "src/a.cs")];
    let docstore_docs = vec![
        docstore_doc("code", "doc1", "src/a.cs"), // Different namespace!
    ];

    let result = build_test_mismatches(1, 1, 0, &tantivy_docs, &docstore_docs);

    // Both should be orphans in their respective stores
    let tantivy_orphan = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::TantivyOrphan));
    let docstore_orphan = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::DocstoreOrphan));
    assert!(
        tantivy_orphan.is_some() || docstore_orphan.is_some(),
        "Namespace skew should be detected as orphan(s)"
    );
}

/// Canary 8: Empty stores — should produce no mismatches.
#[test]
fn canary_empty_stores_no_mismatches() {
    let result = build_test_mismatches(0, 0, 0, &[], &[]);
    assert!(
        result.is_empty(),
        "Empty stores should produce no mismatches"
    );
}

/// Canary 9: Repair policy override logic.
#[test]
fn canary_repair_policy_override() {
    // Config says auto_repair=true, request says false → false
    assert!(!resolve_auto_repair(true, Some(false)));
    // Config says auto_repair=false, request says true → true
    assert!(resolve_auto_repair(false, Some(true)));
    // Config says auto_repair=true, request says None → true
    assert!(resolve_auto_repair(true, None));
}

// ── Test helpers ────────────────────────────────────────────────────────────

fn search_doc(namespace: &str, doc_id: &str, path: &str) -> engram_index::hybrid::SearchDocSummary {
    engram_index::hybrid::SearchDocSummary {
        namespace: namespace.to_string(),
        doc_id: doc_id.to_string(),
        path: path.to_string(),
    }
}

fn docstore_doc(namespace: &str, doc_id: &str, path: &str) -> engram_index::docstore::DocSummary {
    engram_index::docstore::DocSummary {
        namespace: namespace.to_string(),
        doc_id: doc_id.to_string(),
        path: path.to_string(),
    }
}

/// Thin wrapper that delegates to the production `build_integrity_mismatches`
/// function. Using the production path directly ensures that changes to detection
/// logic are immediately reflected in canary results, eliminating the logic-drift
/// risk that existed when the helper reimplemented the same logic independently.
fn build_test_mismatches(
    tantivy_count: u64,
    docstore_count: u64,
    vector_count: u64,
    tantivy_docs: &[engram_index::hybrid::SearchDocSummary],
    docstore_docs: &[engram_index::docstore::DocSummary],
) -> Vec<IntegrityMismatch> {
    engram_server::services::integrity_service::build_integrity_mismatches(
        tantivy_count,
        docstore_count,
        vector_count,
        tantivy_docs,
        docstore_docs,
    )
}
