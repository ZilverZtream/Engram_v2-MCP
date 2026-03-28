#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use engram_server::state::AppEvent;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_cooccurrence_uses_pk_not_chunk_id() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Create two files with IDENTICAL content
    let content = r#"fn shared_utility() { println!("same"); }"#;
    std::fs::write(root.join("FileA.rs"), content).unwrap();
    std::fs::write(root.join("FileB.rs"), content).unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, mut events_rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // 2. Index project
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "CoocTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // 3. Search for the shared content
    let search_res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.clone(),
            query: "shared_utility".to_string(),
            namespace: "memory".to_string(),
            max_results: 10,
            fts_mode: engram_server::models::FtsMode::Loose,
            ..Default::default()
        }))
        .await
        .unwrap();

    let _text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    // Let's grab the event.
    let ev = events_rx.recv().await.unwrap();
    let hits = match ev {
        AppEvent::SearchSession { hits, .. } => hits,
        _ => panic!("Expected SearchSession event"),
    };

    assert!(
        hits.len() >= 2,
        "Should have at least 2 hits for identical content"
    );

    // Manually record co-occurrence
    engram_server::actors::dreamer::record_cooccurrence(&state, project_id, &hits)
        .await
        .unwrap();

    // 4. Verify graph nodes: should have two DIFFERENT pk nodes
    let all_nodes = engram
        .state
        .graph
        .query_nodes(project_id, Some("chunk"), None, None, 10)
        .unwrap();

    let pk_nodes: Vec<_> = all_nodes
        .iter()
        .filter(|n| n.node_id.starts_with("pk:"))
        .collect();
    assert_eq!(
        pk_nodes.len(),
        2,
        "Should have two separate chunk nodes even with same content. Nodes: {:?}",
        pk_nodes
    );

    assert!(
        pk_nodes
            .iter()
            .any(|n| n.file_path.as_str().contains("FileA.rs"))
    );
    assert!(
        pk_nodes
            .iter()
            .any(|n| n.file_path.as_str().contains("FileB.rs"))
    );
}
