use super::*;

fn node(id: &str, name: &str, body: JsonValue, parameters: &[&str]) -> Node {
    let program = json!({"version":1,"language":"cs","offset_unit":"utf8_bytes",
        "parameters":parameters.iter().enumerate().map(|(i,name)| json!({"name":name,"index":i,"site":{"line":1,"text":name}})).collect::<Vec<_>>(),"body":body});
    Node {
        node_id: id.into(),
        name: name.into(),
        node_type: "function".into(),
        namespace: "memory".into(),
        language: "cs".into(),
        file_path: "Flow.cs".into(),
        start_line: 1,
        end_line: 100,
        generation: 1,
        metadata: Some(json!({"fqn":name, METADATA_KEY:program.to_string()})),
    }
}
fn name(value: &str) -> JsonValue {
    json!({"kind":"name","name":value,"site":{"line":2,"text":value}})
}
fn literal(value: &str) -> JsonValue {
    json!({"kind":"literal","site":{"line":3,"text":value}})
}
fn declare(value: &str, expression: JsonValue) -> JsonValue {
    json!({"kind":"declare","name":value,"site":{"line":2,"text":value},"expression":expression})
}
fn assign(value: &str, expression: JsonValue) -> JsonValue {
    json!({"kind":"expression","expression":{"kind":"assign","operator":"=","site":{"line":3,"text":value},"children":[name(value),expression]}})
}
fn returned(expression: JsonValue) -> JsonValue {
    json!({"kind":"return","expression":expression,"site":{"line":4,"text":"return"}})
}
fn block(body: Vec<JsonValue>) -> JsonValue {
    json!({"kind":"block","body":body})
}
fn flow(body: JsonValue) -> local::LocalFlow {
    let owner = node("sym:run", "Flow.Run", body, &["input", "flag"]);
    let program = serde_json::from_str(field(&owner, METADATA_KEY).unwrap()).unwrap();
    local::LocalFlow::build(owner, program)
}
fn ancestors(flow: &local::LocalFlow, end: &str) -> std::collections::BTreeSet<String> {
    let mut seen = std::collections::BTreeSet::from([end.to_string()]);
    loop {
        let before = seen.len();
        for edge in &flow.edges {
            if seen.contains(&edge.target_id) {
                seen.insert(edge.source_id.clone());
            }
        }
        if seen.len() == before {
            return seen;
        }
    }
}

#[test]
fn overwritten_values_and_nested_scopes_do_not_leak() {
    let flow = flow(block(vec![
        declare("x", name("input")),
        block(vec![declare("x", literal("9"))]),
        assign("x", literal("7")),
        returned(name("x")),
    ]));
    let values = ancestors(&flow, &flow.returns[0]);
    assert!(!values.contains(&flow.parameters[0]));
    assert!(
        flow.nodes
            .iter()
            .any(|n| values.contains(&n.node_id) && n.name == "7")
    );
    assert!(
        !flow
            .nodes
            .iter()
            .any(|n| values.contains(&n.node_id) && n.name == "9")
    );
    let scoped = self::flow(block(vec![
        declare("x", name("input")),
        block(vec![declare("x", literal("9"))]),
        returned(name("x")),
    ]));
    assert!(ancestors(&scoped, &scoped.returns[0]).contains(&scoped.parameters[0]));
}

#[test]
fn joins_preserve_possible_definitions_but_exclude_terminated_branches() {
    let joined = flow(block(vec![
        declare("x", name("input")),
        json!({"kind":"if","expression":name("flag"),"body":[assign("x",literal("7"))]}),
        returned(name("x")),
    ]));
    let values = ancestors(&joined, &joined.returns[0]);
    assert!(values.contains(&joined.parameters[0]));
    assert!(
        joined
            .nodes
            .iter()
            .any(|n| values.contains(&n.node_id) && n.name == "7")
    );
    let terminated = flow(block(vec![
        declare("x", literal("7")),
        json!({"kind":"if","expression":name("flag"),"body":[assign("x",name("input")),returned(name("x"))]}),
        returned(name("x")),
    ]));
    assert!(ancestors(&terminated, &terminated.returns[0]).contains(&terminated.parameters[0]));
    assert!(!ancestors(&terminated, &terminated.returns[1]).contains(&terminated.parameters[0]));
}

#[test]
fn unsupported_control_and_reference_writes_invalidate_old_values() {
    let unknown = flow(block(vec![
        declare("x", name("input")),
        json!({"kind":"unknown","site":{"text":"while"}}),
        returned(name("x")),
    ]));
    assert!(!ancestors(&unknown, &unknown.returns[0]).contains(&unknown.parameters[0]));
    assert!(!unknown.issues.is_empty());
    let reference = flow(block(vec![
        declare("x", name("input")),
        json!({"kind":"expression","expression":{"kind":"call","name":"Mutate","arguments":[{"index":0,"by_ref":true,"value":name("x")}]}}),
        returned(name("x")),
    ]));
    assert!(!ancestors(&reference, &reference.returns[0]).contains(&reference.parameters[0]));
    assert!(reference.issues.iter().any(|s| s.contains("ByRef")));
}

