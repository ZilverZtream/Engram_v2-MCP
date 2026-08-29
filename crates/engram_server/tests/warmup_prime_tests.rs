#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3 (owner: keep looping): release 26 — the
//! first get_change_set after a daemon restart still takes 9.6 s because the
//! node snapshot, the settings prior and the co-change partner path are
//! mined on first use. The warm-up primes them with one background
//! change-set call per project, so the first user call is served warm.

use engram_core::config::Config;
use engram_server::actors::warmup::warm_all_projects;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_warm_up_primes_the_change_set_caches() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    std::fs::write(
        root.join("Site/App_Code/a.vb"),
        "Public Class a\n    Public Function Go() As Integer\n        Return 1\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let cfg = || Config {
        allowed_roots: vec![root.clone()],
        data_dir: data_dir.clone(),
        max_project_files: Some(20),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let pid = {
        let (state, _rx) = AppState::new(cfg()).unwrap();
        let engram = Engram::new(state.clone());
        engram
            .index_project(Parameters(engram_server::IndexProjectRequest {
                directory: root.to_string_lossy().to_string(),
                project_name: "WarmupPrimeFixture".into(),
                project_type: engram_server::models::ProjectType::DotnetWebformsVb,
                wait: true,
                dedupe_by_directory: false,
            }))
            .await
            .unwrap();
        let pid = state.registry.list_projects().unwrap()[0]
            .project_id
            .clone();
        drop(engram);
        pid
    };
    let (fresh, _rx2) = AppState::new(cfg()).unwrap();
    assert!(
        !fresh.node_snapshot_cache.contains_key(&pid)
            && !fresh.setting_prior_cache.contains_key(&pid),
        "a fresh daemon has cold change-set caches"
    );
    assert_eq!(warm_all_projects(&fresh).await, 1);
    assert!(
        fresh.node_snapshot_cache.contains_key(&pid),
        "the warm-up primed the node snapshot"
    );
    assert!(
        fresh.setting_prior_cache.contains_key(&pid),
        "the warm-up primed the settings prior"
    );
}
