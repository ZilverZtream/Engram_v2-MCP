#![allow(clippy::unwrap_used)]
//! `search_memory` must be able to recall across knowledge namespaces.
//!
//! Knowledge lives in several namespaces — `memory_bank`, `insights`,
//! `business_logic`, `antipattern`, `wontfix_patterns`, `quality_gate` — and
//! `search_memory` queried exactly one per call, defaulting to code. An agent
//! had to already know a memory existed, and which bucket it was in, to find
//! it. The `search_scope` param lets one search span all knowledge
//! namespaces, fused by rank and labelled by source.
//!
//! `search_scope: "code"` (the default) must be byte-for-byte the old
//! behaviour, so existing callers are untouched.

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
    // Code hit: contains "widget".
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn render_widget() -> u8 {\n    // draws the widget\n    1\n}\n",
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
            project_name: "ScopeTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // memory_bank hit, via the real write path.
    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: pid.clone(),
            section_id: Some("widget-cache".into()),
            section: "Widget cache".into(),
            content: "The widget cache warms lazily on first render.".into(),
        }))
        .await
        .unwrap();

    (tmp, state, engram, pid)
}

/// Index a doc straight into a namespace (for `insights`, which has no public
/// write tool — it is produced by the dreamer).
async fn index_into_namespace(
    state: &AppState,
    pid: &str,
    namespace: &str,
    id: &str,
    content: &str,
    ts: u64,
) {
    let engine = state.get_project_cached(pid).unwrap().search;
    let doc = IndexDoc {
        generation: 0, // GlobalMutable knowledge namespaces are written at gen 0
        chunk_id: 0,
        doc_id: format!("{namespace}:{id}"),
        content_hash: format!("hash_{namespace}_{id}"),
        path: engram_core::RelPath::new(&format!("__{namespace}/{id}.md")),
        content: content.to_string(),
        language: "markdown".into(),
        namespace: namespace.to_string(),
        author: None,
        timestamp: Some(ts),
        start_line: 0,
        end_line: 0,
    };
    engine
        .index_docs(pid, &[doc], &CancellationToken::new())
        .await
        .unwrap();
}

fn req(pid: &str, scope: &str) -> engram_server::SearchMemoryRequest {
    engram_server::SearchMemoryRequest {
        project_id: pid.to_string(),
        query: "widget".into(),
        max_results: 20,
        semantic: false, // fts_only: exercise the lexical path deterministically
        search_scope: scope.to_string(),
        ..Default::default()
    }
}

async fn search(engram: &engram_server::Engram, r: engram_server::SearchMemoryRequest) -> String {
    let res = engram.search_memory(Parameters(r)).await.unwrap();
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The default scope is code-only and must not change.
#[tokio::test]
async fn default_scope_is_code_only_and_unlabelled() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "w1",
        "Insight: widget rendering clusters with layout.",
        1000,
    )
    .await;

    let out = search(&engram, req(&pid, "code")).await;
    assert!(out.contains("src/render.rs"), "code hit missing:\n{out}");
    assert!(
        !out.contains("memory_bank") && !out.contains("insights"),
        "code scope must not pull knowledge namespaces:\n{out}"
    );
    assert!(
        !out.contains("source:"),
        "code scope output must be unchanged — no source label:\n{out}"
    );
}

/// `knowledge` spans the curated namespaces and excludes code.
#[tokio::test]
async fn knowledge_scope_spans_memory_and_insights_and_labels_source() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "w1",
        "Insight: the widget cache is a hot path.",
        1000,
    )
    .await;

    let out = search(&engram, req(&pid, "knowledge")).await;
    assert!(
        out.contains("memory_bank:widget-cache"),
        "memory_bank hit missing:\n{out}"
    );
    assert!(
        out.contains("__insights/w1"),
        "insights hit missing:\n{out}"
    );
    assert!(
        !out.contains("src/render.rs"),
        "knowledge scope must not return code:\n{out}"
    );
    assert!(
        out.contains("source: memory_bank") && out.contains("source: insights"),
        "each hit must be labelled with its source namespace:\n{out}"
    );
}

/// `all` returns code and knowledge together.
#[tokio::test]
async fn all_scope_returns_code_and_knowledge() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "w1",
        "Insight: widget layout coupling.",
        1000,
    )
    .await;

    let out = search(&engram, req(&pid, "all")).await;
    assert!(out.contains("src/render.rs"), "code hit missing:\n{out}");
    assert!(
        out.contains("memory_bank:widget-cache"),
        "memory hit missing:\n{out}"
    );
    assert!(out.contains("__insights/w1"), "insight hit missing:\n{out}");
}

/// An unknown scope is rejected, not silently treated as code.
#[tokio::test]
async fn unknown_scope_is_rejected() {
    let (_t, _state, engram, pid) = setup().await;
    let err = engram
        .search_memory(Parameters(req(&pid, "everythingg")))
        .await
        .expect_err("an invalid scope must error");
    let msg = format!("{err}");
    assert!(
        msg.contains("search_scope") && msg.contains("code"),
        "the error must name the param and the valid values:\n{msg}"
    );
}

/// Date filters (already in the engine, never exposed) now reach the caller.
#[tokio::test]
async fn date_before_excludes_newer_knowledge() {
    let (_t, state, engram, pid) = setup().await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "old",
        "Insight: widget cache from the past.",
        1_000,
    )
    .await;
    index_into_namespace(
        &state,
        &pid,
        "insights",
        "new",
        "Insight: widget cache from the future.",
        9_000,
    )
    .await;

    let mut r = req(&pid, "knowledge");
    r.date_before = Some(5_000);
    let out = search(&engram, r).await;
    assert!(
        out.contains("__insights/old"),
        "the older insight must survive:\n{out}"
    );
    assert!(
        !out.contains("__insights/new"),
        "date_before must exclude the newer insight:\n{out}"
    );
}
