#![allow(clippy::unwrap_used)]
//! App_Code fallback must preserve qualification and declaration ambiguity.
use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_server::services::graph_service::resolve_app_code_globals;
use serde_json::{Value, json};

fn node(id: &str, name: &str, file: &str, arity: Option<u64>) -> Node {
    Node {
        node_id: id.into(),
        node_type: "function".into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(file),
        start_line: 1,
        end_line: 5,
        generation: 1,
        metadata: Some(json!({"fqn": name, "arity": arity})),
    }
}
fn call(target: &str, language: &str, metadata: Value) -> Edge {
    Edge {
        source_id: "caller".into(),
        target_id: target.into(),
        namespace: "memory".into(),
        language: language.into(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: Some(metadata),
        updated_at_ms: 1,
    }
}
fn bound(graph: &GraphStore) -> Vec<(String, String)> {
    let mut result: Vec<_> = graph
        .list_edges("p", None)
        .unwrap()
        .into_iter()
        .filter(|e| !e.target_id.starts_with("::"))
        .map(|e| (e.edge_kind.as_str().into(), e.target_id))
        .collect();
    result.sort();
    result
}
fn setup(nodes: &[Node], edge: Edge) -> (tempfile::TempDir, GraphStore) {
    let tmp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&tmp.path().join("graph.redb")).unwrap();
    graph.upsert_nodes("p", nodes).unwrap();
    graph.upsert_edges("p", &[edge]).unwrap();
    (tmp, graph)
}
fn declarations() -> Vec<Node> {
    vec![
        node(
            "early",
            "Alpha.Logger.Write",
            "Site/App_Code/Early.vb",
            Some(1),
        ),
        node("valid", "Omega.Api.Write", "Site/App_Code/Api.vb", Some(1)),
    ]
}

#[test]
fn unrelated_large_edge_metadata_and_known_targets_survive_binding_unchanged() {
    let mut nodes = declarations();
    nodes.push(node("known", "Other.Work", "Site/Handlers/Other.vb", None));
    let (_tmp, graph) = setup(&nodes, call("::Omega.Api.Write", "vbnet", json!({"args":1})));
    let mut known = call("known", "vbnet", json!({"body":"x".repeat(2*1024*1024)}));
    known.source_id = "unrelated".into();
    let mut fenced = call("::Omega.Api.Write", "vbnet", json!({"unresolved":true,"body":"y".repeat(2*1024*1024)}));
    fenced.source_id = "fenced".into();
    let mut history = call("history-target", "vbnet", json!({"body":"z".repeat(2*1024*1024)}));
    history.source_id = "history-source".into();history.edge_kind = EdgeKind::TemporalCoupling;
    let endpoints = [known, fenced, history].into_iter().map(|edge| {
        let endpoint = (edge.source_id.clone(), edge.edge_kind.clone(), edge.target_id.clone());
        graph.upsert_edges("p", &[edge]).unwrap();endpoint
    }).collect::<Vec<_>>();
    let before = bincode::serialize(&graph.get_edges_by_endpoints("p", &endpoints).unwrap()).unwrap();
    assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 1);
    assert_eq!(before, bincode::serialize(&graph.get_edges_by_endpoints("p", &endpoints).unwrap()).unwrap());
    assert!(graph.get_edges_by_endpoints("p", &[("caller".into(), EdgeKind::Calls, "valid".into())]).unwrap().len() == 1);
}

#[test]
fn dependency_precedes_calls_when_both_supply_the_same_resolved_dependency() {
    let mut dependency = call("::Omega.Api.Write", "vbnet", json!({"args":1,"witness":"dependency"}));
    dependency.edge_kind = EdgeKind::Dependency;dependency.weight = 3;
    let (_tmp, graph) = setup(&declarations(), dependency);
    let mut calls = call("::Omega.Api.Write", "vbnet", json!({"args":1,"witness":"calls"}));
    calls.weight = 9;graph.upsert_edges("p", &[calls]).unwrap();
    assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 1);
    let resolved = graph.get_edges_by_endpoints("p", &[("caller".into(), EdgeKind::Dependency, "valid".into()),
        ("caller".into(), EdgeKind::Calls, "valid".into())]).unwrap();
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].weight, 3);
    assert_eq!(resolved[0].metadata.as_ref().unwrap()["witness"], "dependency");
    assert_eq!(resolved[1].weight, 9);
    assert_eq!(resolved[1].metadata.as_ref().unwrap()["witness"], "calls");
}

