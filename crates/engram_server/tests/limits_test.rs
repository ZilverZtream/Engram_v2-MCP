use engram_core::Config;
use engram_server::Engram;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_max_files_limit() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("limits_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create 5 files
    for i in 0..5 {
        std::fs::write(project_dir.join(format!("file_{}.rs", i)), "fn main() {}").unwrap();
    }

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: Some(3), // Limit to 3 files
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "limit_test".into(),
        project_type: "code".into(),
        wait: true,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    assert!(
        text.contains("Too many files"),
        "Should fail due to file limit. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_binary_file_skip() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("binary_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Valid file
    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    // Binary file (with .rs extension to trick iter_files, but should be caught by content check)
    // Actually iter_files checks extension. If I name it .rs but put binary content.
    // Null byte at beginning.
    let binary_content = vec![0u8, 1, 2, 3];
    std::fs::write(project_dir.join("bin.rs"), &binary_content).unwrap();

    // Large file (11MB)
    let large_file_path = project_dir.join("large.rs");
    let f = std::fs::File::create(&large_file_path).unwrap();
    f.set_len(11 * 1024 * 1024).unwrap(); // Sparse file is fast

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
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "binary_test".into(),
        project_type: "code".into(),
        wait: true,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    // Should process 1 file (main.rs). bin.rs and large.rs skipped.
    // stats.files counts scanned files, so it will be 3.
    // stats.chunks counts indexed chunks.
    assert!(
        text.contains("chunks=1"),
        "Should index 1 valid chunk. Output: {}",
        text
    );
}
