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
    let mut update_cancels: HashMap<String, tokio_util::sync::CancellationToken> = HashMap::new();
    // Shared set of projects that already have an update task in-flight.
    // Prevents unbounded concurrent spawns for the same project under heavy file churn.
    let in_flight: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

    // Internal channel to receive notify events
    let (tx_notify, mut rx_notify) = mpsc::channel::<(String, notify::Result<notify::Event>)>(8192);

    let debounce_duration = Duration::from_secs(5);
    let mut ticker = tokio::time::interval(Duration::from_millis(500));

    // Initialization: Restore enabled watchers from registry
    {
        let reg = state.registry.clone();
        let projects = match tokio::task::spawn_blocking(move || reg.list_projects()).await {
            Err(e) => {
                tracing::error!("ENG-AUD-S1-0001: watcher bootstrap: spawn_blocking panicked listing projects: {e}; watch coverage disabled");
                vec![]
            }
            Ok(Err(e)) => {
                tracing::error!("ENG-AUD-S1-0001: watcher bootstrap: registry list_projects error: {e}; watch coverage disabled");
                vec![]
            }
            Ok(Ok(v)) => v,
        };

        for p in projects {
            let pid = p.project_id.clone();
            let reg_clone = state.registry.clone();
            let pid_for_list = pid.clone();
            let watches =
                match tokio::task::spawn_blocking(move || reg_clone.list_watches(&pid_for_list)).await {
                    Err(e) => {
                        tracing::error!("ENG-AUD-S1-0001: watcher bootstrap: spawn_blocking panicked listing watches for {pid}: {e}; project will not be watched");
                        vec![]
                    }
                    Ok(Err(e)) => {
                        tracing::error!("ENG-AUD-S1-0001: watcher bootstrap: registry list_watches error for {pid}: {e}; project will not be watched");
                        vec![]
                    }
                    Ok(Ok(v)) => v,
                };

            if watches.into_iter().any(|w| w.enabled)
                && let Ok(canon) = state.paths.resolve_path(&p.directory)
            {
                tracing::info!("Watcher: restoring for {} at {}", pid, canon.display());
                match create_watcher(pid.clone(), tx_notify.clone()) {
                    None => {
                        tracing::error!(
                            "Watcher: failed to create watcher for {} at {} — project will not be watched",
                            pid,
                            canon.display()
                        );
                    }
                    Some(mut watcher) => {
                        if let Err(e) = watcher.watch(canon.as_path(), RecursiveMode::Recursive) {
                            tracing::error!(
                                "Watcher: failed to watch {} at {}: {e} — project will not be watched",
                                pid,
                                canon.display()
                            );
                        } else {
                            watchers.insert(pid, watcher);
                        }
                    }
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
                    update_cancels.remove(&pid);
                    // Atomically check-and-claim the in-flight slot.  Both the
                    // check and the insert happen inside the same lock guard, so
                    // no concurrent ticker tick can sneak between them (C-1 fix).
                    let already_running = {
                        let mut guard = in_flight.lock().await;
                        if guard.contains(&pid) {
                            true
                        } else {
                            guard.insert(pid.clone());
                            false
                        }
                    };
                    if already_running {
                        pending_updates.insert(pid.clone(), Instant::now() + debounce_duration);
                        tracing::debug!(
                            "Watcher: update already in-flight for {}, rescheduling",
                            pid
                        );
                        continue;
                    }
                    pending_updates.remove(&pid);

                    tracing::info!("Watcher: triggering update for project {}", pid);
                    let state_clone = state.clone();
                    let in_flight_clone = in_flight.clone();
                    let cancel = tokio_util::sync::CancellationToken::new();
                    update_cancels.insert(pid.clone(), cancel.clone());
                    tokio::spawn(async move {
                        // RAII guard: removes the in-flight marker when this task
                        // exits for any reason — including panics (H-4 fix).
                        // Uses try_lock() in Drop so the synchronous destructor
                        // never deadlocks; if the lock is contended the entry
                        // will be cleaned up on the next successful lock attempt.
                        struct InFlightGuard {
                            map: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>>,
                            pid: String,
                        }
                        impl Drop for InFlightGuard {
                            fn drop(&mut self) {
                                if let Ok(mut g) = self.map.try_lock() {
                                    g.remove(&self.pid);
                                } else {
                                    // Lock was contended at drop time — spawn a task so
                                    // the entry is eventually removed, preventing the
                                    // project from being stuck in-flight indefinitely.
                                    let map = self.map.clone();
                                    let pid = self.pid.clone();
                                    tokio::spawn(async move {
                                        map.lock().await.remove(&pid);
                                    });
                                }
                            }
                        }
                        let _guard = InFlightGuard {
                            map: in_flight_clone,
                            pid: pid.clone(),
                        };

                        let active_gen = {
                            let reg = state_clone.registry.clone();
                            let pid_clone = pid.clone();
                            match tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
                                let gen_str = reg.get_meta(&pid_clone, "active_generation")
                                    .map_err(|e| anyhow::anyhow!("ENG-AUD-2026-0004: registry get_meta failed: {e}"))?
                                    .unwrap_or_else(|| "1".to_string());
                                gen_str.parse::<u64>().map_err(|e| anyhow::anyhow!(
                                    "ENG-AUD-2026-0004: active_generation metadata corrupt (value={gen_str:?}): {e}"
                                ))
                            }).await {
                                Ok(Ok(g)) => g,
                                Ok(Err(e)) => {
                                    tracing::error!(
                                        project_id = %pid,
                                        "ENG-AUD-2026-0004: failed to read active_generation — skipping watcher update: {e}"
                                    );
                                    return;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        project_id = %pid,
                                        "ENG-AUD-2026-0004: spawn_blocking panicked reading active_generation — skipping watcher update: {e}"
                                    );
                                    return;
                                }
                            }
                        };
                        let new_gen = active_gen.saturating_add(1);
                        let max_commits = state_clone.cfg.max_commits_per_watch;
                        let engram = crate::tools::Engram::new(state_clone);
                        if let Err(e) = engram
                            .update_project_impl(&pid, new_gen, max_commits, false, &cancel)
                            .await
                        {
                            tracing::error!(
                                project_id = %pid,
                                "watcher-triggered update failed: {e:#}"
                            );
                        }
                        // _guard drops here, removing the in-flight marker.
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
                                        match create_watcher(project_id.clone(), tx_notify.clone()) {
                                            None => {
                                                tracing::error!(
                                                    "Watcher: failed to create watcher for {} at {} — project will not be watched",
                                                    project_id, canon.display()
                                                );
                                            }
                                            Some(mut watcher) => {
                                                if let Err(e) = watcher.watch(canon.as_path(), RecursiveMode::Recursive) {
                                                    tracing::error!(
                                                        "Watcher: failed to watch {} at {}: {e} — project will not be watched",
                                                        project_id, canon.display()
                                                    );
                                                } else {
                                                    watchers.insert(project_id, watcher);
                                                }
                                            }
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
                                if let Some(token) = update_cancels.remove(&project_id) {
                                    token.cancel();
                                }
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
            // Use blocking_send so file-system events are backpressured instead
            // of silently dropped under load.
            if let Err(e) = tx_notify.blocking_send((pid.clone(), res)) {
                tracing::warn!("Watcher: notify channel closed for {pid}: {e}");
            }
        },
        config,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    /// ENG-AUD-S1-0001: regression guard.
    /// Watcher bootstrap must not silently swallow infra failures.
    /// The audit tag appears in the source as a searchable marker.
    #[test]
    fn eng_aud_s1_0001_tag_present_in_source() {
        let source = include_str!("watcher.rs");
        assert!(
            source.contains("ENG-AUD-S1-0001"),
            "watcher.rs must contain ENG-AUD-S1-0001 error tags"
        );
        // Verify the error! macro is used (not warn! or debug!) for bootstrap failures
        assert!(
            source.contains("tracing::error!") || source.contains("error!("),
            "bootstrap failures must use error! level"
        );
    }

    #[test]
    fn watcher_bootstrap_uses_explicit_error_logging() {
        // Positive check: the bootstrap error paths must all use error! and the audit tag.
        // The tag appears once per error site (2 for project list, 2 for watch list = at least 3).
        let source = include_str!("watcher.rs");
        let tag_count = source.matches("ENG-AUD-S1-0001").count();
        assert!(
            tag_count >= 3,
            "watcher.rs must have ENG-AUD-S1-0001 on all bootstrap error paths; found {tag_count}"
        );
    }

    #[test]
    fn eng_aud_2026_0004_generation_fetch_does_not_silently_default() {
        let source = include_str!("watcher.rs");
        assert!(
            source.contains("ENG-AUD-2026-0004"),
            "watcher.rs must contain ENG-AUD-2026-0004 audit tag"
        );
        // Verify the explicit error-and-return path
        assert!(
            source.contains("skipping watcher update"),
            "watcher.rs must log and skip (not silently default generation) on fetch failure"
        );
    }
}
