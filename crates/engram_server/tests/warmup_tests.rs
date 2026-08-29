#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3: the first get_change_set after a daemon
//! restart took 38 s on OciusX — the project runtime opened on the first
//! user's call. A fresh AppState over an existing data dir has no runtime
//! cached; the warm-up opens every registered project so the first call is
//! served from a warm daemon.

use engram_core::config::Config;
use engram_server::actors::warmup::warm_all_projects;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_daemon_warms_every_registered_project_runtime() {
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
                project_name: "WarmupFixture".into(),
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
    // A restarted daemon: same data dir, empty runtime cache.
    let (fresh, _rx2) = AppState::new(cfg()).unwrap();
    assert!(
        fresh.get_project_cached(&pid).is_none(),
        "a fresh state has no runtime cached"
    );
    let n = warm_all_projects(&fresh).await;
    assert_eq!(n, 1, "one registered project warmed");
    assert!(
        fresh.get_project_cached(&pid).is_some(),
        "the runtime is cached after warm-up"
    );
}
