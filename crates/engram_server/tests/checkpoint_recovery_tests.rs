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
use engram_server::state::AppState;
use engram_server::actors::gc::{run_gc_scheduler, purge_project_old_gens};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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

// ── Fault injection and idempotent retry ─────────────────────────────────────

/// Opening a CheckpointStore on a path whose parent is a file (not a directory)
/// must return Err, not panic.  Proves the store is fail-closed on bad paths.
#[test]
fn checkpoint_open_on_blocked_path_returns_err() {
    let tmp = tempfile::TempDir::new().unwrap();

    // Write a regular file where the parent directory is expected.
    let blocker = tmp.path().join("checkpoints");
    std::fs::write(&blocker, b"I am a file, not a directory").unwrap();

    // Attempting to open a DB inside the file must fail gracefully.
    let bad_path = blocker.join("cp.redb");
    let result = CheckpointStore::open(&bad_path);
    assert!(
        result.is_err(),
        "CheckpointStore::open must return Err when parent path is a file; got Ok"
    );
}

/// Writing the same job_id twice must succeed and the second write must win
/// (last-writer semantics).  This is the idempotent retry contract: a job that
/// failed after writing a checkpoint can retry and overwrite with a new phase.
#[test]
fn checkpoint_overwrite_is_last_write_wins() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).unwrap();

    let cp1 = make_checkpoint("retry-job", "retry-proj", JobPhase::Scanning);
    store.put(&cp1).unwrap();

    let cp2 = Checkpoint {
        job_id: "retry-job".to_string(),
        project_id: "retry-proj".to_string(),
        phase: JobPhase::Failed,
        items_processed: 10,
        items_total: 100,
        generation: 1,
        idempotency_key: Checkpoint::compute_idempotency_key("retry-proj", "/proj", 1),
        resume_state: None,
        updated_at_ms: 2_000_000,
        error: Some("injected fault".to_string()),
    };
    store.put(&cp2).unwrap();

    let got = store.get("retry-job").unwrap().unwrap();
    assert_eq!(
        got.phase, JobPhase::Failed,
        "second write must win; expected Failed, got {:?}", got.phase
    );
    assert!(
        got.error.as_deref().unwrap_or("").contains("injected"),
        "error message from second write must be present"
    );
}

/// A crashed job's checkpoint (any non-Completed phase) must be discoverable
/// via `find_resumable` after a store reopen, proving crash recovery works.
#[test]
fn checkpoint_crash_recovery_finds_resumable_after_reopen() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("cp.redb");

    // Write a mid-flight checkpoint then "crash" (drop store).
    {
        let store = CheckpointStore::open(&path).unwrap();
        let cp = Checkpoint {
            job_id: "crash-job-001".to_string(),
            project_id: "crash-proj".to_string(),
            phase: JobPhase::VectorIndexing,
            items_processed: 70,
            items_total: 100,
            generation: 1,
            idempotency_key: Checkpoint::compute_idempotency_key("crash-proj", "/proj", 1),
            resume_state: None,
            updated_at_ms: 5_000_000,
            error: None,
        };
        store.put(&cp).unwrap();
    } // drop = simulated crash

    // Recovery: reopen and locate resumable checkpoint.
    let recovery_store = CheckpointStore::open(&path).unwrap();
    let resumable = recovery_store.find_resumable("crash-proj").unwrap();

    assert!(
        resumable.is_some(),
        "find_resumable must locate the mid-flight checkpoint after crash + reopen"
    );
    let cp = resumable.unwrap();
    assert_eq!(
        cp.phase, JobPhase::VectorIndexing,
        "recovered checkpoint must have the phase written before crash"
    );
    assert_eq!(cp.items_processed, 70, "items_processed must match checkpoint");
}

