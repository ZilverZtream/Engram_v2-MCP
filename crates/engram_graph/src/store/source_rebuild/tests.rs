use super::*;
use serde_json::json;

fn node(id: &str, kind: &str, name: &str, path: &str, generation: u64) -> Node {
    Node {
        node_id: id.into(),
        node_type: kind.into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "csharp".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 4,
        generation,
        metadata: (kind == "file").then(|| json!({"file_hash":format!("hash:{path}")})),
    }
}

fn edge(source: &str, target: &str, kind: EdgeKind) -> Edge {
    Edge {
        source_id: source.into(),
        target_id: target.into(),
        namespace: "memory".into(),
        language: "csharp".into(),
        edge_kind: kind,
        weight: 7,
        generation: 3,
        updated_at_ms: 1234,
        metadata: Some(json!({"evidence":"retained verbatim"})),
    }
}

fn rows(graph: &GraphStore) -> Vec<u8> {
    let tx = graph.db.begin_read().unwrap();
    let mut result = Vec::new();
    for definition in [NODES, EDGES, CENTRALITY] {
        let table = tx.open_table(definition).unwrap();
        for row in table.iter().unwrap() {
            let (key, value) = row.unwrap();
            result.push((key.value().to_owned(), value.value().to_vec()));
        }
    }
    bincode::serialize(&result).unwrap()
}

