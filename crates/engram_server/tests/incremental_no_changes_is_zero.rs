#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{
    AppState, Engram, IndexProjectRequest, SearchMemoryRequest, UpdateProjectRequest,
};
use rmcp::handler::server::tool::Parameters;
use rmcp::model::CallToolResult;
use tempfile::tempdir;

#[tokio::test]
async fn test_incremental_no_changes_is_zero() {
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

    // 2. Run update_project without any changes
    let update_req = UpdateProjectRequest {
        project_id: project_id.to_string(),
        wait: true,
        max_commits: 100,
        index_antipatterns: false,
    };
    let res: CallToolResult = engram.update_project(Parameters(update_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    // Assert update summary reports files=0
    assert!(
        text.contains("files=0"),
        "Should re-index 0 files when nothing changed. Output: \n{}",
        text
    );

    // 3. Assert search still returns hits from file1.rs in the new generation
    let search_req = SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "file1".into(),
        max_results: 10,
        use_mmr: false,
        fts_mode: engram_server::models::FtsMode::Strict,
        include_content: true,
        ..Default::default()
    };

    let res: CallToolResult = engram.search_memory(Parameters(search_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    assert!(
        text.contains("file1.rs"),
        "Should still find file1.rs in the new generation after zero-change update. Output: \n{}",
        text
    );
}
