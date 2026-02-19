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

        // Offload blocking Redb write to the blocking pool
        let reg = state.registry.clone();
        let jid = job_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            let now = now_ms();
            let jr = JobRecord {
                job_id: jid,
                kind: "unknown".into(),
                project_id: None,
                status: "cancelled".into(),
                message: "cancelled by user".into(),
                progress_pct: 0,
                estimated_time_remaining_ms: None,
                created_at_ms: now,
                updated_at_ms: now,
            };
            let _ = reg.put_job(&jr);
        })
        .await;
        true
    } else {
        false
    }
}
