use crate::state::{AppEvent, AppState};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc;

pub async fn run_watcher(state: AppState, mut rx: Receiver<AppEvent>) {
    let mut watchers: HashMap<String, RecommendedWatcher> = HashMap::new();
    let mut pending_updates: HashMap<String, Instant> = HashMap::new();
    // Shared set of projects that already have an update task in-flight.
    // Prevents unbounded concurrent spawns for the same project under heavy file churn.
    let in_flight: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

    // Internal channel to receive notify events
    let (tx_notify, mut rx_notify) = mpsc::channel::<(String, notify::Result<notify::Event>)>(100);

    let debounce_duration = Duration::from_secs(5);
    let mut ticker = tokio::time::interval(Duration::from_millis(500));

    // Initialization: Restore enabled watchers from registry
    {
        let reg = state.registry.clone();
        let projects = tokio::task::spawn_blocking(move || reg.list_projects())
            .await
            .unwrap_or(Ok(vec![]))
            .unwrap_or(vec![]);

        for p in projects {
            let pid = p.project_id.clone();
            let reg_clone = state.registry.clone();
            let pid_for_list = pid.clone();
            let watches =
                tokio::task::spawn_blocking(move || reg_clone.list_watches(&pid_for_list))
                    .await
                    .unwrap_or(Ok(vec![]))
                    .unwrap_or(vec![]);

            if watches.into_iter().any(|w| w.enabled)
                && let Ok(canon) = state.paths.resolve_path(&p.directory)
            {
                tracing::info!("Watcher: restoring for {} at {}", pid, canon.display());
                if let Some(mut watcher) = create_watcher(pid.clone(), tx_notify.clone())
                    && watcher
                        .watch(canon.as_path(), RecursiveMode::Recursive)
                        .is_ok()
                {
                    watchers.insert(pid, watcher);
                }
            }
        }
    }

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = Instant::now();
                let mut to_trigger = Vec::new();
                for (pid, deadline) in &pending_updates {
                    if now >= *deadline {
                        to_trigger.push(pid.clone());
                    }
                }
                for pid in to_trigger {
                    pending_updates.remove(&pid);

                    // Skip if an update task is already running for this project.
                    {
                        let guard = in_flight.lock().await;
                        if guard.contains(&pid) {
                            tracing::debug!(
                                "Watcher: update already in-flight for {}, skipping spawn",
                                pid
                            );
                            continue;
                        }
                    }

                    tracing::info!("Watcher: triggering update for project {}", pid);
                    let state_clone = state.clone();
                    let in_flight_clone = in_flight.clone();
                    // Mark as in-flight before spawning.
                    in_flight.lock().await.insert(pid.clone());
                    tokio::spawn(async move {
                        let active_gen = {
                            let reg = state_clone.registry.clone();
                            let pid_clone = pid.clone();
                            tokio::task::spawn_blocking(move || {
                                reg.get_meta(&pid_clone, "active_generation")
                                    .ok()
                                    .flatten()
                                    .and_then(|s| s.parse::<u64>().ok())
                                    .unwrap_or(1)
                            }).await.unwrap_or(1)
                        };
                        let new_gen = active_gen.saturating_add(1);
                        let max_commits = state_clone.cfg.max_commits_per_watch;
                        let engram = crate::tools::Engram::new(state_clone);
                        let cancel = tokio_util::sync::CancellationToken::new();
                        let _ = engram.update_project_impl(&pid, new_gen, max_commits, false, &cancel).await;
                        // Clear in-flight marker so future debounce ticks can re-trigger.
                        in_flight_clone.lock().await.remove(&pid);
                    });
                }
            }
            maybe_ev = rx.recv() => {
                match maybe_ev {
                    Ok(ev) => {
                        if let AppEvent::WatchUpdate { project_id, directory, enabled } = ev {
                            if enabled {
                                // Enforce path allowlist
                                match state.paths.resolve_path(&directory) {
                                    Ok(canon) => {
                                        tracing::info!("Watcher: enabling for {} at {}", project_id, canon.display());
                                        if let Some(mut watcher) = create_watcher(project_id.clone(), tx_notify.clone())
                                            && watcher.watch(canon.as_path(), RecursiveMode::Recursive).is_ok() {
                                                watchers.insert(project_id, watcher);
                                            }
                                    }
                                    Err(e) => {
                                        tracing::error!("Watcher: cannot enable for {}: {}", project_id, e);
                                    }
                                }
                            } else {
                                tracing::info!("Watcher: disabling for {}", project_id);
                                watchers.remove(&project_id);
                                pending_updates.remove(&project_id);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
            maybe_notify = rx_notify.recv() => {
                let Some((pid, res)) = maybe_notify else { continue; };
                match res {
                    Ok(event) => {
                        if event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove() {
                            pending_updates.insert(pid, Instant::now() + debounce_duration);
                        }
                    }
                    Err(e) => tracing::error!("Watcher error for {}: {:?}", pid, e),
                }
            }
        }
    }
}

fn create_watcher(
    pid: String,
    tx_notify: mpsc::Sender<(String, notify::Result<notify::Event>)>,
) -> Option<RecommendedWatcher> {
    let config = Config::default().with_poll_interval(Duration::from_secs(2));
    RecommendedWatcher::new(
        move |res| {
            // Use try_send so the notify OS thread never blocks. If the channel
            // is full (Tokio is busy with heavy indexing), drop the event and log
            // a warning rather than hanging the dedicated watcher thread.
            if let Err(e) = tx_notify.try_send((pid.clone(), res)) {
                tracing::warn!("Watcher: notify channel full, dropping fs event for {pid}: {e}");
            }
        },
        config,
    )
    .ok()
}
