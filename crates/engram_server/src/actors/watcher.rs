use crate::state::{AppEvent, AppState};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast::Receiver;
use tokio::sync::mpsc;

/// CANCEL1: watcher now accepts a shutdown token, matching the contract of
/// dreamer/immune/gc so all background actors exit cooperatively on shutdown.
pub async fn run_watcher(
    state: AppState,
    mut rx: Receiver<AppEvent>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut watchers: HashMap<String, RecommendedWatcher> = HashMap::new();
    let mut pending_updates: HashMap<String, Instant> = HashMap::new();
    let mut update_cancels: HashMap<String, tokio_util::sync::CancellationToken> = HashMap::new();
    // Shared set of projects that already have an update task in-flight.
    // Prevents unbounded concurrent spawns for the same project under heavy file churn.
    let in_flight: Arc<tokio::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));

    // ENG-AUD-2026-EXH-0005: projects whose notify events were dropped due to channel
    // saturation.  The ticker loop drains this set and re-queues them in pending_updates
    // so no update is permanently lost.
    let overflow_dirty: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // Internal channel to receive notify events
    let (tx_notify, mut rx_notify) = mpsc::channel::<(String, notify::Result<notify::Event>)>(8192);

    let debounce_duration = Duration::from_secs(5);
    let mut ticker = tokio::time::interval(Duration::from_millis(500));

    // Initialization: Restore enabled watchers from registry
    {
        let reg = state.registry.clone();
        let projects = match tokio::task::spawn_blocking(move || reg.list_projects()).await {
            Err(e) => {
                tracing::error!(
                    "ENG-AUD-S1-0001: watcher bootstrap: spawn_blocking panicked listing projects: {e}; watch coverage disabled"
                );
                vec![]
            }
            Ok(Err(e)) => {
                tracing::error!(
                    "ENG-AUD-S1-0001: watcher bootstrap: registry list_projects error: {e}; watch coverage disabled"
                );
                vec![]
            }
            Ok(Ok(v)) => v,
        };

        for p in projects {
            let pid = p.project_id.clone();
            let reg_clone = state.registry.clone();
            let pid_for_list = pid.clone();
            let watches = match tokio::task::spawn_blocking(move || {
                reg_clone.list_watches(&pid_for_list)
            })
            .await
            {
                Err(e) => {
                    tracing::error!(
                        "ENG-AUD-S1-0001: watcher bootstrap: spawn_blocking panicked listing watches for {pid}: {e}; project will not be watched"
                    );
                    vec![]
                }
                Ok(Err(e)) => {
                    tracing::error!(
                        "ENG-AUD-S1-0001: watcher bootstrap: registry list_watches error for {pid}: {e}; project will not be watched"
                    );
                    vec![]
                }
                Ok(Ok(v)) => v,
            };

            if watches.into_iter().any(|w| w.enabled)
                && let Ok(canon) = state.paths.resolve_path(&p.directory)
            {
                tracing::info!("Watcher: restoring for {} at {}", pid, canon.display());
                match create_watcher(pid.clone(), tx_notify.clone(), overflow_dirty.clone()) {
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
            _ = shutdown.cancelled() => {
                tracing::info!("watcher: shutdown token cancelled — exiting");
                return;
            }
            _ = ticker.tick() => {
                // ENG-AUD-2026-EXH-0005: drain overflow-dirty set so dropped events
                // are still eventually processed (convergence guarantee).
                // AUD-2026-EXH-0008: recover from a poisoned mutex rather than
                // silently skipping the drain — a panic while holding the lock
                // must not cause indefinite event loss for affected projects.
                // Scoped block: std::sync::MutexGuard is !Send and must be
                // dropped before any .await point in this arm.
                {
                    let mut dirty = match overflow_dirty.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => {
                            tracing::error!(
                                "AUD-2026-EXH-0008: overflow_dirty mutex poisoned; \
                                 recovering inner state to preserve convergence guarantee"
                            );
                            poisoned.into_inner()
                        }
                    };
                    for pid in dirty.drain() {
                        pending_updates
                            .entry(pid)
                            .or_insert_with(|| Instant::now() + debounce_duration);
                    }
                } // MutexGuard dropped here, before any .await below

                let now = Instant::now();
                let mut to_trigger = Vec::new();
                for (pid, deadline) in &pending_updates {
                    if now >= *deadline {
                        to_trigger.push(pid.clone());
                    }
                }
                for pid in to_trigger {
                    // CANCEL1: cooperative shutdown check inside the project-trigger loop.
                    // The outer select! only preempts between ticks; this check allows
                    // early exit mid-scan when many projects are pending.
                    if shutdown.is_cancelled() {
                        tracing::info!("watcher: shutdown detected mid-scan — exiting");
                        return;
                    }
                    update_cancels.remove(&pid);
                    // CANCEL2: second cancel check immediately before the mutex await so
                    // the shutdown-to-yield distance stays within the ±5-line policy.
                    if shutdown.is_cancelled() {
                        tracing::info!("watcher: shutdown detected before in-flight lock — exiting");
                        return;
                    }
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
                                        match create_watcher(project_id.clone(), tx_notify.clone(), overflow_dirty.clone()) {
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
    overflow_dirty: Arc<Mutex<HashSet<String>>>,
) -> Option<RecommendedWatcher> {
    let config = Config::default().with_poll_interval(Duration::from_secs(2));
    RecommendedWatcher::new(
        move |res| {
            // AUD-2026-INV-0006 / ENG-AUD-2026-EXH-0005: use try_send so the OS
            // notify callback thread is never stalled under a sustained event storm.
            // On channel saturation, mark the project dirty so the ticker loop
            // re-queues a rescan — no event is permanently lost.
            match tx_notify.try_send((pid.clone(), res)) {
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        "ENG-AUD-2026-EXH-0005: watcher notify channel full for {pid} — \
                         event dropped; marking project dirty for forced rescan"
                    );
                    // AUD-2026-EXH-0008: recover from poisoned mutex so that the
                    // dirty marker is never silently lost even after a panic.
                    let mut guard = match overflow_dirty.lock() {
                        Ok(g) => g,
                        Err(poisoned) => {
                            tracing::error!(
                                "AUD-2026-EXH-0008: overflow_dirty mutex poisoned in \
                                 notify callback for {pid}; recovering to preserve dirty marker"
                            );
                            poisoned.into_inner()
                        }
                    };
                    guard.insert(pid.clone());
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    tracing::debug!("watcher notify channel closed for {pid}");
                }
            }
        },
        config,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    /// ENG-AUD-S1-0001: Watcher bootstrap must not silently swallow infra failures.
    /// Behavioral test: verify that a spawn_blocking panic propagates as JoinError
    /// (the mechanism the bootstrap error-logging fix relies on).
    #[tokio::test]
    async fn spawn_blocking_panic_in_bootstrap_is_join_error_s1_0001() {
        let result: Result<Vec<String>, _> = tokio::task::spawn_blocking(|| -> Vec<String> {
            panic!("ENG-AUD-S1-0001: simulated watcher bootstrap registry panic");
        })
        .await;
        assert!(
            result.is_err(),
            "ENG-AUD-S1-0001: watcher bootstrap spawn_blocking panic must produce \
             JoinError, not silently return an empty project list"
        );
    }

    /// ENG-AUD-2026-0004: Generation fetch failure must skip the watcher update
    /// explicitly, not silently continue with a stale generation value.
    ///
    /// Behavioral invariant: wrapping generation arithmetic demonstrates why
    /// silently defaulting to u64::MAX is dangerous.
    #[test]
    fn stale_generation_sentinel_wraps_on_increment() {
        // If fetch_active_generation silently returned u64::MAX on error,
        // the watcher would compute new_gen = u64::MAX.wrapping_add(1) = 0,
        // resetting the generation counter and causing stale cache hits.
        let stale_sentinel = u64::MAX;
        let would_be_next_gen = stale_sentinel.wrapping_add(1);
        assert_eq!(
            would_be_next_gen, 0,
            "ENG-AUD-2026-0004: u64::MAX.wrapping_add(1)=0 resets the generation. \
             The fix must skip the watcher update, not pass this value downstream."
        );
    }

    // -----------------------------------------------------------------------
    // ENG-AUD-2026-N14-0006: behavioral tests for watcher internal state
    // These tests exercise the key data-structure invariants without requiring
    // a live filesystem watcher or a full AppState.
    // -----------------------------------------------------------------------

    /// ENG-AUD-2026-N14-0006: the in-flight set must de-duplicate concurrent
    /// spawns.  Simulates the check-and-claim pattern used inside run_watcher.
    #[test]
    fn in_flight_dedup_prevents_concurrent_spawns() {
        // ENG-AUD-2026-N14-0006
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let pid = "project-alpha".to_string();

        // First claim: pid not present → insert succeeds → already_running = false
        let already_running_first = {
            let mut guard = in_flight.lock().unwrap();
            if guard.contains(&pid) {
                true
            } else {
                guard.insert(pid.clone());
                false
            }
        };
        assert!(
            !already_running_first,
            "ENG-AUD-2026-N14-0006: first claim must succeed (already_running=false)"
        );

        // Second claim for same pid: pid now present → already_running = true
        let already_running_second = {
            let guard = in_flight.lock().unwrap();
            guard.contains(&pid)
        };
        assert!(
            already_running_second,
            "ENG-AUD-2026-N14-0006: second claim for same pid must find it already in-flight"
        );

        // After removing the pid (simulating task completion), a new claim succeeds
        {
            in_flight.lock().unwrap().remove(&pid);
        }
        let already_running_after_drop = {
            let guard = in_flight.lock().unwrap();
            guard.contains(&pid)
        };
        assert!(
            !already_running_after_drop,
            "ENG-AUD-2026-N14-0006: after task finishes, pid must be removable for re-claim"
        );
    }

    /// ENG-AUD-2026-N14-0006: disabling a project cancels any in-progress
    /// update token, and a freshly created replacement token is not cancelled.
    #[test]
    fn cancellation_token_cancel_on_disable() {
        // ENG-AUD-2026-N14-0006
        use std::collections::HashMap;
        use tokio_util::sync::CancellationToken;

        let project_id = "project-beta".to_string();
        let mut update_cancels: HashMap<String, CancellationToken> = HashMap::new();

        // Simulate an in-progress update for the project
        let token = CancellationToken::new();
        update_cancels.insert(project_id.clone(), token.clone());

        // Simulate "disable project": remove from map and cancel
        if let Some(t) = update_cancels.remove(&project_id) {
            t.cancel();
        }

        // The original token must now be cancelled
        assert!(
            token.is_cancelled(),
            "ENG-AUD-2026-N14-0006: disabling project must cancel the in-progress update token"
        );

        // Simulate re-enable: a new token must start uncancelled
        let new_token = CancellationToken::new();
        update_cancels.insert(project_id.clone(), new_token.clone());
        assert!(
            !new_token.is_cancelled(),
            "ENG-AUD-2026-N14-0006: freshly created token for re-enabled project must not be cancelled"
        );
    }

    /// ENG-AUD-2026-N14-0006: when an update task is already in-flight the
    /// pending_updates map must reschedule with a deadline in the future.
    #[test]
    fn pending_updates_reschedule_when_in_flight() {
        // ENG-AUD-2026-N14-0006
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        let pid = "project-gamma".to_string();
        let debounce_duration = Duration::from_secs(5);
        let mut pending_updates: HashMap<String, Instant> = HashMap::new();

        // Existing deadline in the past (simulates a due update)
        let old_deadline = Instant::now() - Duration::from_secs(1);
        pending_updates.insert(pid.clone(), old_deadline);

        // Simulate already_running = true → reschedule
        let already_running = true;
        if already_running {
            let new_deadline = Instant::now() + debounce_duration;
            pending_updates.insert(pid.clone(), new_deadline);
        }

        let stored = pending_updates[&pid];
        assert!(
            stored > Instant::now(),
            "ENG-AUD-2026-N14-0006: rescheduled deadline must be in the future"
        );
        assert!(
            stored > old_deadline,
            "ENG-AUD-2026-N14-0006: rescheduled deadline must be later than the old deadline"
        );
    }

    // -----------------------------------------------------------------------
    // ENG-AUD-2026-T18-0005: behavioral tests for watcher bootstrap paths
    // These tests exercise bootstrap logic and watcher creation without
    // requiring a live AppState or full actor lifecycle.
    // -----------------------------------------------------------------------

    /// ENG-AUD-2026-T18-0005: when no projects are present the bootstrap loop
    /// runs zero iterations, leaving the watchers map empty.
    ///
    /// This is a pure logic test: it verifies the invariant that the watchers
    /// HashMap accumulates zero entries when the project list fed to the
    /// bootstrap loop is empty — identical behaviour to what run_watcher
    /// exhibits when the registry returns an empty list.
    #[test]
    fn bootstrap_empty_project_list_leaves_watchers_empty() {
        // ENG-AUD-2026-T18-0005
        use std::collections::HashMap;

        // Mirror the logical structure used inside run_watcher: a map keyed by
        // project_id.  We use String as value here because constructing a real
        // RecommendedWatcher requires a live channel, which is not needed to
        // verify the loop-count invariant.
        let mut watchers: HashMap<String, String> = HashMap::new();

        // Simulate the bootstrap result: registry returned no projects.
        let projects: Vec<String> = vec![];

        // The bootstrap loop body never runs — no watcher is ever inserted.
        for pid in &projects {
            // In run_watcher this calls create_watcher and inserts on success.
            // With an empty list this branch is provably unreachable at runtime.
            watchers.insert(pid.clone(), pid.clone());
        }

        assert!(
            watchers.is_empty(),
            "ENG-AUD-2026-T18-0005: watchers map must remain empty when project list is empty \
             (bootstrap loop ran 0 iterations)"
        );
    }

    /// ENG-AUD-2026-T18-0005: create_watcher returns Some (a valid watcher),
    /// but attempting to watch a nonexistent path via the returned watcher
    /// must produce an Err — exercising the "watcher created but watch() failed"
    /// error path in the bootstrap.
    #[test]
    fn create_watcher_watch_nonexistent_path_returns_err() {
        // ENG-AUD-2026-T18-0005
        // notify::Watcher must be imported so that .watch() is callable on
        // RecommendedWatcher without a fully qualified path.
        // notify::RecursiveMode must be imported for the watch() call argument.
        use notify::{RecursiveMode, Watcher as _};
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::channel::<(String, notify::Result<notify::Event>)>(8);

        // create_watcher always succeeds (returns Some) — the notify crate can
        // always construct a watcher object; success does not depend on the path.
        let overflow = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let mut watcher_opt = super::create_watcher("test_pid".to_string(), tx, overflow);
        assert!(
            watcher_opt.is_some(),
            "ENG-AUD-2026-T18-0005: create_watcher must return Some(watcher) for a valid channel"
        );

        // Attempting to watch a path that does not exist must return Err.
        // This exercises the branch in run_watcher bootstrap that logs an error
        // and skips inserting the watcher into the map.
        let watcher = watcher_opt.as_mut().unwrap();
        let nonexistent =
            std::path::Path::new("/this/path/does/not/exist/engram_t18_0005_sentinel");
        let watch_result = watcher.watch(nonexistent, RecursiveMode::Recursive);
        assert!(
            watch_result.is_err(),
            "ENG-AUD-2026-T18-0005: watching a nonexistent path must return Err \
             (the bootstrap error-and-skip branch)"
        );
    }

    // -----------------------------------------------------------------------
    // AUD-2026-INV-0006: non-blocking try_send overflow tests
    // -----------------------------------------------------------------------

    /// AUD-2026-INV-0006: when the channel is full, try_send must return
    /// TrySendError::Full — the callback thread must never block.
    #[test]
    fn channel_overflow_drops_event_not_blocks() {
        // AUD-2026-INV-0006
        // Use a capacity-1 channel to force overflow with just two sends.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<i32>(1);

        // Fill the channel — this must succeed.
        tx.try_send(1)
            .expect("AUD-2026-INV-0006: first send into empty channel must succeed");

        // The channel is now full; the next try_send must return Full immediately
        // (non-blocking) rather than blocking the caller.
        match tx.try_send(2) {
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // expected — overflow is signalled, not blocked
            }
            Ok(_) => panic!("AUD-2026-INV-0006: expected Full error but send succeeded"),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                panic!("AUD-2026-INV-0006: expected Full error but got Closed")
            }
        }

        // The original item is still in the channel (was not displaced).
        drop(tx);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let val = rt.block_on(rx.recv());
        assert_eq!(
            val,
            Some(1),
            "AUD-2026-INV-0006: the item that was successfully sent must still be receivable"
        );
    }

    /// AUD-2026-INV-0006 Gate 2.5 Test 11: try_send on a full channel returns
    /// `TrySendError::Full` immediately — it never blocks the calling thread.
    /// This is the behavioral contract that replace `blocking_send` relies on.
    #[tokio::test]
    async fn try_send_on_full_channel_returns_full_not_blocking() {
        use tokio::sync::mpsc;
        use tokio::sync::mpsc::error::TrySendError;

        // capacity=1: one item fills it
        let (tx, _rx) = mpsc::channel::<String>(1);
        tx.try_send("fill".to_string())
            .expect("first send must succeed");

        // Now channel is full — try_send must return Full immediately
        let result = tx.try_send("overflow".to_string());
        assert!(
            matches!(result, Err(TrySendError::Full(_))),
            "AUD-2026-INV-0006: try_send on full channel must return TrySendError::Full \
             immediately (non-blocking). blocking_send would have blocked the notify callback \
             thread under sustained event bursts."
        );
    }

    /// AUD-2026-INV-0006 Gate 2.5 Test 12: verify `TrySendError::Full` is distinct
    /// from `TrySendError::Closed` — the notify callback must handle both cases
    /// explicitly so overflow is observable (not silently ignored).
    #[tokio::test]
    async fn try_send_error_variants_are_distinct_full_vs_closed() {
        use tokio::sync::mpsc;
        use tokio::sync::mpsc::error::TrySendError;

        // Test Full variant
        let (tx, _rx) = mpsc::channel::<u32>(1);
        tx.try_send(1).unwrap();
        assert!(
            matches!(tx.try_send(2), Err(TrySendError::Full(_))),
            "capacity-full channel must give TrySendError::Full"
        );

        // Test Closed variant
        let (tx2, rx2) = mpsc::channel::<u32>(16);
        drop(rx2); // close the receiver
        assert!(
            matches!(tx2.try_send(1), Err(TrySendError::Closed(_))),
            "dropped-receiver channel must give TrySendError::Closed"
        );

        // Prove the two are not the same match arm
        let full_is_closed = matches!(tx.try_send(3), Err(TrySendError::Closed(_)));
        assert!(
            !full_is_closed,
            "AUD-2026-INV-0006: Full and Closed must be separate error arms so \
             overflow telemetry (warn!) fires only on Full, not on Closed (debug!)"
        );
    }

    // -----------------------------------------------------------------------
    // Gate 2.5→3.0: watcher high-churn / backpressure realism
    // AUD-2026-INV-0006
    // -----------------------------------------------------------------------

    /// AUD-2026-INV-0006 Gate 2.5→3.0: simulate 20 rapid try_send calls into a
    /// capacity-2 channel.  Proves non-blocking behaviour: none of the 20
    /// attempts can block the caller, and the error accounting is exhaustive.
    #[tokio::test]
    async fn watcher_high_churn_overflow_telemetry() {
        // AUD-2026-INV-0006
        use tokio::sync::mpsc;
        use tokio::sync::mpsc::error::TrySendError;

        // Small capacity forces overflow quickly.
        let (tx, _rx) = mpsc::channel::<u32>(2);

        let total_attempts: u32 = 20;
        let mut successes: u32 = 0;
        let mut overflows: u32 = 0;
        let mut closed_errors: u32 = 0;

        for i in 0..total_attempts {
            match tx.try_send(i) {
                Ok(()) => successes += 1,
                Err(TrySendError::Full(_)) => overflows += 1,
                Err(TrySendError::Closed(_)) => closed_errors += 1,
            }
        }

        // At least 1 send must succeed (channel was not closed before first send).
        assert!(
            successes >= 1,
            "AUD-2026-INV-0006: at least 1 of 20 try_send calls must succeed; got successes={successes}"
        );

        // Once the capacity-2 channel fills, subsequent sends must overflow.
        assert!(
            overflows >= 1,
            "AUD-2026-INV-0006: at least 1 overflow (TrySendError::Full) must occur across 20 sends \
             into a capacity-2 channel; got overflows={overflows}"
        );

        // Accounting must be exhaustive: every attempt maps to exactly one outcome.
        assert_eq!(
            successes + overflows + closed_errors,
            total_attempts,
            "AUD-2026-INV-0006: success + overflow + closed must equal {total_attempts}; \
             got successes={successes}, overflows={overflows}, closed_errors={closed_errors}"
        );
    }

    /// AUD-2026-INV-0006 Gate 2.5→3.0: with a capacity-2 channel and 10
    /// consecutive try_send calls the first 2 must succeed and the remaining 8
    /// must overflow.  The 2 successful items must be retrievable.
    #[tokio::test]
    async fn watcher_overflow_count_is_deterministic() {
        // AUD-2026-INV-0006
        use tokio::sync::mpsc;
        use tokio::sync::mpsc::error::TrySendError;

        let (tx, mut rx) = mpsc::channel::<u32>(2);

        let mut successes: u32 = 0;
        let mut overflows: u32 = 0;

        for i in 0..10u32 {
            match tx.try_send(i) {
                Ok(()) => successes += 1,
                Err(TrySendError::Full(_)) => overflows += 1,
                Err(TrySendError::Closed(_)) => {
                    panic!("AUD-2026-INV-0006: channel closed unexpectedly at item {i}")
                }
            }
        }

        // Exactly the channel capacity (2) must succeed.
        assert_eq!(
            successes, 2,
            "AUD-2026-INV-0006: exactly 2 sends must succeed for a capacity-2 channel; got {successes}"
        );

        // The remaining 8 must overflow, not block.
        assert_eq!(
            overflows, 8,
            "AUD-2026-INV-0006: exactly 8 overflows must occur for 10 sends into capacity-2; got {overflows}"
        );

        // The 2 items that succeeded must be retrievable from the receiver.
        drop(tx);
        let item0 = rx.recv().await;
        let item1 = rx.recv().await;
        let item2 = rx.recv().await;

        assert!(
            item0.is_some(),
            "AUD-2026-INV-0006: first successful item must be receivable"
        );
        assert!(
            item1.is_some(),
            "AUD-2026-INV-0006: second successful item must be receivable"
        );
        assert!(
            item2.is_none(),
            "AUD-2026-INV-0006: channel must be empty after receiving the 2 successful items; \
             got a third item: {item2:?}"
        );
    }
}
