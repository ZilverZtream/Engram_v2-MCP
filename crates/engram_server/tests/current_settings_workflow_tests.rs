use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

#[path = "literal_settings_workflow_tests.rs"]
mod indexed_workflows;

const PID: &str = "current-settings-without-index";
const SOURCE: &str = "Public Class ReportReader\n Public Sub New()\n  Dim locale = SystemSettings.GetText(\"report_locale\")\n  Dim count = SystemSettings.GetInt(\"report_count\")\n  Dim derived = SystemSettings.GetText(\"computed_\" & tenant)\n  ' SystemSettings.GetText(\"comment_only\")\n End Sub\nEnd Class\n";
fn fixture() -> (tempfile::TempDir, AppState, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Reader.vb"), SOURCE).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    let e = Engram::new(state.clone());
    (temp, state, e)
}
fn text(value: rmcp::model::CallToolResult) -> String {
    value.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn current_files_supply_all_three_workflows_without_index_publication() {
    let (temp, state, e) = fixture();
    let before = state.registry.get_meta(PID, "active_generation").unwrap();
    let catalog = text(
        e.handle_list_settings(
            serde_json::from_value(json!({"project_id":PID,"source_files":["Reader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    let catalog: Value = serde_json::from_str(&catalog).unwrap();
    assert_eq!(catalog["observations"].as_array().unwrap().len(), 2);
    assert_eq!(
        catalog["source_files"][0]["raw_blake3"],
        blake3::hash(SOURCE.as_bytes()).to_hex().to_string()
    );
    assert!(
        !catalog.to_string().contains("computed_") && !catalog.to_string().contains("comment_only")
    );
    let lookup = text(
        e.handle_get_setting(
            serde_json::from_value(
                json!({"project_id":PID,"name":"report_locale","source_files":["Reader.vb"]}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let lookup: Value = serde_json::from_str(&lookup).unwrap();
    assert_eq!(lookup["observations"][0]["getter"], "GetText");
    assert_eq!(lookup["observations"][0]["line"], 3);
    let matrix = text(
        e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":PID,"files":["Reader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        matrix.contains("report_locale")
            && matrix.contains("report_count")
            && matrix.contains("GetInt"),
        "{matrix}"
    );
    assert!(
        matrix.contains("current code observation only")
            && matrix.contains("indexed role/state/business axes remain incomplete"),
        "{matrix}"
    );
    assert!(
        matrix.contains(&blake3::hash(SOURCE.as_bytes()).to_hex().to_string()),
        "{matrix}"
    );
    assert_eq!(
        before,
        state.registry.get_meta(PID, "active_generation").unwrap()
    );
    assert!(
        state
            .graph
            .query_nodes(PID, None, None, None, 10)
            .unwrap()
            .is_empty()
    );
    let changed = SOURCE.replace("report_locale", "current_locale");
    std::fs::write(temp.path().join("project/Reader.vb"), &changed).unwrap();
    let fresh = text(
        e.handle_list_settings(
            serde_json::from_value(json!({"project_id":PID,"source_files":["Reader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(fresh.contains("current_locale") && !fresh.contains("report_locale"));
    assert!(fresh.contains(&blake3::hash(changed.as_bytes()).to_hex().to_string()));
}

#[tokio::test]
async fn invalid_current_scopes_refuse_before_project_or_source_access() {
    let (_temp, _state, e) = fixture();
    for files in [
        json!([]),
        json!(["../Reader.vb"]),
        json!(["C:/Reader.vb"]),
        json!(["Reader.js"]),
        json!(["./Reader.vb"]),
        json!(vec!["Reader.vb"; 17]),
    ] {
        let result = e
            .handle_list_settings(
                serde_json::from_value(
                    json!({"project_id":"missing-project","source_files":files}),
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            result.message.contains("source_files"),
            "{}",
            result.message
        );
    }
    assert!(
        e.handle_list_settings(
            serde_json::from_value(
                json!({"project_id":PID,"scope":"Reader","source_files":["Reader.vb"]})
            )
            .unwrap()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn historical_cutoff_does_not_borrow_live_literal_settings() {
    let (_temp, _state, e) = fixture();
    let matrix = text(
        e.handle_derive_test_matrix(
            serde_json::from_value(
                json!({"project_id":PID,"files":["Reader.vb"],"knowledge_before":"2026-01-01"}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        !matrix.contains("report_locale") && !matrix.contains("report_count"),
        "{matrix}"
    );
    assert!(
        matrix.contains("current literal-setting fallback withheld under knowledge_before"),
        "{matrix}"
    );
}
