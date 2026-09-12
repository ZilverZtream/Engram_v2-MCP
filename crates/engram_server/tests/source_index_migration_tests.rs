#![allow(clippy::unwrap_used)]
use engram_core::{Config, MemorySection};
use engram_server::{AppState, Engram};
use serde_json::json;

#[tokio::test]
async fn unchanged_legacy_source_is_reextracted_without_losing_project_or_knowledge() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Source.rs"),
        "pub fn source_probe() -> bool { true }\n",
    )
    .unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("Source.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
        .unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots: vec![root.clone()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram
        .handle_index_project(
            serde_json::from_value(json!({
                "directory":root, "project_name":"migration", "project_type":"general", "wait":true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let section: MemorySection = serde_json::from_value(json!({
        "section_id":"decision", "title":"Preserved decision", "content":"Retain this evidence.", "updated_at_ms":1
    }))
    .unwrap();
    state.registry.put_memory_section(&pid, &section).unwrap();
    let grep_request = || {
        serde_json::from_value(
            json!({"project_id":pid,"pattern":"source_probe","output_json":true}),
        )
        .unwrap()
    };
    let before = engram.handle_grep_project(grep_request()).await.unwrap();
    let before: serde_json::Value =
        serde_json::from_str(&before.content[0].as_text().unwrap().text).unwrap();
    let old_doc = before["matches"][0]["doc_id"].as_str().unwrap();
    let mut node = state
        .graph
        .get_node(&pid, "file:Source.rs")
        .unwrap()
        .unwrap();
    node.metadata
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("source_index_version");
    state.graph.upsert_nodes(&pid, &[node]).unwrap();
    let freshness = engram
        .handle_get_index_freshness(
            serde_json::from_value(json!({"project_id":pid,"check_disk":true})).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        freshness.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("source_index_format: reindex_required")
    );
    let legacy = engram
        .handle_get_chunk(
            serde_json::from_value(json!({"project_id":pid,"doc_id":old_doc})).unwrap(),
        )
        .await;
    assert!(
        legacy
            .unwrap_err()
            .message
            .contains("Legacy source chunk withheld")
    );
    let overlay = engram.handle_grep_project(grep_request()).await.unwrap();
    let overlay: serde_json::Value =
        serde_json::from_str(&overlay.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(overlay["matches"][0]["source"], "working_tree");
    let (changed, _) = engram_server::services::project_service::get_incremental_changes(
        &state,
        &pid,
        &root,
        &["rs"],
    )
    .await
    .unwrap();
    assert_eq!(changed, vec![root.join("Source.rs")]);
    engram
        .handle_update_project(
            serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(state.registry.list_projects().unwrap().len(), 1);
    assert_eq!(
        state
            .registry
            .get_memory_section(&pid, "decision")
            .unwrap()
            .unwrap()
            .content,
        "Retain this evidence."
    );
    assert!(
        engram_server::services::project_service::outdated_source_index_paths(&state, &pid)
            .await
            .unwrap()
            .is_empty()
    );
    let (changed, _) = engram_server::services::project_service::get_incremental_changes(
        &state,
        &pid,
        &root,
        &["rs"],
    )
    .await
    .unwrap();
    assert!(
        changed.is_empty(),
        "migration must not repeat on every refresh"
    );
    let after = engram.handle_grep_project(grep_request()).await.unwrap();
    let after: serde_json::Value =
        serde_json::from_str(&after.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(after["matches"][0]["source"], "index");
    let chunk = engram
        .handle_get_chunk(
            serde_json::from_value(after["matches"][0]["recovery"]["arguments"].clone()).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        chunk.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("source_probe")
    );
}
