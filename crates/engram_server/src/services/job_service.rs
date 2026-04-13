use crate::state::AppState;
use crate::utils::now_ms;
use engram_core::{CheckpointStore, JobPhase, JobRecord};

/// Mark any resumable checkpoint for `job_id` as Failed so that it cannot be
/// accidentally resumed after cancellation.
///
/// Returns `true` if the checkpoint was either successfully marked or was
/// already terminal (nothing to do), `false` if an I/O error prevented the
/// mark from being persisted for a resumable checkpoint.
///
/// # ENG-AUD-P1-0005
/// The previous implementation silently swallowed failures, meaning a cancelled
/// job could still be resumed.  Callers must now check the return value and
/// propagate a failure into the job's terminal metadata when `false` is returned
/// for a resumable checkpoint.
async fn mark_checkpoint_cancelled(cp_store: &CheckpointStore, job_id: &str) -> bool {
    let cp_store = cp_store.clone();
    let jid = job_id.to_string();
    match tokio::task::spawn_blocking(move || {
        match cp_store.get(&jid) {
            Ok(Some(mut cp)) if cp.is_resumable() => {
                cp.phase = JobPhase::Failed;
                cp.error = Some("cancelled by user".into());
                cp.updated_at_ms = now_ms();
                match cp_store.put(&cp) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::error!(
                            job_id = %jid,
                            "ENG-AUD-P1-0005: failed to mark resumable checkpoint as \
                             Failed on cancel — checkpoint may still be resumable: {e}"
                        );
                        false
                    }
                }
            }
            Ok(_) => true, // Already terminal or no checkpoint — nothing to do.
            Err(e) => {
                tracing::error!(
                    job_id = %jid,
                    "ENG-AUD-P1-0005: failed to read checkpoint for cancel marking — \
                     checkpoint state is unknown: {e}"
                );
                false
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(
                job_id = %job_id,
                "ENG-AUD-P1-0005: spawn_blocking panicked marking checkpoint on cancel: {e}"
            );
            false
        }
    }
}

/// Outcome of a job cancellation attempt.
///
/// ENG-AUD-2026-EXH-P1-0003: callers must distinguish between a successful
/// cancellation (with audit tombstone persisted) and a partial cancellation
/// (job stopped but tombstone write failed) to avoid treating audit gaps as
/// full success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationOutcome {
    /// Job was cancelled and its tombstone was persisted to the registry.
    CancelledWithTombstone,
    /// Job was cancelled (token fired / handle aborted) but the tombstone
    /// persistence write failed. The job will not resume, but audit/replay
    /// metadata for this cancellation may be missing.
    CancelledWithoutTombstone,
    /// No active job or handle found for the given ID.
    NotFound,
}

impl CancellationOutcome {
    /// Returns `true` if the job was cancelled (regardless of tombstone status).
    pub fn was_cancelled(&self) -> bool {
        matches!(
            self,
            Self::CancelledWithTombstone | Self::CancelledWithoutTombstone
        )
    }
}

