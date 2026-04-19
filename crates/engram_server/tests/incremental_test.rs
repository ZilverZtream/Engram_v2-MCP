#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::Engram;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_incremental_indexing() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("incremental_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // 1. Initial state: file A and file B
    let repo = git2::Repository::init(&project_dir).unwrap();
    std::fs::write(project_dir.join("A.rs"), "fn function_a() {}").unwrap();
    std::fs::write(project_dir.join("B.rs"), "fn function_b() {}").unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("A.rs")).unwrap();
    index.add_path(std::path::Path::new("B.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 2. Initial Indexing
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "inc_test".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    assert!(text.contains("Files indexed: 2"));

    // 3. Modify A, Delete B, Create C
    std::fs::write(
        project_dir.join("A.rs"),
        "fn function_a() { println!(\"changed\"); }",
    )
    .unwrap();
    std::fs::remove_file(project_dir.join("B.rs")).unwrap();
    std::fs::write(project_dir.join("C.rs"), "fn function_c() {}").unwrap();

    // 4. Update Project (Incremental)
    let update_res = engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_commits: 0,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    let text = match &update_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    // We expect 2 files indexed (A changed, C created). B is deleted so not indexed.
    // Wait, update_project_impl returns summary with stats from index_files.
    // index_files returns stats for *processed* files.
    // So if A and C are processed, files=2.
    assert!(
        text.contains("files=2"),
        "Should index 2 files (A changed, C new). Output: {}",
        text
    );

    // 5. Verify Search
    let search_req = engram_server::SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "changed".into(),
        max_results: 5,
        use_mmr: false,
        fts_mode: engram_server::models::FtsMode::Strict,
        include_content: true,
        max_content_chars_per_result: 1200,
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        metadata_filter: None,
semantic: true,
    };

    let res = engram.search_memory(Parameters(search_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("A.rs"), "Should find changed A.rs");

    // Verify B is gone
    let search_req_b = engram_server::SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "function_b".into(),
        max_results: 5,
        use_mmr: false,
        fts_mode: engram_server::models::FtsMode::Strict,
        include_content: true,
        max_content_chars_per_result: 1200,
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        metadata_filter: None,
semantic: true,
    };
    let res_b = engram
        .search_memory(Parameters(search_req_b))
        .await
        .unwrap();
    let text_b = match &res_b.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        !text_b.contains("B.rs"),
        "Should not find B.rs. Output: {}",
        text_b
    );

    // Verify C is present
    let search_req_c = engram_server::SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "function_c".into(),
        max_results: 5,
        use_mmr: false,
        fts_mode: engram_server::models::FtsMode::Strict,
        include_content: true,
        max_content_chars_per_result: 1200,
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        metadata_filter: None,
semantic: true,
    };
    let res_c = engram
        .search_memory(Parameters(search_req_c))
        .await
        .unwrap();
    let text_c = match &res_c.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text_c.contains("C.rs"), "Should find C.rs");
}
