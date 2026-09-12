//! Daemon-start warm-up (external audit 2026-08-29 P0-3, the ≤ 5 s gate).
//!
//! Release 23 live: the FIRST `get_change_set` after a daemon restart took
//! 38 s — 24 s opening the project runtime (tantivy + LanceDB) and 5 s loading
//! the co-change snapshot. That is the daemon's work, not the first user's:
//! a bounded set of recently updated projects is opened in the background.
//! Warming the entire registry would evict earlier runtimes and retain derived
//! caches for projects the user may never request. Other projects load on demand.

use crate::state::{AppState, MAX_CACHED_PROJECTS};

/// Most recently updated project first. Update time is a startup ordering proxy,
/// not proof of current client activity. Equal timestamps have a stable ID order.
pub fn warm_order(mut recs: Vec<engram_core::ProjectRecord>) -> Vec<engram_core::ProjectRecord> {
    recs.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.project_id.cmp(&b.project_id))
    });
    recs
}

/// Prime at most the runtime cache's capacity, selected before any runtime opens
/// or derived-cache tasks are spawned. Other registered projects remain available
/// through normal demand loading. Returns successful runtime opens, not a promise
/// that concurrent client activity has left every selected runtime cached.
pub async fn warm_all_projects(state: &AppState) -> usize {
    let recs = match state.registry.list_projects() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("warm-up: list_projects failed: {e}");
            return 0;
        }
    };
    let registered = recs.len();
    let mut recs = warm_order(recs);
    recs.truncate(MAX_CACHED_PROJECTS);
    tracing::info!(
        registered,
        selected = recs.len(),
        runtime_capacity = MAX_CACHED_PROJECTS,
        "warm-up: bounded startup selection"
    );
    // The change-set primes run concurrently (bounded) instead of one project
    // after another; the function still returns only when every prime is done.
    let prime_limit = std::sync::Arc::new(tokio::sync::Semaphore::new(3));
    let mut primes = tokio::task::JoinSet::new();
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
                let st2 = state.clone();
                let p2 = pid.clone();
                let permit = prime_limit.clone();
                primes.spawn(async move {
                    let _p = permit.acquire_owned().await;
                    let t1 = std::time::Instant::now();
                    let req: Result<crate::models::GetChangeSetRequest, _> =
                        serde_json::from_value(serde_json::json!({
                            "project_id": p2,
                            "story": "warm-up: prime the change-set caches",
                        }));
                    if let Ok(req) = req {
                        let eng = crate::tools::Engram::new(st2.clone());
                        match eng.handle_get_change_set(req).await {
                            Ok(_) => tracing::info!(
                                project_id = %p2,
                                ms = t1.elapsed().as_millis() as u64,
                                "warm-up: change-set caches primed"
                            ),
                            Err(e) => {
                                tracing::debug!(project_id = %p2, "warm-up: change-set prime skipped: {e}")
                            }
                        }
                    }
                    // Row 5: the UI family catalog behind the change-set contract.
                    let st3 = st2.clone();
                    let p3 = p2.clone();
                    match tokio::task::spawn_blocking(move || {
                        crate::services::ui_catalog::families_cached(&st3, &p3).map(|f| f.len())
                    })
                    .await
                    {
                        Ok(Ok(n)) => tracing::info!(
                            project_id = %p2,
                            families = n,
                            "warm-up: UI catalog primed"
                        ),
                        Ok(Err(e)) => {
                            tracing::debug!(project_id = %p2, "warm-up: UI catalog skipped: {e}")
                        }
                        Err(e) => {
                            tracing::debug!(project_id = %p2, "warm-up: UI catalog task failed: {e}")
                        }
                    }
                });
            }
            Err(e) => tracing::warn!(project_id = %pid, "warm-up: runtime load failed: {e}"),
        }
    }
    while let Some(r) = primes.join_next().await {
        if let Err(e) = r {
            tracing::debug!("warm-up: a prime task ended abnormally: {e}");
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
