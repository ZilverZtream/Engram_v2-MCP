//! Daemon-start warm-up (external audit 2026-08-29 P0-3, the ≤ 5 s gate).
//!
//! Release 23 live: the FIRST `get_change_set` after a daemon restart took
//! 38 s — 24 s opening the project runtime (tantivy + LanceDB) and 5 s loading
//! the co-change snapshot. That is the daemon's work, not the first user's:
//! every registered project is opened and its snapshot loaded in the
//! background right after the actors start, so the first call is warm.

use crate::state::AppState;

/// Open every registered project's runtime and load its co-change snapshot.
/// Returns the number of projects whose runtime is now cached. Failures are
/// logged per project and never abort the others.
pub async fn warm_all_projects(state: &AppState) -> usize {
    let recs = match state.registry.list_projects() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("warm-up: list_projects failed: {e}");
            return 0;
        }
    };
    let mut warmed = 0usize;
    for rec in recs {
        let pid = rec.project_id.clone();
        let t0 = std::time::Instant::now();
        match crate::services::project_service::ensure_project_runtime(state, &pid).await {
            Ok(_) => {
                let st = state.clone();
                let p = pid.clone();
                let cc = tokio::task::spawn_blocking(move || {
                    crate::handlers::planning_tools::warm_co_change_snapshot_blocking(&st, &p)
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("join: {e}")));
                if let Err(e) = cc {
                    tracing::debug!(project_id = %pid, "warm-up: co-change snapshot not loaded: {e:#}");
                }
                warmed += 1;
                tracing::info!(
                    project_id = %pid,
                    ms = t0.elapsed().as_millis() as u64,
                    "warm-up: project runtime (+ co-change snapshot) ready"
                );
                // Prime the change-set caches (node snapshot, settings prior, the
                // co-change partner path) with one background call — release 26
                // live: the first user call after a restart still took 9.6 s.
                let t1 = std::time::Instant::now();
                let req: Result<crate::models::GetChangeSetRequest, _> =
                    serde_json::from_value(serde_json::json!({
                        "project_id": pid,
                        "story": "warm-up: prime the change-set caches",
                    }));
                if let Ok(req) = req {
                    let eng = crate::tools::Engram::new(state.clone());
                    match eng.handle_get_change_set(req).await {
                        Ok(_) => tracing::info!(
                            project_id = %pid,
                            ms = t1.elapsed().as_millis() as u64,
                            "warm-up: change-set caches primed"
                        ),
                        Err(e) => {
                            tracing::debug!(project_id = %pid, "warm-up: change-set prime skipped: {e}")
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(project_id = %pid, "warm-up: runtime load failed: {e}"),
        }
    }
    warmed
}

/// Actor entry: runs once at daemon start, cancellable on shutdown.
pub async fn run_warmup(state: AppState, shutdown: tokio_util::sync::CancellationToken) {
    tokio::select! {
        _ = shutdown.cancelled() => {}
        n = warm_all_projects(&state) => {
            tracing::info!(projects = n, "warm-up complete");
        }
    }
}