#[test]
fn unrelated_corrupt_edge_refuses_binding_before_owned_retraction_or_substrate_changes() {
    let (temporary, graph) = setup(&declarations(), call("::Omega.Api.Write", "vbnet", json!({"args":1})));
    let old = call("old-target", "vbnet", json!({"resolved_from":"app_code","resolution":"app_code_unique"}));
    graph.upsert_edges("p", &[old]).unwrap();
    let endpoints = [("caller".into(), EdgeKind::Calls, "old-target".into()),
        ("caller".into(), EdgeKind::Calls, "::Omega.Api.Write".into())];
    let before = bincode::serialize(&graph.get_edges_by_endpoints("p", &endpoints).unwrap()).unwrap();
    drop(graph);
    let raw = engram_core::storage::open_database(&temporary.path().join("graph.redb"),
        engram_core::storage::DatabaseCache::Graph).unwrap();
    {
        let write = raw.begin_write().unwrap();
        let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("edges_v2");
        let key = format!("p\0{}\0broken\0edge", EdgeKind::TemporalCoupling.as_str());
        write.open_table(table).unwrap().insert(key.as_str(), &[1,2,3][..]).unwrap();
        write.commit().unwrap();
    }
    drop(raw);
    let graph = GraphStore::open(&temporary.path().join("graph.redb")).unwrap();
    assert!(resolve_app_code_globals(&graph, "p", 2).is_err());
    assert_eq!(before, bincode::serialize(&graph.get_edges_by_endpoints("p", &endpoints).unwrap()).unwrap());
}

