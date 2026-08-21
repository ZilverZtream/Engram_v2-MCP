#![allow(clippy::unwrap_used)]
//! P1: the memory record carries judgement, and writes don't clobber.
//!
//! A MemorySection used to be {id, title, content, updated_at_ms} — no
//! provenance, no age, no kind, and whole-section last-write-wins. Two
//! sessions updating one note silently lost a side's work. This pins the
//! enriched record, append mode, optimistic-concurrency, kind validation,
//! and chunking of long bodies.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

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
            project_name: "MemP1".into(),
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

fn write(pid: &str, id: &str, content: &str) -> engram_server::UpdateMemoryBankRequest {
    engram_server::UpdateMemoryBankRequest {
        project_id: pid.to_string(),
        section_id: Some(id.to_string()),
        section: format!("title of {id}"),
        content: content.to_string(),
        ..Default::default()
    }
}

/// created_at is stamped once and preserved; updated_at advances.
#[tokio::test]
async fn created_at_is_preserved_across_updates() {
    let (_t, state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(&pid, "note", "first body")))
        .await
        .unwrap();
    let v1 = state
        .registry
        .get_memory_section(&pid, "note")
        .unwrap()
        .unwrap();
    assert!(v1.created_at_ms > 0, "created_at must be stamped");
    assert_eq!(
        v1.created_at_ms, v1.updated_at_ms,
        "on first write they match"
    );

    // Second write with a later updated_at (now_ms advances by real time is
    // not guaranteed within a test, so just assert created_at is unchanged).
    engram
        .update_memory_bank(Parameters(write(&pid, "note", "second body")))
        .await
        .unwrap();
    let v2 = state
        .registry
        .get_memory_section(&pid, "note")
        .unwrap()
        .unwrap();
    assert_eq!(
        v2.created_at_ms, v1.created_at_ms,
        "created_at must survive an update"
    );
    assert_eq!(v2.content, "second body", "default write replaces");
}

/// append concatenates instead of replacing.
#[tokio::test]
async fn append_concatenates() {
    let (_t, state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(&pid, "log", "line one")))
        .await
        .unwrap();

    let mut r = write(&pid, "log", "line two");
    r.append = true;
    engram.update_memory_bank(Parameters(r)).await.unwrap();

    let v = state
        .registry
        .get_memory_section(&pid, "log")
        .unwrap()
        .unwrap();
    assert_eq!(v.content, "line one\nline two");
}

/// A stale expected_updated_at_ms is rejected as a conflict; the section is
/// untouched.
#[tokio::test]
async fn optimistic_concurrency_rejects_a_stale_write() {
    let (_t, state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(&pid, "shared", "original")))
        .await
        .unwrap();

    let mut r = write(&pid, "shared", "clobbered");
    r.expected_updated_at_ms = Some(1); // definitely not the real version
    let err = engram
        .update_memory_bank(Parameters(r))
        .await
        .expect_err("a stale CAS write must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("conflict") && msg.contains("current="),
        "the error must name the conflict and current version: {msg}"
    );

    let v = state
        .registry
        .get_memory_section(&pid, "shared")
        .unwrap()
        .unwrap();
    assert_eq!(v.content, "original", "a rejected write must not persist");
}

/// A matching expected_updated_at_ms succeeds.
#[tokio::test]
async fn optimistic_concurrency_allows_a_current_write() {
    let (_t, state, engram, pid) = setup().await;
    engram
        .update_memory_bank(Parameters(write(&pid, "shared", "v1")))
        .await
        .unwrap();
    let cur = state
        .registry
        .get_memory_section(&pid, "shared")
        .unwrap()
        .unwrap();

    let mut r = write(&pid, "shared", "v2");
    r.expected_updated_at_ms = Some(cur.updated_at_ms);
    engram
        .update_memory_bank(Parameters(r))
        .await
        .expect("a matching CAS write must succeed");

    let v = state
        .registry
        .get_memory_section(&pid, "shared")
        .unwrap()
        .unwrap();
    assert_eq!(v.content, "v2");
}

