use super::*;
use anyhow::ensure;
use std::time::Duration;

fn owned_case(action: impl AsyncFnOnce(&Path) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(action(owned.path()))
    };
    let cleanup = owned.close();
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Owned fixture cleanup failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}

#[test]
fn deletion_after_restart_removes_a_stale_running_job_without_a_live_owner() -> anyhow::Result<()> {
    owned_case(async |root| {
        let engram = fixture_at(root, "fts_only");
        engram.state.registry.put_job(&engram_core::JobRecord {
            job_id: "interrupted-before-restart".into(),
            kind: "index_project".into(),
            project_id: Some(PROJECT.into()),
            status: "running".into(),
            message: "interrupted".into(),
            progress_pct: 0,
            estimated_time_remaining_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        })?;
        let config = (*engram.state.cfg).clone();
        drop(engram);
        let (state, _) = AppState::new(config)?;
        let engram = Engram::new(state);
        ensure!(engram.state.active_jobs.read().await.is_empty());
        ensure!(engram.state.cancellation_tokens.read().await.is_empty());
        ensure!(engram.state.registry.list_jobs(Some(PROJECT))?.len() == 1);
        ensure!(
            engram
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into()
                })
                .await?
                .is_error
                != Some(true)
        );
        ensure!(engram.state.registry.get_project(PROJECT)?.is_none());
        ensure!(engram.state.registry.list_jobs(Some(PROJECT))?.is_empty());
        Ok(())
    })
}

#[test]
fn cancelling_delete_rpc_during_job_drain_still_finishes_owned_deletion() -> anyhow::Result<()> {
    owned_case(async |root| {
        let engram = fixture_at(
            root,
            if cfg!(feature = "vector") {
                "local"
            } else {
                "fts_only"
            },
        );
        let project_dir = engram.state.cfg.data_dir.join("projects").join(PROJECT);
        let lane = engram.state.acquire_project_update_lock(PROJECT).await;
        let job = engram
            .spawn_job_index_directory(
                PROJECT.into(),
                "general".into(),
                root.join("repo"),
                project_dir.join("tantivy"),
                project_dir.join("lancedb"),
            )
            .await?;
        let cancellation = engram
            .state
            .cancellation_tokens
            .read()
            .await
            .get(&job)
            .unwrap()
            .clone();
        let caller = Engram::new(engram.state.clone());
        let request = tokio::spawn(async move {
            caller
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into(),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(30), cancellation.cancelled()).await?;
        request.abort();
        ensure!(request.await.unwrap_err().is_cancelled());
        drop(lane);
        tokio::time::timeout(Duration::from_secs(30), async {
            while engram
                .state
                .registry
                .get_project(PROJECT)
                .unwrap()
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        // Registry removal precedes physical cleanup; wait on both actual
        // deletion lanes rather than interpreting metadata absence as teardown.
        let _update = engram.state.acquire_project_update_lock(PROJECT).await;
        let _runtime = engram.state.acquire_project_runtime_lock(PROJECT).await;
        ensure!(!project_dir.exists());
        ensure!(engram.state.registry.list_jobs(Some(PROJECT))?.is_empty());
        ensure!(
            engram
                .state
                .active_indexing_count
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
        );
        Ok(())
    })
}

#[test]
fn deletion_refuses_late_job_admission_then_retry_removes_settled_metadata() -> anyhow::Result<()> {
    owned_case(async |root| {
        let engram = fixture_at(
            root,
            if cfg!(feature = "vector") {
                "local"
            } else {
                "fts_only"
            },
        );
        engram.handle_repair_project(serde_json::from_value(json!({
            "project_id": PROJECT, "scope": "full", "max_commits": 0, "index_antipatterns": false
        }))?).await?;
        finish_owned_jobs(&engram).await;
        let head = engram.state.registry.capture_source_head(PROJECT)?;
        let held = engram.state.acquire_project_update_lock(PROJECT).await;
        let lane = engram
            .state
            .project_update_locks
            .read()
            .await
            .get(PROJECT)
            .unwrap()
            .clone();
        let caller = Engram::new(engram.state.clone());
        let request = tokio::spawn(async move {
            caller
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into(),
                })
                .await
        });
        // Map + held guard + observer own three references. A fourth means
        // deletion has finished its initial job snapshot and queued on the lane.
        tokio::time::timeout(Duration::from_secs(30), async {
            while Arc::strong_count(&lane) < 4 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        engram
            .spawn_job_update_project(PROJECT.into(), head.generation() + 1, 0, false, vec![])
            .await?;
        drop(held);
        let response = request.await?;
        ensure!(
            response.as_ref().err().is_some_and(|error| error
                .message
                .contains("job entered while deletion was waiting")),
            "Late job admission was not refused: {response:?}"
        );
        finish_owned_jobs(&engram).await;
        ensure!(engram.state.registry.source_head_is_current(&head)?);
        ensure!(
            engram
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into()
                })
                .await?
                .is_error
                != Some(true)
        );
        ensure!(engram.state.registry.list_jobs(Some(PROJECT))?.is_empty());
        ensure!(
            !engram
                .state
                .cfg
                .data_dir
                .join("projects")
                .join(PROJECT)
                .exists()
        );
        // Registration itself is fenced after deletion; no failed-job rows or
        // physical stores may be created for the deleted identifier.
        ensure!(
            engram
                .spawn_job_update_project(PROJECT.into(), 2, 0, false, vec![])
                .await
                .is_err()
        );
        ensure!(engram.state.registry.list_jobs(Some(PROJECT))?.is_empty());
        Ok(())
    })
}

#[test]
fn deletion_refuses_a_source_only_reader_without_changing_published_data_and_retries()
-> anyhow::Result<()> {
    owned_case(async |root| {
        let engram = fixture_at(
            root,
            if cfg!(feature = "vector") {
                "local"
            } else {
                "fts_only"
            },
        );
        engram.handle_repair_project(serde_json::from_value(json!({
            "project_id": PROJECT, "scope": "full", "max_commits": 0, "index_antipatterns": false
        }))?).await?;
        finish_owned_jobs(&engram).await;
        let head = engram.state.registry.capture_source_head(PROJECT)?;
        let ps = engram.ensure_project_runtime(PROJECT).await?;
        let reader = ps
            .search
            .pin_source_generation(PROJECT, head.generation())?;
        drop(ps);
        engram.state.projects.remove(PROJECT);
        let response = engram
            .handle_delete_project(ProjectIdRequest {
                project_id: PROJECT.into(),
            })
            .await;
        ensure!(
            response
                .as_ref()
                .err()
                .is_some_and(|error| error.message.contains("active readers or workers")),
            "Source-only ownership did not refuse deletion: {response:?}"
        );
        ensure!(engram.state.registry.source_head_is_current(&head)?);
        let ps = engram.ensure_project_runtime(PROJECT).await?;
        complete_foreground_source(
            &ps.search,
            head.generation(),
            "new_marker",
            "pub fn new_marker() -> u32 { 2 }\n",
        )
        .await?;
        drop(ps);
        drop(reader);
        ensure!(
            engram
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into()
                })
                .await?
                .is_error
                != Some(true)
        );
        ensure!(
            !engram
                .state
                .cfg
                .data_dir
                .join("projects")
                .join(PROJECT)
                .exists()
        );
        ensure!(engram.state.registry.get_project(PROJECT)?.is_none());
        eprintln!(
            "PROJECT_DELETION_READER_RETRY native_vectors={}",
            cfg!(feature = "vector")
        );
        Ok(())
    })
}

