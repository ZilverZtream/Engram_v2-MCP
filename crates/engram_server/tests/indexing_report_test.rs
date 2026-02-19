use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_indexing_report_smoke() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let project_dir = root.join("my_project");
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create some files
    std::fs::write(project_dir.join("main.py"), "def hello(): pass\n").unwrap();
    std::fs::write(project_dir.join("utils.py"), "def util(): pass\n").unwrap();

    // A file with invalid UTF-8 to trigger skip
    use std::io::Write;
    let mut f = std::fs::File::create(project_dir.join("bad.py")).unwrap();
    f.write_all(&[0xFF, 0xFE, 0xFD]).unwrap();
    drop(f);

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

    // Index project
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "ReportTest".into(),
            project_type: "python".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    println!("INDEX RESULT:\n{}", text);

    assert!(
        text.contains("# Indexing Report"),
        "Should contain report header"
    );
    assert!(
        text.contains("Files indexed: 2"),
        "Should show 2 files indexed"
    );
    assert!(
        text.contains("Files skipped: 1"),
        "Should show 1 file skipped"
    );
    assert!(text.contains("python: 2"), "Should show python language");

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Verify it's in the memory bank
    let mb_res = engram
        .list_memory_bank(Parameters(engram_server::ProjectIdRequest {
            project_id: project_id.clone(),
        }))
        .await
        .unwrap();

    let mb_text = &mb_res.content[0].as_text().unwrap().text;
    assert!(
        mb_text.contains("engram/index_report"),
        "Should be in memory bank"
    );

    let report_res = engram
        .read_memory_bank(Parameters(engram_server::MemorySectionRequest {
            project_id: project_id.clone(),
            section: "engram/index_report".to_string(),
        }))
        .await
        .unwrap();

    let report_text = &report_res.content[0].as_text().unwrap().text;
    assert!(
        report_text.contains("# Indexing Report"),
        "Report content should be correct"
    );
    assert!(
        report_text.contains("bad.py"),
        "Should list bad.py in skipped files"
    );
}