/// kind is a controlled vocabulary — an unknown value is rejected, a known
/// one is stored.
#[tokio::test]
async fn kind_is_validated_and_stored() {
    let (_t, state, engram, pid) = setup().await;

    let mut bad = write(&pid, "k", "body");
    bad.kind = Some("miscellaneous".into());
    let err = engram
        .update_memory_bank(Parameters(bad))
        .await
        .expect_err("an unknown kind must be rejected");
    assert!(
        format!("{err}").contains("kind"),
        "the error must name the parameter"
    );

    let mut good = write(&pid, "k", "body");
    good.kind = Some("gotcha".into());
    good.author = Some("session-7".into());
    good.tags = Some(vec!["deploy".into(), "ops".into()]);
    engram.update_memory_bank(Parameters(good)).await.unwrap();

    let v = state
        .registry
        .get_memory_section(&pid, "k")
        .unwrap()
        .unwrap();
    assert_eq!(v.kind.as_deref(), Some("gotcha"));
    assert_eq!(v.author.as_deref(), Some("session-7"));
    assert_eq!(v.tags, vec!["deploy".to_string(), "ops".to_string()]);
}

/// A long body is split into multiple search docs so recall can hit any
/// paragraph, not just a single truncated chunk.
#[tokio::test]
async fn long_sections_are_chunked_for_search() {
    let (_t, state, engram, pid) = setup().await;

    // ~6 KB of distinct paragraphs — well past the 2000-char chunk budget.
    let mut body = String::new();
    for i in 0..120 {
        body.push_str(&format!(
            "Paragraph {i}: the quick brown fox jumps over the lazy dog.\n"
        ));
    }
    engram
        .update_memory_bank(Parameters(write(&pid, "big", &body)))
        .await
        .unwrap();

    let engine = state.get_project_cached(&pid).unwrap().search;
    let docs = engine
        .list_docs_in_namespace(&pid, "memory_bank")
        .unwrap()
        .into_iter()
        .filter(|d| d.path == "memory_bank:big")
        .count();
    assert!(
        docs > 1,
        "a 6 KB section must index as more than one chunk, got {docs}"
    );

    // Shrinking the section must not leave the old chunks behind.
    engram
        .update_memory_bank(Parameters(write(&pid, "big", "now tiny")))
        .await
        .unwrap();
    let docs_after = engine
        .list_docs_in_namespace(&pid, "memory_bank")
        .unwrap()
        .into_iter()
        .filter(|d| d.path == "memory_bank:big")
        .count();
    assert_eq!(docs_after, 1, "stale chunks must be cleared on re-write");
}

/// related_files round-trips and read_memory_bank returns a provenance header.
#[tokio::test]
async fn related_files_and_read_header() {
    let (_t, _state, engram, pid) = setup().await;

    let mut r = write(&pid, "wiring", "The dispatch arm is in hybrid.rs.");
    r.kind = Some("gotcha".into());
    r.author = Some("session-3".into());
    r.related_files = Some(vec!["src/hybrid.rs".into()]);
    engram.update_memory_bank(Parameters(r)).await.unwrap();

    let res = engram
        .read_memory_bank(Parameters(engram_server::MemorySectionRequest {
            project_id: pid.clone(),
            section: "wiring".into(),
        }))
        .await
        .unwrap();
    let out = res
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        out.contains("kind: gotcha"),
        "header must carry kind:\n{out}"
    );
    assert!(
        out.contains("author: session-3"),
        "header must carry author:\n{out}"
    );
    assert!(
        out.contains("related_files: src/hybrid.rs"),
        "header must carry related_files:\n{out}"
    );
    assert!(
        out.contains("updated_at_ms="),
        "header must expose updated_at_ms for a safe concurrent edit:\n{out}"
    );
    assert!(
        out.contains("The dispatch arm is in hybrid.rs."),
        "the body must still be present:\n{out}"
    );
}
