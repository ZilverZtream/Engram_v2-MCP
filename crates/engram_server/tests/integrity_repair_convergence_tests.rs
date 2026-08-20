#![allow(clippy::unwrap_used)]
//! An auto-repair that does not fix anything must not run forever.
//!
//! Live 2026-08-20: the `vector_orphan` repair on one project had been
//! firing every hour for more than three days without converging. It called
//! `repair_project_scoped(scope="vector_only")`, which wrote the registry
//! key `vector_needs_repair` — a key no code has ever read. The repair
//! therefore reported `success: true`, `overall_healthy` came out true, the
//! condition was logged at debug, and the next hourly tick did it again.
//!
//! Two properties are needed: a repair must not claim success for work it
//! did not do, and a mismatch that survives repeated repair must escalate
//! once instead of retrying indefinitely.

use engram_server::services::integrity_service::{
    MAX_AUTO_REPAIR_ATTEMPTS, MismatchKind, RepairDecision, RepairOutcome,
    clear_repair_attempts_for_resolved, compute_overall_healthy, note_repair_attempt,
};

/// After MAX_AUTO_REPAIR_ATTEMPTS consecutive attempts on the SAME
/// project+kind, auto-repair stands down instead of retrying hourly forever.
#[test]
fn auto_repair_gives_up_after_max_consecutive_attempts() {
    let pid = "conv-test-gives-up";
    let kind = MismatchKind::VectorOrphan;

    for expected in 1..=MAX_AUTO_REPAIR_ATTEMPTS {
        match note_repair_attempt(pid, &kind) {
            RepairDecision::Attempt { attempt } => assert_eq!(attempt, expected),
            RepairDecision::Exhausted { attempts } => {
                panic!("gave up too early at {attempts} attempts")
            }
        }
    }

    for _ in 0..3 {
        match note_repair_attempt(pid, &kind) {
            RepairDecision::Exhausted { attempts } => {
                assert_eq!(attempts, MAX_AUTO_REPAIR_ATTEMPTS)
            }
            RepairDecision::Attempt { attempt } => {
                panic!("attempt {attempt} ran past the cap — this is the 3-day loop")
            }
        }
    }
}

/// A mismatch that clears must reset its counter, so a later, unrelated
/// recurrence still gets its repair attempts.
#[test]
fn resolved_mismatch_resets_the_counter() {
    let pid = "conv-test-resets";
    let kind = MismatchKind::VectorOrphan;

    for _ in 0..MAX_AUTO_REPAIR_ATTEMPTS {
        note_repair_attempt(pid, &kind);
    }
    assert!(matches!(
        note_repair_attempt(pid, &kind),
        RepairDecision::Exhausted { .. }
    ));

    // Next check finds no mismatches at all.
    clear_repair_attempts_for_resolved(pid, &[]);

    assert!(
        matches!(
            note_repair_attempt(pid, &kind),
            RepairDecision::Attempt { attempt: 1 }
        ),
        "a healed project must get a fresh budget"
    );
    clear_repair_attempts_for_resolved(pid, &[]);
}

/// Counters must not bleed between projects or between mismatch kinds —
/// otherwise one noisy project would silence repairs everywhere.
#[test]
fn counters_are_scoped_per_project_and_kind() {
    let a = "conv-test-scope-a";
    let b = "conv-test-scope-b";

    for _ in 0..MAX_AUTO_REPAIR_ATTEMPTS {
        note_repair_attempt(a, &MismatchKind::VectorOrphan);
    }
    assert!(matches!(
        note_repair_attempt(a, &MismatchKind::VectorOrphan),
        RepairDecision::Exhausted { .. }
    ));

    assert!(
        matches!(
            note_repair_attempt(a, &MismatchKind::VectorShortfall),
            RepairDecision::Attempt { attempt: 1 }
        ),
        "a different mismatch kind on the same project keeps its own budget"
    );
    assert!(
        matches!(
            note_repair_attempt(b, &MismatchKind::VectorOrphan),
            RepairDecision::Attempt { attempt: 1 }
        ),
        "a different project keeps its own budget"
    );

    clear_repair_attempts_for_resolved(a, &[]);
    clear_repair_attempts_for_resolved(b, &[]);
}

/// Clearing must only touch kinds that are gone — a still-present mismatch
/// keeps counting toward the cap.
#[test]
fn still_present_mismatch_keeps_its_count() {
    let pid = "conv-test-partial-clear";
    note_repair_attempt(pid, &MismatchKind::VectorOrphan);
    note_repair_attempt(pid, &MismatchKind::VectorShortfall);

    clear_repair_attempts_for_resolved(pid, &[MismatchKind::VectorOrphan]);

    assert!(
        matches!(
            note_repair_attempt(pid, &MismatchKind::VectorOrphan),
            RepairDecision::Attempt { attempt: 2 }
        ),
        "the still-present kind must keep its history"
    );
    assert!(
        matches!(
            note_repair_attempt(pid, &MismatchKind::VectorShortfall),
            RepairDecision::Attempt { attempt: 1 }
        ),
        "the resolved kind must have been reset"
    );
    clear_repair_attempts_for_resolved(pid, &[]);
}

/// A stood-down repair is not a healthy project. `overall_healthy` must stay
/// false so the condition is reported rather than swallowed.
#[test]
fn exhausted_repair_does_not_read_as_healthy() {
    let outcome = RepairOutcome {
        mismatch_kind: MismatchKind::VectorOrphan,
        action: "auto_repair_exhausted".into(),
        success: false,
        items_repaired: 0,
    };
    assert!(
        !compute_overall_healthy(1, std::slice::from_ref(&outcome)),
        "standing down is not the same as fixing it"
    );
}
