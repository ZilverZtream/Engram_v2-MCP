//! Ordinary typed settings reads must reach catalog, source lookup and QA axes.
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::json;

#[path = "matrix_setting_identity_tests.rs"]
mod existing_identity;

async fn fixture(root: &std::path::Path) -> (AppState, Engram, String) {
    std::fs::create_dir(root.join("project")).unwrap();
    std::fs::write(root.join("project/ReceiptReader.vb"),
        "Public Class ReceiptReader\n Public Sub New()\n  Dim currency = SystemSettings.GetText(\"invoice_currency\") : Dim limit = SystemSettings.GetInt(\"constructor_limit\")\n End Sub\n Public Function Currency() As String\n  Dim enabled = SystemSettings.GetBool(\"invoice_enabled\")\n  Return SystemSettings.GetText(\"invoice_currency\")\n End Function\nEnd Class\n").unwrap();
    std::fs::write(root.join("project/SyncReader.vb"),
        "Public Class SyncReader\n Public Function Currency() As String\n  ' SystemSettings.GetText(\"comment_only\")\n  Dim text = \"SystemSettings.GetText(\"\"quoted_only\"\")\"\n  Dim other = Helpers.GetText(\"not_a_setting\") : Dim dynamicValue = SystemSettings.GetText(\"dynamic_\" & tenant)\n  Return SystemSettings.GetText(\"invoice_currency\")\n End Function\nEnd Class\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: root.join("data"),
        allowed_roots: vec![root.join("project")],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let e = Engram::new(state.clone());
    e.handle_index_project(serde_json::from_value(json!({"directory":root.join("project"),"project_name":"Literal setting fixture","project_type":"general","wait":true})).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (state, e, pid)
}
fn text(result: rmcp::model::CallToolResult) -> String {
    assert!(!result.is_error.unwrap_or(false));
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn three_workflows_expose_literal_keys_and_getters_without_inventing_declarations() {
    let temp = tempfile::tempdir().unwrap();
    let (_state, e, pid) = fixture(temp.path()).await;
    let catalog = text(
        e.handle_list_settings(serde_json::from_value(json!({"project_id":pid})).unwrap())
            .await
            .unwrap(),
    );
    assert_eq!(
        catalog
            .matches("**systemsettings[\"invoice_currency\"]**")
            .count(),
        1,
        "{catalog}"
    );
    assert!(
        catalog.contains("systemsettings[\"invoice_enabled\"]")
            && catalog.contains("literal-key settings getter observations"),
        "{catalog}"
    );
    for omitted in ["comment_only", "quoted_only", "not_a_setting", "dynamic_"] {
        assert!(!catalog.contains(omitted), "{catalog}");
    }
    let lookup = text(
        e.handle_get_setting(
            serde_json::from_value(json!({"project_id":pid,"name":"invoice_currency"})).unwrap(),
        )
        .await
        .unwrap(),
    );
    for site in [
        "ReceiptReader.vb:3",
        "ReceiptReader.vb:7",
        "SyncReader.vb:6",
    ] {
        assert!(lookup.contains(site), "{lookup}");
    }
    assert!(
        lookup.contains("Source status: current at observation")
            && lookup.contains("GetText")
            && lookup.contains("not web.config declarations"),
        "{lookup}"
    );
    let matrix = text(
        e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":pid,"files":["ReceiptReader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        matrix.contains("invoice_currency")
            && matrix.contains("GetText")
            && matrix.contains("invoice_enabled")
            && matrix.contains("GetBool"),
        "{matrix}"
    );
    assert!(
        matrix.contains("constructor_limit") && matrix.contains("GetInt"),
        "Constructor-only setting read must remain visible: {matrix}"
    );
    assert!(
        !matrix.contains("SyncReader.vb"),
        "Unrequested file must not be presented as selected-file QA evidence: {matrix}"
    );
    assert!(
        matrix.contains("store binding/value unverified"),
        "{matrix}"
    );
}

#[tokio::test]
async fn edited_source_withholds_current_call_excerpt_and_matrix_axes_until_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let (_state, e, pid) = fixture(temp.path()).await;
    std::fs::write(
        temp.path().join("project/ReceiptReader.vb"),
        "Public Class ReceiptReader\nEnd Class\n",
    )
    .unwrap();
    let lookup = text(
        e.handle_get_setting(
            serde_json::from_value(json!({"project_id":pid,"name":"invoice_enabled"})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        lookup.contains("STALE_OR_UNAVAILABLE") && !lookup.contains("call-site excerpt:"),
        "{lookup}"
    );
    let matrix = text(
        e.handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":pid,"files":["ReceiptReader.vb"]})).unwrap(),
        )
        .await
        .unwrap(),
    );
    assert!(
        !matrix.contains("getter GetBool"),
        "Stale axes must remain withheld: {matrix}"
    );
}
