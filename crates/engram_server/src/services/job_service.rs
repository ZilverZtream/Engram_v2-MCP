use crate::state::AppState;
use crate::utils::now_ms;
use engram_core::{JobPhase, JobRecord};

/// Cancellation acknowledgement is distinct from the producer's final result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationOutcome {
    CancelledWithTombstone,
    CancelledWithoutTombstone,
    /// The producer settled and its persisted result was preserved verbatim.
    Settled {
        status: String,
        message: String,
        cancellation_signalled: bool,
    },
    /// Ownership or persistence prevents confirming that the job has stopped.
    Unconfirmed {
        message: String,
        cancellation_signalled: bool,
    },
    NotFound,
}

impl CancellationOutcome {
    /// Whether cancellation was signalled/recorded, not proof that work stopped.
    pub fn was_cancelled(&self) -> bool {
        match self {
            Self::CancelledWithTombstone | Self::CancelledWithoutTombstone => true,
            Self::Settled {
                cancellation_signalled,
                ..
            }
            | Self::Unconfirmed {
                cancellation_signalled,
                ..
            } => *cancellation_signalled,
            Self::NotFound => false,
        }
    }
}

fn terminal(status: &str) -> bool {
    matches!(
        status,
        "done" | "completed" | "degraded" | "failed" | "cancelled"
    )
}

fn unconfirmed(message: impl Into<String>, cancellation_signalled: bool) -> CancellationOutcome {
    CancellationOutcome::Unconfirmed {
        message: message.into(),
        cancellation_signalled,
    }
}

