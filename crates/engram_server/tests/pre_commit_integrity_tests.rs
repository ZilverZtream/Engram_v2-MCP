#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-4: "a structurally incomplete index does not
//! necessarily return an error — search-backed gates can pass against 5-10 %
//! of the expected corpus unless pre-commit depends on a real integrity
//! result." Every gate that searches the index (antipattern, product intent,
//! co-added family) must run DEGRADED — never a clean pass — when the
//! published generation is incomplete, and say why.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_server::services::pre_commit_review_service::{
    GateStatus, ReviewConfig, all_gates, run_pre_commit_review_with,
};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

const CODE_NS: &str = "memory";
const SEARCH_GATES: [&str; 3] = ["antipattern", "product_intent", "co_added_family"];
const DIFF: &str = "diff --git a/Site/App_Code/mod00.vb b/Site/App_Code/mod00.vb\n\
--- a/Site/App_Code/mod00.vb\n\
+++ b/Site/App_Code/mod00.vb\n\
@@ -1,5 +1,6 @@\n \
Public Class mod00\n \
    Public Function GetByID0(id As Integer) As String\n\
+        Dim sql = \"SELECT * FROM projekt WHERE id = \" & id\n \
        Return \"x0\"\n \
    End Function\n \
End Class\n";

async fn build(
    files: usize,
) -> (
    tempfile::TempDir,
    AppState,
    String,
    std::path::PathBuf,
    Vec<String>,
) {
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
            project_name: "IntegrityGateFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, pid, root, paths)
}

fn generation(state: &AppState, pid: &str) -> u64 {
    state
        .registry
        .get_meta(pid, "active_generation")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_backed_gates_run_degraded_on_an_incomplete_generation() {
    let (_tmp, state, pid, root, paths) = build(10).await;
    // The collapse: 80 % of the published generation's code chunks are gone.
    let engine = state.get_project_cached(&pid).unwrap().search;
    let gone: Vec<RelPath> = paths[..8].iter().map(|p| RelPath::new(p)).collect();
    engine.delete_files(&pid, CODE_NS, &gone).await.unwrap();

    let (_findings, _gates_run, _files, outcomes) = run_pre_commit_review_with(
        &state,
        &pid,
        &root,
        generation(&state, &pid),
        DIFF,
        &ReviewConfig::default(),
        all_gates(),
    )
    .await
    .unwrap();

    for name in SEARCH_GATES {
        let o = outcomes
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("gate {name} must run; outcomes: {outcomes:?}"));
        match &o.status {
            GateStatus::Degraded { notes, .. } => assert!(
                notes.iter().any(|n| n.contains("INCOMPLETE")),
                "{name}: the degradation must name the incomplete generation, notes: {notes:?}"
            ),
            other => panic!(
                "{name} searched a generation holding 20 % of the corpus and reported {other:?} — that is a silent fail-open"
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_backed_gates_are_not_degraded_on_a_complete_generation() {
    let (_tmp, state, pid, root, _paths) = build(10).await;
    let (_f, _g, _n, outcomes) = run_pre_commit_review_with(
        &state,
        &pid,
        &root,
        generation(&state, &pid),
        DIFF,
        &ReviewConfig::default(),
        all_gates(),
    )
    .await
    .unwrap();
    for name in SEARCH_GATES {
        let o = outcomes.iter().find(|o| o.name == name).unwrap();
        if let GateStatus::Degraded { notes, .. } = &o.status {
            assert!(
                !notes.iter().any(|n| n.contains("INCOMPLETE")),
                "{name}: a complete generation must not be reported incomplete: {notes:?}"
            );
        }
    }
}