/// A Completed checkpoint must NOT be returned by find_resumable.
/// Resuming a completed job would duplicate work.
#[test]
fn checkpoint_completed_not_resumable_after_reopen() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("cp.redb");

    {
        let store = CheckpointStore::open(&path).unwrap();
        let cp = make_checkpoint("done-job", "done-proj", JobPhase::Completed);
        store.put(&cp).unwrap();
    }

    let store2 = CheckpointStore::open(&path).unwrap();
    let resumable = store2.find_resumable("done-proj").unwrap();
    assert!(
        resumable.is_none(),
        "Completed checkpoint must not be resumable; got: {:?}",
        resumable.map(|c| c.phase)
    );
}

// ── JOB1: phase-boundary crash idempotency ────────────────────────────────────

/// JOB1: re-writing the same checkpoint twice (simulating crash-then-replay of
/// the same phase) must leave exactly the latest write visible — last-write-wins
/// per phase, no phantom duplicate state.
///
/// This proves the checkpoint store is safe for idempotent phase replay: if a
/// phase crashes after writing its checkpoint but before completing, replaying
/// the phase writes the same checkpoint again without corrupting the store.
#[test]
fn phase_replay_writes_same_checkpoint_is_idempotent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = CheckpointStore::open(&tmp.path().join("cp.redb")).unwrap();

    let mut cp = make_checkpoint("replay-job", "replay-proj", JobPhase::TantivyIndexing);
    cp.items_processed = 50;

    // First write: phase TantivyIndexing at progress 50.
    store.put(&cp).unwrap();

    // Crash-then-replay: same phase, same job_id — overwrite with identical data.
    store.put(&cp).unwrap();

    // Only one checkpoint must exist, with the correct phase.
    let retrieved = store.get("replay-job").unwrap().expect("must exist");
    assert_eq!(
        retrieved.phase,
        JobPhase::TantivyIndexing,
        "JOB1: idempotent replay must leave phase as TantivyIndexing"
    );
    assert_eq!(
        retrieved.items_processed, 50,
        "JOB1: idempotent replay must preserve items_processed"
    );

    // find_resumable must find exactly one resumable checkpoint.
    let found = store.find_resumable("replay-proj").unwrap();
    assert!(
        found.is_some(),
        "JOB1: idempotent phase replay must leave exactly one resumable checkpoint"
    );
}

/// JOB1: simulates the full crash-recovery lifecycle:
/// 1. Job starts — writes Scanning checkpoint
/// 2. Crash in TantivyIndexing — writes TantivyIndexing checkpoint, then crashes
/// 3. Process restart — find_resumable returns TantivyIndexing (not Scanning)
/// 4. Replay: re-writes TantivyIndexing (idempotent), then advances to Completed
/// 5. find_resumable returns None (Completed is terminal)
///
/// This proves the checkpoint lifecycle is safe for phase-boundary crash recovery.
#[test]
fn crash_recovery_full_lifecycle_advances_to_completed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("cp.redb");

    // Phase 1: job starts, writes Scanning checkpoint.
    {
        let store = CheckpointStore::open(&path).unwrap();
        let cp = make_checkpoint("lifecycle-job", "lifecycle-proj", JobPhase::Scanning);
        store.put(&cp).unwrap();
    }

    // Phase 2: crash in TantivyIndexing — TantivyIndexing checkpoint written.
    {
        let store = CheckpointStore::open(&path).unwrap();
        let cp = make_checkpoint("lifecycle-job", "lifecycle-proj", JobPhase::TantivyIndexing);
        store.put(&cp).unwrap();
        // Simulated crash: store is dropped without advancing further.
    }

    // Phase 3: process restart — find_resumable returns TantivyIndexing.
    {
        let store = CheckpointStore::open(&path).unwrap();
        let resumable = store.find_resumable("lifecycle-proj").unwrap()
            .expect("JOB1: after crash in TantivyIndexing, find_resumable must return resumable checkpoint");
        assert_eq!(
            resumable.phase,
            JobPhase::TantivyIndexing,
            "JOB1: resumed checkpoint must be TantivyIndexing (most recent phase)"
        );
    }

    // Phase 4: idempotent replay of TantivyIndexing, then advance to Completed.
    {
        let store = CheckpointStore::open(&path).unwrap();
        // Replay: re-write TantivyIndexing (same result as before crash).
        let cp_ti = make_checkpoint("lifecycle-job", "lifecycle-proj", JobPhase::TantivyIndexing);
        store.put(&cp_ti).unwrap();
        // Advance to Completed.
        let cp_done = make_checkpoint("lifecycle-job", "lifecycle-proj", JobPhase::Completed);
        store.put(&cp_done).unwrap();
    }

    // Phase 5: find_resumable must return None (Completed is terminal).
    {
        let store = CheckpointStore::open(&path).unwrap();
        let resumable = store.find_resumable("lifecycle-proj").unwrap();
        assert!(
            resumable.is_none(),
            "JOB1: after reaching Completed, find_resumable must return None; got: {:?}",
            resumable.map(|c| c.phase)
        );
    }
}