#[test]
fn graph_rebuild_replaces_calls_but_preserves_learning_history_and_other_projects() {
    let graph = TemporaryGraphStore::new();
    let files = [
        node("file:caller.cs", "file", "caller.cs", "caller.cs", 2),
        node("file:callee.cs", "file", "callee.cs", "callee.cs", 7),
    ];
    let old = node(
        "sym:method:callee.cs:Run:1",
        "method",
        "Run",
        "callee.cs",
        7,
    );
    let mut successor = old.clone();
    successor.node_id = "sym:method:callee.cs:Run:2".into();
    successor.start_line = 2;
    let chunk = node("stored-chunk", "chunk", "text", "caller.cs", 2);
    let history = edge(&files[0].node_id, &old.node_id, EdgeKind::TemporalCoupling);
    let learning = edge(&files[0].node_id, &chunk.node_id, EdgeKind::Dependency);
    let obsolete = edge(&files[0].node_id, "::Obsolete", EdgeKind::Calls);
    let fresh = edge(&files[0].node_id, &successor.node_id, EdgeKind::Calls);
    graph
        .upsert_nodes_and_edges(
            "p",
            &[
                files[0].clone(),
                files[1].clone(),
                old.clone(),
                chunk.clone(),
            ],
            &[history.clone(), learning.clone(), obsolete.clone()],
        )
        .unwrap();
    graph
        .upsert_nodes_and_edges("p-other", &[old.clone()], &[obsolete.clone()])
        .unwrap();
    graph
        .set_cached_centrality("p", 7, &HashMap::new())
        .unwrap();
    let replacements = [files[0].clone(), files[1].clone(), successor.clone()];
    let result = graph
        .replace_source_graph("p", &files, &replacements, &[fresh.clone()])
        .unwrap();
    assert_eq!(
        (
            result.removed_nodes,
            result.removed_edges,
            result.remapped_edges
        ),
        (1, 1, 1)
    );
    assert_eq!(result.retained_edges_with_retired_endpoints, 0);
    assert!(graph.get_node("p", &old.node_id).unwrap().is_none());
    assert_eq!(
        serde_json::to_value(graph.get_node("p", &chunk.node_id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(&chunk).unwrap()
    );
    let edges = graph.list_edges("p", None).unwrap();
    let mut remapped = history;
    remapped.target_id = successor.node_id.clone();
    for wanted in [learning, remapped, fresh] {
        assert!(
            edges
                .iter()
                .any(|actual| serde_json::to_value(actual).unwrap()
                    == serde_json::to_value(&wanted).unwrap())
        );
        assert_eq!(
            graph
                .neighbors("p", wanted.edge_kind.clone(), &wanted.source_id, 20)
                .unwrap(),
            vec![(wanted.target_id.clone(), wanted.weight)]
        );
        let tx = graph.db.begin_read().unwrap();
        let incoming = tx.open_table(ADJ_IN).unwrap();
        let key = adj_key("p", &wanted.edge_kind, &wanted.target_id);
        let value = incoming
            .get((key.as_str(), wanted.source_id.as_str()))
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_adj_value(value.value()).unwrap(),
            (wanted.weight, wanted.updated_at_ms)
        );
    }
    assert_eq!(edges.len(), 3);
    assert!(graph.get_cached_centrality("p", 7).unwrap().is_none());
    assert!(graph.get_node("p-other", &old.node_id).unwrap().is_some());
    assert_eq!(graph.list_edges("p-other", None).unwrap().len(), 1);
}

#[test]
fn graph_rebuild_keeps_unresolved_history_without_guessing_between_overloads() {
    let graph = TemporaryGraphStore::new();
    let file = node("file:a.cs", "file", "a.cs", "a.cs", 2);
    let old = node("sym:method:a.cs:Run:1", "method", "Run", "a.cs", 2);
    let mut first = old.clone();
    first.node_id = "sym:method:a.cs:Run:2".into();
    let mut second = first.clone();
    second.node_id = "sym:method:a.cs:Run:3".into();
    let history = edge(&file.node_id, &old.node_id, EdgeKind::Insight);
    graph
        .upsert_nodes_and_edges("p", &[file.clone(), old.clone()], &[history.clone()])
        .unwrap();
    let result = graph
        .replace_source_graph("p", &[file.clone()], &[file, first, second], &[])
        .unwrap();
    assert_eq!(result.remapped_edges, 0);
    assert_eq!(result.retained_edges_with_retired_endpoints, 1);
    assert!(graph.get_node("p", &old.node_id).unwrap().is_none());
    assert_eq!(
        serde_json::to_value(graph.list_edges("p", None).unwrap()).unwrap(),
        json!([history])
    );
}

#[test]
fn graph_rebuild_refuses_source_drift_and_incomplete_extraction_without_changes() {
    let graph = TemporaryGraphStore::new();
    let original = node("file:a.cs", "file", "a.cs", "a.cs", 2);
    let mut changed = original.clone();
    changed.generation = 3;
    graph.upsert_nodes("p", &[changed.clone()]).unwrap();
    let before = rows(&graph);
    assert!(
        graph
            .replace_source_graph("p", &[original.clone()], &[original], &[])
            .is_err()
    );
    assert_eq!(rows(&graph), before);
    assert!(
        graph
            .replace_source_graph("p", &[changed.clone()], &[], &[])
            .is_err()
    );
    assert_eq!(rows(&graph), before);
    let mut altered = changed.clone();
    altered.metadata = Some(json!({"file_hash":"different"}));
    assert!(
        graph
            .replace_source_graph("p", &[changed], &[altered], &[])
            .is_err()
    );
    assert_eq!(rows(&graph), before);
}

#[test]
fn graph_rebuild_collision_preserves_all_original_evidence_and_adjacency() {
    let graph = TemporaryGraphStore::new();
    let file = node("file:a.cs", "file", "a.cs", "a.cs", 2);
    let old = node("sym:method:a.cs:Run:1", "method", "Run", "a.cs", 2);
    let mut next = old.clone();
    next.node_id = "sym:method:a.cs:Run:2".into();
    let history = edge(&file.node_id, &old.node_id, EdgeKind::TemporalCoupling);
    let collision = edge(&file.node_id, &next.node_id, EdgeKind::TemporalCoupling);
    graph
        .upsert_nodes_and_edges("p", &[file.clone(), old], &[history, collision])
        .unwrap();
    let before = rows(&graph);
    let neighbors = graph
        .neighbors("p", EdgeKind::TemporalCoupling, &file.node_id, 20)
        .unwrap();
    let result = graph.replace_source_graph("p", &[file.clone()], &[file.clone(), next], &[]);
    assert!(result.unwrap_err().to_string().contains("collides"));
    assert_eq!(rows(&graph), before);
    assert_eq!(
        graph
            .neighbors("p", EdgeKind::TemporalCoupling, &file.node_id, 20)
            .unwrap(),
        neighbors
    );
}

#[test]
fn graph_rebuild_preserves_learning_even_when_its_endpoint_node_is_missing() {
    let graph = TemporaryGraphStore::new();
    let file = node("file:a.cs", "file", "a.cs", "a.cs", 2);
    let learning = edge(
        &file.node_id,
        "search-document:missing",
        EdgeKind::Dependency,
    );
    let unknown = edge("custom:missing", "custom:other", EdgeKind::Dependency);
    let obsolete = edge(
        &file.node_id,
        "::OldExtractedReference",
        EdgeKind::Dependency,
    );
    graph
        .upsert_nodes_and_edges(
            "p",
            &[file.clone()],
            &[learning.clone(), unknown.clone(), obsolete],
        )
        .unwrap();
    let report = graph
        .replace_source_graph("p", &[file.clone()], &[file], &[])
        .unwrap();
    assert_eq!(report.removed_edges, 1);
    let edges = graph.list_edges("p", None).unwrap();
    assert_eq!(edges.len(), 2);
    for retained in [learning, unknown] {
        assert!(edges.iter().any(|actual| json!(actual) == json!(retained)));
        assert_eq!(
            graph
                .neighbors("p", EdgeKind::Dependency, &retained.source_id, 10)
                .unwrap(),
            vec![(retained.target_id, retained.weight)]
        );
    }
}
