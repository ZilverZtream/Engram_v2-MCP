//! search_history must be usable for leak-free replays on raw story text:
//! story prose never crashes the query, `date_before` is exclusive, and
//! `as_of_rev` keeps only commits in that revision's past. Dates alone cannot
//! give the last guarantee — a squash-merge team's branch commits are dated
//! days before the squash lands, and an index built from a checkout holds
//! whatever branches were checked out when it ran.

use engram_core::Config;
use engram_server::Engram;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

fn commit(repo: &git2::Repository, file: &str, message: &str, when: i64) -> git2::Oid {
    let dir = repo.workdir().unwrap().to_path_buf();
    std::fs::write(dir.join(file), format!("// {message}\n")).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new(file)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::new("Dev", "dev@example.com", &git2::Time::new(when, 0)).unwrap();
    let parents: Vec<git2::Commit> = repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .into_iter()
        .collect();
    let parents: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .unwrap()
}

fn text(res: &rmcp::model::CallToolResult) -> String {
    match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    }
}

fn request(project_id: &str, query: &str) -> engram_server::SearchHistoryRequest {
    serde_json::from_value(serde_json::json!({
        "project_id": project_id, "query": query, "fts_mode": "loose", "limit": 10
    }))
    .unwrap()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    engram: Engram,
    project_id: String,
    repo: git2::Repository,
    base: git2::Oid,
    answer: git2::Oid,
}

/// Two commits touching one story. With `publish_base_first`, an origin
/// default branch points at `base` before history is indexed, so `answer`
/// is a local, unpublished commit.
async fn fixture(publish_base_first: bool) -> Fixture {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let repo = git2::Repository::init(&project_dir).unwrap();
    let base = commit(
        &repo,
        "filters.vb",
        "Photo filter uses a parameter list",
        1_700_000_000,
    );
    if publish_base_first {
        repo.reference("refs/remotes/origin/main", base, true, "published")
            .unwrap();
    }
    let answer = commit(
        &repo,
        "report.vb",
        "Photo filter avoids the parameter limit",
        1_700_086_400,
    );

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);
    let indexed = text(
        &engram
            .index_project(Parameters(engram_server::IndexProjectRequest {
                directory: project_dir.to_string_lossy().to_string(),
                project_name: "cutoff".into(),
                project_type: engram_server::models::ProjectType::General,
                wait: true,
                dedupe_by_directory: true,
            }))
            .await
            .unwrap(),
    );
    let project_id = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            force: false,
            project_id: project_id.clone(),
            max_commits: 10,
            index_antipatterns: false,
            mode: None,
            wait: true,
        }))
        .await
        .unwrap();

    Fixture {
        _tmp: tmp,
        engram,
        project_id,
        repo,
        base,
        answer,
    }
}

