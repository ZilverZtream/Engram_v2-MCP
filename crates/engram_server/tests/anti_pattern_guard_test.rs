#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::process::Command;
use tempfile::tempdir;

#[tokio::test]
async fn test_anti_pattern_guard_smoke() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Initialize a git repo and create a "revert" history
    Command::new("git")
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap();
    Command::new("git")
        .arg("config")
        .arg("user.email")
        .arg("test@example.com")
        .current_dir(root)
        .output()
        .unwrap();
    Command::new("git")
        .arg("config")
        .arg("user.name")
        .arg("Test User")
        .current_dir(root)
        .output()
        .unwrap();

    let file_path = root.join("main.py");
    std::fs::write(&file_path, "def start():\n    print('hello')\n").unwrap();
    Command::new("git")
        .arg("add")
        .arg("main.py")
        .current_dir(root)
        .output()
        .unwrap();
    Command::new("git")
        .arg("commit")
        .arg("-m")
        .arg("Initial commit")
        .current_dir(root)
        .output()
        .unwrap();

    // Add a "bad" pattern
    std::fs::write(
        &file_path,
        "def start():\n    print('hello')\n    # BAD_PATTERN_DETECTED\n    critical_bug = 1/0\n",
    )
    .unwrap();
    Command::new("git")
        .arg("add")
        .arg("main.py")
        .current_dir(root)
        .output()
        .unwrap();
    Command::new("git")
        .arg("commit")
        .arg("-m")
        .arg("Add buggy code")
        .current_dir(root)
        .output()
        .unwrap();

    // Revert it
    Command::new("git")
        .arg("revert")
        .arg("HEAD")
        .arg("--no-edit")
        .current_dir(root)
        .output()
        .unwrap();

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
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "AntiPatternTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    let empty = engram.anti_pattern_guard(Parameters(engram_server::AntiPatternGuardRequest {
        project_id: project_id.clone(), code: Some("critical_bug = 1/0".into()), code_file: None, code_file_blake3: None, limit: 5,
        use_vector: false, include_content: true,
    })).await.unwrap();
    let empty_text = &empty.content[0].as_text().unwrap().text;
    assert!(empty_text.starts_with("verdict: INSUFFICIENT\n"), "{empty_text}");
    assert!(empty_text.contains("comparison: not_run"), "{empty_text}");

    // Analyze reverts to populate antipattern namespace
    engram
        .analyze_reverts(Parameters(engram_server::AnalyzeRevertsRequest {
            project_id: project_id.clone(),
            max_commits: 10,
        }))
        .await
        .unwrap();

    // 2. Test the guard
    let res = engram
        .anti_pattern_guard(Parameters(engram_server::AntiPatternGuardRequest {
            project_id: project_id.clone(),
            code: Some("critical_bug = 1/0".to_string()), code_file: None, code_file_blake3: None,
            limit: 5,
            use_vector: false,
            include_content: true,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    println!("GUARD OUTPUT:\n{}", text);

    assert!(
        text.contains("BLOCK") || text.contains("WARN"),
        "Should detect anti-pattern"
    );
    assert!(
        text.contains("BAD_PATTERN_DETECTED") || text.contains("critical_bug"),
        "Should show the matched code"
    );
    assert!(text.contains("risky"), "Should explain why it's risky");
    assert!(text.contains("alternative"), "Should suggest alternative");
    let clean = engram.anti_pattern_guard(Parameters(engram_server::AntiPatternGuardRequest {
        project_id: project_id.clone(), code: Some("unique_unrelated_identifier_923715".into()), code_file: None, code_file_blake3: None,
        limit: 5, use_vector: false, include_content: true,
    })).await.unwrap();
    let clean_text = &clean.content[0].as_text().unwrap().text;
    assert!(clean_text.starts_with("verdict: PASS\n"), "{clean_text}");
    assert!(clean_text.contains("comparison: completed"), "{clean_text}");
    assert!(clean_text.contains("does not certify code safety"), "{clean_text}");

}

// The populated fixture above must also distinguish a completed clean search.
