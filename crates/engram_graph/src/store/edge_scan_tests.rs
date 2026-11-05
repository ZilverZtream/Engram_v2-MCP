use super::*;
use serde_json::json;

fn edge(source: &str, target: &str, kind: EdgeKind, metadata: JsonValue) -> Edge {
    Edge {
        source_id: source.into(),
        target_id: target.into(),
        edge_kind: kind,
        namespace: "memory".into(),
        language: "vbnet".into(),
        weight: 7,
        generation: 19,
        updated_at_ms: 123,
        metadata: Some(metadata),
    }
}

#[test]
fn edge_visit_preserves_complete_values_project_scope_and_stored_order() {
    let graph = TemporaryGraphStore::new();
    graph
        .upsert_edges(
            "p",
            &[
                edge(
                    "later",
                    "target",
                    EdgeKind::TemporalCoupling,
                    json!({"body":"x","nested":[1,true]}),
                ),
                edge("z", "b", EdgeKind::Dependency, json!(42)),
                edge(
                    "a",
                    "b",
                    EdgeKind::Calls,
                    json!(["non-object", {"value":null}]),
                ),
            ],
        )
        .unwrap();
    {
        let write = graph.db.begin_write().unwrap();
        write
            .open_table(EDGES)
            .unwrap()
            .insert("p-extra\0calls\0corrupt\0foreign", &[1, 2, 3][..])
            .unwrap();
        write.commit().unwrap();
    }
    let expected = graph.list_edges("p", None).unwrap();
    let mut observed = Vec::new();
    graph.visit_edges("p", |edge| observed.push(edge)).unwrap();
    assert_eq!(
        bincode::serialize(&observed).unwrap(),
        bincode::serialize(&expected).unwrap()
    );
    let mut empty = 0;
    graph.visit_edges("absent", |_| empty += 1).unwrap();
    assert_eq!(empty, 0);
}

#[derive(Serialize)]
struct WireEdge<'a> {
    source_id: &'a str,
    target_id: &'a str,
    namespace: &'a str,
    language: &'a str,
    edge_kind: EdgeKind,
    weight: u32,
    generation: u64,
    metadata: Option<&'a str>,
    updated_at_ms: u64,
}

#[test]
fn edge_visit_refuses_corrupt_unrelated_kinds_like_the_complete_list() {
    let invalid_json = bincode::serialize(&WireEdge {
        source_id: "bad",
        target_id: "edge",
        namespace: "memory",
        language: "vbnet",
        edge_kind: EdgeKind::TemporalCoupling,
        weight: 1,
        generation: 1,
        metadata: Some("{not complete JSON"),
        updated_at_ms: 1,
    })
    .unwrap();
    for corrupt in [vec![1, 2, 3], invalid_json] {
        let graph = TemporaryGraphStore::new();
        let valid = edge("a", "b", EdgeKind::Calls, json!({"preserve":"original"}));
        graph.upsert_edges("p", &[valid]).unwrap();
        let endpoints = [("a".into(), EdgeKind::Calls, "b".into())];
        let before =
            bincode::serialize(&graph.get_edges_by_endpoints("p", &endpoints).unwrap()).unwrap();
        {
            let write = graph.db.begin_write().unwrap();
            let key = edge_key("p", &EdgeKind::TemporalCoupling, "bad", "edge");
            write
                .open_table(EDGES)
                .unwrap()
                .insert(key.as_str(), corrupt.as_slice())
                .unwrap();
            write.commit().unwrap();
        }
        assert!(graph.list_edges("p", Some(EdgeKind::Calls)).is_err());
        assert!(graph.visit_edges("p", |_| {}).is_err());
        assert_eq!(
            before,
            bincode::serialize(&graph.get_edges_by_endpoints("p", &endpoints).unwrap()).unwrap()
        );
    }
}
