#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P1-2): "no call-time Git walk" was
//! false — index/update warmed 500 commits while get_change_set requested
//! 800, so a later call could diff commits 501–800, and the snapshot builder
//! walked commit oids on every invocation. The warm depth IS the request
//! depth, and a snapshot that already covers HEAD at that depth is served
//! without a git walk.

use engram_core::config::Config;
use engram_server::handlers::planning_tools::CO_CHANGE_DEPTH;
use engram_server::models::FindSimilarChangesRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

fn git(root: &std::path::Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success(),
        "git {args:?}"
    );
}

async fn build() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    for i in 0..4 {
        std::fs::write(
            root.join(format!("Site/App_Code/mod{i}.vb")),
            format!("Public Class mod{i}\n    Public Function Get{i}() As String\n        Return \"x\"\n    End Function\nEnd Class\n"),
        )
        .unwrap();
    }
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    std::fs::write(
        root.join("Site/App_Code/mod0.vb"),
        "Public Class mod0\n    Public Function Get0() As String\n        Return \"y\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/mod1.vb"),
        "Public Class mod1\n    Public Function Get1() As String\n        Return \"y\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    git(&root, &["add", "-A"]);
    git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "second",
        ],
    );
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1 << 20),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "CoChangeDepth".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    for _ in 0..100 {
        if state.co_change_cache.get(&pid).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    (tmp, state, engram, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexing_warms_the_co_change_snapshot_to_the_request_depth() {
    let (_tmp, state, _engram, pid) = build().await;
    let snap = state
        .co_change_cache
        .get(&pid)
        .map(|e| e.value().clone())
        .expect("indexing leaves the co-change snapshot warm");
    assert!(
        snap.walked >= CO_CHANGE_DEPTH,
        "the warm depth ({}) must be the depth get_change_set requests ({CO_CHANGE_DEPTH}); otherwise a later call diffs the tail at call time",
        snap.walked
    );
    assert!(
        CO_CHANGE_DEPTH >= 800,
        "the request depth is at least the 800 the planner used"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_snapshot_is_served_without_a_git_walk() {
    let (_tmp, _state, engram, pid) = build().await;
    let req = FindSimilarChangesRequest {
        project_id: pid.clone(),
        files: vec!["Site/App_Code/mod0.vb".into()],
        max_commits: CO_CHANGE_DEPTH,
        top: 5,
    };
    let res = engram.handle_find_similar_changes(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    assert!(
        t.contains("co-change snapshot: warm (served without a git walk"),
        "a snapshot covering HEAD at the request depth is served as-is:\n{t}"
    );
    assert!(
        !t.contains("fresh diffs: 1") && !t.contains("fresh diffs: 2"),
        "nothing was diffed at call time:\n{t}"
    );
}
