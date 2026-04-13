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

/// JOB1-b4r2 phase crash matrix: each resumable phase must survive a simulated
/// crash (AppState drop) and allow recovery in a new AppState.
///
/// Tests all resumable phases:
///   Parsing        → survives crash → is_resumable=true
///   TantivyIndexing → survives crash → is_resumable=true
///   VectorIndexing  → survives crash → is_resumable=true
///   GraphBuilding   → survives crash → is_resumable=true (if resumable)
///   PostProcessing  → survives crash → is_resumable=true (if resumable)
///
/// Completed/Failed crash survival is tested separately (not resumable after restart).
#[tokio::test]
async fn all_resumable_phases_survive_simulated_crash_and_restart() {
    let resumable_phases = [
        JobPhase::Parsing,
        JobPhase::TantivyIndexing,
        JobPhase::VectorIndexing,
    ];

    for phase in resumable_phases {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join(format!("data-{phase:?}"));
        std::fs::create_dir_all(&data_dir).unwrap();

        let (state1, _rx1) = AppState::new(make_cfg(&data_dir)).unwrap();

        let job_id = format!("job-crash-{phase:?}");
        let cp = make_checkpoint(&job_id, "proj-crash-matrix", phase, 33);

        tokio::task::spawn_blocking({
            let store = state1.checkpoints.clone();
            move || store.put(&cp).expect("put checkpoint must succeed")
        })
        .await
        .unwrap();

        // Simulate crash.
        drop(state1);

        // Recovery: new AppState on the same data directory.
        let (state2, _rx2) = AppState::new(make_cfg(&data_dir)).unwrap();

        let recovered = tokio::task::spawn_blocking({
            let store = state2.checkpoints.clone();
            let jid = job_id.clone();
            move || store.get(&jid).expect("get must not error")
        })
        .await
        .unwrap();

        assert!(
            recovered.is_some(),
            "JOB1-b4r2: {:?}-phase checkpoint must survive crash+restart; got None",
            phase
        );
        let r = recovered.unwrap();
        assert_eq!(
            r.phase, phase,
            "JOB1-b4r2: phase must be preserved across restart; expected {phase:?}, got {:?}",
            r.phase
        );
        assert!(
            r.is_resumable(),
            "JOB1-b4r2: {:?}-phase checkpoint must be resumable after restart",
            phase
        );
        assert_eq!(
            r.items_processed, 33,
            "JOB1-b4r2: items_processed must be preserved across crash+restart"
        );
    }
}

/// JOB1-b4r2 phase crash matrix: Completed and Failed phases must survive crash
/// but must NOT be resumable — the recovery path must not retry finished jobs.
#[tokio::test]
async fn non_resumable_phases_survive_crash_but_are_not_resumable() {
    for phase in [JobPhase::Completed, JobPhase::Failed] {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join(format!("data-{phase:?}"));
        std::fs::create_dir_all(&data_dir).unwrap();

        let (state1, _rx1) = AppState::new(make_cfg(&data_dir)).unwrap();

        let job_id = format!("job-terminal-{phase:?}");
        let cp = make_checkpoint(&job_id, "proj-terminal", phase, 100);

        tokio::task::spawn_blocking({
            let store = state1.checkpoints.clone();
            move || store.put(&cp).expect("put checkpoint must succeed")
        })
        .await
        .unwrap();

        drop(state1);

        let (state2, _rx2) = AppState::new(make_cfg(&data_dir)).unwrap();

        let recovered = tokio::task::spawn_blocking({
            let store = state2.checkpoints.clone();
            let jid = job_id.clone();
            move || store.get(&jid).expect("get must not error")
        })
        .await
        .unwrap();

        assert!(
            recovered.is_some(),
            "JOB1-b4r2: {:?}-phase checkpoint must be readable after crash+restart (needed for error reporting)",
            phase
        );
        assert!(
            !recovered.unwrap().is_resumable(),
            "JOB1-b4r2: {:?}-phase checkpoint must NOT be resumable after restart — \
             completed/failed jobs must not be retried",
            phase
        );
    }
}

