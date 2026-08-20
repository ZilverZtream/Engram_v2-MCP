#![allow(clippy::unwrap_used)]
//! `repair_project`'s `scope` parameter has to mean something.
//!
//! The tool schema documents it as
//! `"full" | "graph_only" | "tantivy_only" | "vector_only"`, and the handler
//! never read it — every call ran a full repair. A caller asking for a narrow
//! repair silently got a wide one, and a caller who typo'd the scope got a
//! full reindex instead of an error.
//!
//! Widening is not harmless here: a full repair re-indexes the corpus and
//! bumps the generation, which is minutes of work and invalidates every
//! doc_id the caller is holding.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

fn repair_request(pid: &str, scope: &str) -> engram_server::RepairProjectRequest {
    engram_server::RepairProjectRequest {
        project_id: pid.to_string(),
        scope: scope.to_string(),
        wipe_and_reindex: false,
        max_commits: 500,
        index_antipatterns: false,
    }
}

async fn setup() -> (tempfile::TempDir, AppState, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("repo");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() -> u8 { 1 }\npub fn beta() -> u8 { 2 }\n",
    )
    .unwrap();

    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir,
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "RepairScope".into(),
            project_type: engram_server::models::ProjectType::General,
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

async fn active_generation(state: &AppState, pid: &str) -> u64 {
    state
        .registry
        .get_meta(pid, "active_generation")
        .unwrap()
        .unwrap_or_else(|| "1".into())
        .parse()
        .unwrap()
}

/// A narrow scope must NOT run a full reindex. The generation bump is the
/// observable difference: a full repair advances it, a scoped purge does not.
#[tokio::test]
async fn narrow_scope_does_not_reindex_the_project() {
    let (_tmp, state, engram, pid) = setup().await;
    let before = active_generation(&state, &pid).await;

    engram
        .repair_project(Parameters(repair_request(&pid, "vector_only")))
        .await
        .expect("scoped repair must succeed");

    assert_eq!(
        active_generation(&state, &pid).await,
        before,
        "vector_only must not advance the generation — that is a full reindex"
    );
}

/// The default scope still does the full repair, which does advance the
/// generation. Without this the fix could pass by making everything narrow.
#[tokio::test]
async fn full_scope_still_reindexes() {
    let (_tmp, state, engram, pid) = setup().await;
    let before = active_generation(&state, &pid).await;

    engram
        .repair_project(Parameters(repair_request(&pid, "full")))
        .await
        .expect("full repair must succeed");

    assert!(
        active_generation(&state, &pid).await > before,
        "full repair must re-index and advance the generation"
    );
}

/// graph_only must actually rebuild the graph, not just delete it. The
/// underlying scoped helper purges graph data and returns Ok — routing the
/// tool straight at it would turn a repair request into data loss.
#[tokio::test]
async fn graph_only_leaves_the_graph_populated() {
    let (_tmp, state, engram, pid) = setup().await;
    let before = state.graph.count_nodes(&pid).unwrap();
    assert!(before > 0, "precondition: the graph has nodes to repair");

    engram
        .repair_project(Parameters(repair_request(&pid, "graph_only")))
        .await
        .expect("graph repair must succeed");

    let after = state.graph.count_nodes(&pid).unwrap();
    assert!(
        after > 0,
        "graph_only must REBUILD the graph, not leave it empty ({before} nodes before, {after} after)"
    );
}

/// An unknown scope must be rejected, not silently upgraded to a full
/// reindex. Same fail-closed rule the freshness and namespace parameters
/// already follow.
#[tokio::test]
async fn unknown_scope_is_rejected() {
    let (_tmp, state, engram, pid) = setup().await;
    let before = active_generation(&state, &pid).await;

    let err = engram
        .repair_project(Parameters(repair_request(&pid, "tantivy")))
        .await
        .expect_err("a typo'd scope must be an error");

    let msg = format!("{err}");
    assert!(
        msg.contains("tantivy") && msg.contains("full"),
        "the error must name the bad value and the valid ones; got: {msg}"
    );
    assert_eq!(
        active_generation(&state, &pid).await,
        before,
        "a rejected request must not have done any work"
    );
}
