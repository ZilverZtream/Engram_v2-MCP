#![allow(clippy::unwrap_used)]
//! Agent memory must surface without being asked for by name.
//!
//! The dreamer writes insight nodes hourly and update_memory_bank persists
//! notes, but the orientation composites ignored both: get_codebase_overview
//! printed a rule *count* and nothing else, and ask_codebase blended
//! memory_bank + business_logic but not insights. So a session started blind
//! to what past sessions had learned. These tests pin the wiring that makes
//! the first thing an agent reads include the memory.

use engram_core::Config;
use engram_index::IndexDoc;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

async fn setup() -> (tempfile::TempDir, AppState, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/pipeline.rs"),
        "pub fn run_pipeline() -> u8 {\n    1\n}\n",
    )
    .unwrap();

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
            project_name: "MemComposites".into(),
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

/// get_codebase_overview must name recent memory sections and insights — the
/// session-start briefing that makes file-based agent memory work.
#[tokio::test]
async fn overview_surfaces_memory_sections_and_insights() {
    let (_t, state, engram, pid) = setup().await;

    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: pid.clone(),
            section_id: Some("deploy-note".into()),
            section: "Deploy note: restart the frobnicator".into(),
            content: "Restart the frobnicator service after every deploy.".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

    // Insight nodes live in the graph, written by the dreamer.
    state
        .graph
        .create_insight(
            &pid,
            "insight:test1",
            "run_pipeline clusters with the scheduler",
            "These two change together.",
            &[],
            None,
            None,
            0,
        )
        .unwrap();

    let out = text(
        &engram
            .get_codebase_overview(Parameters(engram_server::ProjectIdRequest {
                project_id: pid.clone(),
            }))
            .await
            .unwrap(),
    );

    assert!(
        out.contains("Deploy note: restart the frobnicator"),
        "overview must name recent memory sections:\n{out}"
    );
    assert!(
        out.contains("run_pipeline clusters with the scheduler"),
        "overview must name recent insights:\n{out}"
    );
}

/// An empty project still advertises the memory capability (0 counts), so an
/// agent learns the surface exists.
#[tokio::test]
async fn overview_shows_memory_section_even_when_empty() {
    let (_t, _state, engram, pid) = setup().await;
    let out = text(
        &engram
            .get_codebase_overview(Parameters(engram_server::ProjectIdRequest {
                project_id: pid.clone(),
            }))
            .await
            .unwrap(),
    );
    assert!(
        out.to_lowercase().contains("memory"),
        "overview must mention the memory surface even when empty:\n{out}"
    );
}

/// list_memory_bank must carry age and size, not just id + title, so an agent
/// can judge which notes are current without reading each one.
#[tokio::test]
async fn list_memory_bank_shows_age_and_size() {
    let (_t, _state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: pid.clone(),
            section_id: Some("sizing".into()),
            section: "Sizing note".into(),
            content: "This body is forty-two bytes long, precisely.".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

    let out = text(
        &engram
            .list_memory_bank(Parameters(engram_server::ProjectIdRequest {
                project_id: pid.clone(),
            }))
            .await
            .unwrap(),
    );

    assert!(out.contains("sizing"), "section id must still show:\n{out}");
    assert!(
        out.contains('B') && (out.contains("ago") || out.contains("just now")),
        "each row must carry size (bytes) and age:\n{out}"
    );
}

/// ask_codebase must blend dreamer insights, not just memory_bank +
/// business_logic.
#[tokio::test]
async fn ask_codebase_blends_insights() {
    let (_t, state, engram, pid) = setup().await;

    // Index an insight doc into the search index (what ask_codebase reads).
    let engine = state.get_project_cached(&pid).unwrap().search;
    let doc = IndexDoc {
        generation: 0,
        chunk_id: 0,
        doc_id: "insights:frobnicator".into(),
        content_hash: "hash_frob".into(),
        path: engram_core::RelPath::new("__insights/frobnicator.md"),
        content: "Insight: the frobnicator retry loop is a recurring hotspot.".into(),
        language: "markdown".into(),
        namespace: "insights".into(),
        author: None,
        timestamp: Some(1_000),
        start_line: 0,
        end_line: 0,
    };
    engine
        .index_docs(&pid, &[doc], &CancellationToken::new())
        .await
        .unwrap();

    let out = text(
        &engram
            .ask_codebase(Parameters(engram_server::AskCodebaseRequest {
                project_id: pid.clone(),
                question: "frobnicator".into(),
            }))
            .await
            .unwrap(),
    );

    assert!(
        out.contains("insights"),
        "ask_codebase must surface the insights namespace in its team-knowledge block:\n{out}"
    );
}
