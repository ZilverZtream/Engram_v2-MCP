#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3 (≤ 5 s): release 24 still showed ≈ 1.9 s
//! unattributed inside the arms region and 1.3 s after them. Every pass gets
//! a checkpoint — co-change, vector, presentation co-change, family, symmetry
//! — so the next cut is aimed at measured time.

use engram_core::config::Config;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As an admin I want to set a main reporting category (huvudredovisningskategori) for each production code list category";

async fn build() -> (tempfile::TempDir, Engram, String) {
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
            project_name: "StageTiming2Fixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_pass_has_a_cumulative_checkpoint_in_call_order() {
    let (_tmp, engram, pid) = build().await;
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    let cov = &v["coverage"];
    let stages = cov["stages"]
        .as_object()
        .unwrap_or_else(|| panic!("coverage.stages missing: {cov}"));
    let order = [
        "node_scan",
        "cochange_done",
        "vector_done",
        "presentation_done",
        "family_done",
        "arms_done",
        "symmetry_done",
        "before_render",
        "render",
    ];
    let mut prev = 0u64;
    for k in order {
        let ms = stages
            .get(k)
            .and_then(|x| x.as_u64())
            .unwrap_or_else(|| panic!("checkpoint {k} missing: {stages:?}"));
        assert!(
            ms >= prev,
            "checkpoints are cumulative in call order: {k}={ms} < previous {prev} ({stages:?})"
        );
        prev = ms;
    }
    assert!(prev <= cov["wall_ms"].as_u64().unwrap());
}