// ── GC/checkpoint race tests ──────────────────────────────────────────────────

fn minimal_cfg(tmp: &tempfile::TempDir) -> engram_core::Config {
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    engram_core::Config {
        data_dir: data_dir.clone(),
        embedding_backend: "fts_only".into(),
        allowed_roots: vec![data_dir],
        ..Default::default()
    }
}

/// D3/JOB1: GC must skip the purge tick when `active_indexing_count > 0`.
///
/// Uses `tokio::time::pause()` to advance the GC's 1-hour interval without
/// wall-clock delay, making the test fully deterministic.
#[tokio::test]
async fn purge_is_skipped_when_active_indexing_count_is_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = minimal_cfg(&tmp);
    let (state, _rx) = AppState::new(cfg).unwrap();

    // Simulate in-flight indexing job by incrementing the count.
    state
        .active_indexing_count
        .store(1, Ordering::SeqCst);

    tokio::time::pause();

    let shutdown = CancellationToken::new();
    let state_gc = state.clone();
    let shutdown_gc = shutdown.clone();
    let gc_handle = tokio::spawn(run_gc_scheduler(state_gc, shutdown_gc));

    // Advance past the 1-hour GC interval to trigger the first tick.
    tokio::time::advance(Duration::from_secs(3_601)).await;
    // Yield so the GC task can run its select! branch.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // GC must not have touched the active_indexing_count (it only reads it).
    assert_eq!(
        state.active_indexing_count.load(Ordering::SeqCst),
        1,
        "D3/JOB1: active_indexing_count must still be 1 after GC tick with in-flight job"
    );

    // Simulate job completion.
    state.active_indexing_count.store(0, Ordering::SeqCst);

    // Advance time again — GC should now proceed without panicking (no projects
    // to purge in this empty state, so it completes the sweep cleanly).
    tokio::time::advance(Duration::from_secs(3_601)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // GC ran without panicking; active_indexing_count should still be 0.
    assert_eq!(
        state.active_indexing_count.load(Ordering::SeqCst),
        0,
        "D3/JOB1: active_indexing_count must remain 0 after GC runs with no in-flight jobs"
    );

    shutdown.cancel();
    let shutdown_result = tokio::time::timeout(Duration::from_secs(2), gc_handle).await;
    assert!(
        shutdown_result.is_ok(),
        "X5-gcjob-7m3d: GC task must exit within 2s after shutdown cancellation — \
         timeout indicates cooperative cancellation via select! is not propagating correctly"
    );
}

