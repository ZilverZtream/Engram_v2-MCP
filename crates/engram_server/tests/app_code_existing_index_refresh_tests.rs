use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_server::services::graph_service::resolve_app_code_globals;
use serde_json::{Value, json};

fn node(id: &str, name: &str, generation: u64) -> Node {
    Node {
        node_id: id.into(),
        node_type: "function".into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new("App_Code/Service.vb"),
        start_line: 1,
        end_line: 5,
        generation,
        metadata: Some(json!({"fqn": name})),
    }
}
fn edge(source: &str, target: &str, kind: EdgeKind, metadata: Value, generation: u64) -> Edge {
    Edge {
        source_id: source.into(),
        target_id: target.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        edge_kind: kind,
        weight: 1,
        generation,
        metadata: Some(metadata),
        updated_at_ms: 1,
    }
}
fn setup() -> (tempfile::TempDir, GraphStore) {
    let temp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
    (temp, graph)
}

#[test]
fn symbol_resolution_preserves_app_code_refresh_substrate() {
    let (_temp, graph) = setup();
    graph.upsert_nodes("p", &[
        node("caller", "Caller.Run", 1),
        node("target", "Api.Write", 1),
    ]).unwrap();
    graph.upsert_edges("p", &[
        edge("caller", "::Api.Write", EdgeKind::Calls, json!({}), 1),
    ]).unwrap();
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    resolve_app_code_globals(&graph, "p", 2).unwrap();
    let edges = graph.list_edges("p", None).unwrap();
    assert!(edges.iter().any(|e| e.source_id == "caller"
        && e.target_id == "target" && e.edge_kind == EdgeKind::Dependency),
        "an unrelated incremental refresh must retain the derived dependency");
    assert!(edges.iter().any(|e| e.source_id == "caller"
        && e.target_id == "::Api.Write" && e.edge_kind == EdgeKind::Calls),
        "the substrate must survive both resolvers for future re-evaluation");
}

#[test]
fn retained_substrate_retracts_ambiguous_binding_across_both_resolvers() {
    let (_temp, graph) = setup();
    graph.upsert_nodes("p", &[
        node("caller", "Caller.Run", 1),
        node("target", "Api.Write", 1),
    ]).unwrap();
    graph.upsert_edges("p", &[
        edge("caller", "::Api.Write", EdgeKind::Calls, json!({}), 1),
    ]).unwrap();
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    graph.upsert_nodes("p", &[node("overload", "Api.Write", 2)]).unwrap();
    resolve_app_code_globals(&graph, "p", 2).unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    let edges = graph.list_edges("p", None).unwrap();
    assert_eq!(edges.len(), 1, "ambiguity must retract both derived edge kinds");
    assert_eq!(edges[0].target_id, "::Api.Write");
    assert_eq!(edges[0].generation, 1, "retention must not advance extraction generation");
}

#[test]
fn retained_substrate_cannot_rebind_after_source_reextraction() {
    let (_temp, graph) = setup();
    graph.upsert_nodes("p", &[
        node("caller", "Caller.Run", 1),
        node("target", "Api.Write", 1),
    ]).unwrap();
    graph.upsert_edges("p", &[
        edge("caller", "::Api.Write", EdgeKind::Calls, json!({}), 1),
    ]).unwrap();
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    graph.upsert_nodes("p", &[node("caller", "Caller.Run", 2)]).unwrap();
    resolve_app_code_globals(&graph, "p", 2).unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    assert!(graph.list_edges("p", None).unwrap().iter()
        .all(|e| e.target_id != "target"), "a removed call must remain removed");
}

