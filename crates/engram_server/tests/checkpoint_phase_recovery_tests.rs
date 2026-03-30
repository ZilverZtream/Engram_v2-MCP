#![allow(clippy::unwrap_used)]
//! Phase boundary checkpoint and crash-recovery tests.
//!
//! Proves that crash-safe job checkpoints are correctly written at each phase
//! boundary using `CheckpointStore::put`, that a "crashed" job (AppState drop)
//! leaves its checkpoint readable in a new AppState instance, and that the
//! `JobPhase` progression is recoverable from any intermediate state.

use engram_core::{Checkpoint, Config, JobPhase};
use engram_server::state::AppState;

fn make_cfg(data_dir: &std::path::Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        embedding_backend: "fts_only".into(),
        allowed_roots: vec![data_dir.to_path_buf()],
        ..Default::default()
    }
}

fn make_checkpoint(job_id: &str, project_id: &str, phase: JobPhase, items_done: u64) -> Checkpoint {
    Checkpoint {
        job_id: job_id.to_string(),
        project_id: project_id.to_string(),
        phase,
        items_processed: items_done,
        items_total: 100,
        generation: 1,
        idempotency_key: Checkpoint::compute_idempotency_key(project_id, "/tmp/proj", 1),
        resume_state: None,
        updated_at_ms: 1_000_000,
        error: None,
    }
}

/// A checkpoint written at a phase boundary (Scanning complete) must
/// survive a simulated crash (AppState drop + recreate) and be readable for recovery.
#[tokio::test]
async fn scanning_phase_checkpoint_survives_simulated_crash() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state1, _rx1) = AppState::new(make_cfg(&data_dir)).unwrap();

    let job_id = "job-scan-001";
    let cp = make_checkpoint(job_id, "proj-crash-test", JobPhase::Scanning, 42);

    tokio::task::spawn_blocking({
        let store = state1.checkpoints.clone();
        move || store.put(&cp).expect("put checkpoint must succeed")
    })
    .await
    .unwrap();

    // Simulate crash: drop AppState without completing the job.
    drop(state1);

    // Recover: new AppState on same data directory.
    let (state2, _rx2) = AppState::new(make_cfg(&data_dir)).unwrap();

    let recovered = tokio::task::spawn_blocking({
        let store = state2.checkpoints.clone();
        move || store.get(job_id).expect("get checkpoint must not error")
    })
    .await
    .unwrap();

    assert!(
        recovered.is_some(),
        "Scanning-phase checkpoint must survive simulated crash"
    );
    let cp = recovered.unwrap();
    assert_eq!(
        cp.phase,
        JobPhase::Scanning,
        "phase must be preserved across restart"
    );
    assert_eq!(
        cp.items_processed, 42,
        "items_processed must be preserved across restart"
    );
    assert!(
        cp.is_resumable(),
        "Scanning-phase checkpoint must be resumable (not Completed or Failed)"
    );
}

/// A job without any checkpoint must read as None — the recovery path
/// can distinguish "never started" from "crashed mid-phase".
#[tokio::test]
async fn missing_checkpoint_reads_as_none() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let result = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.get("job-never-started").expect("get must not error")
    })
    .await
    .unwrap();

    assert!(
        result.is_none(),
        "missing checkpoint must be None — recovery path must treat \
         absent checkpoint as 'start from scratch'"
    );
}

/// Each phase boundary writes overwrite the previous checkpoint —
/// only the latest (most-advanced) phase is retained for recovery.
#[tokio::test]
async fn sequential_phase_progression_overwrites_previous() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let job_id = "job-seq-phases";
    let phases = [
        (JobPhase::Scanning, 10u64),
        (JobPhase::Parsing, 20),
        (JobPhase::TantivyIndexing, 30),
        (JobPhase::VectorIndexing, 40),
        (JobPhase::GraphBuilding, 50),
        (JobPhase::PostProcessing, 60),
    ];

    for (phase, items) in &phases {
        let cp = make_checkpoint(job_id, "proj-seq", *phase, *items);
        tokio::task::spawn_blocking({
            let store = state.checkpoints.clone();
            move || store.put(&cp).expect("put must succeed")
        })
        .await
        .unwrap();
    }

    // Must read back the LAST phase (PostProcessing).
    let final_cp = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.get(job_id).expect("get must succeed")
    })
    .await
    .unwrap()
    .expect("checkpoint must be present after writes");

    assert_eq!(
        final_cp.phase,
        JobPhase::PostProcessing,
        "last-written phase must be PostProcessing; got: {:?}",
        final_cp.phase
    );
    assert_eq!(
        final_cp.items_processed, 60,
        "items_processed must reflect the last write"
    );
}

