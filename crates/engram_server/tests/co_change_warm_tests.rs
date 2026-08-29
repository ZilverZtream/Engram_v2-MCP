#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29, row 9 / P0-3 latency: get_change_set's co-change
//! arm and find_similar_changes still WALK GIT at call time (11.7 s live);
//! the snapshot they share (`data_dir/co_change/<pid>.bin`, incremental by
//! walked commit) is only built by the first caller. Indexing must leave the
//! snapshot warm so call time only reads it.

use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

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
    for args in [
        vec!["init", "-q"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
    }
    // a second commit so the walk has history
    std::fs::write(root.join("Site/App_Code/mod0.vb"), "Public Class mod0\n    Public Function Get0() As String\n        Return \"y\"\n    End Function\nEnd Class\n").unwrap();
    for args in [
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "second",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "CoChangeWarmFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexing_leaves_the_co_change_snapshot_warm() {
    let (_tmp, state, _engram, pid) = build().await;
    let snapshot = state
        .cfg
        .data_dir
        .join("co_change")
        .join(format!("{pid}.bin"));
    // The index job may warm the snapshot asynchronously; give it a moment.
    for _ in 0..50 {
        if snapshot.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        snapshot.exists(),
        "index_project must leave the co-change snapshot warm at {} so get_change_set / find_similar_changes never walk git at call time",
        snapshot.display()
    );
    assert!(
        state.co_change_cache.get(&pid).is_some(),
        "the in-memory co-change cache is populated by indexing, not by the first caller"
    );
}
