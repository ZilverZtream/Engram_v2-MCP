#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10) P0-2 — "generation completeness is
//! not a completeness measurement": the check divided code CHUNKS by graph
//! FILE nodes (1,371 % live) — chunks and files are different units, a few
//! large files mask the loss of most files, the vector store is never
//! checked, and the graph is both the denominator and a store under test.
//! Completeness is a PATH-SET comparison per store: eligible repository paths
//! ↔ Tantivy paths in the active generation ↔ LanceDB paths in the active
//! generation ↔ graph File nodes — with missing/extra paths reported.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_server::models::{GetIndexFreshnessRequest, ProjectIdRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

const CODE_NS: &str = "memory";

/// 2 LARGE files (40 functions each → many chunks) + 8 small ones (1 chunk).
async fn build() -> (
    tempfile::TempDir,
    AppState,
    Engram,
    String,
    Vec<String>,
    Vec<String>,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    let mut big = Vec::new();
    for b in 0..2 {
        let rel = format!("Site/App_Code/big{b}.vb");
        let mut src = format!("Public Class big{b}\n");
        for f in 0..40 {
            src.push_str(&format!(
                "    ''' <summary>Function {f} of the big class {b}, documented at length so that each one becomes its own chunk.</summary>\n    Public Function Compute{b}_{f}(id As Integer) As String\n        Dim s = \"value {f}\"\n        If id > {f} Then s = s & \" high\"\n        Return s\n    End Function\n\n"
            ));
        }
        src.push_str("End Class\n");
        std::fs::write(root.join(&rel), src).unwrap();
        big.push(rel);
    }
    let mut small = Vec::new();
    for i in 0..8 {
        let rel = format!("Site/App_Code/small{i:02}.vb");
        std::fs::write(
            root.join(&rel),
            format!("Public Class small{i:02}\n    Public Function GetByID{i}(id As Integer) As String\n        Return \"x{i}\"\n    End Function\nEnd Class\n"),
        )
        .unwrap();
        small.push(rel);
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "local".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "PathSetFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid, big, small)
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

/// The healthy fixture: every store holds every eligible path; the report
/// speaks in PATHS and never prints a figure above 100 %.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_healthy_index_reports_the_path_sets_of_every_store() {
    let (_tmp, _state, engram, pid, _big, _small) = build().await;
    let h = health(&engram, &pid).await;
    assert!(h.starts_with("Health: OK"), "{h}");
    assert!(
        h.contains("expected paths: 10")
            && h.contains("tantivy: 10")
            && h.contains("vectors: 10")
            && h.contains("graph: 10"),
        "the report compares PATH SETS per store (expected / tantivy / vectors / graph):\n{h}"
    );
    assert!(h.contains("missing: 0"), "{h}");
    assert!(
        !h.contains("1371") && !h.contains("%") || h.contains("100.0 %") || h.contains("100 %"),
        "no percentage above 100 can appear:\n{h}"
    );
    let f = freshness(&engram, &pid).await;
    assert!(f.contains("generation_complete: true"), "{f}");
}

/// The auditor's masking case: 8 of 10 files (80 %) vanish from the searchable
/// generation while the 2 large files keep the CHUNK ratio high. Chunk/file
/// arithmetic says "complete"; path sets say 8 paths are missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn losing_most_files_behind_a_few_large_ones_is_incomplete() {
    let (_tmp, state, engram, pid, _big, small) = build().await;
    let engine = state.get_project_cached(&pid).unwrap().search;
    let gone: Vec<RelPath> = small.iter().map(|p| RelPath::new(p)).collect();
    engine.delete_files(&pid, CODE_NS, &gone).await.unwrap();

    let h = health(&engram, &pid).await;
    assert!(
        !h.starts_with("Health: OK"),
        "80 % of the files are gone — health must not open with OK:\n{h}"
    );
    assert!(h.contains("INCOMPLETE"), "{h}");
    assert!(
        h.contains("missing: 8"),
        "eight paths are missing from the searchable generation:\n{h}"
    );
    assert!(
        h.contains("small00.vb"),
        "the report names sample missing paths:\n{h}"
    );
    let f = freshness(&engram, &pid).await;
    assert!(f.contains("generation_complete: false"), "{f}");
}

/// A loss the old check could never see: the vector store lost three paths
/// while Tantivy still holds them. That is a cross-store mismatch, and the
/// generation is INCOMPLETE for vector-backed evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vector_only_loss_is_a_cross_store_mismatch() {
    let (_tmp, state, engram, pid, _big, small) = build().await;
    let engine = state.get_project_cached(&pid).unwrap().search;
    let gone: Vec<RelPath> = small[..3].iter().map(|p| RelPath::new(p)).collect();
    engine
        .delete_vector_rows_for_paths(&pid, CODE_NS, &gone)
        .await
        .unwrap();

    let h = health(&engram, &pid).await;
    assert!(!h.starts_with("Health: OK"), "{h}");
    assert!(
        h.contains("vectors: 7") && h.contains("tantivy: 10"),
        "per-store path counts expose the one-sided loss:\n{h}"
    );
    assert!(
        h.contains("cross-store mismatch: 3"),
        "paths present in one store and absent in another are a mismatch:\n{h}"
    );
}
