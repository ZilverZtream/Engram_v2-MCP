use engram_core::Config;
use engram_server::{
    AppState, Engram, IndexProjectRequest, ProjectIdRequest, UpdateProjectRequest,
};
use rmcp::handler::server::tool::Parameters;
use rmcp::model::CallToolResult;
use tempfile::tempdir;

#[tokio::test]
async fn test_vector_store_does_not_bloat_on_repeat() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("my_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Initialize git repo and make initial commit
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    let sig = repo.signature().unwrap();

    std::fs::write(
        project_dir.join("file1.rs"),
        "fn main() { println!(\"file1\"); }",
    )
    .unwrap();
    index.add_path(std::path::Path::new("file1.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "projection".into(), // Use real-ish vector storage
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. Index Project
    let index_req = IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "test_project".into(),
        project_type: "code".into(),
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

    // Perform one update to establish "history" baseline
    let update_req = UpdateProjectRequest {
        project_id: project_id.to_string(),
        wait: true,
        max_commits: 100,
        index_antipatterns: false,
    };
    engram.update_project(Parameters(update_req)).await.unwrap();

    // Get initial health after baseline established
    let health_res: CallToolResult = engram
        .project_health(Parameters(ProjectIdRequest {
            project_id: project_id.to_string(),
        }))
        .await
        .unwrap();
    let health_text = match &health_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    // Extract lancedb_rows
    let get_rows = |t: &str| -> u64 {
        t.lines()
            .find(|l| l.contains("lancedb_rows: "))
            .unwrap()
            .split("lancedb_rows: ")
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    };

    let initial_rows = get_rows(health_text);
    assert!(initial_rows > 0);

    // 2. Re-run index multiple times without changing anything.
    for _ in 0..2 {
        let update_req = UpdateProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_commits: 100,
            index_antipatterns: false,
        };
        let _: CallToolResult = engram.update_project(Parameters(update_req)).await.unwrap();
    }

    // 3. Assert Lance row count does not grow unbounded for same docs.
    let health_res: CallToolResult = engram
        .project_health(Parameters(ProjectIdRequest {
            project_id: project_id.to_string(),
        }))
        .await
        .unwrap();
    let health_text = match &health_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    let final_rows = get_rows(health_text);
    assert_eq!(
        final_rows, initial_rows,
        "LanceDB should not bloat on repeat indexing of same content. \n{}",
        health_text
    );
}
