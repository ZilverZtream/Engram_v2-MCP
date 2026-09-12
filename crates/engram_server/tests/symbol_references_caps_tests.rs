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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kind_filter_is_applied_before_the_incoming_cap() {
    let (_tmp, state) = build_state();
    let target = func("Site/target.vb", "T", "Target");
    let mut nodes = vec![target.clone()];
    let mut edges = Vec::new();
    for i in 0..12 {
        let n = func(&format!("Site/a{i}.vb"), "Other", "UnrelatedKind");
        let mut e = calls(&n.node_id, &target.node_id);
        e.edge_kind = EdgeKind::Dependency;
        edges.push(e);
        nodes.push(n);
    }
    let caller = func("Site/z.vb", "C", "RealCaller");
    edges.push(calls(&caller.node_id, &target.node_id));
    nodes.push(caller);
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    let engram = Engram::new(state);
    let out = engram
        .handle_find_symbol_references(
            serde_json::from_value(json!({
                "project_id": PID, "symbol_name": "Target", "max_incoming": 1,
                "edge_kind_filter": ["calls"]
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &out.content[0].as_text().unwrap().text;
    assert!(text.contains("RealCaller"), "{text}");
    assert!(!text.contains("UnrelatedKind"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_reference_kind_is_rejected_and_isolated_symbols_remain_graph_results() {
    let (_tmp, state) = build_state();
    state
        .graph
        .upsert_nodes(PID, &[func("Site/only.vb", "C", "Isolated")])
        .unwrap();
    let engram = Engram::new(state);
    let bad = engram
        .handle_find_symbol_references(
            serde_json::from_value(json!({
                "project_id": PID, "symbol_name": "Isolated", "edge_kind_filter": ["calss"]
            }))
            .unwrap(),
        )
        .await;
    assert!(bad.is_err());
    let out = refs(&engram, "Isolated", 1).await;
    assert!(out.contains("Symbol:"), "{out}");
    assert!(!out.contains("No graph symbol found"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outgoing_reference_truncation_is_visible() {
    let (_tmp, state) = build_state();
    let target = func("Site/source.vb", "C", "Source");
    let a = func("Site/a.vb", "A", "A");
    let b = func("Site/b.vb", "B", "B");
    state
        .graph
        .upsert_nodes(PID, &[target.clone(), a.clone(), b.clone()])
        .unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[
                calls(&target.node_id, &a.node_id),
                calls(&target.node_id, &b.node_id),
            ],
        )
        .unwrap();
    let out = Engram::new(state)
        .handle_find_symbol_references(
            serde_json::from_value(json!({
                "project_id": PID, "symbol_name": "Source", "max_outgoing_per_kind": 1
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        out.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("truncated at 1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definition_scope_preserves_callers_outside_the_selected_file() {
    let (_tmp, state) = build_state();
    let target = func("Site/ata/target.vb", "T", "Target");
    let caller = func("Site/map/caller.vb", "C", "ExternalCaller");
    let wrong = func("Site/ata-old/target.vb", "T", "Target");
    state
        .graph
        .upsert_nodes(PID, &[target.clone(), caller.clone(), wrong])
        .unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&caller.node_id, &target.node_id)])
        .unwrap();
    let out = Engram::new(state)
        .handle_find_symbol_references(
            serde_json::from_value(json!({
                "project_id": PID, "symbol_name": "Target", "file_scope": "Site/ata"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &out.content[0].as_text().unwrap().text;
    assert!(text.contains("ExternalCaller"), "{text}");
    assert!(!text.contains("ata-old"), "{text}");
}

#[tokio::test]
async fn declaration_anchors_are_not_presented_as_verified_call_sites() {
    let (_tmp, state) = build_state();
    let target = func("Site/target.vb", "Target", "Run");
    let caller = func("Site/caller.vb", "Caller", "CallRun");
    state
        .graph
        .upsert_nodes(PID, &[target.clone(), caller.clone()])
        .unwrap();
    let mut edge = calls(&caller.node_id, &target.node_id);
    edge.metadata = Some(json!({"src_line":"12"}));
    state.graph.upsert_edges(PID, &[edge]).unwrap();
    let out = refs(&Engram::new(state), "Run", 10).await;
    assert!(
        out.contains("extractor anchor L12; call site unverified"),
        "{out}"
    );
    assert!(
        !out.contains("@L12"),
        "a declaration anchor is not a proven call-site line"
    );
}

#[tokio::test]
async fn indexed_call_sites_survive_unrelated_high_degree_metadata_crowding() {
    let (_tmp, state) = build_state();
    let target = func("src/target.vb", "Target", "Run");
    let caller = func("src/caller.vb", "Caller", "Execute");
    state
        .graph
        .upsert_nodes(PID, &[target.clone(), caller.clone()])
        .unwrap();
    let mut edges = Vec::new();
    for i in 0..1100 {
        let mut edge = calls(&target.node_id, &format!("child:{i}"));
        edge.edge_kind = EdgeKind::Contains;
        edges.push(edge);
    }
    let mut edge = calls(&caller.node_id, &target.node_id);
    edge.metadata =
        Some(json!({"src_line":"12", "call_site_line":"34", "call_site_lines":[34,40]}));
    edges.push(edge);
    state.graph.upsert_edges(PID, &edges).unwrap();
    let result = Engram::new(state)
        .handle_find_symbol_references(
            serde_json::from_value(json!({
                "project_id":PID,"symbol_name":"Run","edge_kind_filter":["calls"]
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("indexed call sites L34, L40"), "{text}");
    assert!(!text.contains("extractor anchor L12"), "{text}");
    assert!(text.contains("verify source freshness"), "{text}");
}

#[tokio::test]
async fn call_site_location_truncation_is_reported() {
    let (_tmp, state) = build_state();
    let target = func("src/target.vb", "Target", "Run");
    let caller = func("src/caller.vb", "Caller", "Execute");
    state
        .graph
        .upsert_nodes(PID, &[target.clone(), caller.clone()])
        .unwrap();
    let mut edge = calls(&caller.node_id, &target.node_id);
    edge.metadata = Some(json!({"call_site_lines":(1..=25).collect::<Vec<_>>()}));
    state.graph.upsert_edges(PID, &[edge]).unwrap();
    let text = refs(&Engram::new(state), "Run", 10).await;
    assert!(
        text.contains("locations truncated; inspect source"),
        "{text}"
    );
    assert!(!text.contains("L21"), "{text}");
}
