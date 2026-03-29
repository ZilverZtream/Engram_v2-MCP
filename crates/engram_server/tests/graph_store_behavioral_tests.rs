#![allow(clippy::unwrap_used)]
//! Behavioral tests for the production GraphStore (Subsystem 6).
//!
//! Tests call production code directly:
//!  - `engram_graph::GraphStore::open`
//!  - `upsert_nodes`, `get_node`, `count_nodes`, `count_nodes_by_type`
//!  - `upsert_edges`, `neighbors`, `list_edges_by_kind`
//!  - `count_edges`, `count_edges_by_kind`
//!  - `increment_edge` (co-occurrence weight accumulation)
//!  - `get_node` / `query_nodes` lookup contracts

use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};

// ── helpers ───────────────────────────────────────────────────────────────────

fn make_node(node_id: &str, node_type: &str, name: &str) -> Node {
    Node {
        node_id: node_id.to_string(),
        node_type: node_type.to_string(),
        name: name.to_string(),
        namespace: "test-ns".to_string(),
        language: "rust".to_string(),
        file_path: RelPath::new("src/lib.rs"),
        start_line: 1,
        end_line: 20,
        generation: 1,
        metadata: None,
    }
}

fn make_edge(source: &str, target: &str, kind: EdgeKind, weight: u32) -> Edge {
    Edge {
        source_id: source.to_string(),
        target_id: target.to_string(),
        namespace: "test-ns".to_string(),
        language: "rust".to_string(),
        edge_kind: kind,
        weight,
        generation: 1,
        metadata: None,
        updated_at_ms: 1_000_000,
    }
}

fn open_store(tmp: &tempfile::TempDir) -> GraphStore {
    GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open must succeed")
}

// ── GraphStore construction ───────────────────────────────────────────────────

/// GraphStore::open must succeed on a fresh tempdir path.
#[test]
fn graph_store_open_on_fresh_path_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let result = GraphStore::open(&tmp.path().join("graph.redb"));
    assert!(
        result.is_ok(),
        "GraphStore::open must succeed on a fresh path; got: {:?}",
        result.err()
    );
}

// ── upsert_nodes / get_node ───────────────────────────────────────────────────

/// Upserting a node then getting it must return the same node.
#[test]
fn graph_store_upsert_and_get_node_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let node = make_node("node-001", "function", "my_function");
    store
        .upsert_nodes("proj-test", std::slice::from_ref(&node))
        .expect("upsert_nodes must succeed");

    let retrieved = store
        .get_node("proj-test", "node-001")
        .expect("get_node must not error");

    assert!(retrieved.is_some(), "get_node must return the upserted node");
    let n = retrieved.unwrap();
    assert_eq!(n.node_id, "node-001");
    assert_eq!(n.node_type, "function");
    assert_eq!(n.name, "my_function");
}

/// get_node for a non-existent node must return None, not Err.
#[test]
fn graph_store_get_node_nonexistent_returns_none() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let result = store
        .get_node("proj-test", "no-such-node")
        .expect("get_node must not error");
    assert!(
        result.is_none(),
        "get_node for unknown node_id must return None"
    );
}

/// Upserting the same node twice must update it (idempotent upsert).
#[test]
fn graph_store_upsert_node_is_idempotent() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let n1 = make_node("node-idem", "function", "old_name");
    store.upsert_nodes("proj", &[n1]).expect("first upsert");

    let mut n2 = make_node("node-idem", "function", "new_name");
    n2.generation = 2;
    store.upsert_nodes("proj", &[n2]).expect("second upsert");

    let count = store.count_nodes("proj").expect("count");
    assert_eq!(count, 1, "idempotent upsert must not create duplicate nodes");
}

// ── count_nodes / count_nodes_by_type ────────────────────────────────────────

