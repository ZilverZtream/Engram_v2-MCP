//! Scoped cleanup must not tell an agent that an index rebuild occurred.
use engram_core::Config;
use engram_server::{AppState, Engram, tool_surface};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

#[tokio::test]
async fn lexical_cleanup_reports_maintenance_and_exact_reconstruction_handoff() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let content = "export function acceptedWorkflow() { return 1; }\n";
    std::fs::write(source.join("Workflow.js"), content).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![source.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(serde_json::from_value(json!({
        "directory":source,"project_name":"Repair contract","project_type":"general","wait":true
    })).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let before = state.registry.get_meta(&pid, "active_generation").unwrap();
    let result = engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":pid,"scope":"tantivy_only"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Maintenance only")
    );
    let outcome: Value = result
        .content
        .iter()
        .filter_map(|part| part.as_text())
        .filter_map(|text| serde_json::from_str::<Value>(&text.text).ok())
        .find(|value| value["status"] == "maintenance_only")
        .unwrap();
    assert_eq!(outcome["repair_performed"], false);
    assert_eq!(outcome["index_health_verified"], false);
    assert_eq!(outcome["scope"], "tantivy_only");
    let action = &outcome["next_tools"][0];
    assert_eq!(action["tool"], "repair_project");
    assert_eq!(
        action["arguments"],
        json!({"project_id":pid,"scope":"full",
        "wipe_and_reindex":false,"max_commits":0})
    );
    // The suggested action must deserialize as an existing real request. It
    // is not executed automatically by cleanup or by this contract check.
    let _: engram_server::RepairProjectRequest =
        serde_json::from_value(action["arguments"].clone()).unwrap();
    assert_eq!(
        state.registry.get_meta(&pid, "active_generation").unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_to_string(source.join("Workflow.js")).unwrap(),
        content
    );
    drop(engram);
    drop(state);
    temp.close().unwrap();
}

#[test]
fn advertised_repair_scope_discloses_cleanup_and_actual_vector_initialization() {
    let tools = tool_surface::advertised(Engram::tool_registry().list_all(), true);
    let tool = tools
        .iter()
        .find(|tool| tool.name == "repair_project")
        .unwrap();
    let description = tool.description.as_deref().unwrap();
    assert!(description.contains("does not rebuild the lexical index"));
    assert!(description.contains("initialize_vectors"));
    let scope = tool.input_schema["properties"]["scope"]["description"]
        .as_str()
        .unwrap();
    assert!(scope.contains("maintenance_only"));
    assert!(scope.contains("does not rebuild the lexical index"));
}
