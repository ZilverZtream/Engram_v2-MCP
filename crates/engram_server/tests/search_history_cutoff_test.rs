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

#[tokio::test]
async fn search_history_is_crash_free_and_leak_free_on_story_text() {
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

    // Raw story prose: backticks, NOT/IN/OR, field-like colons, brackets, quotes.
    let story = "[Chore] Photo filter must NOT hit the `parameter` limit IN reports OR maps. Note: \"missing\" photos!";
    let all = text(
        &engram
            .search_history(Parameters(request(&project_id, story)))
            .await
            .unwrap(),
    );
    assert!(
        all.contains(&answer.to_string()),
        "unfiltered search finds the answer commit: {all}"
    );
    assert!(all.contains(&base.to_string()), "{all}");
    // Commit identity, subject line and a readable date — not a bare epoch.
    assert!(
        all.contains("message: Photo filter avoids the parameter limit"),
        "{all}"
    );
    assert!(all.contains("date: 2023-11-15"), "{all}");
    assert!(all.contains("file: report.vb"), "{all}");

    // date_before at the answer commit's own instant excludes it (exclusive).
    let mut dated = request(&project_id, story);
    dated.date_before = Some(1_700_086_400);
    let dated = text(&engram.search_history(Parameters(dated)).await.unwrap());
    assert!(!dated.contains(&answer.to_string()), "{dated}");
    assert!(dated.contains(&base.to_string()), "{dated}");

    // as_of_rev keeps only commits reachable from the revision.
    let mut as_of = request(&project_id, story);
    as_of.as_of_rev = Some(base.to_string());
    let as_of = text(&engram.search_history(Parameters(as_of)).await.unwrap());
    assert!(!as_of.contains(&answer.to_string()), "{as_of}");
    assert!(as_of.contains(&base.to_string()), "{as_of}");

    // An unresolvable revision is an error, never a silently unfiltered search.
    let mut bad = request(&project_id, story);
    bad.as_of_rev = Some("no-such-branch".into());
    assert!(engram.search_history(Parameters(bad)).await.is_err());
}