#[test]
fn deletion_drains_a_queued_index_job_without_recreating_stores_or_job_records()
-> anyhow::Result<()> {
    owned_case(async |root| {
        let engram = fixture_at(
            root,
            if cfg!(feature = "vector") {
                "local"
            } else {
                "fts_only"
            },
        );
        let project_dir = engram.state.cfg.data_dir.join("projects").join(PROJECT);
        let lane = engram.state.acquire_project_update_lock(PROJECT).await;
        let job = engram
            .spawn_job_index_directory(
                PROJECT.into(),
                "general".into(),
                root.join("repo"),
                project_dir.join("tantivy"),
                project_dir.join("lancedb"),
            )
            .await?;
        let cancellation = engram
            .state
            .cancellation_tokens
            .read()
            .await
            .get(&job)
            .unwrap()
            .clone();
        let caller = Engram::new(engram.state.clone());
        let deletion = tokio::spawn(async move {
            caller
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into(),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(30), cancellation.cancelled()).await?;
        ensure!(
            !deletion.is_finished(),
            "Deletion completed while a queued index job still owned admission"
        );
        drop(lane);
        ensure!(deletion.await??.is_error != Some(true));
        finish_owned_jobs(&engram).await;
        ensure!(
            !project_dir.exists(),
            "A queued job recreated deleted stores"
        );
        ensure!(engram.state.registry.get_project(PROJECT)?.is_none());
        ensure!(
            engram.state.registry.list_jobs(Some(PROJECT))?.is_empty(),
            "Job settlement recreated deleted metadata"
        );
        ensure!(
            engram
                .state
                .active_indexing_count
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
        );
        eprintln!(
            "PROJECT_DELETION_QUEUED_JOB native_vectors={}",
            cfg!(feature = "vector")
        );
        Ok(())
    })
}

#[cfg(windows)]
#[test]
fn deletion_reports_physical_cleanup_failure_and_retry_removes_the_remaining_directory()
-> anyhow::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    owned_case(async |root| {
        let engram = fixture_at(root, "fts_only");
        let project_dir = engram.state.cfg.data_dir.join("projects").join(PROJECT);
        std::fs::create_dir_all(&project_dir)?;
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(project_dir.join("owned-cleanup-obstruction"))?;
        let response = engram
            .handle_delete_project(ProjectIdRequest {
                project_id: PROJECT.into(),
            })
            .await;
        drop(held);
        ensure!(
            response
                .as_ref()
                .err()
                .is_some_and(|error| error.message.contains("physical cleanup failed")
                    && error.message.contains("retry delete_project")),
            "Failed cleanup was not reported with recovery: {response:?}"
        );
        ensure!(project_dir.exists());
        ensure!(engram.state.registry.get_project(PROJECT)?.is_none());
        ensure!(
            engram
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into()
                })
                .await?
                .is_error
                != Some(true)
        );
        ensure!(!project_dir.exists());
        Ok(())
    })
}

