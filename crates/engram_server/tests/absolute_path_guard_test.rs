#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_absolute_path_rejection() {
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

    // Register project
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "GuardTest".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Manually create stats with an absolute path to trigger the guard
    let mut stats = engram_index::IngestStats::default();

    // Test rejection in all_files
    let abs_path = if cfg!(windows) {
        r"C:\Absolute\Path.rs"
    } else {
        "/absolute/path.rs"
    };
    stats.all_files.push(engram_core::RelPath::new(abs_path));

    let res: anyhow::Result<()> = engram
        .process_ingest_stats_for_test(project_id, 1, &stats)
        .await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("absolute path"));
}