#[tokio::test]
async fn search_history_is_crash_free_and_leak_free_on_story_text() {
    // Bind the temp dir: `..` would drop it, and the index with it.
    let Fixture {
        _tmp,
        engram,
        project_id,
        base,
        answer,
        repo: _repo,
    } = fixture(false).await;
    // Raw story prose: backticks, NOT/IN/OR, field-like colons, brackets, quotes.
    let story = "[Chore] Photo filter must NOT hit the `parameter` limit IN reports OR maps. Note: \"missing\" photos!";
    let all = text(
        &engram
            .search_history(Parameters(request(&project_id, story)))
            .await
            .unwrap(),
    );
    assert!(
        all.contains(&format!("commit: {}", answer)),
        "unfiltered search finds the answer commit: {all}"
    );
    assert!(all.contains(&format!("commit: {}", base)), "{all}");
    // Commit identity, subject line and a readable date — not a bare epoch.
    assert!(
        all.contains("message: Photo filter avoids the parameter limit"),
        "{all}"
    );
    assert!(all.contains("date: 2023-11-15"), "{all}");
    // One result per commit: its message and diff fold together.
    assert!(all.contains("files: report.vb"), "{all}");
    assert_eq!(
        all.matches(&format!("commit: {answer}")).count(),
        1,
        "{all}"
    );
    assert!(all.contains("author: Dev"), "{all}");

    // date_before at the answer commit's own instant excludes it (exclusive).
    let mut dated = request(&project_id, story);
    dated.date_before = Some(1_700_086_400);
    let dated = text(&engram.search_history(Parameters(dated)).await.unwrap());
    assert!(!dated.contains(&format!("commit: {}", answer)), "{dated}");
    assert!(dated.contains(&format!("commit: {}", base)), "{dated}");

    // as_of_rev keeps only commits reachable from the revision.
    let mut as_of = request(&project_id, story);
    as_of.as_of_rev = Some(base.to_string());
    let as_of = text(&engram.search_history(Parameters(as_of)).await.unwrap());
    assert!(!as_of.contains(&format!("commit: {}", answer)), "{as_of}");
    assert!(as_of.contains(&format!("commit: {}", base)), "{as_of}");

    // An unresolvable revision is an error, never a silently unfiltered search.
    let mut bad = request(&project_id, story);
    bad.as_of_rev = Some("no-such-branch".into());
    assert!(engram.search_history(Parameters(bad)).await.is_err());
}

const STORY: &str = "Photo filter parameter limit";

#[tokio::test]
async fn default_search_is_limited_to_published_history() {
    let fixture = fixture(false).await;
    // Indexed from a checkout whose tip was never published: once an origin
    // default branch exists, the unpublished commit no longer surfaces.
    fixture
        .repo
        .reference("refs/remotes/origin/main", fixture.base, true, "published")
        .unwrap();
    let out = text(
        &fixture
            .engram
            .search_history(Parameters(request(&fixture.project_id, STORY)))
            .await
            .unwrap(),
    );
    assert!(out.contains("published history"), "{out}");
    assert!(out.contains(&format!("commit: {}", fixture.base)), "{out}");
    assert!(
        !out.contains(&format!("commit: {}", fixture.answer)),
        "{out}"
    );
}

#[tokio::test]
async fn history_indexing_walks_the_published_branch_not_the_checkout() {
    let fixture = fixture(true).await;
    // Even asking for the checkout's own tip finds nothing past origin/main:
    // the unpublished commit was never indexed.
    let mut req = request(&fixture.project_id, STORY);
    req.as_of_rev = Some(fixture.answer.to_string());
    let out = text(
        &fixture
            .engram
            .search_history(Parameters(req))
            .await
            .unwrap(),
    );
    assert!(out.contains(&format!("commit: {}", fixture.base)), "{out}");
    assert!(
        !out.contains(&format!("commit: {}", fixture.answer)),
        "{out}"
    );
}

/// Commit `file` with explicit parents; `head` moves HEAD to it.
fn commit_on(
    repo: &git2::Repository,
    file: &str,
    message: &str,
    when: i64,
    parents: &[git2::Oid],
    head: bool,
) -> git2::Oid {
    let dir = repo.workdir().unwrap().to_path_buf();
    std::fs::write(dir.join(file), format!("// {message}\n")).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new(file)).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::new("Dev", "dev@example.com", &git2::Time::new(when, 0)).unwrap();
    let parents: Vec<git2::Commit> = parents
        .iter()
        .map(|p| repo.find_commit(*p).unwrap())
        .collect();
    let parents: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(head.then_some("HEAD"), &sig, &sig, message, &tree, &parents)
        .unwrap()
}

