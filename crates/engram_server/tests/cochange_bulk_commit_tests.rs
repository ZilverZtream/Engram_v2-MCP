#![allow(clippy::unwrap_used)]
//! A bulk commit (more files than the co-change cap) is not evidence that its
//! files belong together: it adds no co-change weight, and index_git_history
//! reports how many such commits it skipped.
use engram_core::{Config, ProjectRecord};
use engram_graph::EdgeKind;
use engram_server::{AppState, Engram};
use git2::{Repository, Signature};
use serde_json::json;

const PID: &str = "cochange-bulk-test";

fn setup() -> (tempfile::TempDir, AppState, Repository) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let repo = Repository::init(&root).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (temp, state, repo)
}

fn commit_files(repo: &Repository, files: &[String], text: &str) {
    let root = repo.workdir().unwrap().to_path_buf();
    let mut index = repo.index().unwrap();
    for f in files {
        let path = root.join(f);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        index.add_path(std::path::Path::new(f)).unwrap();
    }
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    let sig = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &parents)
        .unwrap();
}

#[tokio::test]
async fn a_bulk_commit_adds_no_cochange_weight_and_is_reported() {
    let (_temp, state, repo) = setup();
    let pair = vec!["src/a.txt".to_string(), "src/b.txt".to_string()];
    commit_files(&repo, &pair, "one\n");
    let bulk: Vec<String> = (0..81).map(|i| format!("bulk/f{i:03}.txt")).collect();
    commit_files(&repo, &bulk, "two\n");
    commit_files(&repo, &pair, "three\n");

    let engram = Engram::new(state.clone());
    let request = serde_json::from_value(json!({"project_id": PID, "max_commits": 10, "wait": true}))
        .unwrap();
    let response = engram.handle_index_git_history(request).await.unwrap();
    let text = response.content[0].as_text().unwrap().text.clone();
    assert!(
        text.contains("bulk_commits_skipped_for_coupling: 1"),
        "the skipped bulk commit must be reported:\n{text}"
    );

    let bulk_partners = state
        .graph
        .neighbors(PID, EdgeKind::TemporalCoupling, "file:bulk/f000.txt", 200)
        .unwrap();
    assert!(
        bulk_partners.is_empty(),
        "files that only changed together in a bulk commit are not coupled: {bulk_partners:?}"
    );
    let pair_partners = state
        .graph
        .neighbors(PID, EdgeKind::TemporalCoupling, "file:src/a.txt", 10)
        .unwrap();
    assert!(
        pair_partners.iter().any(|(id, _)| id == "file:src/b.txt"),
        "files changed together in ordinary commits stay coupled: {pair_partners:?}"
    );
}
