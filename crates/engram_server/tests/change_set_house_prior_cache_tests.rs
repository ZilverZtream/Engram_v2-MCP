#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3 (≤ 5 s): release 25 checkpoints put 1.3 s of
//! every warm call in the house-prior mining (60 most recent PR docs scanned
//! per call) and 1.9 s in the presentation co-change pass. The prior is a
//! property of the corpus, not the story — cached per project generation;
//! the presentation pass is seeded with a bounded anchor set.

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
    std::fs::create_dir_all(root.join("Site/modules/pages")).unwrap();
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/redovisningskategorier.vb"),
        "Public Class redovisningskategorier\n    Public Function GetByProjectId(pr_id As Integer) As Object\n        Return Nothing\n    End Function\nEnd Class\n",
    )
    .unwrap();
    for i in 0..30 {
        std::fs::write(
            root.join(format!("Site/modules/pages/category_page{i:02}.aspx")),
            format!("<%@ Page Language=\"VB\" %>\n<asp:Panel ID=\"pnlCategory{i}\" runat=\"server\" CssClass=\"form-group row\"><asp:Label ID=\"lblMain\" runat=\"server\" Text=\"category\" /></asp:Panel>\n"),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(100),
        max_project_bytes: Some(2 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "HousePriorFixture".into(),
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

async fn coverage(engram: &Engram, pid: &str) -> Value {
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    v["coverage"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_house_prior_is_cached_per_generation_and_presentation_anchors_are_bounded() {
    let (_tmp, state, engram, pid) = build().await;
    let c1 = coverage(&engram, &pid).await;
    assert_eq!(
        c1["house_prior_cached"].as_bool(),
        Some(false),
        "first call mines the corpus: {c1}"
    );
    let anchors = c1["presentation_anchors"]
        .as_u64()
        .unwrap_or_else(|| panic!("presentation_anchors missing: {c1}"));
    assert!(
        anchors <= 20,
        "the presentation pass is seeded with at most 20 anchors, got {anchors}"
    );
    let c2 = coverage(&engram, &pid).await;
    assert_eq!(
        c2["house_prior_cached"].as_bool(),
        Some(true),
        "second call reuses the cached prior: {c2}"
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
    let c3 = coverage(&engram, &pid).await;
    assert_eq!(
        c3["house_prior_cached"].as_bool(),
        Some(false),
        "a new generation re-mines: {c3}"
    );
}