// Keep runtime shutdown inside the fixture lifetime, including assertion unwind.
#[test]
fn deletion_waits_for_foreground_publication_and_preserves_the_serialization_lane()
-> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let engram = fixture_at(owned.path(), if cfg!(feature = "vector") { "local" } else { "fts_only" });
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id": PROJECT, "scope": "full", "max_commits": 0, "index_antipatterns": false
            }))?).await?;
            finish_owned_jobs(&engram).await;
            let record = engram.state.registry.get_project(PROJECT)?.unwrap();
            let head = engram.state.registry.active_source_generation(PROJECT)?;
            let lane = engram.state.project_update_locks.read().await.get(PROJECT).unwrap().clone();
            std::fs::write(owned.path().join("repo/source.rs"), "pub fn before_delete_marker() -> u32 { 55 }\n")?;
            let pause = ForegroundPublicationPause {
                project: PROJECT.into(), after_commit: false, cancel: false, fail_step: None,
                entered: Arc::new(tokio::sync::Notify::new()), resume: Arc::new(tokio::sync::Notify::new()),
            };
            let registry_id = Arc::as_ptr(&engram.state.registry) as usize;
            FOREGROUND_PUBLICATION_PAUSES.lock().unwrap().insert(registry_id, pause.clone());
            let release = ResumeForegroundPublication { registry_id, resume: pause.resume.clone() };
            let caller = Engram::new(engram.state.clone());
            let request = tokio::spawn(async move {
                caller.handle_update_project(serde_json::from_value(json!({
                    "project_id": PROJECT, "wait": true, "max_commits": 0, "index_antipatterns": false
                })).unwrap()).await
            });
            tokio::time::timeout(Duration::from_secs(30), pause.entered.notified()).await?;
            let caller = Engram::new(engram.state.clone());
            let mut deletion = tokio::spawn(async move {
                caller.handle_delete_project(ProjectIdRequest { project_id: PROJECT.into() }).await
            });
            // A live publisher owns the lane. Delete must not erase its stores,
            // registry authority or lane while that operation is still admitted.
            let early = tokio::time::timeout(Duration::from_millis(200), &mut deletion).await;
            let kept_head = engram.state.registry.active_source_generation(PROJECT).ok() == Some(head);
            let kept_record = engram.state.registry.get_project(PROJECT)?.is_some();
            let kept_lane = engram.state.project_update_locks.read().await.get(PROJECT)
                .is_some_and(|current| Arc::ptr_eq(current, &lane));
            drop(release);
            let updated = request.await?;
            let (returned_early, deleted) = match early {
                Ok(result) => (true, result?),
                Err(_) => (false, deletion.await?),
            };
            finish_owned_jobs(&engram).await;
            ensure!(!returned_early && kept_head && kept_record && kept_lane,
                "Delete overtook an admitted foreground publisher: early={returned_early}, head={kept_head}, record={kept_record}, lane={kept_lane}");
            ensure!(updated?.is_error != Some(true));
            ensure!(deleted?.is_error != Some(true));
            ensure!(engram.state.registry.get_project(PROJECT)?.is_none());
            ensure!(!engram.state.cfg.data_dir.join("projects").join(PROJECT).exists(), "Successful deletion left physical project stores");
            ensure!(engram.ensure_project_runtime(PROJECT).await.is_err(), "A deleted project reopened from stale cache");
            let next_lane = engram.state.acquire_project_update_lock(PROJECT).await;
            ensure!(Arc::ptr_eq(engram.state.project_update_locks.read().await.get(PROJECT).unwrap(), &lane),
                "Deletion replaced the mutex still held by queued operations");
            drop(next_lane);
            // A subsequent registration starts with fresh storage and can index.
            engram.state.registry.register_source_project(&record)?;
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id": PROJECT, "scope": "full", "max_commits": 0, "index_antipatterns": false
            }))?).await?;
            let fresh = engram.ensure_project_runtime(PROJECT).await?;
            let generation = engram.state.registry.active_source_generation(PROJECT)?;
            complete_foreground_source(&fresh.search, generation, "before_delete_marker", "pub fn before_delete_marker() -> u32 { 55 }\n").await?;
            eprintln!("PROJECT_DELETION_SETTLEMENT {}", json!({"native_vectors": cfg!(feature = "vector"), "old_generation": head, "fresh_generation": generation}));
            finish_owned_jobs(&engram).await;
            Ok::<(), anyhow::Error>(())
        })
    };
    let cleanup = owned.close();
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Owned fixture cleanup failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