async fn read_job(state: &AppState, job_id: &str) -> Result<Option<JobRecord>, String> {
    let registry = state.registry.clone();
    let job_id = job_id.to_owned();
    tokio::task::spawn_blocking(move || registry.get_job(&job_id))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

/// Dropping the requesting RPC must not interrupt cooperative settlement.
pub async fn cancel_job_internal(state: &AppState, job_id: &str) -> CancellationOutcome {
    let state = state.clone();
    let job_id = job_id.to_owned();
    match tokio::spawn(async move { cancel_job_settled(&state, &job_id).await }).await {
        Ok(outcome) => outcome,
        Err(error) => unconfirmed(
            format!(
                "Cancellation observer failed: {error}; inspect get_job_status before retrying"
            ),
            false,
        ),
    }
}

async fn cancel_job_settled(state: &AppState, job_id: &str) -> CancellationOutcome {
    // Retain producer-owned tracking. Deletion and other cancellation callers
    // must still see an unsettled job. Never await while holding either map.
    let token = state.cancellation_tokens.read().await.get(job_id).cloned();
    let handle_state = state
        .active_jobs
        .read()
        .await
        .get(job_id)
        .map(|handle| handle.is_finished());
    if let Some(token) = &token {
        token.cancel();
    }
    let signalled = token.is_some();
    if token.is_none() && handle_state.is_none() {
        return cancel_stale_job(state, job_id).await;
    }
    if handle_state.is_none() {
        return unconfirmed(
            "Cancellation requested, but no task handle is available to confirm settlement; inspect get_job_status",
            signalled,
        );
    }
    if !signalled {
        return unconfirmed(
            "Job has no cancellation token; its task remains tracked and cancellation cannot certify its cleanup",
            false,
        );
    }

    loop {
        let (present, finished) = {
            let mut handles = state.active_jobs.write().await;
            let present = handles.contains_key(job_id);
            let finished = if handles
                .get(job_id)
                .is_some_and(|handle| handle.is_finished())
            {
                handles.remove(job_id)
            } else {
                None
            };
            (present, finished)
        };
        if let Some(handle) = finished {
            // Only a finished producer is reaped here. Live producers remove
            // their own entries after persisting finalization/checkpoint state.
            let result = handle.await;
            if let Err(error) = result {
                return unconfirmed(
                    format!(
                        "Worker ended abnormally: {error}; source publication is not determined by cancellation. Inspect project_info and get_job_status"
                    ),
                    signalled,
                );
            }
            state.cancellation_tokens.write().await.remove(job_id);
            break;
        }
        if !present && !state.cancellation_tokens.read().await.contains_key(job_id) {
            // Deletion may have taken the handle to drain it. The producer's
            // token remains visible until its finalization has finished.
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    match read_job(state, job_id).await {
        Ok(Some(job)) if terminal(&job.status) => CancellationOutcome::Settled {
            status: job.status,
            message: job.message,
            cancellation_signalled: signalled,
        },
        Ok(Some(_)) => unconfirmed(
            "Worker tracking ended without a persisted terminal result; source publication is not determined. Inspect project_info and get_job_status",
            signalled,
        ),
        Ok(None) => unconfirmed(
            "Job record is no longer present; cancellation did not recreate it",
            signalled,
        ),
        Err(error) => unconfirmed(
            format!("Could not read the settled job result: {error}"),
            signalled,
        ),
    }
}

async fn cancel_stale_job(state: &AppState, job_id: &str) -> CancellationOutcome {
    // Preserve successful/committed terminal results, including checkpoints.
    let original = match read_job(state, job_id).await {
        Ok(job) => job,
        Err(error) => return unconfirmed(format!("Cannot inspect stale job: {error}"), false),
    };
    if original.as_ref().is_some_and(|job| terminal(&job.status)) {
        return CancellationOutcome::NotFound;
    }
    let checkpoint = {
        let store = state.checkpoints.clone();
        let id = job_id.to_owned();
        match tokio::task::spawn_blocking(move || store.get(&id)).await {
            Ok(Ok(checkpoint)) => checkpoint,
            result => {
                return unconfirmed(
                    format!("Cannot inspect stale checkpoint: {result:?}"),
                    false,
                );
            }
        }
    };
    let project = original
        .as_ref()
        .and_then(|job| job.project_id.as_deref())
        .or_else(|| {
            checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.project_id.as_str())
        });
    let runtime = match project {
        Some(project) => Some(state.acquire_project_runtime_lock(project).await),
        None => None,
    };
    // Registration/deletion may have won while this request waited for the lane.
    if state.cancellation_tokens.read().await.contains_key(job_id)
        || state.active_jobs.read().await.contains_key(job_id)
    {
        return unconfirmed(
            "Job became active while stale cancellation was waiting; retry cancel_job",
            false,
        );
    }
    let registry = state.registry.clone();
    let checkpoints = state.checkpoints.clone();
    let job_id = job_id.to_owned();
    let had_record = original.is_some();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let _runtime = runtime;
        let current = registry.get_job(&job_id)?;
        if current.as_ref().is_some_and(|job| terminal(&job.status))
            || (had_record && current.is_none())
        {
            return Ok(());
        }
        // Re-read under lifecycle exclusion; never put a captured old record
        // back after deletion. No new record is invented for an unknown job.
        if let Some(mut checkpoint) = checkpoints.get(&job_id)?
            && checkpoint.is_resumable()
        {
            checkpoint.phase = JobPhase::Failed;
            checkpoint.error = Some("cancelled by user".into());
            checkpoint.updated_at_ms = now_ms();
            checkpoints.put(&checkpoint)?;
        }
        if let Some(mut job) = current
            && matches!(job.status.as_str(), "running" | "queued" | "pending")
        {
            job.status = "cancelled".into();
            job.message = "cancelled: stale job tombstoned on cancel request".into();
            job.estimated_time_remaining_ms = None;
            job.updated_at_ms = now_ms();
            registry.put_job(&job)?;
        }
        Ok(())
    })
    .await
    {
        Ok(Ok(())) => CancellationOutcome::NotFound,
        result => unconfirmed(
            format!(
                "Stale cancellation metadata could not be persisted: {result:?}; resumption may still be possible"
            ),
            false,
        ),
    }
}

#[cfg(test)]
mod cancellation_tests;