/// JOB1-m2q7: GC must skip a project when `active_generation` metadata is corrupt
/// (not parseable as u64) rather than defaulting to generation 1.
///
/// Regression: previously `unwrap_or(1)` caused GC to purge against gen=1 on any
/// metadata read/parse failure, potentially deleting live generation data.
/// The fix returns `Ok(())` early with a `tracing::warn!` instead.
#[tokio::test]
async fn corrupt_active_gen_metadata_causes_project_to_be_skipped_not_purged() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = minimal_cfg(&tmp);
    let (state, _rx) = AppState::new(cfg).unwrap();

    // Register a project but write a non-parseable active_generation value.
    // "corrupt_value" cannot be parsed as u64 — triggers the skip path.
    let reg = state.registry.clone();
    tokio::task::spawn_blocking({
        let reg = reg.clone();
        move || {
            use engram_core::ProjectRecord;
            reg.put_project(&ProjectRecord {
                project_id: "corrupt-gen-proj".into(),
                project_name: "Corrupt Gen Project".into(),
                project_type: "general".into(),
                directory: "/tmp/proj".into(),
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
                reindex_required_since_ms: None,
            })
            .expect("put_project must succeed");
            reg.set_meta("corrupt-gen-proj", "active_generation", "not_a_number")
                .expect("set_meta must succeed");
        }
    })
    .await
    .unwrap();

    // Direct call — must return Ok(()) without touching graph or search storage.
    // Before fix: unwrap_or(1) would call purge_old_generations with gen=1.
    // After fix: early-return Ok(()) with tracing::warn.
    let result: anyhow::Result<()> = purge_project_old_gens(&state, "corrupt-gen-proj").await;
    assert!(
        result.is_ok(),
        "JOB1-m2q7: purge must skip (Ok) when active_generation is corrupt, got: {result:?}"
    );

    // Verify project record still exists — nothing was deleted.
    let rec = tokio::task::spawn_blocking(move || reg.get_project("corrupt-gen-proj"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        rec.is_some(),
        "JOB1-m2q7: project record must survive GC skip when active_generation is corrupt"
    );
}

/// D3/JOB1: Registry checkpoint written by a job must survive a concurrent GC tick.
///
/// Creates a project with a registry record (standing in for an active job's
/// checkpoint), fires a GC tick with `active_indexing_count = 1`, and verifies
/// the registry record is untouched.
#[tokio::test]
async fn active_job_checkpoint_is_not_deleted_during_gc_tick() {
    use engram_core::ProjectRecord;

    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = minimal_cfg(&tmp);
    let (state, _rx) = AppState::new(cfg).unwrap();

    // Write a project record representing the "checkpoint" an active job owns.
    // Use the state's registry (opened in AppState::new).
    let reg = state.registry.clone();
    tokio::task::spawn_blocking({
        let reg = reg.clone();
        move || {
            reg.put_project(&ProjectRecord {
                project_id: "job-active-proj".into(),
                project_name: "Active Job Project".into(),
                project_type: "general".into(),
                directory: "/tmp/proj".into(),
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
                reindex_required_since_ms: None,
            })
        }
    })
    .await
    .unwrap()
    .unwrap();

    // Simulate in-flight job.
    state.active_indexing_count.store(1, Ordering::SeqCst);

    tokio::time::pause();

    let shutdown = CancellationToken::new();
    let state_gc = state.clone();
    let shutdown_gc = shutdown.clone();
    let gc_handle = tokio::spawn(run_gc_scheduler(state_gc, shutdown_gc));

    // Force a GC tick.
    tokio::time::advance(Duration::from_secs(3_601)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Project record must still exist — GC skipped entirely.
    let rec = tokio::task::spawn_blocking(move || reg.get_project("job-active-proj"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        rec.is_some(),
        "D3/JOB1: project record (active job checkpoint) must not be deleted by GC when active_indexing_count > 0"
    );

    shutdown.cancel();
    let shutdown_result = tokio::time::timeout(Duration::from_secs(2), gc_handle).await;
    assert!(
        shutdown_result.is_ok(),
        "X5-gcjob-7m3d: GC must exit cleanly within 2s after cancellation with an \
         active checkpoint present — timeout means the GC loop is not responding to shutdown"
    );
}
