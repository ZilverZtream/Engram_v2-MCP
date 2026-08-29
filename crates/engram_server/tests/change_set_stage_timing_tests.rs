#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3 (the ≤ 5 s gate): on OciusX the reference
//! story takes 10 s while the timed arms sum to ≈ 3 s — 7 s hide in stages
//! that report nothing. Coverage must carry the call's wall-clock and
//! cumulative checkpoints so the missing time is measured, not inferred.

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
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/category_helper.vb"),
        "Public Class category_helper\n    Public Function MainReporting() As String\n        Return \"category\"\n    End Function\nEnd Class\n",
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
            project_name: "StageTimingFixture".into(),
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
async fn coverage_reports_wall_clock_and_stage_checkpoints() {
    let (_tmp, engram, pid) = build().await;
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    let cov = &v["coverage"];
    let wall = cov["wall_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("coverage.wall_ms missing: {cov}"));
    assert!(wall > 0, "wall-clock is measured: {cov}");
    let stages = cov["stages"]
        .as_object()
        .unwrap_or_else(|| panic!("coverage.stages missing: {cov}"));
    for k in ["node_scan", "arms_done", "before_render", "render"] {
        let ms = stages
            .get(k)
            .and_then(|x| x.as_u64())
            .unwrap_or_else(|| panic!("checkpoint {k} missing: {stages:?}"));
        assert!(
            ms <= wall,
            "checkpoint {k}={ms} ms cannot exceed wall={wall} ms"
        );
    }
    // Checkpoints are cumulative: monotone in call order.
    let order: Vec<u64> = ["node_scan", "arms_done", "before_render", "render"]
        .iter()
        .map(|k| stages[*k].as_u64().unwrap())
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] <= w[1]),
        "cumulative checkpoints: {order:?}"
    );
    // The markdown coverage names the wall too.
    let md: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": v["story"].as_str().map(|_| pid.clone()).unwrap_or(pid.clone()), "story": STORY})).unwrap();
    let res2 = engram.handle_get_change_set(md).await.unwrap();
    let t2 = res2.content[0].as_text().unwrap().text.clone();
    assert!(
        t2.contains("- wall: "),
        "markdown coverage shows the wall:\n{t2}"
    );
}