#[tokio::test]
async fn branch_fragments_fold_into_the_merge_that_published_them() {
    let tmp = tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&project_dir).unwrap();
    let repo = git2::Repository::init(&project_dir).unwrap();
    let base = commit_on(
        &repo,
        "filters.vb",
        "Initial photo filters",
        1_700_000_000,
        &[],
        true,
    );
    // A feature branch: work-in-progress steps a squash-era team never published alone.
    let wip1 = commit_on(
        &repo,
        "report.vb",
        "WIP photo limit",
        1_700_010_000,
        &[base],
        false,
    );
    let wip2 = commit_on(
        &repo,
        "report.vb",
        "Merge with master photo limit",
        1_700_020_000,
        &[wip1],
        false,
    );
    let merge = commit_on(
        &repo,
        "report.vb",
        "Merged PR 12: Photo filter avoids the parameter limit",
        1_700_030_000,
        &[base, wip2],
        true,
    );
    repo.reference("refs/remotes/origin/main", merge, true, "published")
        .unwrap();

    let cfg = Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);
    let indexed = text(
        &engram
            .index_project(Parameters(engram_server::IndexProjectRequest {
                directory: project_dir.to_string_lossy().to_string(),
                project_name: "fold".into(),
                project_type: engram_server::models::ProjectType::General,
                wait: true,
                dedupe_by_directory: true,
            }))
            .await
            .unwrap(),
    );
    let project_id = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            force: false,
            project_id: project_id.clone(),
            max_commits: 10,
            index_antipatterns: false,
            mode: None,
            wait: true,
        }))
        .await
        .unwrap();

    let out = text(
        &engram
            .search_history(Parameters(request(&project_id, "photo limit")))
            .await
            .unwrap(),
    );
    assert!(out.contains(&format!("commit: {merge}")), "{out}");
    assert!(
        out.contains("message: Merged PR 12: Photo filter avoids the parameter limit"),
        "{out}"
    );
    assert!(out.contains("branch commit(s) it published"), "{out}");
    for fragment in [wip1, wip2] {
        assert!(
            !out.contains(&format!("commit: {fragment}")),
            "fragment {fragment} surfaced alone: {out}"
        );
    }
}

#[tokio::test]
async fn precedent_mode_ranks_whole_published_changes_within_the_cutoff() {
    let fixture = fixture(false).await;
    let story = "As a project manager I want the photo filter to avoid the SQL parameter limit \
                 when many markers are missing photos";
    let mut req = request(&fixture.project_id, story);
    req.mode = Some("precedent".into());
    req.as_of_rev = Some(fixture.base.to_string());
    let out = text(
        &fixture
            .engram
            .search_history(Parameters(req))
            .await
            .unwrap(),
    );
    assert!(out.starts_with("Precedents:"), "{out}");
    assert!(out.contains(&format!("commit: {}", fixture.base)), "{out}");
    assert!(
        !out.contains(&format!("commit: {}", fixture.answer)),
        "{out}"
    );
    // No OpenRouter LLM in tests: the order is semantic, and the output says so.
    assert!(out.contains("rerank unavailable"), "{out}");

    // Auto picks precedent mode for story prose and text mode for a short phrase.
    let auto = text(
        &fixture
            .engram
            .search_history(Parameters(request(&fixture.project_id, story)))
            .await
            .unwrap(),
    );
    assert!(auto.starts_with("Precedents:"), "{auto}");
    let short = text(
        &fixture
            .engram
            .search_history(Parameters(request(&fixture.project_id, "parameter limit")))
            .await
            .unwrap(),
    );
    assert!(short.starts_with("History search results"), "{short}");

    let mut bad = request(&fixture.project_id, story);
    bad.mode = Some("fuzzy".into());
    assert!(
        fixture
            .engram
            .search_history(Parameters(bad))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn find_merged_work_rejects_an_unresolvable_revision() {
    let fixture = fixture(false).await;
    let req: engram_server::models::FindMergedWorkRequest =
        serde_json::from_value(serde_json::json!({
            "project_id": fixture.project_id, "story": STORY, "as_of_rev": "no-such-branch"
        }))
        .unwrap();
    assert!(fixture.engram.handle_find_merged_work(req).await.is_err());
}
