use engram_core::RelPath;
use engram_graph::{EdgeKind, GraphStore, Node};
const REVIEW: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn legacy(path: &str) -> Node {
    Node {
        node_id: format!("file:{path}"),
        node_type: "file".into(),
        name: path.rsplit('/').next().unwrap().into(),
        namespace: "memory".into(),
        language: "markdown".into(),
        file_path: RelPath::new(path),
        start_line: 0,
        end_line: 0,
        generation: 7,
        metadata: None,
    }
}
#[test]
fn only_reviewed_nodes_change_and_learning_edges_remain() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&tmp.path().join("graph")).unwrap();
    let chosen = legacy("notes/decision.md");
    let ambiguous = legacy("legacy/source.md");
    graph
        .upsert_nodes("p", &[chosen.clone(), ambiguous.clone()])
        .unwrap();
    graph.upsert_nodes("other", &[chosen.clone()]).unwrap();
    graph
        .batch_increment_undirected_edges(
            "p",
            "memory",
            "text",
            7,
            &[
                (
                    EdgeKind::Dependency,
                    chosen.node_id.clone(),
                    "pk:p:business_logic:0:note".into(),
                    3,
                ),
                (
                    EdgeKind::CoOccurrence,
                    "pk:p:business_logic:0:note".into(),
                    "pk:p:memory_bank:0:other".into(),
                    4,
                ),
            ],
        )
        .unwrap();
    let before = graph.count_edges_by_kind("p").unwrap();
    assert_eq!(
        graph
            .reclassify_reviewed_learning_files("p", &[chosen.clone()], REVIEW)
            .unwrap(),
        1
    );
    let after = graph.get_node("p", &chosen.node_id).unwrap().unwrap();
    assert_eq!(after.node_id, chosen.node_id);
    assert_eq!(after.node_type, "search_document");
    assert_eq!(after.file_path, chosen.file_path);
    assert_eq!(after.generation, 7);
    assert_eq!(after.metadata.unwrap()["review_receipt_sha256"], REVIEW);
    assert_eq!(graph.count_edges_by_kind("p").unwrap(), before);
    assert_eq!(
        graph
            .neighbors("p", EdgeKind::Dependency, &chosen.node_id, 10)
            .unwrap(),
        vec![("pk:p:business_logic:0:note".into(), 3)]
    );
    assert_eq!(
        graph
            .neighbors(
                "p",
                EdgeKind::CoOccurrence,
                "pk:p:business_logic:0:note",
                10
            )
            .unwrap(),
        vec![("pk:p:memory_bank:0:other".into(), 4)]
    );
    assert_eq!(
        serde_json::to_value(graph.get_node("p", &ambiguous.node_id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(ambiguous).unwrap()
    );
    assert_eq!(
        serde_json::to_value(graph.get_node("other", &chosen.node_id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(chosen).unwrap()
    );
    assert_eq!(
        graph.list_file_node_metadata("p").unwrap().len(),
        1,
        "Unreviewed legacy source stays visible"
    );
}
#[test]
fn conflict_in_last_snapshot_aborts_every_replacement() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&tmp.path().join("graph")).unwrap();
    let a = legacy("a.md");
    let b = legacy("b.md");
    graph.upsert_nodes("p", &[a.clone(), b.clone()]).unwrap();
    let mut changed = b.clone();
    changed.generation = 8;
    graph.upsert_nodes("p", &[changed.clone()]).unwrap();
    assert!(
        graph
            .reclassify_reviewed_learning_files("p", &[a.clone(), b], REVIEW)
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(graph.get_node("p", &a.node_id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(a).unwrap()
    );
    assert_eq!(
        serde_json::to_value(graph.get_node("p", &changed.node_id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(changed).unwrap()
    );
}
#[test]
fn fingerprints_ranges_namespaces_and_missing_review_are_not_eligible() {
    let tmp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&tmp.path().join("graph")).unwrap();
    let base = legacy("source.md");
    let variants = [
        {
            let mut n = base.clone();
            n.metadata = Some(serde_json::json!({"file_hash":"real"}));
            n
        },
        {
            let mut n = base.clone();
            n.start_line = 1;
            n
        },
        {
            let mut n = base.clone();
            n.namespace = "other".into();
            n
        },
        {
            let mut n = base.clone();
            n.name = "different".into();
            n
        },
        {
            let mut n = base.clone();
            n.language = "vb".into();
            n
        },
    ];
    for n in variants {
        assert!(
            graph
                .reclassify_reviewed_learning_files("p", &[n], REVIEW)
                .is_err()
        );
    }
    assert!(
        graph
            .reclassify_reviewed_learning_files("p", &[base.clone()], "")
            .is_err()
    );
    assert!(
        graph
            .reclassify_reviewed_learning_files("p", &[base.clone(), base.clone()], REVIEW)
            .is_err()
    );
    assert!(
        graph
            .reclassify_reviewed_learning_files("p", &vec![base; 129], REVIEW)
            .is_err()
    );
}