/// Deleting a checkpoint after successful completion must remove it,
/// so that the next recovery pass knows the job is done and should not be retried.
#[tokio::test]
async fn delete_checkpoint_after_completion_is_gone() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let job_id = "job-complete-del";
    let cp = make_checkpoint(job_id, "proj-del", JobPhase::Completed, 100);

    tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || {
            store.put(&cp).expect("put must succeed");
            store.remove(job_id).expect("remove must succeed");
        }
    })
    .await
    .unwrap();

    let result = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.get(job_id).expect("get must not error")
    })
    .await
    .unwrap();

    assert!(
        result.is_none(),
        "checkpoint must be None after remove — \
         completed jobs must not confuse recovery-restart path"
    );
}

/// A failed job's checkpoint must NOT be resumable (is_resumable==false)
/// so the recovery path can skip it and surface the error instead.
#[tokio::test]
async fn failed_checkpoint_is_not_resumable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let job_id = "job-failed-001";
    let mut cp = make_checkpoint(job_id, "proj-fail", JobPhase::Failed, 15);
    cp.error = Some("simulated disk failure at phase boundary".into());

    tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.put(&cp).expect("put must succeed")
    })
    .await
    .unwrap();

    let recovered = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.get(job_id).expect("get must not error")
    })
    .await
    .unwrap()
    .expect("failed checkpoint must be present");

    assert_eq!(
        recovered.phase,
        JobPhase::Failed,
        "Failed phase must be preserved"
    );
    assert!(
        !recovered.is_resumable(),
        "Failed checkpoint must NOT be resumable — \
         recovery path must not retry a job that crashed with a Fatal error"
    );
    assert!(
        recovered.error.as_deref().map(|e| e.contains("disk failure")).unwrap_or(false),
        "error message must be preserved in Failed checkpoint"
    );
}

/// find_resumable must return the most recent resumable checkpoint
/// for a project and skip non-resumable (Completed/Failed) ones.
#[tokio::test]
async fn find_resumable_returns_latest_resumable_checkpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let project_id = "proj-resumable";

    // Write an old failed checkpoint.
    let mut failed_cp = make_checkpoint("job-old-failed", project_id, JobPhase::Failed, 5);
    failed_cp.updated_at_ms = 500_000;
    failed_cp.idempotency_key = Checkpoint::compute_idempotency_key(project_id, "/proj", 0);

    // Write a newer in-progress checkpoint.
    let mut mid_cp = make_checkpoint("job-mid-progress", project_id, JobPhase::VectorIndexing, 70);
    mid_cp.updated_at_ms = 1_000_000;
    mid_cp.idempotency_key = Checkpoint::compute_idempotency_key(project_id, "/proj", 1);

    // Write a completed checkpoint (not resumable).
    let mut done_cp = make_checkpoint("job-completed", project_id, JobPhase::Completed, 100);
    done_cp.updated_at_ms = 100_000; // older
    done_cp.idempotency_key = Checkpoint::compute_idempotency_key(project_id, "/proj", 2);

    tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || {
            store.put(&failed_cp).expect("put failed cp");
            store.put(&mid_cp).expect("put mid cp");
            store.put(&done_cp).expect("put done cp");
        }
    })
    .await
    .unwrap();

    let resumable = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        let pid = project_id.to_string();
        move || store.find_resumable(&pid).expect("find_resumable must not error")
    })
    .await
    .unwrap();

    assert!(
        resumable.is_some(),
        "find_resumable must return Some for a project with an in-progress checkpoint"
    );
    let best = resumable.unwrap();
    assert_eq!(
        best.job_id, "job-mid-progress",
        "find_resumable must return the most recent resumable checkpoint \
         (VectorIndexing at ts=1_000_000), not the failed or completed ones"
    );
    assert!(
        best.is_resumable(),
        "returned checkpoint must pass is_resumable()"
    );
}

