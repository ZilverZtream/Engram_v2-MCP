use super::*;
use anyhow::ensure;
use engram_core::{Checkpoint, Config, ProjectRecord};
use std::{path::Path, sync::Arc, time::Duration};

#[test]
fn abnormal_worker_join_keeps_its_unfinished_cleanup_discoverable() -> anyhow::Result<()> {
    owned_case(async |root| {
        let state = state(root)?;
        let token = tokio_util::sync::CancellationToken::new();
        state
            .cancellation_tokens
            .write()
            .await
            .insert("abnormal".into(), token);
        state.registry.put_job(&record("abnormal", "running"))?;
        // A producer's guard can outlive its panicked task while persisting its
        // final record. Hold that separate cleanup owner until after cancellation.
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let cleaning = state.clone();
        let cleanup = tokio::spawn(async move {
            let _ = released.await;
            cleaning.registry.put_job(&record("abnormal", "failed"))?;
            cleaning
                .cancellation_tokens
                .write()
                .await
                .remove("abnormal");
            Ok::<(), anyhow::Error>(())
        });
        let worker = tokio::spawn(async {
            panic!("Owned fixture: worker failed before its cleanup settled")
        });
        state
            .active_jobs
            .write()
            .await
            .insert("abnormal".into(), worker);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !state
                .active_jobs
                .read()
                .await
                .get("abnormal")
                .unwrap()
                .is_finished()
            {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let outcome = cancel_job_internal(&state, "abnormal").await;
        let discoverable = state
            .cancellation_tokens
            .read()
            .await
            .contains_key("abnormal");
        let unfinished = state.registry.get_job("abnormal")?.unwrap().status == "running";
        let _ = release.send(());
        cleanup.await??;
        ensure!(
            matches!(
                outcome,
                CancellationOutcome::Unconfirmed {
                    cancellation_signalled: true,
                    ..
                }
            ),
            "{outcome:?}"
        );
        ensure!(
            discoverable && unfinished,
            "Cancellation hid or overwrote unfinished producer cleanup"
        );
        ensure!(state.registry.get_job("abnormal")?.unwrap().status == "failed");
        ensure!(
            !state
                .cancellation_tokens
                .read()
                .await
                .contains_key("abnormal")
        );
        Ok(())
    })
}

fn owned_case(action: impl AsyncFnOnce(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(action(owned.path()))
    };
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Owned fixture cleanup failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

fn state(root: &Path) -> anyhow::Result<AppState> {
    let data = root.join("data");
    std::fs::create_dir_all(&data)?;
    let (state, _) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root.to_path_buf()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })?;
    Ok(state)
}

fn record(id: &str, status: &str) -> JobRecord {
    JobRecord {
        job_id: id.into(),
        kind: "update_project".into(),
        project_id: Some("project".into()),
        status: status.into(),
        message: "Producer result: source generation 7 committed".into(),
        progress_pct: 100,
        estimated_time_remaining_ms: None,
        created_at_ms: 10,
        updated_at_ms: 20,
    }
}

fn checkpoint(id: &str, phase: JobPhase) -> Checkpoint {
    Checkpoint {
        job_id: id.into(),
        project_id: "project".into(),
        phase,
        items_processed: 7,
        items_total: 9,
        generation: 7,
        idempotency_key: Checkpoint::compute_idempotency_key("project", "/owned", 7),
        resume_state: None,
        updated_at_ms: 20,
        error: None,
    }
}

#[test]
fn token_only_cancellation_signals_without_claiming_settlement_or_fabricating_metadata()
-> anyhow::Result<()> {
    owned_case(async |root| {
        let state = state(root)?;
        let token = tokio_util::sync::CancellationToken::new();
        state
            .cancellation_tokens
            .write()
            .await
            .insert("token-only".into(), token.clone());
        let outcome = cancel_job_internal(&state, "token-only").await;
        ensure!(
            matches!(
                outcome,
                CancellationOutcome::Unconfirmed {
                    cancellation_signalled: true,
                    ..
                }
            ),
            "{outcome:?}"
        );
        ensure!(token.is_cancelled());
        ensure!(
            state
                .cancellation_tokens
                .read()
                .await
                .contains_key("token-only"),
            "Unsettled ownership became invisible"
        );
        ensure!(state.registry.get_job("token-only")?.is_none());
        ensure!(state.checkpoints.get("token-only")?.is_none());
        state.cancellation_tokens.write().await.remove("token-only");
        Ok(())
    })
}

