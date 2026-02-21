use engram_core::Config;
use engram_server::Engram;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use rmcp::model::RawContent;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

fn first_text(res: &rmcp::model::CallToolResult) -> &str {
    match &res.content[0].raw {
        RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    }
}

fn extract_field<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::trim)
}

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
        ..Default::default()
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
    let text = first_text(&res);

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
        ..Default::default()
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
    let text = first_text(&res);

    // Should process 1 file (main.rs). bin.rs and large.rs skipped.
    // stats.files counts scanned files, so it will be 3.
    // stats.chunks counts indexed chunks.
    assert!(
        text.contains("chunks=1"),
        "Should index 1 valid chunk. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_initial_index_byte_budget_enforced() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("byte_limit_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(project_dir.join("a.rs"), "fn a() { println!(\"aaaa\"); }\n").unwrap();
    std::fs::write(project_dir.join("b.rs"), "fn b() { println!(\"bbbb\"); }\n").unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: Some(10),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "byte_limit_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let text = first_text(&res);

    assert!(
        text.contains("Project byte budget exceeded"),
        "Expected byte-limit failure. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_incremental_update_byte_budget_enforced() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("update_byte_limit_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(project_dir.join("main.rs"), "fn main() {}\n").unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: Some(128),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "update_byte_limit_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let index_text = first_text(&index_res);
    assert!(index_text.contains("✅ Indexed project_id:"));
    let project_id = index_text
        .lines()
        .find_map(|line| line.strip_prefix("✅ Indexed project_id: "))
        .unwrap()
        .trim()
        .to_string();

    std::fs::write(project_dir.join("main.rs"), "x".repeat(2048)).unwrap();

    let update_res = engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id,
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();
    let update_text = first_text(&update_res);

    assert!(
        update_text.contains("Project byte budget exceeded"),
        "Expected byte-limit failure during update. Output: {}",
        update_text
    );
}

#[tokio::test]
async fn test_chunk_cap_respected_for_index_and_update() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("chunk_limit_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let initial = (0..80)
        .map(|i| format!("fn before_{i}() {{ println!(\"before {i}\"); }}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(project_dir.join("main.rs"), initial).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        max_chunks_per_file: 1,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "chunk_limit_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let index_text = first_text(&index_res);
    assert!(
        index_text.contains("chunks=1"),
        "Expected index_project to respect max_chunks_per_file=1. Output: {}",
        index_text
    );

    let project_id = index_text
        .lines()
        .find_map(|line| line.strip_prefix("✅ Indexed project_id: "))
        .unwrap()
        .trim()
        .to_string();

    let changed = (0..80)
        .map(|i| format!("fn after_{i}() {{ println!(\"after {i}\"); }}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(project_dir.join("main.rs"), changed).unwrap();

    let update_res = engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id,
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();
    let update_text = first_text(&update_res);

    assert!(
        update_text.contains("chunks=1"),
        "Expected update_project to respect max_chunks_per_file=1. Output: {}",
        update_text
    );
}

#[tokio::test]
async fn test_background_index_jobs_respect_shared_parse_guard_under_load() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let mut projects = Vec::new();
    for p in 0..4 {
        let project_dir = tmp.path().join(format!("stress_project_{p}"));
        std::fs::create_dir_all(&project_dir).unwrap();
        for i in 0..30 {
            let content = (0..100)
                .map(|n| format!("fn f_{p}_{i}_{n}() {{ println!(\"{n}\"); }}"))
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(project_dir.join(format!("file_{i}.rs")), content).unwrap();
        }
        projects.push(project_dir);
    }

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: projects.clone(),
        max_project_files: None,
        max_project_bytes: None,
        max_parse_concurrency: 1,
        max_concurrent_jobs: 8,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Arc::new(Engram::new(state));

    let mut tasks = Vec::new();
    for (idx, project_dir) in projects.iter().enumerate() {
        let engram = engram.clone();
        let directory = project_dir.to_string_lossy().to_string();
        tasks.push(tokio::spawn(async move {
            engram
                .index_project(Parameters(engram_server::IndexProjectRequest {
                    directory,
                    project_name: format!("stress_{idx}"),
                    project_type: "code".into(),
                    wait: false,
                    dedupe_by_directory: true,
                }))
                .await
                .unwrap()
        }));
    }

    let mut job_ids = Vec::new();
    for task in tasks {
        let res = task.await.unwrap();
        let text = first_text(&res).to_string();
        let job_id = extract_field(&text, "job_id: ").expect("job_id in response");
        job_ids.push(job_id.to_string());
    }
    assert_eq!(job_ids.len(), projects.len());

    for _ in 0..120 {
        let mut done = 0;
        for job_id in &job_ids {
            let status_res = engram
                .get_job_status(Parameters(engram_server::CancelJobRequest {
                    job_id: job_id.clone(),
                }))
                .await
                .unwrap();
            let status_text = first_text(&status_res);
            if status_text.contains("status: done") {
                done += 1;
                continue;
            }
            assert!(
                !status_text.contains("status: failed"),
                "background index job failed: {status_text}"
            );
        }
        if done == job_ids.len() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    panic!("Timed out waiting for background indexing jobs to complete");
}
