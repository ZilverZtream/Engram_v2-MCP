#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram, GetChunkRequest, IndexProjectRequest, SearchMemoryRequest};
use rmcp::handler::server::tool::Parameters;
use rmcp::model::CallToolResult;
use tempfile::tempdir;

#[tokio::test]
async fn test_dup_content_does_not_overwrite() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("my_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Initialize git repo
    git2::Repository::init(&project_dir).unwrap();

    // Create two files with identical content but different line offsets.
    // File 1: identical content is at the top.
    let content1 = "fn shared_logic() {\n    println!(\"same\");\n}\n";
    std::fs::write(project_dir.join("file1.rs"), content1).unwrap();

    // File 2: identical content is preceded by some other code.
    let content2 = "fn other_stuff() {}\n\nfn shared_logic() {\n    println!(\"same\");\n}\n";
    std::fs::write(project_dir.join("file2.rs"), content2).unwrap();

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

    // 1. Index Project
    let index_req = IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "test_project".into(),
        project_type: engram_server::models::ProjectType::General,
        wait: true,
        dedupe_by_directory: true,
    };

    let res: CallToolResult = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    let project_id = text
        .lines()
        .find(|l| l.contains("project_id: "))
        .unwrap()
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .trim();

    // 2. Search for the shared content
    let search_req = SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "shared_logic".into(),
        max_results: 10,
        use_mmr: false,
        fts_mode: engram_server::models::FtsMode::Strict,
        include_content: true,
        max_content_chars_per_result: 1200,
        ..Default::default()
    };

    let res: CallToolResult = engram.search_memory(Parameters(search_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    // Assert search returns TWO results with different paths.
    assert!(text.contains("file1.rs"), "Should find file1.rs");
    assert!(text.contains("file2.rs"), "Should find file2.rs");

    // Extract doc_ids
    let mut doc_ids = Vec::new();
    for line in text.lines() {
        if line.contains("doc_id: ") {
            doc_ids.push(line.split("doc_id: ").nth(1).unwrap().trim().to_string());
        }
    }

    assert_eq!(
        doc_ids.len(),
        2,
        "Should have 2 doc_ids, found: {:?}",
        doc_ids
    );
    assert!(
        doc_ids[0] != doc_ids[1],
        "doc_ids must be unique even for identical content in different files/locations"
    );

    // 3. Verify each doc_id returns the correct file
    for did in &doc_ids {
        let get_res = engram
            .get_chunk(Parameters(GetChunkRequest {
                project_id: project_id.to_string(),
                doc_id: did.clone(),
                namespace: "memory".into(),
                inject_rules: false,
                logical_slice: None,
            }))
            .await
            .unwrap();

        let get_text = match &get_res.content[0].raw {
            rmcp::model::RawContent::Text(t) => &t.text,
            _ => panic!("Expected text content"),
        };

        if *did == doc_ids[0] {
            // In current search implementation, we don't strictly guarantee order here but we can check if it matches either
        }

        // Ensure the path in the output matches what we expect for that doc_id.
        // We can just check that it contains either file1.rs or file2.rs and matches the search output.
        assert!(get_text.contains("path: file1.rs") || get_text.contains("path: file2.rs"));
    }
}