#[test]
fn handle_only_cancellation_does_not_abort_or_hide_the_worker() -> anyhow::Result<()> {
    owned_case(async |root| {
        let state = state(root)?;
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_completed = completed.clone();
        let worker = tokio::spawn(async move {
            // Sender drop also releases this owned worker during unwinding.
            let _ = released.await;
            worker_completed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        state
            .active_jobs
            .write()
            .await
            .insert("handle-only".into(), worker);
        let outcome = cancel_job_internal(&state, "handle-only").await;
        let tracked_and_live = state
            .active_jobs
            .read()
            .await
            .get("handle-only")
            .is_some_and(|handle| !handle.is_finished());
        let no_job = state.registry.get_job("handle-only")?.is_none();
        let no_checkpoint = state.checkpoints.get("handle-only")?.is_none();
        let _ = release.send(());
        let worker = state.active_jobs.write().await.remove("handle-only");
        if let Some(worker) = worker {
            worker.await?;
        }
        ensure!(
            matches!(
                outcome,
                CancellationOutcome::Unconfirmed {
                    cancellation_signalled: false,
                    ..
                }
            ),
            "{outcome:?}"
        );
        ensure!(
            tracked_and_live,
            "Cancellation aborted or hid an unsignalled worker"
        );
        ensure!(completed.load(std::sync::atomic::Ordering::SeqCst));
        ensure!(
            no_job && no_checkpoint,
            "Cancellation fabricated metadata for an unknown owner"
        );
        Ok(())
    })
}

#[test]
fn terminal_producer_results_and_checkpoint_bytes_survive_repeat_cancellation() -> anyhow::Result<()>
{
    owned_case(async |root| {
        let state = state(root)?;
        for (status, phase) in [
            ("done", JobPhase::Completed),
            ("completed", JobPhase::Completed),
            ("degraded", JobPhase::Parsing),
        ] {
            let job = record(status, status);
            let checkpoint = checkpoint(status, phase);
            state.registry.put_job(&job)?;
            state.checkpoints.put(&checkpoint)?;
            let before_job = serde_json::to_value(&job)?;
            let before_checkpoint = serde_json::to_value(&checkpoint)?;
            let outcome = cancel_job_internal(&state, status).await;
            ensure!(outcome == CancellationOutcome::NotFound, "{outcome:?}");
            ensure!(serde_json::to_value(state.registry.get_job(status)?.unwrap())? == before_job);
            ensure!(
                serde_json::to_value(state.checkpoints.get(status)?.unwrap())? == before_checkpoint,
                "Cancellation rewrote the terminal producer's checkpoint for {status}"
            );
        }
        Ok(())
    })
}

#[test]
fn stale_cancellation_waiting_behind_deletion_does_not_restore_captured_metadata()
-> anyhow::Result<()> {
    owned_case(async |root| {
        let state = state(root)?;
        state.registry.put_project(&ProjectRecord {
            project_id: "project".into(),
            project_name: "Owned cancellation fixture".into(),
            project_type: "rust".into(),
            directory: root.to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        state.registry.put_job(&record("stale", "running"))?;
        state
            .checkpoints
            .put(&checkpoint("stale", JobPhase::Parsing))?;
        let deletion_lane = state.acquire_project_runtime_lock("project").await;
        let lane = state
            .project_runtime_locks
            .read()
            .await
            .get("project")
            .unwrap()
            .clone();
        let observer = state.clone();
        let request = tokio::spawn(async move { cancel_job_internal(&observer, "stale").await });
        // Map, held guard and this observer own three references; the fourth
        // proves cancellation read the stale records and queued on deletion.
        tokio::time::timeout(Duration::from_secs(10), async {
            while Arc::strong_count(&lane) < 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        // Model the owned deletion's metadata phase under its real runtime lane.
        state.registry.delete_all_for_project("project")?;
        state.checkpoints.remove("stale")?;
        drop(deletion_lane);
        ensure!(request.await? == CancellationOutcome::NotFound);
        ensure!(state.registry.get_project("project")?.is_none());
        ensure!(
            state.registry.get_job("stale")?.is_none(),
            "Cancellation recreated the deleted job"
        );
        ensure!(
            state.checkpoints.get("stale")?.is_none(),
            "Cancellation recreated the deleted checkpoint"
        );
        ensure!(state.active_jobs.read().await.is_empty());
        ensure!(state.cancellation_tokens.read().await.is_empty());
        Ok(())
    })
}
