#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::json;

#[tokio::test]
async fn indexed_property_state_stays_off_the_preceding_method_and_enters_matrix() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Account.cs"), "public class Account {\n public bool CheckAccess() {\n  return true;\n }\n public string Theme {\n  get {\n   return (string)Session[\"Theme\"];\n  }\n }\n public object ReadCart() {\n  return Session[\"Primary\"];\n }\n public object ReadCart(int id) {\n  return Session[\"Secondary\"];\n }\n}\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let server = Engram::new(state.clone());
    server.handle_index_project(serde_json::from_value(json!({
        "directory": root, "project_name": "state-owner", "project_type": "general", "wait": true
    })).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let symbols = state
        .graph
        .query_nodes_in_file(&pid, None, "Account.cs", 100)
        .unwrap();
    let method = symbols
        .iter()
        .find(|node| node.name.ends_with("CheckAccess"))
        .unwrap();
    let false_edges = state
        .graph
        .neighbors(
            &pid,
            engram_graph::EdgeKind::ReadsState,
            &method.node_id,
            100,
        )
        .unwrap();
    assert!(
        false_edges.is_empty(),
        "A property must not create a dependency on its preceding method: {false_edges:?}"
    );
    let property = symbols
        .iter()
        .find(|node| node.node_type == "property" && node.name.ends_with("Theme"))
        .unwrap();
    let edges = state
        .graph
        .neighbors(
            &pid,
            engram_graph::EdgeKind::ReadsState,
            &property.node_id,
            100,
        )
        .unwrap();
    assert!(
        edges
            .iter()
            .any(|(target, _)| target == "state:Session:Theme"),
        "Property evidence must be retained: {edges:?}"
    );
    let mut overloads: Vec<_> = symbols
        .iter()
        .filter(|node| node.name.ends_with("ReadCart"))
        .collect();
    overloads.sort_by_key(|node| node.start_line);
    assert_eq!(overloads.len(), 2);
    for (owner, expected) in overloads
        .iter()
        .zip(["state:Session:Primary", "state:Session:Secondary"])
    {
        let targets = state
            .graph
            .neighbors(
                &pid,
                engram_graph::EdgeKind::ReadsState,
                &owner.node_id,
                100,
            )
            .unwrap();
        assert_eq!(
            targets.len(),
            1,
            "Overloaded members must retain their own state reads: {targets:?}"
        );
        assert_eq!(targets[0].0, expected);
    }
    let matrix = server
        .handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":pid,"files":["Account.cs"]})).unwrap(),
        )
        .await
        .unwrap();
    let text = &matrix.content[0].as_text().unwrap().text;
    assert!(
        text.contains("Session:Theme") && text.contains("Theme (Account.cs:"),
        "Property state must be actionable in the matrix: {text}"
    );
    let state_section = text
        .split("## Shared-state axis")
        .nth(1)
        .unwrap()
        .split("\n## ")
        .next()
        .unwrap();
    assert!(
        !state_section.contains("CheckAccess (Account.cs:"),
        "{text}"
    );
}