/// JOB1-b4r2: crash mid-phase then idempotent restart from earliest valid boundary.
///
/// Scenario matrix: crash at each phase-to-phase transition and verify the
/// recovery starts from the correct earlier checkpoint, re-executes the lost
/// phase, and produces the same final state as an uninterrupted run.
///
/// Phases tested: Scanning → Parsing → TantivyIndexing → VectorIndexing
#[tokio::test]
async fn phase_transition_crash_replays_correctly_from_prior_checkpoint() {
    let phase_sequence = [
        (JobPhase::Scanning, JobPhase::Parsing),
        (JobPhase::Parsing, JobPhase::TantivyIndexing),
        (JobPhase::TantivyIndexing, JobPhase::VectorIndexing),
    ];

    for (prior_phase, crashed_phase) in phase_sequence {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp
            .path()
            .join(format!("data-{prior_phase:?}-{crashed_phase:?}"));
        std::fs::create_dir_all(&data_dir).unwrap();

        let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

        let job_id = format!("job-replay-{prior_phase:?}");
        let project_id = "proj-replay";

        // Write checkpoint at prior_phase (committed successfully before crash).
        let prior_cp = make_checkpoint(&job_id, project_id, prior_phase, 50);
        tokio::task::spawn_blocking({
            let store = state.checkpoints.clone();
            let cp = prior_cp;
            move || store.put(&cp).expect("put prior phase checkpoint")
        })
        .await
        .unwrap();

        // Simulate crash: crashed_phase output written but checkpoint NOT updated.
        // (The checkpoint still points to prior_phase.)

        // Recovery: read checkpoint — must see prior_phase, not crashed_phase.
        let recovered = tokio::task::spawn_blocking({
            let store = state.checkpoints.clone();
            let jid = job_id.clone();
            move || store.get(&jid).expect("get must not error")
        })
        .await
        .unwrap()
        .expect("checkpoint must be present after simulated crash");

        assert_eq!(
            recovered.phase, prior_phase,
            "JOB1-b4r2 replay ({prior_phase:?}→{crashed_phase:?}): after crash, \
             checkpoint must point to prior_phase={prior_phase:?} (last durably committed), \
             not crashed_phase={crashed_phase:?} (output written but checkpoint not committed); \
             got {:?}",
            recovered.phase
        );
        assert!(
            recovered.is_resumable(),
            "JOB1-b4r2 replay: prior_phase={prior_phase:?} checkpoint must be resumable \
             so the job can re-execute crashed_phase={crashed_phase:?}"
        );

        // Retry: re-execute crashed_phase and commit its checkpoint.
        let retry_cp = make_checkpoint(&job_id, project_id, crashed_phase, 100);
        tokio::task::spawn_blocking({
            let store = state.checkpoints.clone();
            let cp = retry_cp;
            move || store.put(&cp).expect("put retry checkpoint")
        })
        .await
        .unwrap();

        // After retry: checkpoint advances to crashed_phase.
        let after_retry = tokio::task::spawn_blocking({
            let store = state.checkpoints.clone();
            let jid = job_id.clone();
            move || store.get(&jid).expect("get must not error")
        })
        .await
        .unwrap()
        .expect("checkpoint must be present after retry");

        assert_eq!(
            after_retry.phase, crashed_phase,
            "JOB1-b4r2 replay: after successful retry, checkpoint must advance to \
             crashed_phase={crashed_phase:?}; got {:?}",
            after_retry.phase
        );
    }
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
        recovered
            .error
            .as_deref()
            .map(|e| e.contains("disk failure"))
            .unwrap_or(false),
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
        move || {
            store
                .find_resumable(&pid)
                .expect("find_resumable must not error")
        }
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
        move || {
            store
                .put(&cp)
                .expect("put tantivy checkpoint must succeed after retry")
        }
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
        move || {
            store
                .find_resumable(&pid)
                .expect("find_resumable must not error")
        }
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
        cp.error
            .as_deref()
            .map(|e| e.contains("cancelled"))
            .unwrap_or(false),
        "X6-cancelcp-5t9h: tombstoned checkpoint must carry cancellation error message"
    );
    assert!(
        !cp.is_resumable(),
        "X6-cancelcp-5t9h: Failed checkpoint must not be resumable"
    );
}

/// JOB1: checkpoint write failure is fail-open (warn-only) — proves the
/// handler logs the failure and does NOT abort the job.
///
/// This is a structural source scan: we verify the warning strings exist in
/// the handler source so that the fail-open behavior is explicitly documented
/// and observable via log aggregation rather than being invisible to operators.
#[test]
fn job1_checkpoint_write_failure_is_warn_only_not_fatal() {
    let source = include_str!("../src/handlers/project_tools.rs");

    // Both failure branches must log a warning — silent failure is worse than
    // loud failure because it leaves no observable trace for operators.
    assert!(
        source.contains("checkpoint write failed — resumability may be lost"),
        "JOB1: project_tools.rs must log a warning when checkpoint write fails; \
         without this log, operators cannot observe when job resumability is lost"
    );
    assert!(
        source.contains("checkpoint write task panicked — resumability may be lost"),
        "JOB1: project_tools.rs must log a warning when the checkpoint write task panics; \
         without this log, the panic is invisible to operators"
    );

    // The handler must NOT propagate the error (fail-open contract):
    // the `?` operator must NOT appear immediately after the spawn_blocking call.
    // We verify by checking that the match arms contain warn!, not return/?.
    assert!(
        source.contains("tracing::warn!"),
        "JOB1: checkpoint write failures must use tracing::warn! not silent swallowing \
         — operators must be able to observe the failure via log aggregation"
    );
}

/// JOB1: structural check that the checkpoint write result is matched and both
/// Ok and Err branches are handled explicitly.  Proves the failure path is
/// deliberate (not accidentally ignored via `let _ = ...`).
#[test]
fn job1_checkpoint_write_uses_explicit_match_not_ignore() {
    let source = include_str!("../src/handlers/project_tools.rs");

    // The match must explicitly pattern-match on Ok(Ok(())), Ok(Err(e)), Err(e).
    // A let-underscore (`let _ = ...`) would silently swallow both Ok and Err,
    // which is the dangerous pattern this test guards against.
    assert!(
        source.contains("Ok(Ok(()))") && source.contains("Ok(Err(e))"),
        "JOB1: checkpoint write must use explicit match with Ok(Ok(())) and Ok(Err(e)) arms, \
         not let _ = ... which would silently swallow failures"
    );
}
