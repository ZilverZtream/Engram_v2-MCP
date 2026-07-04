#![allow(clippy::unwrap_used)]
//! Regression tests for resolve_app_code_globals.
//!
//! Bug 1 (garbage FQN): The inferred_fqn fallback parsed canonical node_ids
//! like "sym:function:Site/App_Code/foo.vb:Name:42" and extracted the path
//! fragment as a "FQN". resolve_symbol can't resolve path-shaped strings.
//!
//! Bug 2 (missing registration): When inferred_fqn was None (no metadata.fqn,
//! no dots in name), the node never got registered in terminal_to_fqn. So
//! bare-name call targets like "SafeRedirect" had no lookup entry.
//!
//! Bug 3 (no tiebreaker): resolve_symbol was called with prefer_file_path=None,
//! so ambiguous FQNs (multiple nodes with same terminal) always failed.

use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};

fn open_store(tmp: &tempfile::TempDir) -> GraphStore {
    GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open")
}

fn make_app_code_function(node_id: &str, name: &str, file_path: &str, fqn: Option<&str>) -> Node {
    let metadata = fqn.map(|f| {
        let mut m = serde_json::Map::new();
        m.insert("fqn".into(), serde_json::Value::String(f.to_string()));
        serde_json::Value::Object(m)
    });
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
        metadata,
    }
}

/// Bug 1 regression: canonical node_ids with path separators must NOT be
/// treated as FQNs. The "/" in the path caused `.contains('.')` to match
/// on the ".vb" extension, storing garbage in terminal_to_fqn.
#[test]
fn rejects_path_shaped_node_id_as_fqn() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-reject-paths";

    // Node with canonical path-shaped node_id, NO metadata.fqn.
    // The old bug would extract "Site/App_Code/sharedfunc.vb:sharedfunc.SafeRedirect:2534"
    // as a "FQN" and store it in terminal_to_fqn. resolve_symbol can't resolve that.
    let target_node = make_app_code_function(
        "sym:function:Site/App_Code/shared-code/sharedfunc.vb:sharedfunc.SafeRedirect:2534",
        "SafeRedirect",
        "Site/App_Code/shared-code/sharedfunc.vb",
        None, // no metadata.fqn — triggers the fallback branches
    );

    let source_node = Node {
        node_id: "sym:function:Site/Pages/Login.aspx.vb:Page_Load:10".to_string(),
        node_type: "function".to_string(),
        name: "Page_Load".to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        file_path: RelPath::new("Site/Pages/Login.aspx.vb"),
        start_line: 10,
        end_line: 30,
        generation: 1,
        metadata: None,
    };

    graph
        .upsert_nodes(pid, &[target_node.clone(), source_node.clone()])
        .unwrap();

    // Unresolved call: Page_Load -> ::SafeRedirect
    let call_edge = Edge {
        source_id: source_node.node_id.clone(),
        target_id: "::SafeRedirect".to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1_000_000,
    };
    graph.upsert_edges(pid, &[call_edge]).unwrap();

    let _resolved =
        engram_server::services::graph_service::resolve_app_code_globals(&graph, pid, 1).unwrap();

    // Step 2 should resolve ::SafeRedirect via app_code_by_name lookup.
    // Verify a Dependency edge was created pointing to the target node.
    let dep_edges = graph.list_edges(pid, Some(EdgeKind::Dependency)).unwrap();
    let resolved: Vec<_> = dep_edges
        .iter()
        .filter(|e| e.source_id == source_node.node_id && e.target_id == target_node.node_id)
        .collect();

    assert!(
        !resolved.is_empty(),
        "Expected ::SafeRedirect to resolve to target node. \
         Dep edges from source: {:?}",
        dep_edges
            .iter()
            .filter(|e| e.source_id == source_node.node_id)
            .map(|e| &e.target_id)
            .collect::<Vec<_>>()
    );
}

