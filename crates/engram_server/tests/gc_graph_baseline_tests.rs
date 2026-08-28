#![allow(clippy::unwrap_used)]
//! The hourly GC and the manual `incremental_indexing_gc` purge the GRAPH
//! against the LAST FULL INDEX generation. Incremental updates write nodes
//! at generations ABOVE it, so "stale" must mean OLDER than the baseline —
//! never "different from" it. Live on OciusX (2026-08-28) the `!=` reading
//! deleted every incrementally re-indexed file's nodes every hour and the
//! watcher re-added them (`[node_missing]` × 175 files, functions 18,144 →
//! 9,904 between ticks).

use engram_core::config::Config;
use engram_core::{ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::actors::gc::purge_project_old_gens;
use engram_server::models::IncrementalIndexingGcRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "gc-baseline-test";

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    // Full index landed at gen 1; three incremental updates since (gen 5).
    state
        .registry
        .set_meta(PID, "active_generation", "5")
        .unwrap();
    state
        .registry
        .set_meta(PID, "last_full_index_generation", "1")
        .unwrap();
    (tmp, state)
}

fn node(id: &str, generation: u64) -> Node {
    Node {
        node_id: id.into(),
        node_type: "function".into(),
        name: id.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new("a.vb"),
        start_line: 1,
        end_line: 2,
        generation,
        metadata: None,
    }
}

fn seed(state: &AppState) {
    state
        .graph
        .upsert_nodes(
            PID,
            &[node("from_full_index", 1), node("from_incremental", 5)],
        )
        .unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[Edge {
                source_id: "from_incremental".into(),
                target_id: "from_full_index".into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                edge_kind: EdgeKind::Calls,
                weight: 1,
                generation: 5,
                metadata: None,
                updated_at_ms: 1,
            }],
        )
        .unwrap();
}

fn both_present(state: &AppState) -> (bool, bool) {
    (
        state
            .graph
            .get_node(PID, "from_full_index")
            .unwrap()
            .is_some(),
        state
            .graph
            .get_node(PID, "from_incremental")
            .unwrap()
            .is_some(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hourly_gc_keeps_nodes_newer_than_the_last_full_index() {
    let (_tmp, state) = build_state();
    seed(&state);

    purge_project_old_gens(&state, PID).await.unwrap();

    assert_eq!(
        both_present(&state),
        (true, true),
        "(full-index node present, incremental node present) — the GC deleted an incrementally indexed node"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_gc_defaults_to_the_full_index_baseline() {
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let req: IncrementalIndexingGcRequest =
        serde_json::from_value(json!({ "project_id": PID })).unwrap();
    let res = engram.handle_incremental_indexing_gc(req).await.unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();

    assert_eq!(
        both_present(&engram.state),
        (true, true),
        "manual GC with no target must purge below the FULL-index baseline, not the incremental counter:\n{text}"
    );
    assert!(
        text.contains("below generation 1") || text.contains("older than generation 1"),
        "output must name the baseline it used:\n{text}"
    );
}