#[test]
fn large_unused_metadata_keeps_global_source_owner_and_string_arity_binding() {
    for fallback_to_id in [false, true] {
        let mut source = node("caller", "UnqualifiedSourceName", "Site/Handlers/job.vb", None);
        // Sources outside App_Code and outside declaration types still supply
        // lexical ownership and generation evidence for a retained reference.
        source.node_type = "handler".into();
        if fallback_to_id {
            source.node_id = "sym:function:Site/Handlers/job.vb:A.Api.Read:12".into();
        }
        source.metadata = Some(json!({"fqn":if fallback_to_id {json!(7)} else {json!("A.Api.Read")},
            "unused_body":"x".repeat(2*1024*1024)}));
        let mut alpha = node("alpha", "A.Api.Write", "Site/App_Code/Alpha.vb", Some(1));
        alpha.metadata = Some(json!({"fqn":"A.Api.Write","arity":"1"}));
        let beta = node("beta", "B.Api.Write", "Site/App_Code/Beta.vb", Some(1));
        let mut unrelated = node("unrelated", "Noise", "Site/Other.vb", None);
        unrelated.node_type = "insight".into();
        unrelated.metadata = Some(json!({"nested":[{"body":"y".repeat(2*1024*1024)}]}));
        let mut reference = call("::Write", "vbnet", json!({"args":"1"}));
        reference.source_id = source.node_id.clone();
        let source_id = source.node_id.clone();
        let metadata = source.metadata.clone();
        let (_tmp, graph) = setup(&[source, alpha, beta, unrelated], reference);
        assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 1);
        assert_eq!(bound(&graph), vec![("calls".into(), "alpha".into()), ("dependency".into(), "alpha".into())]);
        assert_eq!(graph.get_node("p", &source_id).unwrap().unwrap().metadata, metadata,
            "A resolver projection must not rewrite or truncate stored source metadata");
    }
}
#[test]
fn unknown_receiver_and_duplicate_terminals_stay_unresolved() {
    for (name, metadata) in [
        ("::MissingApi.Write", json!({})),
        ("::Write", json!({})),
        ("::Write", json!({"receiver": "MissingApi"})),
    ] {
        let (_tmp, graph) = setup(&declarations(), call(name, "vbnet", metadata));
        assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 0);
        assert!(bound(&graph).is_empty(), "{name}");
    }
}
#[test]
fn qualified_unique_and_vb_case_controls_resolve_without_terminal_guessing() {
    for (name, language) in [
        ("::Omega.Api.Write", "csharp"),
        ("::Api.Write", "csharp"),
        ("::omega.api.write", "vbnet"),
    ] {
        let (_tmp, graph) = setup(&declarations(), call(name, language, json!({"args": "1"})));
        assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 1);
        assert_eq!(
            bound(&graph),
            vec![
                ("calls".into(), "valid".into()),
                ("dependency".into(), "valid".into())
            ]
        );
    }
    let (_tmp, graph) = setup(
        &declarations(),
        call("::omega.api.write", "csharp", json!({})),
    );
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    assert!(bound(&graph).is_empty());
}
#[test]
fn overloads_require_unique_arity_and_unknown_arity_stays_ambiguous() {
    for (second_arity, args, expected) in [
        (Some(2), json!("1"), true),
        (Some(1), json!(1), false),
        (None, json!(1), false),
        (Some(2), Value::Null, false),
        (Some(2), json!(3), false),
    ] {
        let nodes = vec![
            node("one", "Api.Save", "App_Code/Api.vb", Some(1)),
            node("two", "Api.Save", "App_Code/Api.vb", second_arity),
        ];
        let (_tmp, graph) = setup(&nodes, call("::Api.Save", "vbnet", json!({"args": args})));
        resolve_app_code_globals(&graph, "p", 1).unwrap();
        assert_eq!(
            !bound(&graph).is_empty(),
            expected,
            "{second_arity:?} {args}"
        );
        if expected {
            assert!(bound(&graph).iter().all(|(_, id)| id == "one"));
        }
    }
}
#[test]
fn explicit_failures_and_existing_targets_are_preserved() {
    for metadata in [
        json!({"unresolved": "true"}),
        json!({"unresolved": true}),
        json!({"resolution": "compiler_ambiguous"}),
        json!({"dispatch_key": "Save"}),
    ] {
        let (_tmp, graph) = setup(
            &declarations(),
            call("::Omega.Api.Write", "vbnet", metadata),
        );
        resolve_app_code_globals(&graph, "p", 1).unwrap();
        assert!(bound(&graph).is_empty());
    }
    let mut nodes = declarations();
    nodes.push(node(
        "sym:function:Pages/Caller.vb:Write:5",
        "Write",
        "Pages/Caller.vb",
        Some(1),
    ));
    let edge = call(&nodes[2].node_id, "vbnet", json!({}));
    let (_tmp, graph) = setup(&nodes, edge.clone());
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    assert_eq!(bound(&graph), vec![("calls".into(), edge.target_id)]);
}
#[test]
fn repeated_runs_refresh_generation_without_duplicates_or_stale_lookup_cache() {
    let (_tmp, graph) = setup(
        &declarations(),
        call("::Omega.Api.Write", "vbnet", json!({})),
    );
    assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 1);
    let first = bound(&graph);
    assert_eq!(resolve_app_code_globals(&graph, "p", 1).unwrap(), 0);
    assert_eq!(resolve_app_code_globals(&graph, "p", 2).unwrap(), 0);
    assert_eq!(bound(&graph), first);
    assert!(
        graph
            .list_edges("p", None)
            .unwrap()
            .iter()
            .filter(|e| !e.target_id.starts_with("::"))
            .all(|e| e.generation == 2)
    );
    graph
        .upsert_nodes("p", &[node("new", "NewApi.Run", "App_Code/New.vb", None)])
        .unwrap();
    graph
        .upsert_edges("p", &[call("::NewApi.Run", "vbnet", json!({}))])
        .unwrap();
    assert_eq!(resolve_app_code_globals(&graph, "p", 3).unwrap(), 1);
    assert!(bound(&graph).iter().any(|(_, id)| id == "new"));
}
#[test]
fn dependency_class_and_unique_global_helper_remain_supported() {
    let mut class = node("class", "Company.Settings", "App_Code/Settings.cs", None);
    class.node_type = "class".into();
    let mut edge = call("::Settings", "csharp", json!({}));
    edge.edge_kind = EdgeKind::Dependency;
    let (_tmp, graph) = setup(&[class], edge);
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    assert_eq!(bound(&graph), vec![("dependency".into(), "class".into())]);
    let (_tmp, graph) = setup(
        &[node(
            "helper",
            "Helpers.Redirect",
            "App_Code/Helpers.vb",
            None,
        )],
        call("::Redirect", "vbnet", json!({})),
    );
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    assert_eq!(bound(&graph).len(), 2);
}
#[test]
fn file_proximity_does_not_disambiguate_different_owners() {
    let nodes = vec![
        node("a", "A.Create", "App_Code/Shared.vb", None),
        node("b", "B.Create", "App_Code/Other.vb", None),
        node("caller", "Caller.Run", "App_Code/Shared.vb", None),
    ];
    let (_tmp, graph) = setup(&nodes, call("::Create", "vbnet", json!({})));
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    assert!(bound(&graph).is_empty());
}

