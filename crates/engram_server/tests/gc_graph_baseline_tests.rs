#![allow(clippy::unwrap_used)]
//! The hourly GC and the manual `incremental_indexing_gc` purge the GRAPH
//! against the LAST FULL INDEX generation. Incremental updates write nodes
//! at generations ABOVE it, so "stale" must mean OLDER than the baseline —
//! never "different from" it. Live on the pilot corpus (2026-08-28) the `!=` reading
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

async fn build_state(tmp: &std::path::Path) -> AppState {
    let data_dir = tmp.join("data");
    let project_dir = tmp.join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    drop(git2::Repository::init(&project_dir).unwrap());
    let source = project_dir.join("baseline.rs");
    std::fs::write(&source, "// baseline revision 1 x\n").unwrap();
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
        .register_source_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    // The public producer must establish a real managed full baseline1,
    // followed by genuine changed-byte revisions2..5. A raw numeric HEAD
    // cannot authorize either source reads or collection.
    let engram = Engram::new(state.clone());
    engram.handle_repair_project(serde_json::from_value(json!({
        "project_id": PID, "scope": "full", "wipe_and_reindex": false,
        "max_commits": 0, "index_antipatterns": false
    })).unwrap()).await.unwrap();
    finish_owned_jobs(&state).await;
    assert_eq!(state.registry.active_source_generation(PID).unwrap(), 1);
    for generation in 2..=5 {
        std::fs::write(&source, format!("// baseline revision {generation} {}\n", "x".repeat(generation))).unwrap();
        engram.handle_update_project(serde_json::from_value(json!({
            "project_id": PID, "wait": true, "max_commits": 0, "index_antipatterns": false
        })).unwrap()).await.unwrap();
        finish_owned_jobs(&state).await;
        assert_eq!(state.registry.active_source_generation(PID).unwrap(), generation as u64);
    }
    assert_eq!(state.registry.get_meta(PID, "last_full_index_generation").unwrap().as_deref(), Some("1"));
    let project = engram_server::services::project_service::ensure_project_runtime(&state, PID).await.unwrap();
    assert!(project.search.pin_source_generation(PID, 5).unwrap().is_managed());
    assert_eq!(project.search.count_docs_in_generation(PID, "memory", 5).unwrap(), 1);
    drop(project);
    drop(engram);
    state
}

async fn finish_owned_jobs(state: &AppState) {
    loop {
        let jobs = state.active_jobs.write().await.drain().map(|(_, task)| task).collect::<Vec<_>>();
        if jobs.is_empty() { break; }
        for job in jobs { job.await.unwrap(); }
    }
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

#[test]
fn hourly_gc_keeps_nodes_newer_than_the_last_full_index() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Finish the request and actual blocking owners before removing this tree,
    // including panic unwinding (runtime declared after TempDir).
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(async {
    let state = build_state(tmp.path()).await;
    seed(&state);

    assert_eq!(purge_project_old_gens(&state, PID).await.unwrap(),
        engram_server::actors::gc::GcOutcome::Purged,
        "This retention oracle must exercise admitted collection, not a skipped legacy fixture");

    assert_eq!(
        both_present(&state),
        (true, true),
        "(full-index node present, incremental node present) — the GC deleted an incrementally indexed node"
    );

    });
    drop(runtime);
    tmp.close().unwrap();
}

#[test]
fn manual_gc_defaults_to_the_full_index_baseline() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Finish the request and actual blocking owners before removing this tree,
    // including panic unwinding (runtime declared after TempDir).
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(async {
    let state = build_state(tmp.path()).await;
    seed(&state);
    let engram = Engram::new(state);

    let req: IncrementalIndexingGcRequest =
        serde_json::from_value(json!({ "project_id": PID, "compact_vectors": false })).unwrap();
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

    });
    drop(runtime);
    tmp.close().unwrap();
}

#[test]
fn explicit_gc_cutoff_cannot_remove_unchanged_live_graph_nodes() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Finish the request and actual blocking owners before removing this tree,
    // including panic unwinding (runtime declared after TempDir).
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(async {
    let state = build_state(tmp.path()).await;
    seed(&state);
    let engram = Engram::new(state);
    let req = serde_json::from_value(json!({
        "project_id": PID, "target_generation": 5, "compact_vectors": false
    })).unwrap();
    engram.handle_incremental_indexing_gc(req).await.unwrap();
    assert_eq!(both_present(&engram.state), (true, true),
        "a search cutoff must not erase unchanged graph content below the incremental head");
    assert_eq!(engram.state.graph.count_edges(PID).unwrap(), 1);

    });
    drop(runtime);
    tmp.close().unwrap();
}

#[test]
fn future_gc_cutoff_rejects_without_deleting_published_graph_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Finish the request and actual blocking owners before removing this tree,
    // including panic unwinding (runtime declared after TempDir).
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(async {
    let state = build_state(tmp.path()).await;
    seed(&state);
    let engram = Engram::new(state);
    let req = serde_json::from_value(json!({
        "project_id": PID, "target_generation": 6, "compact_vectors": false
    })).unwrap();
    assert!(engram.handle_incremental_indexing_gc(req).await.is_err());
    assert_eq!(both_present(&engram.state), (true, true));
    assert_eq!(engram.state.graph.count_edges(PID).unwrap(), 1);

    });
    drop(runtime);
    tmp.close().unwrap();
}
