use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_instrumentation_pack_and_ingest() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

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

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // 1. Get pack
    let res_pack = engram
        .get_instrumentation_pack(Parameters(engram_server::GetInstrumentationPackRequest {
            language: "csharp".into(),
        }))
        .await
        .unwrap();

    let pack_text = &res_pack.content[0].as_text().unwrap().text;
    assert!(
        pack_text.contains("protected void LogEngramEvent"),
        "Should contain C# snippet"
    );

    // 2. Index dummy project
    let project_dir = root.join("my_project");
    std::fs::create_dir_all(&project_dir).unwrap();
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "InstTest".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // 3. Ingest logs
    let logs = r#"
ENGRAM_LOG|2026-02-19T15:00:00|Default.aspx|OnClick|btnSubmit|a4956f29a36e
ENGRAM_LOG|2026-02-19T15:01:00|~/Order.aspx|OnSave|btnSave|stored_proc_hash
"#;
    let res_ingest = engram
        .ingest_instrumentation_logs(Parameters(
            engram_server::IngestInstrumentationLogsRequest {
                project_id: project_id.clone(),
                log_content: logs.to_string(),
            },
        ))
        .await
        .unwrap();

    let ingest_text = &res_ingest.content[0].as_text().unwrap().text;
    assert!(
        ingest_text.contains("added 2 runtime SQL call edges"),
        "Should show 2 edges added"
    );

    // 4. Verify in graph
    let edges = state
        .graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::SqlCalls))
        .unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e.source_id == "control:Default.aspx:btnSubmit"
                && e.target_id == "sql:inline:a4956f29a36e")
    );
    assert!(
        edges
            .iter()
            .any(|e| e.source_id == "control:Order.aspx:btnSave"
                && e.target_id == "sql:inline:stored_proc_hash")
    );
}