/// Cancel a running job by its ID.
pub async fn cancel_job_internal(state: &AppState, job_id: &str) -> CancellationOutcome {
    // Take cancellation_tokens lock, extract token, then release lock
    // BEFORE acquiring active_jobs to avoid lock-order inversion.
    let token = {
        let mut tokens = state.cancellation_tokens.write().await;
        tokens.remove(job_id)
    };

    if let Some(token) = token {
        token.cancel();

        // Now safe to acquire active_jobs (no other lock held)
        {
            let mut handles = state.active_jobs.write().await;
            if let Some(h) = handles.remove(job_id) {
                h.abort();
            }
        }

        // Look up the original job record so the tombstone preserves provenance
        // (kind and project_id). Without this, post-mortem and replay logic loses
        // the ability to attribute cancellations to the correct project/job kind.
        let reg = state.registry.clone();
        let jid = job_id.to_string();
        let original = {
            let reg2 = reg.clone();
            let jid2 = jid.clone();
            match tokio::task::spawn_blocking(move || reg2.get_job(&jid2)).await {
                Ok(Ok(record)) => record,
                Ok(Err(e)) => {
                    tracing::warn!(
                        job_id = %jid,
                        "failed to read prior job record for cancel provenance: {e}"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(
                        job_id = %jid,
                        "spawn_blocking panicked reading prior job record for cancel provenance: {e}"
                    );
                    None
                }
            }
        };

        // Mark any resumable checkpoint as Failed so it cannot be accidentally resumed.
        // ENG-AUD-P1-0005: capture result and embed failure into the job tombstone.
        let cp_mark_ok = mark_checkpoint_cancelled(&state.checkpoints, job_id).await;

        // ENG-AUD-2026-EXH-P1-0003: persist tombstone and capture whether it succeeded.
        // The closure returns true iff put_job succeeded so the outer code can
        // return the appropriate CancellationOutcome.
        let tombstone_ok = match tokio::task::spawn_blocking(move || {
            let now = now_ms();
            let message = if cp_mark_ok {
                "cancelled by user".to_string()
            } else {
                "cancelled by user; WARNING: checkpoint could not be marked as failed \
                 — resumption may still be possible (ENG-AUD-P1-0005)"
                    .to_string()
            };
            let jr = JobRecord {
                job_id: jid.clone(),
                kind: original
                    .as_ref()
                    .map(|j| j.kind.clone())
                    .unwrap_or_else(|| "unknown".into()),
                project_id: original.as_ref().and_then(|j| j.project_id.clone()),
                status: "cancelled".into(),
                message,
                progress_pct: 0,
                estimated_time_remaining_ms: None,
                created_at_ms: original
                    .as_ref()
                    .map(|j| j.created_at_ms)
                    .unwrap_or_else(now_ms),
                updated_at_ms: now,
            };
            match reg.put_job(&jr) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(job_id = %jid, "ENG-AUD-2026-EXH-P1-0003: failed to persist cancelled-job tombstone: {e}");
                    false
                }
            }
        })
        .await
        {
            Ok(ok) => ok,
            Err(e) => {
                tracing::warn!(job_id = %job_id, "spawn_blocking panicked writing cancelled-job tombstone: {e}");
                false
            }
        };
        if tombstone_ok {
            CancellationOutcome::CancelledWithTombstone
        } else {
            CancellationOutcome::CancelledWithoutTombstone
        }
    } else {
        // Cancellation token not found — check for divergence where active_jobs
        // still holds a handle with no corresponding token and abort if present.
        let mut handles = state.active_jobs.write().await;
        if let Some(h) = handles.remove(job_id) {
            h.abort();
            drop(handles);
            let reg = state.registry.clone();
            let jid = job_id.to_string();
            let original = {
                let reg2 = reg.clone();
                let jid2 = jid.clone();
                match tokio::task::spawn_blocking(move || reg2.get_job(&jid2)).await {
                    Ok(Ok(record)) => record,
                    Ok(Err(e)) => {
                        tracing::warn!(
                            job_id = %jid,
                            "failed to read prior job record for cancel provenance (divergence): {e}"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            job_id = %jid,
                            "spawn_blocking panicked reading prior job record for cancel provenance (divergence): {e}"
                        );
                        None
                    }
                }
            };
            // Mark any resumable checkpoint as Failed so it cannot be accidentally resumed.
            // ENG-AUD-P1-0005: capture result and embed failure into the job tombstone.
            let cp_mark_ok = mark_checkpoint_cancelled(&state.checkpoints, job_id).await;

            // ENG-AUD-2026-EXH-P1-0003: capture tombstone persistence outcome.
            let tombstone_ok = match tokio::task::spawn_blocking(move || {
                let now = now_ms();
                let base_msg = "cancelled by user (token/handle divergence recovery)";
                let message = if cp_mark_ok {
                    base_msg.to_string()
                } else {
                    format!(
                        "{base_msg}; WARNING: checkpoint could not be marked as failed \
                         — resumption may still be possible (ENG-AUD-P1-0005)"
                    )
                };
                let jr = JobRecord {
                    job_id: jid.clone(),
                    kind: original
                        .as_ref()
                        .map(|j| j.kind.clone())
                        .unwrap_or_else(|| "unknown".into()),
                    project_id: original.as_ref().and_then(|j| j.project_id.clone()),
                    status: "cancelled".into(),
                    message,
                    progress_pct: 0,
                    estimated_time_remaining_ms: None,
                    created_at_ms: original
                        .as_ref()
                        .map(|j| j.created_at_ms)
                        .unwrap_or_else(now_ms),
                    updated_at_ms: now,
                };
                match reg.put_job(&jr) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(job_id = %jid, "ENG-AUD-2026-EXH-P1-0003: failed to persist cancelled-job tombstone (divergence): {e}");
                        false
                    }
                }
            })
            .await
            {
                Ok(ok) => ok,
                Err(e) => {
                    tracing::warn!(job_id = %job_id, "spawn_blocking panicked writing cancelled-job tombstone (divergence): {e}");
                    false
                }
            };
            if tombstone_ok {
                CancellationOutcome::CancelledWithTombstone
            } else {
                CancellationOutcome::CancelledWithoutTombstone
            }
        } else {
            // ENG-AUD-2026-S12-0001: no active token or handle was found, but a
            // resumable checkpoint may still exist for this job_id (e.g. after a
            // process restart or state divergence).  Tombstone it so the job
            // cannot be accidentally resumed by a future recovery path.
            //
            // We still return NotFound (no live job was running) but we must not
            // leave a resumable checkpoint orphaned when the caller believes the
            // cancellation request was acknowledged.
            let cp_mark_ok = mark_checkpoint_cancelled(&state.checkpoints, job_id).await;
            if !cp_mark_ok {
                tracing::warn!(
                    job_id = %job_id,
                    "ENG-AUD-2026-S12-0001: NotFound cancel path: failed to tombstone \
                     resumable checkpoint — checkpoint may still be resumable"
                );
            }
            // S14-001: if a stale registry record exists with a non-terminal status
            // (e.g. 'running'), overwrite it with a cancelled tombstone so that
            // post-mortem and replay logic sees the correct terminal state rather
            // than a dangling 'running' record.  Return NotFound regardless to
            // preserve the existing cancellation contract.
            let reg = state.registry.clone();
            let jid = job_id.to_string();
            if let Err(e) = tokio::task::spawn_blocking(move || {
                const NON_TERMINAL: &[&str] = &["running", "queued", "pending"];
                match reg.get_job(&jid) {
                    Ok(Some(existing)) if NON_TERMINAL.contains(&existing.status.as_str()) => {
                        let tombstone = JobRecord {
                            job_id: jid.clone(),
                            kind: existing.kind.clone(),
                            project_id: existing.project_id.clone(),
                            status: "cancelled".into(),
                            message: "cancelled: stale job tombstoned on cancel request".into(),
                            progress_pct: existing.progress_pct,
                            estimated_time_remaining_ms: None,
                            created_at_ms: existing.created_at_ms,
                            updated_at_ms: now_ms(),
                        };
                        if let Err(e) = reg.put_job(&tombstone) {
                            tracing::warn!(
                                job_id = %jid,
                                "S14-001: failed to write stale job tombstone: {e}"
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            job_id = %jid,
                            "S14-001: failed to read registry for stale job tombstone: {e}"
                        );
                    }
                }
            })
            .await
            {
                tracing::warn!(
                    job_id = %job_id,
                    "S14-001: spawn_blocking JoinError writing stale job tombstone: {e}"
                );
            }
            CancellationOutcome::NotFound
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{Checkpoint, CheckpointStore, JobPhase};

    /// ENG-AUD-P1-0005 + ENG-AUD-S1-0006: behavioral regression test.
    /// Verifies that mark_checkpoint_cancelled actually persists the Failed phase.
    #[tokio::test]
    async fn checkpoint_marking_failure_reflected_in_job_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = CheckpointStore::open(&tmp.path().join("checkpoints.redb"))
            .expect("open checkpoint store");

        let job_id = "test-job-cancellation-001";
        let cp = Checkpoint {
            job_id: job_id.to_string(),
            project_id: "test-project".to_string(),
            phase: JobPhase::Parsing, // Parsing is resumable (not Completed or Failed)
            items_processed: 0,
            items_total: 0,
            generation: 1,
            idempotency_key: Checkpoint::compute_idempotency_key("test-project", "/dir", 1),
            resume_state: None,
            updated_at_ms: 0,
            error: None,
        };
        store.put(&cp).expect("put checkpoint");

        let ok = mark_checkpoint_cancelled(&store, job_id).await;
        assert!(ok, "mark_checkpoint_cancelled must return true on success");

        let stored = store.get(job_id).expect("get").expect("present");
        assert_eq!(
            stored.phase,
            JobPhase::Failed,
            "checkpoint phase must be Failed after cancellation"
        );
        assert_eq!(
            stored.error.as_deref(),
            Some("cancelled by user"),
            "checkpoint error must be set to 'cancelled by user'"
        );
    }

    /// ENG-AUD-S1-0006: already-terminal checkpoints are not re-processed.
    #[tokio::test]
    async fn mark_checkpoint_cancelled_noop_on_terminal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = CheckpointStore::open(&tmp.path().join("checkpoints.redb"))
            .expect("open checkpoint store");

        let job_id = "test-job-already-done";
        let cp = Checkpoint {
            job_id: job_id.to_string(),
            project_id: "test-project".to_string(),
            phase: JobPhase::Completed,
            items_processed: 100,
            items_total: 100,
            generation: 1,
            idempotency_key: Checkpoint::compute_idempotency_key("test-project", "/dir", 1),
            resume_state: None,
            updated_at_ms: 0,
            error: None,
        };
        store.put(&cp).expect("put checkpoint");

        let ok = mark_checkpoint_cancelled(&store, job_id).await;
        assert!(
            ok,
            "mark_checkpoint_cancelled must return true for terminal checkpoints (nothing to do)"
        );

        let stored = store.get(job_id).expect("get").expect("present");
        assert_eq!(
            stored.phase,
            JobPhase::Completed,
            "terminal checkpoint phase must not be changed"
        );
    }

    /// ENG-AUD-S1-0006: non-existent checkpoint returns true (nothing to do).
    #[tokio::test]
    async fn mark_checkpoint_cancelled_noop_when_no_checkpoint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = CheckpointStore::open(&tmp.path().join("checkpoints.redb"))
            .expect("open checkpoint store");

        let ok = mark_checkpoint_cancelled(&store, "nonexistent-job-id").await;
        assert!(
            ok,
            "mark_checkpoint_cancelled must return true when there is no checkpoint (nothing to do)"
        );
    }
}
