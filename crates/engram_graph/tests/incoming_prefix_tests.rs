use engram_graph::{Edge, EdgeKind, GraphStore};

#[test]
fn incoming_prefix_is_kind_and_target_scoped_with_exact_truncation() {
    let temp=tempfile::tempdir().unwrap();
    let graph=GraphStore::open(&temp.path().join("graph")).unwrap();
    let edges:Vec<_>=(0..250).map(|i| Edge {
        source_id:format!("source-{i:04}"), target_id:"::Format".into(),
        namespace:"memory".into(), language:"vb".into(), edge_kind:EdgeKind::Calls,
        weight:i+1, generation:1, metadata:None, updated_at_ms:0,
    }).collect();
    graph.upsert_edges("p",&edges).unwrap();
    let (one,capped)=graph.incoming_edge_prefix("p",EdgeKind::Calls,"::Format",1).unwrap();
    assert!(capped);
    // Key order, deliberately not the highest-weight edge at the far end.
    assert_eq!(one[0].0,"source-0000");
    assert_eq!(one.len(),1);
    let (all,capped)=graph.incoming_edge_prefix("p",EdgeKind::Calls,"::Format",250).unwrap();
    assert_eq!(all.len(),250);
    assert!(!capped);
    let (none,capped)=graph.incoming_edge_prefix("p",EdgeKind::Calls,"::Format",0).unwrap();
    assert!(none.is_empty() && capped);
    for (project,kind,target) in [("other",EdgeKind::Calls,"::Format"),("p",EdgeKind::Dependency,"::Format"),("p",EdgeKind::Calls,"::Other")] {
        let (none,capped)=graph.incoming_edge_prefix(project,kind,target,1).unwrap();
        assert!(none.is_empty() && !capped);
    }
}