#[test]
fn legacy_resolver_edges_without_original_target_are_retracted_and_valid_control_survives() {
    let (_temp, graph) = setup();
    graph
        .upsert_nodes(
            "p",
            &[
                node("caller", "Caller.Run", 1),
                node("bad", "Other.Write", 1),
                node("good", "Api.Write", 1),
            ],
        )
        .unwrap();
    let strong = edge(
        "control",
        "good",
        EdgeKind::Calls,
        json!({"resolution":"compiler_verified"}),
        1,
    );
    graph
        .upsert_edges(
            "p",
            &[
                edge("caller", "::External.Write", EdgeKind::Calls, json!({}), 1),
                edge(
                    "caller",
                    "bad",
                    EdgeKind::Calls,
                    json!({"original_target_name":"Write", "resolved_target_fqn":"Other.Write"}),
                    1,
                ),
                strong.clone(),
                edge("history", "bad", EdgeKind::TemporalCoupling, json!({}), 1),
            ],
        )
        .unwrap();
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    let edges = graph.list_edges("p", None).unwrap();
    assert!(
        !edges
            .iter()
            .any(|e| e.source_id == "caller" && e.target_id == "bad")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.source_id == "caller" && e.target_id == "::External.Write")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.source_id == "control" && e.metadata == strong.metadata)
    );
    assert!(
        edges
            .iter()
            .any(|e| e.edge_kind == EdgeKind::TemporalCoupling)
    );
    assert!(
        graph
            .find_incoming_edges_with_kind("p", Some(EdgeKind::Calls), "bad", 20)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn changed_target_and_empty_candidate_snapshot_remove_prior_owned_bindings() {
    for keep_unrelated_declaration in [false, true] {
        let (_temp, graph) = setup();
        if keep_unrelated_declaration {
            graph
                .upsert_nodes("p", &[node("other", "Other.Run", 1)])
                .unwrap();
        }
        graph.upsert_edges("p", &[
            edge("caller", "::Api.Write", EdgeKind::Calls, json!({}), 1),
            edge("caller", "gone", EdgeKind::Calls, json!({"resolved_from":"app_code", "original_target":"::Api.Write", "resolution":"app_code_unique"}), 1),
        ]).unwrap();
        resolve_app_code_globals(&graph, "p", 1).unwrap();
        assert!(
            !graph
                .list_edges("p", None)
                .unwrap()
                .iter()
                .any(|e| e.target_id == "gone")
        );
    }
}

#[test]
fn same_generation_rebinding_and_repeated_refresh_are_idempotent() {
    let (_temp, graph) = setup();
    graph
        .upsert_nodes("p", &[node("old", "Api.Write", 1)])
        .unwrap();
    graph
        .upsert_edges(
            "p",
            &[edge("caller", "::Api.Write", EdgeKind::Calls, json!({}), 1)],
        )
        .unwrap();
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    graph
        .upsert_nodes(
            "p",
            &[node("old", "Api.Renamed", 1), node("new", "Api.Write", 1)],
        )
        .unwrap();
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    let after = graph.list_edges("p", None).unwrap();
    assert!(!after.iter().any(|e| e.target_id == "old"));
    assert_eq!(after.iter().filter(|e| e.target_id == "new").count(), 2);
    assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 0);
    assert_eq!(graph.list_edges("p", None).unwrap().len(), after.len());
    assert_eq!(
        serde_json::to_value(graph.list_edges("p", None).unwrap()).unwrap(),
        serde_json::to_value(after).unwrap()
    );
}

#[test]
fn stale_raw_call_from_reextracted_source_does_not_recreate_owned_edge() {
    let (_temp, graph) = setup();
    graph
        .upsert_nodes(
            "p",
            &[
                node("caller", "Caller.Run", 2),
                node("target", "Api.Write", 1),
            ],
        )
        .unwrap();
    graph
        .upsert_edges(
            "p",
            &[
                edge("caller", "::Api.Write", EdgeKind::Calls, json!({}), 1),
                edge(
                    "caller",
                    "target",
                    EdgeKind::Calls,
                    json!({"resolved_from":"app_code"}),
                    1,
                ),
            ],
        )
        .unwrap();
    resolve_app_code_globals(&graph, "p", 2).unwrap();
    assert!(
        !graph
            .list_edges("p", None)
            .unwrap()
            .iter()
            .any(|e| e.target_id == "target")
    );
}

#[test]
fn snapshot_replacement_preserves_unowned_collisions_and_updates_both_adjacencies() {
    let (_temp, graph) = setup();
    let owned = edge(
        "caller",
        "bad",
        EdgeKind::Calls,
        json!({"resolved_from":"app_code"}),
        1,
    );
    let strong = edge(
        "caller",
        "good",
        EdgeKind::Calls,
        json!({"resolution":"compiler_verified"}),
        1,
    );
    graph
        .upsert_edges("p", &[owned.clone(), strong.clone()])
        .unwrap();
    let mut attempted = strong.clone();
    attempted.metadata = Some(json!({"resolved_from":"app_code"}));
    graph
        .replace_edge_snapshot("p", &[owned], &[attempted])
        .unwrap();
    let edges = graph.list_edges("p", None).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].metadata, strong.metadata);
    assert!(
        graph
            .find_incoming_edges_with_kind("p", Some(EdgeKind::Calls), "bad", 20)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        graph
            .find_incoming_edges_with_kind("p", Some(EdgeKind::Calls), "good", 20)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn snapshot_conflict_aborts_all_removals() {
    let (_temp, graph) = setup();
    let first = edge(
        "caller",
        "first",
        EdgeKind::Calls,
        json!({"resolved_from":"app_code"}),
        1,
    );
    let second = edge(
        "caller",
        "second",
        EdgeKind::Calls,
        json!({"resolved_from":"app_code"}),
        1,
    );
    graph
        .upsert_edges("p", &[first.clone(), second.clone()])
        .unwrap();
    let mut replaced = second.clone();
    replaced.metadata = Some(json!({"resolution":"compiler_verified"}));
    graph.upsert_edges("p", &[replaced]).unwrap();
    assert!(
        graph
            .replace_edge_snapshot("p", &[first, second], &[])
            .is_err()
    );
    assert_eq!(graph.list_edges("p", None).unwrap().len(), 2);
    assert_eq!(
        graph
            .find_incoming_edges_with_kind("p", Some(EdgeKind::Calls), "first", 20)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn legacy_hints_with_stronger_resolution_are_not_owned() {
    let (_temp, graph) = setup();
    let strong = edge(
        "caller",
        "external",
        EdgeKind::Calls,
        json!({
            "original_target_name":"Run", "resolved_target_fqn":"External.Run",
            "resolution":"compiler_verified"
        }),
        1,
    );
    graph.upsert_edges("p", &[strong.clone()]).unwrap();
    resolve_app_code_globals(&graph, "p", 2).unwrap();
    let edges = graph.list_edges("p", None).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].metadata, strong.metadata);
}
