#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3 (the ≤ 5 s gate). Release-22 checkpoints on
//! OciusX: the full graph node scan costs 1.75 s on EVERY get_change_set call
//! although the index only changes when a generation is published. The scan
//! is cached per project generation: a repeat call on an unchanged index
//! performs no scan, a new generation triggers one.

use engram_core::config::Config;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As an admin I want to set a main reporting category (huvudredovisningskategori) for each production code list category";

async fn build() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code/redovisning/code")).unwrap();
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/redovisningskategorier.vb"),
        "Public Class redovisningskategorier\n    Public Function GetByProjectId(pr_id As Integer) As Object\n        Return Nothing\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "SnapshotCacheFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

async fn node_scans(engram: &Engram, pid: &str) -> u64 {
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    v["coverage"]["node_scans"]
        .as_u64()
        .unwrap_or_else(|| panic!("coverage.node_scans missing: {}", v["coverage"]))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_node_scan_is_cached_per_generation() {
    let (_tmp, state, engram, pid) = build().await;
    assert_eq!(node_scans(&engram, &pid).await, 1, "first call scans");
    assert_eq!(
        node_scans(&engram, &pid).await,
        0,
        "same generation: the cached snapshot is reused"
    );
    let current: u64 = state
        .registry
        .get_meta(&pid, "active_generation")
        .unwrap()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    state
        .registry
        .set_meta(&pid, "active_generation", &(current + 1).to_string())
        .unwrap();
    assert_eq!(
        node_scans(&engram, &pid).await,
        1,
        "a new generation invalidates the snapshot"
    );
    assert_eq!(node_scans(&engram, &pid).await, 0);
}