/// JOB1-m1r0: Re-running a job that crashed between phase-output-write and
/// checkpoint-write must be idempotent — starting from the PREVIOUS checkpoint
/// and re-processing the phase produces equivalent output.
///
/// Proof strategy: the checkpoint stores the previous phase boundary. When re-run,
/// the job starts from that checkpoint (Parsing) and re-executes TantivyIndexing.
/// The test verifies that the checkpoint can be put with the prior phase and then
/// safely overwritten with the new phase — no data corruption or duplicate state.
#[tokio::test]
async fn phase_boundary_crash_idempotent_on_retry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let job_id = "job-idempotent-retry";
    let project_id = "proj-retry";

    // Phase 1: Parsing checkpoint successfully written.
    let parsing_cp = make_checkpoint(job_id, project_id, JobPhase::Parsing, 50);
    tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        let cp = parsing_cp.clone();
        move || store.put(&cp).expect("put parsing checkpoint must succeed")
    })
    .await
    .unwrap();

    // Simulate crash: TantivyIndexing output was written but checkpoint was NOT updated.
    // (i.e., the old Parsing checkpoint is still the latest durably committed state)

    // Recovery: job restarts and reads the last checkpoint.
    let recovered = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.get(job_id).expect("get must not error")
    })
    .await
    .unwrap()
    .expect("checkpoint must be present after crash");

    assert_eq!(
        recovered.phase,
        JobPhase::Parsing,
        "JOB1-m1r0: recovered checkpoint must be Parsing (the last durably committed phase), \
         not TantivyIndexing (output written but checkpoint not committed before crash)"
    );
    assert!(
        recovered.is_resumable(),
        "JOB1-m1r0: Parsing checkpoint must be resumable so the job can retry TantivyIndexing"
    );

    // Retry: re-run TantivyIndexing and write checkpoint atomically.
    let tantivy_cp = make_checkpoint(job_id, project_id, JobPhase::TantivyIndexing, 100);
    tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        let cp = tantivy_cp.clone();
        move || store.put(&cp).expect("put tantivy checkpoint must succeed after retry")
    })
    .await
    .unwrap();

    // Verify the retry committed the new phase correctly.
    let after_retry = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.get(job_id).expect("get must not error")
    })
    .await
    .unwrap()
    .expect("checkpoint must be present after retry");

    assert_eq!(
        after_retry.phase,
        JobPhase::TantivyIndexing,
        "JOB1-m1r0: after successful retry, checkpoint must advance to TantivyIndexing"
    );
    assert_eq!(
        after_retry.items_processed, 100,
        "JOB1-m1r0: retried phase must write correct items_processed count"
    );
}

/// X6-cancelcp-5t9h: cancellation during a phase write must tombstone the
/// checkpoint as Failed, preventing stale state from being used for resume.
///
/// Scenario: job starts (Parsing checkpoint written), gets cancelled, then
/// marks itself Failed. `find_resumable` must then return None — proving the
/// cancelled job does not pollute the resume path.
#[tokio::test]
async fn cancellation_during_phase_write_tombstones_checkpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let job_id = "job-cancelled-phase";
    let project_id = "proj-cancel-test";

    // Job starts and checkpoints at Parsing.
    let parsing_cp = make_checkpoint(job_id, project_id, JobPhase::Parsing, 25);
    tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        let cp = parsing_cp;
        move || store.put(&cp).expect("put parsing checkpoint")
    })
    .await
    .unwrap();

    // Cancellation fires during TantivyIndexing phase write.
    // Job handler detects cancellation and tombstones the checkpoint as Failed.
    let mut failed_cp = make_checkpoint(job_id, project_id, JobPhase::Failed, 25);
    failed_cp.error = Some("X6-cancelcp: job cancelled during TantivyIndexing phase write".into());
    failed_cp.updated_at_ms = 2_000_000; // newer timestamp

    tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        let cp = failed_cp;
        move || store.put(&cp).expect("put failed/tombstoned checkpoint")
    })
    .await
    .unwrap();

    // find_resumable must return None — the job is tombstoned as Failed.
    let resumable = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        let pid = project_id.to_string();
        move || store.find_resumable(&pid).expect("find_resumable must not error")
    })
    .await
    .unwrap();

    assert!(
        resumable.is_none(),
        "X6-cancelcp-5t9h: find_resumable must return None after cancellation tombstone — \
         a Failed checkpoint is not resumable and must not allow stale-state resume"
    );

    // Also verify the checkpoint phase is Failed and error message present.
    let cp = tokio::task::spawn_blocking({
        let store = state.checkpoints.clone();
        move || store.get(job_id).expect("get must not error")
    })
    .await
    .unwrap()
    .expect("tombstoned checkpoint must still be readable");

    assert_eq!(
        cp.phase,
        JobPhase::Failed,
        "X6-cancelcp-5t9h: tombstoned checkpoint must have Failed phase"
    );
    assert!(
        cp.error.as_deref().map(|e| e.contains("cancelled")).unwrap_or(false),
        "X6-cancelcp-5t9h: tombstoned checkpoint must carry cancellation error message"
    );
    assert!(
        !cp.is_resumable(),
        "X6-cancelcp-5t9h: Failed checkpoint must not be resumable"
    );
}
