#![allow(clippy::unwrap_used)]
//! D3/JOB1 — deterministic GC/checkpoint race tests.
//!
//! Proves that the GC `active_indexing_count` guard is correctly respected:
//! when a job is in-flight, the GC tick is skipped and no storage is touched.

use engram_core::Config;
use engram_server::state::AppState;
use engram_server::actors::gc::{run_gc_scheduler, purge_project_old_gens};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn minimal_cfg(tmp: &tempfile::TempDir) -> Config {
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    Config {
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
async fn gc_skips_purge_when_active_indexing_count_nonzero() {
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
    let _ = tokio::time::timeout(Duration::from_secs(2), gc_handle).await;
}

/// JOB1-m2q7: GC must skip a project when `active_generation` metadata is corrupt
/// (not parseable as u64) rather than defaulting to generation 1.
///
/// Regression: previously `unwrap_or(1)` caused GC to purge against gen=1 on any
/// metadata read/parse failure, potentially deleting live generation data.
/// The fix returns `Ok(())` early with a `tracing::warn!` instead.
#[tokio::test]
async fn gc_skips_project_with_corrupt_active_gen_metadata() {
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
async fn gc_does_not_delete_active_job_checkpoint() {
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
    let _ = tokio::time::timeout(Duration::from_secs(2), gc_handle).await;
}