fn call(value: &str) -> JsonValue {
    json!({"kind":"call","name":"Identity","site":{"line":3,"text":format!("Identity({value})")},"arguments":[{"index":0,"site":{"line":3,"text":value},"value":name(value)}]})
}
fn populate(graph: &GraphStore) -> (Node, Node) {
    let caller = node(
        "sym:run",
        "Flow.Run",
        block(vec![
            declare("a", call("first")),
            declare("b", call("second")),
            returned(name("a")),
        ]),
        &["first", "second"],
    );
    let helper = node(
        "sym:identity",
        "Flow.Identity",
        block(vec![returned(name("value"))]),
        &["value"],
    );
    let mut file = node(
        &engram_core::ids::NodeId::file("Flow.cs").0,
        "Flow.cs",
        JsonValue::Null,
        &[],
    );
    file.node_type = "file".into();
    file.metadata = Some(json!({"file_hash":"current"}));
    graph
        .upsert_nodes("p", &[caller.clone(), helper.clone(), file])
        .unwrap();
    graph
        .upsert_edges(
            "p",
            &[Edge {
                source_id: caller.node_id.clone(),
                target_id: helper.node_id.clone(),
                edge_kind: EdgeKind::Calls,
                namespace: "memory".into(),
                language: "cs".into(),
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 1,
            }],
        )
        .unwrap();
    (caller, helper)
}

#[test]
fn separate_invocations_preserve_returns_and_reject_stale_entry_source() {
    let temp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
    populate(&graph);
    assert!(graph.resolve_symbol_edges("p").unwrap() > 0);
    assert_eq!(graph.resolve_symbol_edges("p").unwrap(), 0);
    let view = graph
        .trace_value_flow("p", "Flow.cs", "Run", "current")
        .unwrap();
    assert_eq!(view["contexts"].as_array().unwrap().len(), 3, "{view}");
    let points = view["nodes"].as_array().unwrap();
    let root_return = points
        .iter()
        .find(|p| p["context"] == "c0" && p["role"] == "return")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let mut seen = std::collections::BTreeSet::from([root_return.to_string()]);
    loop {
        let before = seen.len();
        for edge in view["edges"].as_array().unwrap() {
            if seen.contains(edge["to"].as_str().unwrap()) {
                seen.insert(edge["from"].as_str().unwrap().into());
            }
        }
        if before == seen.len() {
            break;
        }
    }
    for parameter in points
        .iter()
        .filter(|p| p["context"] == "c0" && p["role"] == "parameter")
    {
        assert_eq!(
            seen.contains(parameter["id"].as_str().unwrap()),
            parameter["name"] == "first",
            "{view}"
        );
    }
    let stale = graph
        .trace_value_flow("p", "Flow.cs", "Run", "edited")
        .unwrap();
    assert_eq!(stale["status"], "unavailable");
    assert!(stale["edges"].as_array().unwrap().is_empty());
}

