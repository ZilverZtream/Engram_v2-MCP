#![allow(clippy::unwrap_used)]
//! Row-4 audit (docs/audits/04-concept-and-consumer-discovery.md) A8 for
//! `find_symbol_references`: the initial symbol fetch (50) has no
//! truncation flag — "matches 50 distinct symbols" was a cap stated as a
//! fact (live: `GetByID` = exactly 50); the label resolution cap (400) is
//! silent; a graph failure renders as "not found". Every cap is a fact
//! in the output.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::FindSymbolReferencesRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "symref-caps-test";

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

fn func(path: &str, class: &str, name: &str) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{class}.{name}:1"),
        node_type: "function".into(),
        name: name.into(),
        namespace: class.into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 3,
        generation: 1,
        metadata: None,
    }
}

fn calls(src: &str, tgt: &str) -> Edge {
    Edge {
        source_id: src.into(),
        target_id: tgt.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

async fn refs(engram: &Engram, symbol: &str, max_incoming: usize) -> String {
    let req: FindSymbolReferencesRequest = serde_json::from_value(
        json!({"project_id": PID, "symbol_name": symbol, "max_incoming": max_incoming}),
    )
    .unwrap();
    let res = engram.handle_find_symbol_references(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_than_fifty_same_named_symbols_is_reported_as_a_fetch_cap_not_a_count() {
    let (_tmp, state) = build_state();
    // Every symbol has one real reference (a symbol without edges is not
    // a "reference" result and falls through to the lexical path).
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    for i in 0..55 {
        let sym = func(
            &format!("Site/App_Code/c{i:02}.vb"),
            &format!("c{i:02}"),
            "GetByID",
        );
        let caller = func(
            &format!("Site/pages/p{i:02}.aspx.vb"),
            "p",
            &format!("Use{i:02}"),
        );
        edges.push(calls(&caller.node_id, &sym.node_id));
        nodes.push(sym);
        nodes.push(caller);
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    let engram = Engram::new(state);
    let out = refs(&engram, "GetByID", 200).await;
    assert!(
        out.contains("50+") || out.to_lowercase().contains("fetch cap"),
        "55 symbols exist; the 50 fetched must be presented as a cap, not a total:\n{out}"
    );
    assert!(
        !out.contains("matches 50 distinct symbols —"),
        "the cap must not be stated as an exact count:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_label_resolution_cap_is_stated() {
    let (_tmp, state) = build_state();
    let target = func("Site/App_Code/target.vb", "t", "Check_pr_id");
    let tid = target.node_id.clone();
    let mut nodes = vec![target];
    let mut edges = Vec::new();
    for i in 0..450 {
        let caller = func(
            &format!("Site/App_Code/caller{i:03}.vb"),
            "k",
            &format!("Caller{i:03}"),
        );
        edges.push(calls(&caller.node_id, &tid));
        nodes.push(caller);
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    let engram = Engram::new(state);
    let out = refs(&engram, "Check_pr_id", 500).await;
    assert!(
        out.contains("labels") && out.contains("400"),
        "450 endpoints, 400 labels resolved — the cap must be stated:\n{}",
        out.lines()
            .filter(|l| l.contains("cap") || l.contains("label") || l.starts_with("##"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
