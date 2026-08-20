#![allow(clippy::unwrap_used)]
//! Integrity canary test — synthetic drift injection (Ticket 8).
//!
//! Exercises the production `build_integrity_mismatches` directly, so a
//! change to detection logic shows up here immediately.
//!
//! Scope note: this file used to cover `tantivy_orphan`, `docstore_orphan`
//! and `count_divergence` as well, feeding them synthetic document lists.
//! Those checks compared Tantivy against a separate document store and were
//! gated on that store being non-empty — and nothing in production has ever
//! written to it, so they could not fire on any real project while these
//! tests passed. They have been removed rather than left as decoration. The
//! lesson is in the shape of these tests: synthetic input proved the
//! comparison logic, never that the data existed.

use engram_server::services::integrity_service::*;

/// Canary 1: stores in agreement — no mismatches.
#[test]
fn canary_no_drift_when_counts_agree() {
    let result = build_integrity_mismatches(1000, 1000, true);
    assert!(
        result.is_empty(),
        "aligned stores should have zero mismatches, got {result:?}"
    );
}

/// Canary 2: vector bloat — far more vectors than indexed docs.
#[test]
fn canary_detects_vector_bloat() {
    let result = build_integrity_mismatches(1000, 1200, true);
    let orphan = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::VectorOrphan));
    assert!(
        orphan.is_some(),
        "should detect vector orphan, got {result:?}"
    );
    assert_eq!(orphan.unwrap().actual, 1200);
}

/// Canary 3: a small vector surplus is within tolerance — no alarm.
#[test]
fn canary_no_false_alarm_vector_within_tolerance() {
    let result = build_integrity_mismatches(1000, 1050, true);
    assert!(
        !result
            .iter()
            .any(|m| matches!(m.kind, MismatchKind::VectorOrphan)),
        "50 surplus vectors is inside the 100 margin, got {result:?}"
    );
}

/// Canary 4: the dangerous direction — embeddings expected but missing.
#[test]
fn canary_detects_vector_shortfall() {
    let result = build_integrity_mismatches(1000, 100, true);
    let shortfall = result
        .iter()
        .find(|m| matches!(m.kind, MismatchKind::VectorShortfall));
    assert!(
        shortfall.is_some(),
        "900 missing embeddings should be flagged, got {result:?}"
    );
}

/// Canary 5: an fts_only install legitimately has no vectors at all, and
/// must not be reported as a shortfall.
#[test]
fn canary_no_shortfall_when_embeddings_not_expected() {
    let result = build_integrity_mismatches(1000, 0, false);
    assert!(
        result.is_empty(),
        "fts_only has no vectors by design, got {result:?}"
    );
}

/// Canary 6: empty stores — nothing to compare, nothing to report.
#[test]
fn canary_empty_stores_no_mismatches() {
    let result = build_integrity_mismatches(0, 0, true);
    assert!(result.is_empty(), "empty stores are not drift: {result:?}");
}

/// Canary 7: repair policy override logic.
#[test]
fn canary_repair_policy_override() {
    // Config says auto_repair=true, request says false → false
    assert!(!resolve_auto_repair(true, Some(false)));
    // Config says auto_repair=false, request says true → true
    assert!(resolve_auto_repair(false, Some(true)));
    // Config says auto_repair=true, request says None → true
    assert!(resolve_auto_repair(true, None));
}
