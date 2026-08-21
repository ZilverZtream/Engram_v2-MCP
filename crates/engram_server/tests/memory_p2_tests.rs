#![allow(clippy::unwrap_used)]
//! P2: lifecycle — recency in recall, staleness, dup-detection, portability.

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
            project_name: "MemP2".into(),
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

async fn index_insight(state: &AppState, pid: &str, id: &str, content: &str, ts: u64) {
    let engine = state.get_project_cached(pid).unwrap().search;
    let doc = IndexDoc {
        generation: 0,
        chunk_id: 0,
        doc_id: format!("insights:{id}"),
        content_hash: format!("h_{id}"),
        path: engram_core::RelPath::new(&format!("__insights/{id}.md")),
        content: content.to_string(),
        language: "markdown".into(),
        namespace: "insights".into(),
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

fn write(pid: &str, id: &str, content: &str) -> engram_server::UpdateMemoryBankRequest {
    engram_server::UpdateMemoryBankRequest {
        project_id: pid.to_string(),
        section_id: Some(id.to_string()),
        section: format!("title {id}"),
        content: content.to_string(),
        ..Default::default()
    }
}

async fn knowledge_search(engram: &engram_server::Engram, pid: &str, q: &str) -> String {
    let res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: pid.to_string(),
            query: q.to_string(),
            max_results: 20,
            semantic: false,
            search_scope: "knowledge".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    text(&res)
}

/// A fresher knowledge hit outranks an equally-relevant older one.
#[tokio::test]
async fn recency_lifts_a_fresher_knowledge_hit() {
    let (_t, state, engram, pid) = setup().await;
    let now = 1_000_000_000_000u64;
    // Same term, same length — only recency differs.
    index_insight(
        &state,
        &pid,
        "old",
        "widget cache note alpha alpha",
        now - 400 * 86_400_000,
    )
    .await;
    index_insight(&state, &pid, "new", "widget cache note beta beta", now).await;

    let out = knowledge_search(&engram, &pid, "widget cache note").await;
    let pos_new = out.find("__insights/new").expect("new hit present");
    let pos_old = out.find("__insights/old").expect("old hit present");
    assert!(
        pos_new < pos_old,
        "the fresher insight must rank above the older one:\n{out}"
    );
}

/// A section past its review date is flagged STALE in the listing.
#[tokio::test]
async fn review_overdue_is_flagged_stale() {
    let (_t, _state, engram, pid) = setup().await;
    let mut r = write(&pid, "old-decision", "We chose approach X.");
    r.review_after_ms = Some(1); // in the distant past
    engram.update_memory_bank(Parameters(r)).await.unwrap();

    let out = text(
        &engram
            .list_memory_bank(Parameters(engram_server::ProjectIdRequest {
                project_id: pid.clone(),
            }))
            .await
            .unwrap(),
    );
    assert!(
        out.contains("old-decision") && out.contains("STALE: review overdue"),
        "an overdue section must be flagged stale:\n{out}"
    );
}

/// Writing a section nearly identical to an existing one warns about the
/// duplicate.
#[tokio::test]
async fn near_duplicate_write_is_flagged() {
    let (_t, _state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(
            &pid,
            "original",
            "The frobnicator service must be restarted after every deploy to clear caches.",
        )))
        .await
        .unwrap();

    let res = engram
        .update_memory_bank(Parameters(write(
            &pid,
            "duplicate",
            "The frobnicator service must be restarted after every deploy to clear caches.",
        )))
        .await
        .unwrap();
    let msg = text(&res);
    assert!(
        msg.contains("similar to existing section") && msg.contains("original"),
        "a near-duplicate write must warn and name the existing section:\n{msg}"
    );
}

/// A distinct section does not trigger the duplicate warning.
#[tokio::test]
async fn distinct_write_is_not_flagged() {
    let (_t, _state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(
            &pid,
            "a",
            "Deploy notes about the shipping module.",
        )))
        .await
        .unwrap();
    let res = engram
        .update_memory_bank(Parameters(write(
            &pid,
            "b",
            "Authentication uses session tokens with a sliding expiry window.",
        )))
        .await
        .unwrap();
    assert!(
        !text(&res).contains("similar to existing"),
        "an unrelated section must not be flagged as a duplicate"
    );
}

/// A memory section survives an export → import round-trip with its identity.
#[tokio::test]
async fn export_import_round_trip_preserves_the_record() {
    let (_t, state, engram, pid) = setup().await;
    let mut r = write(&pid, "portable", "Body that must survive the round trip.");
    r.kind = Some("decision".into());
    r.author = Some("session-9".into());
    r.tags = Some(vec!["arch".into()]);
    r.related_files = Some(vec!["src/lib.rs".into()]);
    engram.update_memory_bank(Parameters(r)).await.unwrap();

    let original = state
        .registry
        .get_memory_section(&pid, "portable")
        .unwrap()
        .unwrap();
    let markdown = engram_server::services::memory_portability::to_markdown(&original);

    // Import as a copy into the same (registered) project — the code path is
    // identical for restore-into-reindexed-project and cross-project copy.
    engram
        .import_memory_bank(Parameters(engram_server::ImportMemoryBankRequest {
            project_id: pid.clone(),
            markdown,
            section_id: Some("portable-copy".into()),
        }))
        .await
        .unwrap();

    let restored = state
        .registry
        .get_memory_section(&pid, "portable-copy")
        .unwrap()
        .unwrap();
    assert_eq!(restored.title, original.title);
    assert_eq!(restored.content, original.content);
    assert_eq!(restored.kind.as_deref(), Some("decision"));
    assert_eq!(restored.author.as_deref(), Some("session-9"));
    assert_eq!(restored.tags, vec!["arch".to_string()]);
    assert_eq!(restored.related_files, vec!["src/lib.rs".to_string()]);
    assert_eq!(
        restored.created_at_ms, original.created_at_ms,
        "created_at must be preserved through import"
    );

    // And the restored section is searchable.
    let out = knowledge_search(&engram, &pid, "round trip").await;
    assert!(
        out.contains("memory_bank:portable-copy"),
        "the restored section must be indexed for search:\n{out}"
    );
}
