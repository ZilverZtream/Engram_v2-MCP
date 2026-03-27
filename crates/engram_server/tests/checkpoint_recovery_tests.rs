#![allow(clippy::unwrap_used)]
//! Behavioral tests for job checkpoint/recovery lifecycle.
//!
//! Covers Subsystem 3 (project registry, job orchestration, checkpoint/recovery).
//!
//! All tests call production CheckpointStore and Checkpoint directly:
//!  - `engram_core::CheckpointStore::open`
//!  - `CheckpointStore::put` / `get` / `find_resumable` / `remove` / `cleanup_old`
//!  - `Checkpoint::is_resumable`
//!  - `Checkpoint::compute_idempotency_key`

use engram_core::{Checkpoint, CheckpointStore, JobPhase};

fn make_checkpoint(job_id: &str, project_id: &str, phase: JobPhase) -> Checkpoint {
    Checkpoint {
        job_id: job_id.to_string(),
        project_id: project_id.to_string(),
        phase,
        items_processed: 0,
        items_total: 100,
        generation: 1,
        idempotency_key: Checkpoint::compute_idempotency_key(project_id, "/dir", 1),
        resume_state: None,
        updated_at_ms: 1_000_000,
        error: None,
    }
}

// ── CheckpointStore open / put / get ─────────────────────────────────────────

/// CheckpointStore::open must succeed on a fresh tempdir path.
#[test]
fn checkpoint_store_open_on_fresh_path_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let result = CheckpointStore::open(&tmp.path().join("cp.redb"));
    assert!(
        result.is_ok(),
        "CheckpointStore::open must succeed on a fresh path; got: {:?}",
        result.err()
    );
}

/// put followed by get must return the same checkpoint.
#[test]
fn checkpoint_put_then_get_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    let cp = make_checkpoint("job-001", "proj-abc", JobPhase::Parsing);
    store.put(&cp).expect("put must succeed");

    let retrieved = store.get("job-001").expect("get must not error");
    assert!(retrieved.is_some(), "get must return the checkpoint after put");

    let r = retrieved.unwrap();
    assert_eq!(r.job_id, "job-001");
    assert_eq!(r.project_id, "proj-abc");
    assert_eq!(r.phase, JobPhase::Parsing);
    assert_eq!(r.items_total, 100);
}

/// get on a non-existent job_id must return None, not Err.
#[test]
fn checkpoint_get_nonexistent_returns_none() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    let result = store.get("no-such-job").expect("get must not error");
    assert!(
        result.is_none(),
        "get for unknown job_id must return None, not Err"
    );
}

// ── Checkpoint::is_resumable ──────────────────────────────────────────────────

/// In-progress phases must be resumable.
#[test]
fn checkpoint_in_progress_phases_are_resumable() {
    for phase in [JobPhase::Parsing, JobPhase::TantivyIndexing, JobPhase::VectorIndexing] {
        let cp = make_checkpoint("j", "p", phase);
        assert!(
            cp.is_resumable(),
            "checkpoint in phase {phase:?} must be resumable"
        );
    }
}

/// Completed and Failed checkpoints must NOT be resumable.
#[test]
fn checkpoint_completed_and_failed_are_not_resumable() {
    let completed = make_checkpoint("j1", "p", JobPhase::Completed);
    assert!(
        !completed.is_resumable(),
        "Completed checkpoint must NOT be resumable"
    );

    let failed = make_checkpoint("j2", "p", JobPhase::Failed);
    assert!(
        !failed.is_resumable(),
        "Failed checkpoint must NOT be resumable"
    );
}

// ── find_resumable — production recovery query ────────────────────────────────

/// find_resumable must return a checkpoint for the project when one exists
/// in a resumable phase.
#[test]
fn find_resumable_returns_in_progress_checkpoint() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    let cp = make_checkpoint("job-resume-01", "proj-resumable", JobPhase::VectorIndexing);
    store.put(&cp).expect("put");

    let found = store
        .find_resumable("proj-resumable")
        .expect("find_resumable must not error");
    assert!(
        found.is_some(),
        "find_resumable must find the in-progress checkpoint"
    );
    assert_eq!(found.unwrap().job_id, "job-resume-01");
}

/// find_resumable must return None when only Completed checkpoints exist.
#[test]
fn find_resumable_returns_none_when_only_completed_checkpoints_exist() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    let cp = make_checkpoint("job-done", "proj-done", JobPhase::Completed);
    store.put(&cp).expect("put");

    let found = store
        .find_resumable("proj-done")
        .expect("find_resumable must not error");
    assert!(
        found.is_none(),
        "find_resumable must return None when all checkpoints are Completed"
    );
}

