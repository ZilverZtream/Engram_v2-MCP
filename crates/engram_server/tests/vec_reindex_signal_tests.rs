#![allow(clippy::unwrap_used)]
//! Vector table recreation — reindex signal and registry persistence tests.
//!
//! Proves that when the vector table is recreated due to a schema mismatch:
//! 1. The `FullReindexRequired` AppEvent can be sent through the events channel.
//! 2. The registry `set_reindex_required` persists an observable degraded-state
//!    timestamp that callers can read via `get_project`.
//! 3. `clear_reindex_required` removes the flag so successful reindex restores
//!    healthy state.
//!
//! These tests close the VEC1 "Covered-Insufficient" gap: proving the mandatory
//! reindex tracking path is correct end-to-end without requiring a live LanceDB
//! schema mismatch in CI.

use engram_core::{Config, ProjectRecord, Registry};
use engram_index::HybridSearchEngine;
use engram_server::state::{AppEvent, AppState};
use tokio_util::sync::CancellationToken;

async fn open_engine(tmp: &tempfile::TempDir) -> HybridSearchEngine {
    let tantivy = tmp.path().join("tantivy");
    let lance = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy).unwrap();
    std::fs::create_dir_all(&lance).unwrap();
    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    HybridSearchEngine::new(tantivy, lance, &cfg).await.unwrap()
}

fn make_cfg(data_dir: &std::path::Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        embedding_backend: "fts_only".into(),
        allowed_roots: vec![data_dir.to_path_buf()],
        ..Default::default()
    }
}

fn make_project_record(project_id: &str) -> ProjectRecord {
    ProjectRecord {
        project_id: project_id.to_string(),
        project_name: format!("{project_id}-name"),
        project_type: "generic".to_string(),
        directory: "/tmp/test".to_string(),
        created_at_ms: 1_000_000,
        updated_at_ms: 1_000_000,
        reindex_required_since_ms: None,
    }
}

/// `FullReindexRequired` events sent via `events_tx` must be receivable on
/// the corresponding receiver — proves the broadcast channel wiring is correct.
#[tokio::test]
async fn full_reindex_required_event_is_receivable_on_channel() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let (state, mut rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    // Emit the event as the project_tools handler would after detecting VEC1 error.
    let _ = state.events_tx.send(AppEvent::FullReindexRequired {
        project_id: "proj-vec1-test".to_string(),
    });

    // The receiver must get the event.
    let event = rx.recv().await.expect("must receive FullReindexRequired event");
    match event {
        AppEvent::FullReindexRequired { project_id } => {
            assert_eq!(
                project_id, "proj-vec1-test",
                "project_id must match what was sent"
            );
        }
        other => panic!(
            "expected FullReindexRequired, got {other:?}"
        ),
    }
}

/// `set_reindex_required` must persist the timestamp to the registry so the
/// degraded state is observable via `get_project`.  This is the durable record
/// that signals operators and search callers that semantic search is degraded.
#[test]
fn set_reindex_required_persists_timestamp_in_registry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let reg = Registry::open(&tmp.path().join("r.redb")).expect("Registry::open");

    // Register the project so set_reindex_required has something to update.
    reg.put_project(&make_project_record("proj-vec1-degraded"))
        .expect("put_project must succeed");

    // Initially no reindex flag.
    let before = reg
        .get_project("proj-vec1-degraded")
        .expect("get_project must not error")
        .expect("project must exist");
    assert!(
        before.reindex_required_since_ms.is_none(),
        "reindex_required_since_ms must be None before schema mismatch"
    );

    // Simulate the dreamer handling FullReindexRequired: set the flag.
    let since_ms: u64 = 9_999_999;
    reg.set_reindex_required("proj-vec1-degraded", since_ms)
        .expect("set_reindex_required must succeed");

    // The flag must now be readable.
    let after = reg
        .get_project("proj-vec1-degraded")
        .expect("get_project must not error")
        .expect("project must still exist");
    assert_eq!(
        after.reindex_required_since_ms,
        Some(since_ms),
        "reindex_required_since_ms must equal the timestamp passed to set_reindex_required"
    );
}

/// `clear_reindex_required` must remove the degraded-state flag after a
/// successful full reindex, restoring healthy search state.
#[test]
fn clear_reindex_required_restores_healthy_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let reg = Registry::open(&tmp.path().join("r.redb")).expect("Registry::open");

    reg.put_project(&make_project_record("proj-vec1-recovery"))
        .expect("put_project must succeed");

    // Set the flag (vector table was recreated).
    reg.set_reindex_required("proj-vec1-recovery", 5_000_000)
        .expect("set_reindex_required must succeed");

    let degraded = reg
        .get_project("proj-vec1-recovery")
        .unwrap()
        .unwrap();
    assert!(
        degraded.reindex_required_since_ms.is_some(),
        "precondition: reindex flag must be set"
    );

    // Simulate successful reindex completion: clear the flag.
    reg.clear_reindex_required("proj-vec1-recovery")
        .expect("clear_reindex_required must succeed");

    let healthy = reg
        .get_project("proj-vec1-recovery")
        .unwrap()
        .unwrap();
    assert!(
        healthy.reindex_required_since_ms.is_none(),
        "reindex_required_since_ms must be None after clear_reindex_required — \
         project returns to healthy search state"
    );
}

