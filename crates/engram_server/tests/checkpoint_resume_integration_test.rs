use engram_core::{Checkpoint, Config, JobPhase, metrics::metrics};
use engram_server::Engram;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

fn init_repo(project_dir: &std::path::Path) {
    let repo = git2::Repository::init(project_dir).unwrap();
    std::fs::write(project_dir.join("A.rs"), "fn a() {}").unwrap();
    std::fs::write(project_dir.join("B.rs"), "fn b() {}").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("A.rs")).unwrap();
    index.add_path(std::path::Path::new("B.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
}

#[tokio::test]
#[ignore = "Flaky in CI: rmcp update_project decode path"]
async fn update_project_resumes_from_checkpoint_after_restart() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    init_repo(&project_dir);

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        max_project_files: None,
        max_project_bytes: None,
        ..Default::default()
    };

    let (state1, _) = AppState::new(cfg.clone()).unwrap();
    let engram1 = Engram::new(state1.clone());
    let index_res = engram1
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "resume".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim()
        .to_string();

    std::fs::write(project_dir.join("A.rs"), "fn a() { println!(\"x\"); }").unwrap();
    std::fs::write(project_dir.join("B.rs"), "fn b() { println!(\"y\"); }").unwrap();

    let cp = Checkpoint {
        job_id: "simulated-crash-job".into(),
        project_id: project_id.clone(),
        phase: JobPhase::Parsing,
        items_processed: 1,
        items_total: 2,
        generation: 2,
        idempotency_key: Checkpoint::compute_idempotency_key(
            &project_id,
            &project_dir.to_string_lossy(),
            2,
        ),
        resume_state: Some(
            serde_json::json!({
                "pending_files": ["A.rs"],
                "processed_files": ["B.rs"],
                "processed_chunk_ids": []
            })
            .to_string(),
        ),
        updated_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        error: None,
    };
    state1.checkpoints.put(&cp).unwrap();
    drop(engram1);
    drop(state1);

    // restart server process state
    let (state2, _) = AppState::new(cfg).unwrap();
    let engram2 = Engram::new(state2);
    let before = metrics().checkpoints_resumed.get();

    let res = engram2
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.clone(),
            wait: true,
            max_commits: 0,
            index_antipatterns: false,
        }))
        .await
        .unwrap();
    let out = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        out.contains("files=1"),
        "Expected resumed single-file indexing: {out}"
    );
    assert!(metrics().checkpoints_resumed.get() > before);
}
