#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

async fn fixture() -> (tempfile::TempDir, AppState, Engram, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("Source.rs"), "pub fn old_probe() -> u8 { 1 }\n").unwrap();
    std::fs::write(
        root.join("Removed.rs"),
        "pub fn removed_probe() -> u8 { 2 }\n",
    )
    .unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots: vec![root.clone()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram.handle_index_project(serde_json::from_value(json!({
        "directory": root, "project_name": "non-git-refresh", "project_type": "general", "wait": true
    })).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    assert!(!root.join(".git").exists());
    (temp, state, engram, pid)
}

fn generation(state: &AppState, pid: &str) -> u64 {
    state
        .registry
        .get_meta(pid, "active_generation")
        .unwrap()
        .unwrap()
        .parse()
        .unwrap()
}

async fn indexed_matches(engram: &Engram, pid: &str, pattern: &str) -> Value {
    let result = engram
        .handle_grep_project(
            serde_json::from_value(json!({
                "project_id": pid, "pattern": pattern, "output_json": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
}

#[tokio::test]
async fn non_git_refresh_publishes_changed_and_deleted_source_with_explicit_history_degradation() {
    let (temp, state, engram, pid) = fixture().await;
    let before = generation(&state, &pid);
    let root = temp.path().join("source");
    std::fs::write(
        root.join("Source.rs"),
        "pub fn replacement_probe() -> u8 { 73 }\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("Removed.rs")).unwrap();
    let result = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id": pid, "wait": true, "max_commits": 1
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("status: degraded"), "{text}");
    assert!(
        text.contains("git_update_stream failed (enrichment degraded)"),
        "{text}"
    );
    assert_eq!(generation(&state, &pid), before + 1);
    let found = indexed_matches(&engram, &pid, "replacement_probe").await;
    let hits = found["matches"].as_array().unwrap();
    assert!(!hits.is_empty(), "{found}");
    for hit in hits {
        assert_eq!(hit["source"], "index", "{hit}");
        let chunk = engram
            .handle_get_chunk(serde_json::from_value(hit["recovery"]["arguments"].clone()).unwrap())
            .await
            .unwrap();
        let body = &chunk.content[0].as_text().unwrap().text;
        assert!(
            body.contains("replacement_probe") && body.contains("73"),
            "{body}"
        );
        assert!(!body.contains("old_probe"), "{body}");
    }
    let search = engram.handle_search_memory(serde_json::from_value(json!({
        "project_id": pid, "query": "replacement_probe", "namespace": "memory", "semantic": false,
        "include_content": true, "include_user_memory": false
    })).unwrap()).await.unwrap();
    assert!(
        search.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("replacement_probe")
    );
    assert!(
        indexed_matches(&engram, &pid, "removed_probe").await["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let docs = state
        .get_project_cached(&pid)
        .unwrap()
        .search
        .list_docs_in_namespace(&pid, "memory")
        .unwrap();
    assert!(docs.iter().all(|doc| doc.path != "Removed.rs"));
    assert!(!root.join(".git").exists());
}

#[tokio::test]
async fn cancelled_non_git_refresh_cannot_publish_a_new_generation() {
    let (temp, state, engram, pid) = fixture().await;
    let before = generation(&state, &pid);
    std::fs::write(
        temp.path().join("source/Source.rs"),
        "pub fn cancelled_probe() -> u8 { 99 }\n",
    )
    .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = engram
        .update_project_impl(&pid, before + 1, 1, false, &cancel)
        .await;
    assert!(result.is_err(), "cancelled update unexpectedly succeeded");
    assert_eq!(generation(&state, &pid), before);
}