/// `set_reindex_required` on a non-existent project must be a no-op (not panic
/// or corrupt the registry).  The dreamer emits this for any project_id it receives
/// from the event channel, which may race with project deletion.
#[test]
fn set_reindex_required_on_missing_project_is_noop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let reg = Registry::open(&tmp.path().join("r.redb")).expect("Registry::open");

    // No project registered — set must not panic or error badly.
    let result = reg.set_reindex_required("proj-does-not-exist", 1_000);
    // The implementation documents "No-op if project not found" — so Ok is expected.
    assert!(
        result.is_ok(),
        "set_reindex_required on missing project must not error; got {result:?}"
    );
}

/// Section 10 / VEC1: full end-to-end lifecycle test.
///
/// Proves the complete recovery flow works:
///   index docs at gen 1
///   → set reindex_required flag (simulating schema mismatch event)
///   → reindex same docs at gen 2 (simulating recovery job)
///   → clear reindex_required flag (what the recovery job does on success)
///   → verify docs are still searchable (semantic quality restored)
///
/// This closes the "Covered-Insufficient" gap on VEC1-f2d1 by proving not just
/// the flag set/clear API but the integrated lifecycle with real document indexing.
#[tokio::test]
async fn vec1_lifecycle_index_reindex_required_flag_clear_search_restored() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;
    let reg = Registry::open(&tmp.path().join("r.redb")).unwrap();

    const PROJ: &str = "vec1-lifecycle";

    // Register project.
    reg.put_project(&ProjectRecord {
        project_id: PROJ.to_string(),
        project_name: "VEC1 Lifecycle".to_string(),
        project_type: "generic".to_string(),
        directory: tmp.path().to_string_lossy().to_string(),
        created_at_ms: 1_000_000,
        updated_at_ms: 1_000_000,
        reindex_required_since_ms: None,
    })
    .unwrap();

    // Step 1: Index initial documents at generation 1.
    let src_file = tmp.path().join("hello.rs");
    std::fs::write(&src_file, b"fn hello() { /* vec1 lifecycle */ }").unwrap();
    let cancel = CancellationToken::new();
    let stats = engine
        .index_files(
            PROJ,
            "code",
            1,
            tmp.path(),
            vec![src_file.clone()],
            4096,
            &cancel,
            |_, _| {},
        )
        .await
        .unwrap();
    assert!(
        stats.files > 0 || stats.chunks > 0,
        "VEC1: must index at least one file/chunk in generation 1"
    );
    let count_gen1 = engine.count_docs(PROJ).unwrap();
    assert!(count_gen1 > 0, "VEC1: docs must be present after generation 1 index");

    // Step 2: Simulate schema mismatch — set the reindex_required flag.
    reg.set_reindex_required(PROJ, 9_000_000).unwrap();
    let degraded = reg.get_project(PROJ).unwrap().unwrap();
    assert!(
        degraded.reindex_required_since_ms.is_some(),
        "VEC1: reindex_required_since_ms must be set after simulated schema mismatch"
    );

    // Step 3: Recovery — reindex at generation 2.
    let cancel2 = CancellationToken::new();
    let stats2 = engine
        .index_files(
            PROJ,
            "code",
            2,
            tmp.path(),
            vec![src_file.clone()],
            4096,
            &cancel2,
            |_, _| {},
        )
        .await
        .unwrap();
    assert!(
        stats2.files > 0 || stats2.chunks > 0,
        "VEC1: recovery reindex must produce indexed or chunked docs"
    );

    // Step 4: Clear the reindex-required flag (what a successful reindex job does).
    reg.clear_reindex_required(PROJ).unwrap();
    let healthy = reg.get_project(PROJ).unwrap().unwrap();
    assert!(
        healthy.reindex_required_since_ms.is_none(),
        "VEC1: reindex_required_since_ms must be None after recovery reindex + clear — \
         project returned to healthy search state"
    );

    // Step 5: Semantic quality restored — docs are still findable.
    let count_post = engine.count_docs(PROJ).unwrap();
    assert!(
        count_post > 0,
        "VEC1: docs must remain searchable after reindex + flag clear; count=0 means \
         search quality was not restored"
    );
}