/// Bug 2 + 3 regression: when two App_Code nodes share a terminal name
/// (e.g., both have "Create"), the prefer_file_path tiebreaker resolves
/// ambiguity. Also verifies that nodes without metadata.fqn still get
/// registered in terminal_to_fqn via node.name fallback.
#[test]
fn disambiguates_by_source_file_path_with_bare_names() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-disambig";

    // Two App_Code functions named "Create" in different files, no metadata.fqn.
    let node_a = make_app_code_function(
        "sym:function:Site/App_Code/handelselogg.vb:Create:10",
        "Create",
        "Site/App_Code/handelselogg.vb",
        None,
    );
    let node_b = make_app_code_function(
        "sym:function:Site/App_Code/orders.vb:Create:20",
        "Create",
        "Site/App_Code/orders.vb",
        None,
    );

    // Source lives in same file as node_a.
    let source_node = Node {
        node_id: "sym:function:Site/App_Code/handelselogg.vb:DoStuff:50".to_string(),
        node_type: "function".to_string(),
        name: "DoStuff".to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        file_path: RelPath::new("Site/App_Code/handelselogg.vb"),
        start_line: 50,
        end_line: 60,
        generation: 1,
        metadata: None,
    };

    graph
        .upsert_nodes(pid, &[node_a.clone(), node_b.clone(), source_node.clone()])
        .unwrap();

    // Call edge with composite target that Step 3 processes.
    // extract_terminal_name extracts "Create" from this format.
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

    let _resolved =
        engram_server::services::graph_service::resolve_app_code_globals(&graph, pid, 1).unwrap();

    // Step 3 should have rewritten the call edge.
    let all_calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();

    let rewritten: Vec<_> = all_calls
        .iter()
        .filter(|e| e.source_id == source_node.node_id && e.target_id != call_edge.target_id)
        .collect();

    assert!(
        !rewritten.is_empty(),
        "Expected Step 3 to rewrite the call edge. \
         Calls from source: {:?}",
        all_calls
            .iter()
            .filter(|e| e.source_id == source_node.node_id)
            .map(|e| &e.target_id)
            .collect::<Vec<_>>()
    );

    // Should resolve to node_a (same file as source).
    assert_eq!(
        rewritten[0].target_id, node_a.node_id,
        "Expected resolution to node_a (same file as source), got {}",
        rewritten[0].target_id
    );
}

/// Verifies that nodes WITH metadata.fqn still work correctly (no regression).
#[test]
fn resolves_with_metadata_fqn() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-with-fqn";

    let target_node = make_app_code_function(
        "sym:function:Site/App_Code/sharedfunc.vb:TranslateUnit:452",
        "TranslateUnit",
        "Site/App_Code/sharedfunc.vb",
        Some("sharedfunc.TranslateUnit"), // metadata.fqn present
    );

    let source_node = Node {
        node_id: "sym:function:Site/Pages/Report.aspx.vb:Render:20".to_string(),
        node_type: "function".to_string(),
        name: "Render".to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        file_path: RelPath::new("Site/Pages/Report.aspx.vb"),
        start_line: 20,
        end_line: 40,
        generation: 1,
        metadata: None,
    };

    graph
        .upsert_nodes(pid, &[target_node.clone(), source_node.clone()])
        .unwrap();

    let call_edge = Edge {
        source_id: source_node.node_id.clone(),
        target_id: "::TranslateUnit".to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1_000_000,
    };
    graph.upsert_edges(pid, &[call_edge]).unwrap();

    let _resolved =
        engram_server::services::graph_service::resolve_app_code_globals(&graph, pid, 1).unwrap();

    let dep_edges = graph.list_edges(pid, Some(EdgeKind::Dependency)).unwrap();
    let resolved: Vec<_> = dep_edges
        .iter()
        .filter(|e| e.source_id == source_node.node_id && e.target_id == target_node.node_id)
        .collect();

    assert!(
        !resolved.is_empty(),
        "Expected ::TranslateUnit to resolve via metadata.fqn. \
         Dep edges: {:?}",
        dep_edges
            .iter()
            .map(|e| (&e.source_id, &e.target_id))
            .collect::<Vec<_>>()
    );
}