/// count_nodes must return the exact number of upserted nodes.
#[test]
fn graph_store_count_nodes_matches_upserted_count() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let nodes = vec![
        make_node("n1", "function", "fn_a"),
        make_node("n2", "class", "ClassB"),
        make_node("n3", "function", "fn_c"),
    ];
    store.upsert_nodes("proj-count", &nodes).expect("upsert");

    let count = store.count_nodes("proj-count").expect("count_nodes");
    assert_eq!(count, 3, "count_nodes must return 3 after upserting 3 nodes");
}

/// count_nodes_by_type must report the correct breakdown.
#[test]
fn graph_store_count_nodes_by_type_correct_breakdown() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let nodes = vec![
        make_node("n1", "function", "fn_a"),
        make_node("n2", "function", "fn_b"),
        make_node("n3", "class", "ClassC"),
    ];
    store.upsert_nodes("proj-types", &nodes).expect("upsert");

    let by_type = store
        .count_nodes_by_type("proj-types")
        .expect("count_nodes_by_type");
    assert_eq!(
        by_type.get("function").copied().unwrap_or(0),
        2,
        "must count 2 function nodes"
    );
    assert_eq!(
        by_type.get("class").copied().unwrap_or(0),
        1,
        "must count 1 class node"
    );
}

/// count_nodes must return 0 for a project with no nodes.
#[test]
fn graph_store_count_nodes_zero_for_empty_project() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let count = store.count_nodes("proj-empty").expect("count_nodes");
    assert_eq!(count, 0, "count_nodes must return 0 for a project with no nodes");
}

// ── upsert_edges / neighbors / count_edges ───────────────────────────────────

/// Upserting an edge and then querying neighbors must return the target.
#[test]
fn graph_store_upsert_edge_and_neighbors_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    // Nodes must exist before edges
    let nodes = vec![
        make_node("src-node", "function", "caller"),
        make_node("tgt-node", "function", "callee"),
    ];
    store.upsert_nodes("proj-edges", &nodes).expect("upsert nodes");

    let edge = make_edge("src-node", "tgt-node", EdgeKind::Dependency, 1);
    store
        .upsert_edges("proj-edges", &[edge])
        .expect("upsert_edges must succeed");

    let neighbors = store
        .neighbors("proj-edges", EdgeKind::Dependency, "src-node", 100)
        .expect("neighbors must not error");

    assert!(
        !neighbors.is_empty(),
        "neighbors must return at least one neighbor after edge upsert"
    );
    let neighbor_ids: Vec<&str> = neighbors.iter().map(|(id, _w)| id.as_str()).collect();
    assert!(
        neighbor_ids.contains(&"tgt-node"),
        "neighbors must include 'tgt-node'; got: {neighbor_ids:?}"
    );
}

/// count_edges must return the correct number after upserting edges.
#[test]
fn graph_store_count_edges_matches_upserted_count() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let nodes = vec![
        make_node("a", "function", "a"),
        make_node("b", "function", "b"),
        make_node("c", "class", "c"),
    ];
    store.upsert_nodes("proj-edge-count", &nodes).expect("upsert");

    let edges = vec![
        make_edge("a", "b", EdgeKind::Dependency, 1),
        make_edge("b", "c", EdgeKind::Contains, 1),
    ];
    store
        .upsert_edges("proj-edge-count", &edges)
        .expect("upsert edges");

    let count = store
        .count_edges("proj-edge-count")
        .expect("count_edges");
    assert_eq!(count, 2, "count_edges must return 2 after upserting 2 edges");
}

