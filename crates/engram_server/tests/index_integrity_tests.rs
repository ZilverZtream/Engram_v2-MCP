#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-2: `project_health` initialized its answer
//! with "Health: OK" and turned provider failures into zeros; `get_index_freshness`
//! checked timestamps and modified files but never whether the ACTIVE
//! generation actually holds the corpus — so OciusX reported "Health: OK,
//! index is current and the watcher is active" while 95 % of its code chunks
//! were gone. Both tools must measure generation completeness: code chunks in
//! the published generation against the files the project tracks.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_server::models::{GetIndexFreshnessRequest, ProjectIdRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

const CODE_NS: &str = "memory";

async fn build(files: usize) -> (tempfile::TempDir, AppState, Engram, String, Vec<String>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    let mut paths = Vec::new();
    for i in 0..files {
        let rel = format!("Site/App_Code/mod{i:02}.vb");
        std::fs::write(
            root.join(&rel),
            format!("Public Class mod{i:02}\n    Public Function GetByID{i}(id As Integer) As String\n        Return \"x{i}\"\n    End Function\nEnd Class\n"),
        )
        .unwrap();
        paths.push(rel);
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "IntegrityFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid, paths)
}

/// Simulate the collapse: most code chunks of the published generation vanish
/// while the graph and the registry still describe the whole project.
async fn corrupt(state: &AppState, pid: &str, paths: &[String]) {
    let engine = state.get_project_cached(pid).unwrap().search;
    let gone: Vec<RelPath> = paths.iter().map(|p| RelPath::new(p)).collect();
    engine.delete_files(pid, CODE_NS, &gone).await.unwrap();
}

async fn health(engram: &Engram, pid: &str) -> String {
    let req: ProjectIdRequest = serde_json::from_value(json!({"project_id": pid})).unwrap();
    let res = engram.handle_project_health(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

async fn freshness(engram: &Engram, pid: &str) -> String {
    let req: GetIndexFreshnessRequest = serde_json::from_value(json!({"project_id": pid})).unwrap();
    let res = engram.handle_get_index_freshness(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healthy_index_is_reported_complete() {
    let (_tmp, _state, engram, pid, _paths) = build(10).await;
    let h = health(&engram, &pid).await;
    assert!(h.starts_with("Health: OK"), "{h}");
    assert!(
        h.contains("generation completeness"),
        "the completeness check is part of health:\n{h}"
    );
    let f = freshness(&engram, &pid).await;
    assert!(f.contains("generation_complete: true"), "{f}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_collapsed_generation_is_never_reported_ok() {
    let (_tmp, state, engram, pid, paths) = build(10).await;
    corrupt(&state, &pid, &paths[..8]).await;

    let h = health(&engram, &pid).await;
    assert!(
        !h.starts_with("Health: OK"),
        "80 % of the code chunks are gone — health must not open with OK:\n{h}"
    );
    assert!(
        h.contains("INCOMPLETE") && h.contains("index_project"),
        "health names the collapse and the repair:\n{h}"
    );

    let f = freshness(&engram, &pid).await;
    assert!(f.contains("generation_complete: false"), "{f}");
    assert!(
        !f.contains("index is current"),
        "freshness must not call a collapsed generation current:\n{f}"
    );
    assert!(
        f.contains("index_project"),
        "freshness names the repair:\n{f}"
    );
}
