use crate::state::AppState;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// CANCEL1: accepts a shutdown token so the GC loop exits cooperatively on process shutdown
/// rather than being forcibly killed mid-purge (which could leave storage in a partial state).
///
/// JOB1: skips the purge tick when active indexing is in progress to prevent a GC generation
/// deletion from racing with an in-flight index job that is still writing to the same generation.
pub async fn run_gc_scheduler(state: AppState, shutdown: CancellationToken) {
    let tick = Duration::from_secs(3600); // Once an hour
    let mut interval = tokio::time::interval(tick);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("GC scheduler: shutdown token cancelled — exiting");
                return;
            }
            _ = interval.tick() => {}
            // TODO-40: resumed-job completion nudges an immediate sweep so
            // crash-resume loops don't accumulate stale generations for up
            // to an hour. The JOB1 active-count guard below still applies.
            _ = state.gc_nudge.notified() => {
                tracing::info!("GC: nudged (post-recovery) — running sweep now");
            }
        }

        // JOB1: skip purge when any indexing job is active to avoid racing with
        // in-flight generation writes.  The next hourly tick will retry.
        let active = state
            .active_indexing_count
            .load(std::sync::atomic::Ordering::Relaxed);
        if active > 0 {
            tracing::debug!(
                active_jobs = active,
                "GC: skipping purge tick — {active} indexing job(s) in progress (JOB1 guard)"
            );
            continue;
        }

        tracing::info!("GC: starting periodic generation purge");

        let registry = state.registry.clone();
        let project_ids: Vec<String> = tokio::task::spawn_blocking(move || {
            registry
                .list_projects()
                .map(|v| v.into_iter().map(|p| p.project_id).collect())
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();

        for pid in project_ids {
            if shutdown.is_cancelled() {
                tracing::info!("GC: shutdown requested mid-sweep — stopping");
                return;
            }
            // JOB3-2af4c8: double-check active count before each per-project purge to
            // narrow the TOCTOU window between the initial guard and the purge loop.
            // A job started between the initial load and this point would be caught here.
            let active_now = state
                .active_indexing_count
                .load(std::sync::atomic::Ordering::Relaxed);
            if active_now > 0 {
                tracing::info!(
                    active_jobs = active_now,
                    "GC: aborting mid-sweep — {active_now} indexing job(s) started since guard check (JOB3 guard)"
                );
                break;
            }
            if let Err(e) = purge_project_old_gens(&state, &pid).await {
                tracing::error!("GC error for project {}: {:?}", pid, e);
            }
        }
    }
}

pub async fn purge_project_old_gens(state: &AppState, project_id: &str) -> anyhow::Result<()> {
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let (active_gen_opt, full_gen_opt) = tokio::task::spawn_blocking(move || {
        let parse = |key: &str| {
            reg.get_meta(&pid, key)
                .ok()
                .flatten()
                .and_then(|s| s.trim().parse::<u64>().ok())
        };
        (
            parse("active_generation"),
            parse("last_full_index_generation"),
        )
    })
    .await?;

    // JOB1-m2q7: if active_generation metadata is missing or corrupt, skip this
    // project rather than defaulting to gen=1 (which could purge live data).
    let Some(active_gen) = active_gen_opt else {
        tracing::warn!(
            project_id = project_id,
            "JOB1/GC: skipping purge — active_generation metadata missing or not parseable as u64"
        );
        return Ok(());
    };

    // Purge GraphStore — baselined on the LAST FULL INDEX generation, never
    // the incremental counter. Incremental updates bump active_generation
    // while leaving unchanged files' nodes at older generations (there is
    // no graph copy-forward, unlike the search index), so purging the graph
    // against active_generation deleted almost every node between full
    // indexes and forced a full re-index after each daemon restart. The
    // per-file scoped purge in update_project_impl documents the same
    // invariant: "a GLOBAL purge is unsafe after incremental updates".
    match full_gen_opt {
        Some(full_gen) => state.graph.purge_old_generations(project_id, full_gen)?,
        None => tracing::info!(
            project_id = project_id,
            "GC: skipping GRAPH purge — no last_full_index_generation baseline \
             (incremental generations must not purge the graph)"
        ),
    }

    // Purge Search Index (need to load engine)
    if let Some(ps) = load_project_runtime_minimal(state, project_id).await? {
        ps.search
            .purge_old_generations(project_id, active_gen)
            .await?;
    }

    Ok(())
}

async fn load_project_runtime_minimal(
    state: &AppState,
    project_id: &str,
) -> anyhow::Result<Option<crate::state::ProjectState>> {
    if let Some(p) = state.get_project_cached(project_id) {
        return Ok(Some(p));
    }
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid))
        .await
        .unwrap_or_else(|_| Ok(None))?;
    let Some(rec) = rec else {
        return Ok(None);
    };

    let tantivy_dir = state
        .cfg
        .data_dir
        .join("projects")
        .join(project_id)
        .join("tantivy");
    let lancedb_dir = state
        .cfg
        .data_dir
        .join("projects")
        .join(project_id)
        .join("lancedb");

    // Minimal load: we don't ensure dirs exist if we are just purging (though they should)
    let search = engram_index::HybridSearchEngine::new_with_budget(
        tantivy_dir,
        lancedb_dir,
        &state.cfg,
        Some(state.memory_budget.clone()),
    )
    .await?;
    let ps = crate::state::ProjectState {
        info: crate::state::ProjectInfo {
            project_id: project_id.to_string(),
            project_name: rec.project_name,
            project_type: rec.project_type,
            directory: rec.directory,
            tantivy_dir: PathBuf::from(""), // dummy for GC load
            lancedb_dir: PathBuf::from(""),
        },
        search: std::sync::Arc::new(search),
    };
    // Don't necessarily cache it if we are just purging once an hour to avoid memory bloat
    Ok(Some(ps))
}

use std::path::PathBuf;
