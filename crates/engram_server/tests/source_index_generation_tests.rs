#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::json;

async fn fixture() -> (tempfile::TempDir, AppState, Engram, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Source.rs"), "pub fn source_probe() -> bool { true }\n").unwrap();
    std::fs::write(root.join("Other.rs"), "pub fn other_probe() -> bool { false }\n").unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let mut index = repo.index().unwrap();
    index.add_all(["*.rs"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[]).unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots:vec![root.clone()], data_dir:temp.path().join("data"),
        embedding_backend:"fts_only".into(), ..Default::default()
    }).unwrap();
    let engram = Engram::new(state.clone());
    engram.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"generation","project_type":"general","wait":true
    })).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    (temp,state,engram,pid)
}

async fn grep(engram: &Engram, pid: &str, freshness: &str) -> serde_json::Value {
    let result = engram.handle_grep_project(serde_json::from_value(json!({
        "project_id":pid,"pattern":"source_probe","output_json":true,"freshness":freshness
    })).unwrap()).await.unwrap();
    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
}

#[tokio::test]
async fn unpublished_file_metadata_cannot_verify_or_skip_rebuilding_the_active_snapshot() {
    let (temp,state,engram,pid) = fixture().await;
    let original = grep(&engram,&pid,"strict").await;
    let doc = original["matches"][0]["doc_id"].as_str().unwrap();
    let mut node = state.graph.get_node(&pid,"file:Source.rs").unwrap().unwrap();
    node.generation = 2; // simulate a failed update after graph write, before publication
    state.graph.upsert_nodes(&pid,&[node]).unwrap();
    let unavailable = engram.handle_get_chunk(serde_json::from_value(json!({"project_id":pid,"doc_id":doc})).unwrap()).await.unwrap_err();
    assert!(unavailable.message.contains("newer than the active search generation"));
    for freshness in ["strict","warn","off"] {
        let result = grep(&engram,&pid,freshness).await;
        assert_eq!(result["matches"][0]["source"],"working_tree", "{result}");
        assert_eq!(result["matches"][0]["line"],1);
    }
    let paths = engram_server::services::project_service::outdated_source_index_paths(&state,&pid).await.unwrap();
    assert!(paths.contains(&"Source.rs".to_string()));
    let (changed,_) = engram_server::services::project_service::get_incremental_changes(&state,&pid,&temp.path().join("repo"), &["rs"]).await.unwrap();
    assert_eq!(changed, vec![temp.path().join("repo/Source.rs")]);
    let freshness = engram.handle_get_index_freshness(serde_json::from_value(json!({"project_id":pid,"check_disk":false})).unwrap()).await.unwrap();
    assert!(freshness.content[0].as_text().unwrap().text.contains("source_index_format: reindex_required"));
    engram.update_project_impl(&pid,2,1,false,&tokio_util::sync::CancellationToken::new()).await.unwrap();
    assert_eq!(grep(&engram,&pid,"strict").await["matches"][0]["source"],"index");
}

#[tokio::test]
async fn stale_requested_generation_cannot_copy_an_obsolete_snapshot_over_a_new_one() {
    let (_temp,state,engram,pid) = fixture().await;
    for _ in 0..2 {
        engram.update_project_impl(&pid,2,1,false,&tokio_util::sync::CancellationToken::new()).await.unwrap();
    }
    assert_eq!(state.registry.get_meta(&pid,"active_generation").unwrap().as_deref(),Some("3"));
    let result = grep(&engram,&pid,"strict").await;
    assert_eq!(result["matches"][0]["source"],"index");
    engram.handle_get_chunk(serde_json::from_value(result["matches"][0]["recovery"]["arguments"].clone()).unwrap()).await.unwrap();
}

#[tokio::test]
async fn queued_background_updates_copy_unchanged_files_and_allocate_generation_under_lock() {
    let (_temp,state,engram,pid) = fixture().await;
    let guard = state.acquire_project_update_lock(&pid).await;
    for _ in 0..2 {
        engram.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":false,"max_commits":1})).unwrap()).await.unwrap();
    }
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let jobs: Vec<_> = state.registry.list_jobs(Some(&pid)).unwrap().into_iter().filter(|job| job.kind=="update_project").collect();
            if jobs.len()==2 && jobs.iter().all(|job| ["done","degraded","failed","cancelled"].contains(&job.status.as_str())) {
                assert!(jobs.iter().all(|job| ["done","degraded"].contains(&job.status.as_str())), "{jobs:?}");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    assert_eq!(state.registry.get_meta(&pid,"active_generation").unwrap().as_deref(),Some("3"));
    let engine = state.get_project_cached(&pid).unwrap().search;
    let docs = engine.list_docs_in_namespace(&pid,"memory").unwrap();
    assert!(docs.iter().any(|doc| doc.path=="Source.rs"));
    assert!(docs.iter().any(|doc| doc.path=="Other.rs"));
    let result = grep(&engram,&pid,"strict").await;
    assert_eq!(result["matches"][0]["source"],"index");
    engram.handle_get_chunk(serde_json::from_value(result["matches"][0]["recovery"]["arguments"].clone()).unwrap()).await.unwrap();
}

#[tokio::test]
async fn current_file_marker_cannot_verify_a_legacy_chunk_range() {
    let (_temp,state,engram,pid) = fixture().await;
    let engine = state.get_project_cached(&pid).unwrap().search;
    engine.index_docs(&pid,&[engram_index::IndexDoc {
        generation:1,chunk_id:999,path:engram_core::RelPath::new("Source.rs"),language:"rust".into(),
        content:"pub fn source_probe() -> bool { true }\n".into(),namespace:"memory".into(),
        author:None,timestamp:None,start_line:5,end_line:5,doc_id:"legacy-range".into(),content_hash:"legacy-range-hash".into(),
    }],&tokio_util::sync::CancellationToken::new()).await.unwrap();
    let error = engram.handle_get_chunk(serde_json::from_value(json!({"project_id":pid,"doc_id":"legacy-range"})).unwrap()).await.unwrap_err();
    assert!(error.message.contains("Source chunk range mismatch"),"{error}");
}

#[tokio::test]
async fn lost_file_metadata_is_unknown_and_dedup_registration_does_not_claim_index_success() {
    let (temp,state,engram,pid) = fixture().await;
    state.graph.delete_project_data(&pid).unwrap();
    let freshness = engram.handle_get_index_freshness(serde_json::from_value(json!({"project_id":pid,"check_disk":false})).unwrap()).await.unwrap();
    let text = &freshness.content[0].as_text().unwrap().text;
    assert!(text.contains("source_index_format: unknown (no indexed file metadata)"),"{text}");
    let existing = engram.handle_index_project(serde_json::from_value(json!({
        "directory":temp.path().join("repo"),"project_name":"existing","project_type":"general","wait":true,"dedupe_by_directory":true
    })).unwrap()).await.unwrap();
    let text = &existing.content[0].as_text().unwrap().text;
    assert!(text.contains("status unknown") && text.contains(&pid),"{text}");
    assert!(!text.contains("Already indexed"));
    assert_eq!(state.registry.list_projects().unwrap().len(),1);
}