#[test]
fn ambiguous_call_is_unbound_and_incremental_replacement_retracts_old_lineage() {
    let temp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
    let (mut caller, mut helper) = populate(&graph);
    graph.resolve_symbol_edges("p").unwrap();
    helper.node_id = "sym:overload".into();
    helper.start_line = 20;
    // Even a declaration without value syntax still makes lexical lookup ambiguous.
    helper.metadata = Some(json!({"fqn":"Flow.Identity"}));
    graph.upsert_nodes("p", &[helper]).unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    let view = graph
        .trace_value_flow("p", "Flow.cs", "Run", "current")
        .unwrap();
    assert_eq!(view["contexts"].as_array().unwrap().len(), 1);
    assert!(
        !graph
            .list_edges("p", Some(EdgeKind::ValueFlow))
            .unwrap()
            .iter()
            .any(|e| e.metadata.as_ref().unwrap()["relation"] == "argument_parameter")
    );
    caller.metadata = Some(json!({"fqn":"Flow.Run"}));
    graph.upsert_nodes("p", &[caller]).unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    let remaining = graph
        .query_nodes("p", Some("value_point"), None, None, 1000)
        .unwrap();
    assert!(
        !remaining
            .iter()
            .any(|n| field(n, "owner_id") == Some("sym:run"))
    );
    assert_eq!(
        graph
            .query_nodes_by_symbol_name("p", "first", None, 10)
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn named_argument_mapping_preserves_authored_order_and_ref_requirements() {
    let target = node("helper", "Flow.Helper", block(vec![]), &["first", "second"]);
    let program = serde_json::from_str(field(&target, METADATA_KEY).unwrap()).unwrap();
    let mut target = local::LocalFlow::build(target, program);
    let mut call=local::CallSite{result:"call".into(),arguments:vec![],expression:serde_json::from_value(json!({"kind":"call","arguments":[
        {"index":0,"name":"second","parameter_index":1},{"index":1,"name":"first","parameter_index":0}]})).unwrap()};
    assert_eq!(argument_mapping(&call, &target, true), Some(vec![1, 0]));
    assert_eq!(argument_mapping(&call, &target, false), Some(vec![1, 0]));
    target.program.parameters[0].by_ref = true;
    assert_eq!(argument_mapping(&call, &target, true), None);
    call.expression.arguments[1].by_ref = true;
    assert_eq!(argument_mapping(&call, &target, true), Some(vec![1, 0]));
    call.expression.arguments[1].parameter_index = Some(1);
    assert_eq!(argument_mapping(&call, &target, true), None);
}

#[test]
fn graph_only_replacement_retires_value_nodes_but_preserves_learned_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
    let (mut caller, helper) = populate(&graph);
    graph.resolve_symbol_edges("p").unwrap();
    let points = graph
        .query_nodes("p", Some("value_point"), None, None, 1000)
        .unwrap();
    let retired = points
        .iter()
        .find(|n| field(n, "owner_id") == Some("sym:run"))
        .unwrap();
    let learned = Edge {
        source_id: retired.node_id.clone(),
        target_id: helper.node_id.clone(),
        edge_kind: EdgeKind::Insight,
        namespace: "business_logic".into(),
        language: "cs".into(),
        weight: 3,
        generation: 1,
        metadata: None,
        updated_at_ms: 123,
    };
    graph.upsert_edges("p", &[learned.clone()]).unwrap();
    let file = graph
        .get_node("p", &engram_core::ids::NodeId::file("Flow.cs").0)
        .unwrap()
        .unwrap();
    caller.metadata = Some(json!({"fqn":"Flow.Run"}));
    let result = graph
        .replace_source_graph("p", &[file.clone()], &[file, caller, helper], &[])
        .unwrap();
    assert!(result.removed_nodes >= points.len());
    assert!(
        graph
            .query_nodes("p", Some("value_point"), None, None, 1000)
            .unwrap()
            .is_empty()
    );
    assert!(
        graph
            .list_edges("p", Some(EdgeKind::ValueFlow))
            .unwrap()
            .is_empty()
    );
    let kept = graph.list_edges("p", Some(EdgeKind::Insight)).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].source_id, learned.source_id);
    assert_eq!(kept[0].weight, 3);
}

#[test]
fn pending_refresh_does_not_combine_current_declarations_with_old_value_rows() {
    let temp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
    let (mut caller, _) = populate(&graph);
    graph.resolve_symbol_edges("p").unwrap();
    let mut file = graph
        .get_node("p", &engram_core::ids::NodeId::file("Flow.cs").0)
        .unwrap()
        .unwrap();
    caller.generation = 2;
    file.generation = 2;
    graph.upsert_nodes("p", &[caller, file]).unwrap();
    let view = graph
        .trace_value_flow("p", "Flow.cs", "Run", "current")
        .unwrap();
    assert_eq!(view["status"], "unavailable", "{view}");
    assert!(view["edges"].as_array().unwrap().is_empty());
    graph.resolve_symbol_edges("p").unwrap();
    let view = graph
        .trace_value_flow("p", "Flow.cs", "Run", "current")
        .unwrap();
    assert_eq!(view["contexts"].as_array().unwrap().len(), 3, "{view}");
}

#[test]
fn recursive_calls_end_at_a_disclosed_budget() {
    let temp = tempfile::tempdir().unwrap();
    let graph = GraphStore::open(&temp.path().join("graph.redb")).unwrap();
    let (_, helper) = populate(&graph);
    let recursive = node(
        &helper.node_id,
        "Flow.Identity",
        block(vec![returned(call("value"))]),
        &["value"],
    );
    graph.upsert_nodes("p", &[recursive]).unwrap();
    graph
        .upsert_edges(
            "p",
            &[Edge {
                source_id: helper.node_id.clone(),
                target_id: helper.node_id.clone(),
                edge_kind: EdgeKind::Calls,
                namespace: "memory".into(),
                language: "cs".into(),
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 1,
            }],
        )
        .unwrap();
    graph.resolve_symbol_edges("p").unwrap();
    let view = graph
        .trace_value_flow("p", "Flow.cs", "Run", "current")
        .unwrap();
    assert_eq!(view["truncated"], true, "{view}");
    assert_eq!(view["status"], "partial");
    assert!(view["nodes"].as_array().unwrap().len() <= 128);
    assert!(view["edges"].as_array().unwrap().len() <= 256);
    assert!(view["contexts"].as_array().unwrap().len() <= 16);
}