#[test]
fn incompatible_local_arity_does_not_fall_back_to_another_owner() {
    let nodes = vec![
        node("a", "A.Save", "App_Code/A.vb", Some(2)),
        node("b", "B.Save", "App_Code/B.vb", Some(1)),
        node("caller", "A.Run", "App_Code/A.vb", None),
    ];
    let (_tmp, graph) = setup(&nodes, call("::Save", "vbnet", json!({"args": "1"})));
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    assert!(bound(&graph).is_empty());
}

#[test]
fn qualified_receiver_is_not_discarded_even_with_one_global_candidate() {
    let nodes = vec![node("only", "Logger.Write", "App_Code/Logger.vb", Some(1))];
    for metadata in [json!({}), json!({"fqn": "Write"})] {
        let (_tmp, graph) = setup(&nodes, call("::Missing.Write", "vbnet", metadata));
        resolve_app_code_globals(&graph, "p", 1).unwrap();
        assert!(bound(&graph).is_empty());
    }
}

#[test]
fn declarations_beyond_the_old_display_cap_cannot_create_false_uniqueness() {
    let mut nodes: Vec<_> = (0..10_001)
        .map(|i| {
            let mut n = node(
                &format!("class:{i:05}"),
                &format!("Type{i}"),
                "App_Code/Types.cs",
                None,
            );
            n.node_type = "class".into();
            n
        })
        .collect();
    nodes[0].name = "Shared".into();
    nodes[0].metadata = Some(json!({"fqn": "First.Shared"}));
    nodes[10_000].name = "Shared".into();
    nodes[10_000].metadata = Some(json!({"fqn": "Last.Shared"}));
    let mut edge = call("::Shared", "csharp", json!({}));
    edge.edge_kind = EdgeKind::Dependency;
    let (_tmp, graph) = setup(&nodes, edge);
    resolve_app_code_globals(&graph, "p", 1).unwrap();
    assert!(bound(&graph).is_empty());
}

#[test]
fn existing_resolution_evidence_is_not_overwritten_on_generation_refresh() {
    let (_tmp, graph) = setup(
        &declarations(),
        call("::Omega.Api.Write", "vbnet", json!({})),
    );
    let resolved = call(
        "valid",
        "vbnet",
        json!({"resolution": "compiler_verified", "confidence": 1.0}),
    );
    graph.upsert_edges("p", &[resolved.clone()]).unwrap();
    resolve_app_code_globals(&graph, "p", 2).unwrap();
    let calls = graph.list_edges("p", Some(EdgeKind::Calls)).unwrap();
    let retained = calls.iter().find(|e| e.target_id == "valid").unwrap();
    assert_eq!(retained.metadata, resolved.metadata);
    assert_eq!(retained.generation, resolved.generation);
}
