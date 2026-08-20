#![allow(clippy::unwrap_used)]
//! `find_similar_changes` must not re-diff history it has already diffed.
//!
//! The co-change snapshot used to be keyed on HEAD: one new commit
//! invalidated it and the next call re-ran `git diff` for every one of
//! `max_commits` commits. On an active repo that is the multi-minute hang
//! reported on 2026-08-20 — while `detect_incomplete_changes` answered the
//! neighbouring question instantly off precomputed edges.
//!
//! Reuse is now per commit oid (a commit's diff is immutable), and the walk
//! carries a wall-clock budget so a cold call degrades to partial coverage
//! instead of hanging.

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

    for i in 0..8 {
        commit_file(
            &repo,
            &root,
            &format!("Site/feature{i}.vb"),
            "' code",
            &format!("feature {i}"),
        );
        commit_file(
            &repo,
            &root,
            &format!("Site/feature{i}.aspx"),
            "<%-- markup --%>",
            &format!("feature {i} markup"),
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
            project_name: "SimilarTest".into(),
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

async fn call(engram: &engram_server::Engram, pid: &str, files: &[&str]) {
    engram
        .find_similar_changes(Parameters(engram_server::FindSimilarChangesRequest {
            project_id: pid.to_string(),
            files: files.iter().map(|f| f.to_string()).collect(),
            max_commits: 500,
            top: 5,
        }))
        .await
        .expect("find_similar_changes must succeed");
}

/// A new commit must extend the cached walk, not invalidate it. The
/// snapshot's diffed-oid set must grow by exactly the new commit and must
/// never contain duplicates.
#[tokio::test]
async fn new_commit_extends_the_walk_instead_of_restarting_it() {
    let (tmp, state, engram, pid, repo) = setup().await;

    call(&engram, &pid, &["Site/feature3.vb"]).await;
    let after_first: Vec<String> = state
        .co_change_cache
        .get(&pid)
        .expect("first call must populate the co-change cache")
        .walked_oids
        .clone();
    assert!(
        after_first.len() >= 16,
        "expected the seeded history to be diffed, got {}",
        after_first.len()
    );

    commit_file(&repo, tmp.path(), "Site/feature9.vb", "' new", "feature 9");

    call(&engram, &pid, &["Site/feature3.vb"]).await;
    let after_second: Vec<String> = state
        .co_change_cache
        .get(&pid)
        .expect("cache must survive the new commit")
        .walked_oids
        .clone();

    assert_eq!(
        after_second.len(),
        after_first.len() + 1,
        "the second walk must add exactly the new commit"
    );
    let unique: std::collections::HashSet<&String> = after_second.iter().collect();
    assert_eq!(
        unique.len(),
        after_second.len(),
        "a commit must never be recorded twice"
    );
    for oid in &after_first {
        assert!(
            unique.contains(oid),
            "previously diffed commit {oid} was dropped, so it will be re-diffed"
        );
    }
}

/// Commits dropped as shape noise (bulk / empty) must still be recorded as
/// diffed. Otherwise every refresh pays for them again forever.
#[tokio::test]
async fn bulk_commits_are_recorded_so_they_are_not_re_diffed() {
    let (tmp, state, engram, pid, repo) = setup().await;

    // One commit touching more than the 80-file bulk threshold.
    {
        let root = tmp.path();
        let mut index = repo.index().unwrap();
        for i in 0..90 {
            let rel = format!("vendor/lib{i}.js");
            std::fs::create_dir_all(root.join("vendor")).unwrap();
            std::fs::write(root.join(&rel), "// vendored").unwrap();
            index.add_path(Path::new(&rel)).unwrap();
        }
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let head = repo.head().unwrap().target().unwrap();
        let parent = repo.find_commit(head).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "vendor bulk", &tree, &[&parent])
            .unwrap();
    }

    call(&engram, &pid, &["Site/feature3.vb"]).await;
    let snap = state.co_change_cache.get(&pid).expect("cache");
    let bulk_head = repo.head().unwrap().target().unwrap().to_string();

    assert!(
        snap.walked_oids.contains(&bulk_head),
        "the bulk commit must be recorded as already diffed"
    );
    assert!(
        !snap.commits.iter().any(|c| c.oid == bulk_head),
        "the bulk commit must still be excluded from scoring as shape noise"
    );
}

/// The snapshot is persisted with bincode; a fresh AppState (daemon restart)
/// must be able to load it and reuse the work.
#[tokio::test]
async fn snapshot_survives_a_restart() {
    let (tmp, state, engram, pid, _repo) = setup().await;
    call(&engram, &pid, &["Site/feature3.vb"]).await;
    let expected = state
        .co_change_cache
        .get(&pid)
        .expect("cache")
        .walked_oids
        .len();

    let disk = tmp
        .path()
        .join("engram_data")
        .join("co_change")
        .join(format!("{pid}.bin"));
    let bytes = std::fs::read(&disk).expect("snapshot must be persisted");
    let restored: engram_server::state::CoChangeSnapshot =
        bincode::deserialize(&bytes).expect("snapshot must round-trip");
    assert_eq!(restored.walked_oids.len(), expected);
}
