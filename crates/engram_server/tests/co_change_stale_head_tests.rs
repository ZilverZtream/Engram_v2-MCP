#![allow(clippy::unwrap_used)]
//! Doc-11 P1c (round-2 audit item 6 residue): a warm co-change snapshot is
//! served without a walk ONLY while HEAD is unchanged — one new commit and
//! the call walks git again at request time. The audit's contract: NO
//! call-time git walk on the request path. A changed HEAD serves the stale
//! snapshot immediately and refreshes in the background.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::path::Path;
use tempfile::tempdir;

fn commit_file(repo: &git2::Repository, root: &Path, rel: &str, body: &str, msg: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&abs, body).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new(rel)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parents: Vec<git2::Commit> = match repo.head().ok().and_then(|h| h.target()) {
        Some(oid) => vec![repo.find_commit(oid).unwrap()],
        None => vec![],
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)
        .unwrap();
}

async fn setup() -> (
    tempfile::TempDir,
    AppState,
    engram_server::Engram,
    String,
    git2::Repository,
) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let repo = git2::Repository::init(&root).unwrap();
    for i in 0..6 {
        commit_file(
            &repo,
            &root,
            &format!("Site/feature{i}.vb"),
            "' code",
            &format!("feature {i}"),
        );
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(200),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "StaleHeadTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid, repo)
}

async fn call_text(engram: &engram_server::Engram, pid: &str) -> String {
    let res = engram
        .find_similar_changes(Parameters(engram_server::FindSimilarChangesRequest {
            project_id: pid.to_string(),
            files: vec!["Site/feature3.vb".to_string()],
            max_commits: 500,
            top: 5,
        }))
        .await
        .expect("find_similar_changes must succeed");
    match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    }
}

#[tokio::test]
async fn a_changed_head_serves_the_stale_snapshot_without_a_call_time_walk() {
    let (tmp, state, engram, pid, repo) = setup().await;

    // First call: the snapshot exists (index-time warm or this cold walk).
    let _ = call_text(&engram, &pid).await;
    let warm_head = state
        .co_change_cache
        .get(&pid)
        .expect("snapshot")
        .head
        .clone();

    // HEAD advances by one commit.
    commit_file(&repo, tmp.path(), "Site/feature9.vb", "' new", "feature 9");

    // The request path must NOT walk git: stale snapshot served, refresh in
    // the background.
    let out = call_text(&engram, &pid).await;
    assert!(
        out.contains("served without a git walk"),
        "a changed HEAD must not trigger a call-time walk:\n{out}"
    );
    assert!(
        out.contains("background"),
        "the answer must say a background refresh is running:\n{out}"
    );

    // The background refresh eventually lands the new HEAD in the snapshot.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let head_now = state
            .co_change_cache
            .get(&pid)
            .map(|s| s.head.clone())
            .unwrap_or_default();
        if head_now != warm_head && !head_now.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background refresh never landed (head still {head_now})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
