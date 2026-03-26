use crate::state::AppState;
use crate::utils::now_ms;
use engram_core::JobRecord;

/// Cancel a running job by its ID. Returns true if the job was found and cancelled.
pub async fn cancel_job_internal(state: &AppState, job_id: &str) -> bool {
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
            tokio::task::spawn_blocking(move || reg2.get_job(&jid2))
                .await
                .ok()
                .and_then(|r| r.ok())
                .flatten()
        };

        if let Err(e) = tokio::task::spawn_blocking(move || {
            let now = now_ms();
            let jr = JobRecord {
                job_id: jid.clone(),
                kind: original
                    .as_ref()
                    .map(|j| j.kind.clone())
                    .unwrap_or_else(|| "unknown".into()),
                project_id: original.as_ref().and_then(|j| j.project_id.clone()),
                status: "cancelled".into(),
                message: "cancelled by user".into(),
                progress_pct: 0,
                estimated_time_remaining_ms: None,
                created_at_ms: original
                    .as_ref()
                    .map(|j| j.created_at_ms)
                    .unwrap_or_else(now_ms),
                updated_at_ms: now,
            };
            if let Err(e) = reg.put_job(&jr) {
                tracing::warn!(job_id = %jid, "failed to persist cancelled-job tombstone: {e}");
            }
        })
        .await
        {
            tracing::warn!(job_id = %job_id, "spawn_blocking panicked writing cancelled-job tombstone: {e}");
        }
        true
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
                tokio::task::spawn_blocking(move || reg2.get_job(&jid2))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten()
            };
            if let Err(e) = tokio::task::spawn_blocking(move || {
                let now = now_ms();
                let jr = JobRecord {
                    job_id: jid.clone(),
                    kind: original
                        .as_ref()
                        .map(|j| j.kind.clone())
                        .unwrap_or_else(|| "unknown".into()),
                    project_id: original.as_ref().and_then(|j| j.project_id.clone()),
                    status: "cancelled".into(),
                    message: "cancelled by user (token/handle divergence recovery)".into(),
                    progress_pct: 0,
                    estimated_time_remaining_ms: None,
                    created_at_ms: original
                        .as_ref()
                        .map(|j| j.created_at_ms)
                        .unwrap_or_else(now_ms),
                    updated_at_ms: now,
                };
                if let Err(e) = reg.put_job(&jr) {
                    tracing::warn!(job_id = %jid, "failed to persist cancelled-job tombstone (divergence): {e}");
                }
            })
            .await
            {
                tracing::warn!(job_id = %job_id, "spawn_blocking panicked writing cancelled-job tombstone (divergence): {e}");
            }
            true
        } else {
            false
        }
    }
}
