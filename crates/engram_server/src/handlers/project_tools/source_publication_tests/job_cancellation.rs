use super::*;
use anyhow::{Result, ensure};

#[test]
fn public_cancel_after_source_commit_preserves_the_committed_job_outcome() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let engram = fixture_at(owned.path(), if cfg!(feature="vector") {"local"} else {"fts_only"});
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":0,"index_antipatterns":false
            }))?).await?;
            finish_owned_jobs(&engram).await;
            let previous = engram.state.registry.active_source_generation(PROJECT)?;
            let source = "pub fn public_cancel_committed() -> u32 { 359 }\n";
            std::fs::write(owned.path().join("repo/source.rs"), source)?;
            let pause = PublicationPause {
                project:PROJECT.into(), after_commit:true, fail_cleanup:false,
                entered:Arc::new(tokio::sync::Notify::new()), resume:Arc::new(tokio::sync::Notify::new()),
            };
            let registry_id = Arc::as_ptr(&engram.state.registry) as usize;
            BACKGROUND_PUBLICATION_PAUSES.lock().unwrap_or_else(|e|e.into_inner()).insert(registry_id,pause.clone());
            let release = ResumePublication {registry_id,resume:pause.resume.clone()};
            engram.handle_update_project(serde_json::from_value(json!({
                "project_id":PROJECT,"wait":false,"max_commits":0,"index_antipatterns":false
            }))?).await?;
            tokio::time::timeout(std::time::Duration::from_secs(30),pause.entered.notified()).await?;
            let job = engram.state.registry.list_jobs(Some(PROJECT))?.into_iter()
                .find(|job|job.kind=="update_project" && job.status=="running")
                .ok_or_else(||anyhow::anyhow!("Expected paused running update"))?;
            let token = engram.state.cancellation_tokens.read().await.get(&job.job_id).cloned()
                .ok_or_else(||anyhow::anyhow!("Expected live update cancellation token"))?;
            let cancel_request = tokio::spawn({
                let engram = engram.clone();
                let job_id = job.job_id.clone();
                async move {engram.handle_cancel_job(CancelJobRequest {job_id}).await}
            });
            let signalled = tokio::time::timeout(std::time::Duration::from_secs(30),token.cancelled()).await;
            // Always release the real worker before observing cancellation's
            // terminal response; a cooperative implementation must drain it.
            drop(release);
            let cancellation = cancel_request.await?;
            finish_owned_jobs(&engram).await;
            signalled?;
            let cancellation = cancellation?;
            let head = engram.state.registry.active_source_generation(PROJECT)?;
            ensure!(head==previous+1,"Postcommit cancellation lost the published head");
            let ps = engram.ensure_project_runtime(PROJECT).await?;
            complete_foreground_source(&ps.search,head,"public_cancel_committed",source).await?;
            drop(ps);
            let status = engram.handle_get_job_status(CancelJobRequest {job_id:job.job_id.clone()}).await?;
            let report = &status.content[0].as_text().unwrap().text;
            let stored = engram.state.registry.get_job(&job.job_id)?.unwrap();
            ensure!(stored.kind==job.kind && stored.project_id==job.project_id,"Cancellation lost job provenance");
            ensure!(stored.status=="degraded" && stored.progress_pct==100
                && report.contains(&format!("Source generation {head} was published")),
                "Public cancel erased the committed outcome: {report}; cancellation={cancellation:?}");
            ensure!(cancellation.content[0].as_text().unwrap().text.contains(&format!("Source generation {head} was published")),
                "Cancellation response must expose the committed outcome");
            let config = (*engram.state.cfg).clone();
            drop(engram);
            let (state, _) = AppState::new(config)?;
            let reopened = Engram::new(state);
            let cold = reopened.handle_get_job_status(CancelJobRequest {job_id:job.job_id}).await?;
            ensure!(cold.content[0].as_text().unwrap().text == *report);
            let ps = reopened.ensure_project_runtime(PROJECT).await?;
            complete_foreground_source(&ps.search,head,"public_cancel_committed",source).await?;
            Ok::<(),anyhow::Error>(())
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