/// find_resumable must return None for a project with no checkpoints.
#[test]
fn find_resumable_returns_none_for_unknown_project() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    let found = store
        .find_resumable("proj-unknown-xyz")
        .expect("find_resumable must not error");
    assert!(
        found.is_none(),
        "find_resumable must return None for a project with no checkpoints"
    );
}

// ── remove — checkpoint cancellation ─────────────────────────────────────────

/// remove must delete the checkpoint so subsequent get returns None.
#[test]
fn checkpoint_remove_deletes_record() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    let cp = make_checkpoint("job-remove", "proj-x", JobPhase::Parsing);
    store.put(&cp).expect("put");

    // Verify it exists
    assert!(store.get("job-remove").expect("get").is_some(), "must exist before remove");

    store.remove("job-remove").expect("remove must succeed");

    // Must be gone
    let after = store.get("job-remove").expect("get after remove");
    assert!(
        after.is_none(),
        "checkpoint must be gone after remove; still present: {:?}",
        after
    );
}

/// remove on a non-existent job_id must succeed (idempotent, no error).
#[test]
fn checkpoint_remove_nonexistent_is_idempotent() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    let result = store.remove("no-such-job-xyz");
    assert!(
        result.is_ok(),
        "remove of non-existent job must be idempotent (no error); got: {:?}",
        result.err()
    );
}

// ── Checkpoint::compute_idempotency_key ───────────────────────────────────────

/// Idempotency key must be deterministic: same inputs → same key.
#[test]
fn checkpoint_idempotency_key_is_deterministic() {
    let k1 = Checkpoint::compute_idempotency_key("proj-a", "/path/to/dir", 3);
    let k2 = Checkpoint::compute_idempotency_key("proj-a", "/path/to/dir", 3);
    assert_eq!(k1, k2, "idempotency key must be identical for same inputs");
}

/// Different inputs must produce different idempotency keys.
#[test]
fn checkpoint_idempotency_key_differs_for_different_inputs() {
    let k_gen1 = Checkpoint::compute_idempotency_key("proj-a", "/dir", 1);
    let k_gen2 = Checkpoint::compute_idempotency_key("proj-a", "/dir", 2);
    let k_proj = Checkpoint::compute_idempotency_key("proj-b", "/dir", 1);

    assert_ne!(k_gen1, k_gen2, "different generation → different key");
    assert_ne!(k_gen1, k_proj, "different project → different key");
}

/// Idempotency key must be non-empty and have a reasonable length (blake3 hex).
#[test]
fn checkpoint_idempotency_key_is_nonempty_blake3_hex() {
    let key = Checkpoint::compute_idempotency_key("proj", "/dir", 1);
    assert!(!key.is_empty(), "idempotency key must not be empty");
    // blake3 hex output is 64 hex characters
    assert_eq!(
        key.len(),
        64,
        "blake3 hex idempotency key must be 64 characters; got {}",
        key.len()
    );
    assert!(
        key.chars().all(|c| c.is_ascii_hexdigit()),
        "idempotency key must be hex; got: {key}"
    );
}

// ── cleanup_old — maintenance path ───────────────────────────────────────────

/// cleanup_old must remove checkpoints older than max_age_ms and return count.
#[test]
fn cleanup_old_removes_expired_checkpoints() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).expect("open");

    // Old checkpoint: updated_at_ms = 1 (epoch)
    let mut old_cp = make_checkpoint("job-old", "proj-cleanup", JobPhase::Completed);
    old_cp.updated_at_ms = 1; // very old
    store.put(&old_cp).expect("put old");

    // Recent checkpoint
    let mut recent_cp = make_checkpoint("job-recent", "proj-cleanup", JobPhase::Parsing);
    recent_cp.updated_at_ms = u64::MAX; // far future
    store.put(&recent_cp).expect("put recent");

    // Remove anything older than 1000ms — old_cp qualifies, recent_cp does not
    let removed = store.cleanup_old(1000).expect("cleanup_old must succeed");
    assert!(
        removed >= 1,
        "cleanup_old must remove at least the old expired checkpoint; removed={removed}"
    );

    // Old must be gone, recent must remain
    assert!(store.get("job-old").expect("get").is_none(), "old checkpoint must be removed");
    assert!(
        store.get("job-recent").expect("get").is_some(),
        "recent checkpoint must survive cleanup"
    );
}
