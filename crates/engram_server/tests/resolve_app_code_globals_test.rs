#![allow(clippy::unwrap_used)]
//! Regression test for resolve_app_code_globals prefer_file_path disambiguation.
//!
//! When multiple App_Code nodes share the same FQN terminal (e.g., overloads
//! in different files), the resolver must use the source edge's file_path as
//! a tiebreaker hint. Without this hint, resolve_symbol returns Ambiguous and
//! the edge stays unresolved (the regression that dropped rewrites from 3787 → 0).

use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};

fn open_store(tmp: &tempfile::TempDir) -> GraphStore {
    GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open")
}

fn make_app_code_node(
    node_id: &str,
    name: &str,
    file_path: &str,
    fqn: &str,
) -> Node {
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "fqn".into(),
        serde_json::Value::String(fqn.to_string()),
    );
    Node {
        node_id: node_id.to_string(),
        node_type: "function".to_string(),
        name: name.to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        file_path: RelPath::new(file_path),
        start_line: 1,
        end_line: 10,
        generation: 1,
        metadata: Some(serde_json::Value::Object(metadata)),
    }
}

#[test]
fn resolve_disambiguates_by_source_file_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-project";

    // Two App_Code nodes with the same terminal name "Create" but different files.
    let node_a = make_app_code_node(
        "sym:function:App_Code/handelselogg.vb:Create:10",
        "Create",
        "App_Code/handelselogg.vb",
        "_gd._gd.handelselogg.Create",
    );
    let node_b = make_app_code_node(
        "sym:function:App_Code/orders.vb:Create:20",
        "Create",
        "App_Code/orders.vb",
        "_gd._gd.orders.Create",
    );

    // Source node that lives in the same file as node_a.
    let source_node = Node {
        node_id: "sym:function:App_Code/handelselogg.vb:DoStuff:50".to_string(),
        node_type: "function".to_string(),
        name: "DoStuff".to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        file_path: RelPath::new("App_Code/handelselogg.vb"),
        start_line: 50,
        end_line: 60,
        generation: 1,
        metadata: None,
    };

    graph
        .upsert_nodes(pid, &[node_a.clone(), node_b.clone(), source_node.clone()])
        .unwrap();

    // Call edge with a non-App_Code composite target_id that Step 3 processes.
    // Format: sym:function:<path>:<name>:<line> — extract_terminal_name returns "Create"
    // from Case A (path-shaped composite). unresolved_target_name returns None
    // because line != "0", so Step 2 skips it and Step 3 handles it.
    let call_edge = Edge {
        source_id: source_node.node_id.clone(),
        target_id: "sym:function:Pages/SomePage.aspx.vb:Create:42".to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1_000_000,
    };
    graph.upsert_edges(pid, &[call_edge.clone()]).unwrap();

    // Run the resolver.
    let _resolved =
        engram_server::services::graph_service::resolve_app_code_globals(&graph, pid, 1).unwrap();

    // The edge should have been rewritten to target node_a (same file as source).
    let all_calls = graph
        .list_edges(pid, Some(EdgeKind::Calls))
        .unwrap();

    let rewritten: Vec<_> = all_calls
        .iter()
        .filter(|e| {
            e.source_id == source_node.node_id && e.target_id != call_edge.target_id
        })
        .collect();

    assert!(
        !rewritten.is_empty(),
        "Expected at least one rewritten call edge, got none. \
         All calls edges: {:?}",
        all_calls
            .iter()
            .filter(|e| e.source_id == source_node.node_id)
            .map(|e| &e.target_id)
            .collect::<Vec<_>>()
    );

    // The resolved target should be node_a (same file as source), not node_b.
    let target = &rewritten[0].target_id;
    assert_eq!(
        target, &node_a.node_id,
        "Expected resolution to node_a (same file as source), got {target}"
    );
}