fn owned_case(action: impl AsyncFnOnce(&std::path::Path) -> Result<()>) -> Result<()> {
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

async fn staged_precommit(
    root: &std::path::Path,
) -> Result<(Engram, u64, String, CancellationToken, ResumePublication)> {
    let engram = fixture_at(
        root,
        if cfg!(feature = "vector") {
            "local"
        } else {
            "fts_only"
        },
    );
    engram
        .handle_repair_project(serde_json::from_value(json!({
            "project_id":PROJECT,"scope":"full","max_commits":0,"index_antipatterns":false
        }))?)
        .await?;
    finish_owned_jobs(&engram).await;
    let previous = engram.state.registry.active_source_generation(PROJECT)?;
    std::fs::write(
        root.join("repo/source.rs"),
        "pub fn public_cancel_retry() -> u32 { 761 }\n",
    )?;
    let pause = PublicationPause {
        project: PROJECT.into(),
        after_commit: false,
        fail_cleanup: false,
        entered: Arc::new(tokio::sync::Notify::new()),
        resume: Arc::new(tokio::sync::Notify::new()),
    };
    let registry_id = Arc::as_ptr(&engram.state.registry) as usize;
    BACKGROUND_PUBLICATION_PAUSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(registry_id, pause.clone());
    let release = ResumePublication {
        registry_id,
        resume: pause.resume.clone(),
    };
    engram
        .handle_update_project(serde_json::from_value(json!({
            "project_id":PROJECT,"wait":false,"max_commits":0,"index_antipatterns":false
        }))?)
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(30), pause.entered.notified()).await?;
    let job = engram
        .state
        .registry
        .list_jobs(Some(PROJECT))?
        .into_iter()
        .find(|job| job.kind == "update_project" && job.status == "running")
        .ok_or_else(|| anyhow::anyhow!("Expected paused running update"))?;
    let token = engram
        .state
        .cancellation_tokens
        .read()
        .await
        .get(&job.job_id)
        .unwrap()
        .clone();
    Ok((engram, previous, job.job_id, token, release))
}

#[test]
fn public_precommit_cancellation_preserves_old_source_and_allows_retry() -> Result<()> {
    owned_case(async |root| {
        let (engram, previous, job_id, token, release) = staged_precommit(root).await?;
        let caller = engram.clone();
        let request_id = job_id.clone();
        let request = tokio::spawn(async move {
            caller
                .handle_cancel_job(CancelJobRequest { job_id: request_id })
                .await
        });
        let signalled =
            tokio::time::timeout(std::time::Duration::from_secs(30), token.cancelled()).await;
        let pending = !request.is_finished();
        let tracked = engram.state.active_jobs.read().await.contains_key(&job_id)
            && engram
                .state
                .cancellation_tokens
                .read()
                .await
                .contains_key(&job_id);
        drop(release);
        let response = request.await?;
        finish_owned_jobs(&engram).await;
        signalled?;
        ensure!(
            pending && tracked,
            "Cancellation hid or reported settlement of a still-paused worker"
        );
        ensure!(
            response?.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("status: cancelled")
        );
        ensure!(engram.state.registry.active_source_generation(PROJECT)? == previous);
        let ps = engram.ensure_project_runtime(PROJECT).await?;
        complete_foreground_source(
            &ps.search,
            previous,
            "new_marker",
            "pub fn new_marker() -> u32 { 2 }\n",
        )
        .await?;
        let job = engram.state.registry.get_job(&job_id)?.unwrap();
        ensure!(job.status == "cancelled" && job.progress_pct == 0);
        ensure!(
            !engram
                .state
                .checkpoints
                .get(&job_id)?
                .unwrap()
                .is_resumable()
        );
        engram
            .handle_update_project(serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":0,"index_antipatterns":false
            }))?)
            .await?;
        let current = engram.state.registry.active_source_generation(PROJECT)?;
        ensure!(current > previous);
        complete_foreground_source(
            &ps.search,
            current,
            "public_cancel_retry",
            "pub fn public_cancel_retry() -> u32 { 761 }\n",
        )
        .await?;
        Ok(())
    })
}

#[test]
fn abandoned_cancel_rpc_keeps_worker_visible_until_concurrent_deletion_settles() -> Result<()> {
    owned_case(async |root| {
        let (engram, _, job_id, token, release) = staged_precommit(root).await?;
        let caller = engram.clone();
        let request_id = job_id.clone();
        let request = tokio::spawn(async move {
            caller
                .handle_cancel_job(CancelJobRequest { job_id: request_id })
                .await
        });
        let signalled =
            tokio::time::timeout(std::time::Duration::from_secs(30), token.cancelled()).await;
        request.abort();
        let request_end = request.await;
        let tracked = engram.state.active_jobs.read().await.contains_key(&job_id)
            && engram
                .state
                .cancellation_tokens
                .read()
                .await
                .contains_key(&job_id);
        let caller = engram.clone();
        let deleting = tokio::spawn(async move {
            caller
                .handle_delete_project(ProjectIdRequest {
                    project_id: PROJECT.into(),
                })
                .await
        });
        let draining = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while engram.state.active_jobs.read().await.contains_key(&job_id) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        let pending = !deleting.is_finished();
        drop(release);
        let deleted = deleting.await?;
        finish_owned_jobs(&engram).await;
        signalled?;
        draining?;
        ensure!(request_end.is_err_and(|error| error.is_cancelled()));
        ensure!(
            tracked && pending,
            "Abandoned cancellation let deletion overtake the paused worker"
        );
        deleted?;
        ensure!(engram.state.registry.get_project(PROJECT)?.is_none());
        ensure!(engram.state.registry.get_job(&job_id)?.is_none());
        ensure!(
            !engram
                .state
                .cfg
                .data_dir
                .join("projects")
                .join(PROJECT)
                .exists()
        );
        ensure!(!engram.state.active_jobs.read().await.contains_key(&job_id));
        ensure!(
            !engram
                .state
                .cancellation_tokens
                .read()
                .await
                .contains_key(&job_id)
        );
        Ok(())
    })
}
