#![allow(clippy::unwrap_used)]
//! Row-7 audit (docs/audits/06-causal-trace-engine.md) A6 for
//! `find_connection_path`: the no-path message states the depth that was
//! actually searched (the request is clamped to 1..=12) and the edge-kind
//! set the search used — a caller who passed `max_depth: 50` must not be
//! told "within 50 hops".

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::models::FindConnectionPathRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "conn-path-test";

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
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state)
}

fn func(name: &str) -> Node {
    Node {
        node_id: format!("sym:function:Site/x.vb:api.{name}:1"),
        node_type: "function".into(),
        name: name.into(),
        namespace: "api".into(),
        language: "vbnet".into(),
        file_path: RelPath::new("Site/x.vb"),
        start_line: 1,
        end_line: 3,
        generation: 1,
        metadata: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_no_path_message_states_the_clamped_depth_and_the_kind_set() {
    let (_tmp, state) = build_state();
    let a = func("Alpha");
    let b = func("Omega");
    let (aid, bid) = (a.node_id.clone(), b.node_id.clone());
    state.graph.upsert_nodes(PID, &[a, b]).unwrap();
    let engram = Engram::new(state);
    let req: FindConnectionPathRequest =
        serde_json::from_value(json!({"project_id": PID, "from": aid, "to": bid, "max_depth": 50}))
            .unwrap();
    let res = engram.handle_find_connection_path(req).await.unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();
    assert!(
        text.contains("within 12 hops"),
        "max_depth 50 is clamped to 12 — the message must say 12:\n{text}"
    );
    assert!(
        !text.contains("within 50 hops"),
        "must not echo the unclamped request:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("edge kinds")
            || text.to_lowercase().contains("all edge kinds"),
        "the searched kind set must be stated:\n{text}"
    );
}
