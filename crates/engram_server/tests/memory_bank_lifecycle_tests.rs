#![allow(clippy::unwrap_used)]
//! Deleting a memory must actually forget it.
//!
//! `update_memory_bank` writes two places: the registry (source of truth for
//! list/read) and the search index (namespace `memory_bank`, generation 0,
//! retention KeepForever — deliberately reindex-proof). `delete_memory_bank`
//! deleted only the registry row. The indexed doc — and its vector — stayed
//! forever, so a deleted memory kept surfacing in search_memory with no
//! registry entry behind it: unreadable, unlistable, undeletable, but still
//! recalled. For a memory tool, forgetting that does not forget is a
//! correctness bug, not a hygiene issue.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

async fn setup() -> (tempfile::TempDir, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn noop() {}\n").unwrap();

    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(256 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "MemLifecycle".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

async fn search_memory_bank(engram: &engram_server::Engram, pid: &str, query: &str) -> String {
    let res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: pid.to_string(),
            query: query.to_string(),
            namespace: "memory_bank".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn deleted_memory_stops_surfacing_in_search() {
    let (_tmp, engram, pid) = setup().await;

    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: pid.clone(),
            section_id: Some("deploy-gotcha".into()),
            section: "Deploy gotcha".into(),
            content: "The zorblatt registry must be restarted after every deploy.".into(),
        }))
        .await
        .unwrap();

    // Precondition: the memory is recalled while it exists.
    let before = search_memory_bank(&engram, &pid, "zorblatt registry restart").await;
    assert!(
        before.contains("memory_bank:deploy-gotcha"),
        "a live memory must surface in search; got:\n{before}"
    );

    engram
        .delete_memory_bank(Parameters(engram_server::MemorySectionRequest {
            project_id: pid.clone(),
            section: "deploy-gotcha".into(),
        }))
        .await
        .unwrap();

    let after = search_memory_bank(&engram, &pid, "zorblatt registry restart").await;
    assert!(
        !after.contains("memory_bank:deploy-gotcha"),
        "a deleted memory must STOP surfacing in search — forgetting that \
         does not forget is a ghost memory; got:\n{after}"
    );
}

/// Deleting one section must not take neighbours with it.
#[tokio::test]
async fn deleting_one_section_leaves_the_others() {
    let (_tmp, engram, pid) = setup().await;

    for (id, content) in [
        ("keep-me", "The flarnwick cache warms on first request."),
        ("drop-me", "The flarnwick cache is disabled on Tuesdays."),
    ] {
        engram
            .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
                project_id: pid.clone(),
                section_id: Some(id.into()),
                section: id.into(),
                content: content.into(),
            }))
            .await
            .unwrap();
    }

    engram
        .delete_memory_bank(Parameters(engram_server::MemorySectionRequest {
            project_id: pid.clone(),
            section: "drop-me".into(),
        }))
        .await
        .unwrap();

    let out = search_memory_bank(&engram, &pid, "flarnwick cache").await;
    assert!(
        out.contains("memory_bank:keep-me"),
        "the surviving section must still be recalled; got:\n{out}"
    );
    assert!(
        !out.contains("memory_bank:drop-me"),
        "the deleted section must be gone; got:\n{out}"
    );
}