/// count_edges_by_kind must report the correct edge-type breakdown.
#[test]
fn graph_store_count_edges_by_kind_correct_breakdown() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let nodes = vec![
        make_node("x", "function", "x"),
        make_node("y", "function", "y"),
        make_node("z", "class", "z"),
    ];
    store.upsert_nodes("proj-kind", &nodes).expect("upsert");

    let edges = vec![
        make_edge("x", "y", EdgeKind::Dependency, 1),
        make_edge("x", "z", EdgeKind::Dependency, 1),
        make_edge("z", "y", EdgeKind::Contains, 1),
    ];
    store.upsert_edges("proj-kind", &edges).expect("upsert");

    let by_kind = store
        .count_edges_by_kind("proj-kind")
        .expect("count_edges_by_kind");

    let dep_count = by_kind
        .get(EdgeKind::Dependency.as_str())
        .copied()
        .unwrap_or(0);
    let contains_count = by_kind
        .get(EdgeKind::Contains.as_str())
        .copied()
        .unwrap_or(0);

    assert_eq!(dep_count, 2, "must count 2 Dependency edges");
    assert_eq!(contains_count, 1, "must count 1 Contains edge");
}

// ── list_edges_by_kind ────────────────────────────────────────────────────────

/// list_edges_by_kind must return only edges of the requested kind.
#[test]
fn graph_store_list_edges_by_kind_filters_correctly() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let nodes = vec![
        make_node("f1", "function", "f1"),
        make_node("f2", "function", "f2"),
        make_node("c1", "class", "c1"),
    ];
    store.upsert_nodes("proj-filter", &nodes).expect("upsert");

    let edges = vec![
        make_edge("f1", "f2", EdgeKind::CoOccurrence, 3),
        make_edge("f1", "c1", EdgeKind::Imports, 1),
    ];
    store.upsert_edges("proj-filter", &edges).expect("upsert");

    let co_occ_edges = store
        .list_edges_by_kind("proj-filter", EdgeKind::CoOccurrence, 100)
        .expect("list_edges_by_kind");

    assert_eq!(
        co_occ_edges.len(),
        1,
        "list_edges_by_kind(CoOccurrence) must return exactly 1 edge"
    );
    assert_eq!(co_occ_edges[0].source_id, "f1");
    assert_eq!(co_occ_edges[0].target_id, "f2");
    assert_eq!(co_occ_edges[0].weight, 3);
}

// ── increment_edge — weight accumulation ─────────────────────────────────────

/// increment_edge must accumulate weight on repeated calls.
#[test]
fn graph_store_increment_edge_accumulates_weight() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let nodes = vec![
        make_node("src", "function", "src"),
        make_node("dst", "function", "dst"),
    ];
    store.upsert_nodes("proj-inc", &nodes).expect("upsert");

    // Increment 3 times
    for _ in 0..3 {
        store
            .increment_edge(
                "proj-inc",
                "test-ns",
                "rust",
                EdgeKind::CoOccurrence,
                "src",
                "dst",
                1,
                1u64,
            )
            .expect("increment_edge");
    }

    let edges = store
        .list_edges_by_kind("proj-inc", EdgeKind::CoOccurrence, 100)
        .expect("list_edges");
    assert_eq!(edges.len(), 1, "must have exactly 1 co-occurrence edge");
    assert_eq!(
        edges[0].weight, 3,
        "weight must be 3 after 3 increments; got {}",
        edges[0].weight
    );
}

// ── project isolation ─────────────────────────────────────────────────────────

/// Nodes from different projects must not contaminate each other.
#[test]
fn graph_store_project_isolation_prevents_cross_project_leakage() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    store
        .upsert_nodes("proj-A", &[make_node("node-a", "function", "fn_a")])
        .expect("upsert proj-A");
    store
        .upsert_nodes("proj-B", &[make_node("node-b", "function", "fn_b")])
        .expect("upsert proj-B");

    let count_a = store.count_nodes("proj-A").expect("count proj-A");
    let count_b = store.count_nodes("proj-B").expect("count proj-B");

    assert_eq!(count_a, 1, "proj-A must have exactly 1 node");
    assert_eq!(count_b, 1, "proj-B must have exactly 1 node");

    // proj-A's node must not be visible from proj-B
    let cross = store
        .get_node("proj-B", "node-a")
        .expect("cross-project get");
    assert!(
        cross.is_none(),
        "proj-B must not see proj-A's node 'node-a' — project isolation failure"
    );
}
