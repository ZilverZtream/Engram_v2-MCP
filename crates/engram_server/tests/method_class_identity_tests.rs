#![allow(clippy::unwrap_used)]
//! Class selectors must work on parser-produced identities, including repeated
//! method names. Use the actual indexer rather than hand-built qualified nodes.
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

async fn indexed_context(source: &str, extension: &str, same_name: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let path = format!("panels.{extension}");
    std::fs::write(root.join(&path), source).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    }).unwrap();
    let server = Engram::new(state.clone());
    server.handle_index_project(serde_json::from_value(json!({
        "directory": root, "project_name": "class-identity", "project_type": "general", "wait": true
    })).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    let request = json!({"project_id":pid,"file_path":path,"method_name":"refresh","output_json":true});
    if same_name {
        let error = server.handle_get_method_edit_context(serde_json::from_value(request.clone()).unwrap()).await.unwrap_err();
        assert!(error.message.contains("AMBIGUOUS") && error.message.contains("2 classes"), "{error}");
    }
    for (class, marker, excluded) in if same_name {
        vec![("OrdersPanel", "orders-only", "users-only"), ("UsersPanel", "users-only", "orders-only")]
    } else {
        vec![("OrdersPanel", "orders-only", "users-only")]
    } {
        let mut selected = request.clone();
        selected["class_name"] = json!(class);
        let result = server.handle_get_method_edit_context(serde_json::from_value(selected).unwrap()).await.unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["method_info"]["class_name"], class, "{text}");
        assert_eq!(value["method_info"]["fqn"], format!("{class}.refresh"), "{text}");
        assert!(text.contains(marker) && !text.contains(excluded), "{text}");
    }
}

#[tokio::test]
async fn indexed_typescript_method_accepts_its_declaring_class() {
    indexed_context("class OrdersPanel {\n refresh() { return 'orders-only'; }\n}\n", "ts", false).await;
}

#[tokio::test]
async fn indexed_javascript_same_name_methods_keep_distinct_classes() {
    indexed_context("class OrdersPanel {\n refresh() { return 'orders-only'; }\n}\nclass UsersPanel {\n refresh() { return 'users-only'; }\n}\n", "js", true).await;
}
