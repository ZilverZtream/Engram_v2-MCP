#![allow(clippy::unwrap_used)]
//! P3: user-level memory that spans every project.
//!
//! Standing preferences and knowledge ("eval agents are Sonnet-only") belong
//! to the user, not one repo. They live in the reserved `__user__` project and
//! are folded into every project's knowledge recall, tagged `user:`.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

const USER: &str = "__user__";

async fn setup() -> (tempfile::TempDir, AppState, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn noop() {}\n").unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(512 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "MemP3".into(),
            project_type: engram_server::models::ProjectType::General,
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

fn text(res: &rmcp::model::CallToolResult) -> String {
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn write(pid: &str, id: &str, content: &str) -> engram_server::UpdateMemoryBankRequest {
    engram_server::UpdateMemoryBankRequest {
        project_id: pid.to_string(),
        section_id: Some(id.to_string()),
        section: format!("title {id}"),
        content: content.to_string(),
        ..Default::default()
    }
}

async fn knowledge_search(
    engram: &engram_server::Engram,
    pid: &str,
    q: &str,
    include_user: bool,
) -> String {
    let res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: pid.to_string(),
            query: q.to_string(),
            max_results: 20,
            semantic: false,
            search_scope: "knowledge".into(),
            include_user_memory: include_user,
            ..Default::default()
        }))
        .await
        .unwrap();
    text(&res)
}

/// Writing to `__user__` lazily creates the reserved project — no
/// index_project needed.
#[tokio::test]
async fn user_memory_project_is_created_lazily() {
    let (_t, state, engram, _pid) = setup().await;
    assert!(
        state.registry.get_project(USER).unwrap().is_none(),
        "the user project should not exist until first use"
    );

    engram
        .update_memory_bank(Parameters(write(
            USER,
            "sonnet-only",
            "Eval agents must be Sonnet, always.",
        )))
        .await
        .unwrap();

    assert!(
        state.registry.get_project(USER).unwrap().is_some(),
        "writing user memory must create the reserved project"
    );
}

/// A user memory surfaces in an unrelated project's knowledge recall, tagged
/// as user-level.
#[tokio::test]
async fn user_memory_surfaces_in_every_project() {
    let (_t, _state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(
            USER,
            "sonnet-only",
            "Eval agents must always run on Sonnet, never a session model.",
        )))
        .await
        .unwrap();

    let out = knowledge_search(&engram, &pid, "agents", true).await;
    assert!(
        out.contains("memory_bank:sonnet-only"),
        "user memory must appear in a project's knowledge recall:\n{out}"
    );
    assert!(
        out.contains("source: user:memory_bank"),
        "a user hit must be labelled as user-level:\n{out}"
    );
}

/// include_user_memory=false keeps user memory out.
#[tokio::test]
async fn user_memory_can_be_excluded() {
    let (_t, _state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(
            USER,
            "sonnet-only",
            "Eval agents must always run on Sonnet.",
        )))
        .await
        .unwrap();

    let out = knowledge_search(&engram, &pid, "agents", false).await;
    assert!(
        !out.contains("sonnet-only"),
        "with include_user_memory=false, user memory must not appear:\n{out}"
    );
}

/// A project's own knowledge and user memory both surface, distinctly.
#[tokio::test]
async fn project_and_user_memory_coexist() {
    let (_t, _state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(
            &pid,
            "local-note",
            "This project deploys the widget on Fridays.",
        )))
        .await
        .unwrap();
    engram
        .update_memory_bank(Parameters(write(
            USER,
            "widget-pref",
            "I always want widget code reviewed twice.",
        )))
        .await
        .unwrap();

    let out = knowledge_search(&engram, &pid, "widget", true).await;
    assert!(
        out.contains("memory_bank:local-note"),
        "the project's own note must surface:\n{out}"
    );
    assert!(
        out.contains("memory_bank:widget-pref") && out.contains("source: user:memory_bank"),
        "the user note must surface, labelled user-level:\n{out}"
    );
}
