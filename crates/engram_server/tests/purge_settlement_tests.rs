//! Doc-11 P1a (external audit round 2, P0-1 tail): the post-publication
//! purge settlement — the registry write that records or clears the purge
//! debt is part of the OUTCOME, never a discarded `let _`.
use std::cell::RefCell;

use engram_server::handlers::project_tools::settle_purge_outcome;

#[test]
fn a_successful_purge_clears_the_pending_flag() {
    let wrote = RefCell::new(None);
    let (note, nudge) = settle_purge_outcome(Ok(()), 7, |v| {
        *wrote.borrow_mut() = Some(v.to_string());
        Ok(())
    });
    assert_eq!(note, "purge: ok");
    assert!(!nudge);
    assert_eq!(wrote.borrow().as_deref(), Some(""));
}

#[test]
fn a_failed_purge_records_the_owed_generation_and_nudges_the_gc() {
    let wrote = RefCell::new(None);
    let (note, nudge) = settle_purge_outcome(Err(anyhow::anyhow!("disk full")), 7, |v| {
        *wrote.borrow_mut() = Some(v.to_string());
        Ok(())
    });
    assert!(note.contains("deferred to the GC"), "{note}");
    assert!(nudge);
    assert_eq!(wrote.borrow().as_deref(), Some("7"));
}

#[test]
fn a_failed_purge_whose_recording_fails_is_degraded_not_silent() {
    // The auditor's exact class: the purge failed AND the registry write
    // recording the debt failed — the old `let _` made both vanish.
    let (note, nudge) = settle_purge_outcome(Err(anyhow::anyhow!("disk full")), 7, |_| {
        Err(anyhow::anyhow!("registry read-only"))
    });
    assert!(note.contains("recording the debt failed"), "{note}");
    assert!(note.contains("registry read-only"), "{note}");
    assert!(nudge, "the GC nudge still fires — best effort");
}

#[test]
fn a_successful_purge_whose_clear_fails_still_reports_ok_with_the_warning() {
    let (note, nudge) =
        settle_purge_outcome(Ok(()), 7, |_| Err(anyhow::anyhow!("registry read-only")));
    assert!(note.starts_with("purge: ok"), "{note}");
    assert!(note.contains("clearing the pending flag failed"), "{note}");
    assert!(!nudge);
}
