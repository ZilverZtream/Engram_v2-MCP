#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-1, mechanism step 1: `tokio::time::interval`
//! ticks immediately, so the GC's first purge ran at daemon start — exactly
//! while the watcher was restoring projects and starting incremental
//! updates. The first sweep must wait the configured delay.

use engram_core::config::Config;
use engram_server::state::AppState;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("proj")).unwrap();
    let cfg = Config {
        allowed_roots: vec![tmp.path().join("proj")],
        data_dir: tmp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    (tmp, state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_gc_sweep_waits_for_the_initial_delay() {
    let (_tmp, state) = state();
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(engram_server::actors::gc::run_gc_scheduler_with_delay(
        state.clone(),
        shutdown.clone(),
        Duration::from_millis(400),
    ));

    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        state.gc_sweeps_completed.load(Ordering::SeqCst),
        0,
        "no sweep may run before the initial delay (daemon start = watcher restore window)"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        state.gc_sweeps_completed.load(Ordering::SeqCst) >= 1,
        "the first sweep runs once the delay has elapsed"
    );
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}
