#![allow(clippy::unwrap_used)]
//! D4/NS1 — GlobalMutable concurrent writer stress test.
//!
//! Proves that N concurrent `index_docs` calls for the same pk (doc_id) in a
//! GlobalMutable namespace (memory_bank / insights) result in exactly one row
//! per pk with no data corruption or duplicate rows.

use engram_core::{Config, RelPath};
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn open_engine(tmp: &tempfile::TempDir) -> HybridSearchEngine {
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).unwrap();
    std::fs::create_dir_all(&lance_dir).unwrap();
    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    HybridSearchEngine::new(tantivy_dir, lance_dir, &cfg)
        .await
        .unwrap()
}

/// D4/NS1: N concurrent `index_docs` calls for the same doc_id in a
/// GlobalMutable namespace must produce exactly one hit after all writers
/// complete. No panics or errors are permitted during concurrent writes.
#[tokio::test]
async fn global_mutable_concurrent_writes_last_write_wins() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = Arc::new(open_engine(&tmp).await);

    let project_id = "ns1-concurrent-proj";
    let namespace = "memory_bank"; // GlobalMutable
    let doc_id = "shared-key-001";
    let n_writers = 8;

    // Spawn N concurrent writers for the same doc_id.
    let mut handles = Vec::new();
    for i in 0..n_writers {
        let engine = engine.clone();
        let doc_id = doc_id.to_string();
        handles.push(tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let doc = IndexDoc {
                // GlobalMutable namespaces use generation=0 (clamped by build_pk).
                generation: 0,
                chunk_id: 0,
                path: RelPath::new("notes/shared.md"),
                language: "markdown".into(),
                content: format!("writer {i} content"),
                namespace: namespace.to_string(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 1,
                doc_id: doc_id.clone(),
                content_hash: format!("hash-{i}"),
            };
            engine
                .index_docs(project_id, &[doc], &cancel)
                .await
        }));
    }

    // All writers must complete without errors.
    for (i, handle) in handles.into_iter().enumerate() {
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "D4/NS1: writer {i} must not error; got: {:?}",
            result.unwrap_err()
        );
    }

    // After all writers finish, exactly one row must match the doc_id.
    // Search for something present in all writer payloads: "writer" + "content".
    let hits = engine
        .search(
            &HybridQuery {
                project_id: project_id.to_string(),
                namespace: namespace.to_string(),
                generation: 0,
                text: "writer content".to_string(),
                top_k: 100,
                fts_mode: "strict".to_string(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            },
            None,
        )
        .await
        .unwrap();

    // GlobalMutable last-write-wins: there must be exactly 1 entry for the
    // shared doc_id (no duplicate rows from concurrent writers).
    let matching: Vec<_> = hits.iter().filter(|h| h.doc_id == doc_id).collect();
    assert_eq!(
        matching.len(),
        1,
        "D4/NS1: exactly 1 row must exist for doc_id '{doc_id}' after {n_writers} concurrent writers; \
         got {} — duplicate rows indicate upsert race (GlobalMutable violated)",
        matching.len()
    );
}

/// D4/NS1: Concurrent writes on distinct doc_ids must all survive (no row loss).
/// Each writer owns a unique doc_id; after all complete every doc_id must be
/// queryable.
#[tokio::test]
async fn global_mutable_concurrent_distinct_writes_all_survive() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = Arc::new(open_engine(&tmp).await);

    let project_id = "ns1-distinct-proj";
    let namespace = "insights"; // another GlobalMutable namespace
    let n_writers = 8;

    let mut handles = Vec::new();
    for i in 0..n_writers {
        let engine = engine.clone();
        handles.push(tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let doc = IndexDoc {
                generation: 0,
                chunk_id: 0,
                path: RelPath::new(&format!("insights/item-{i}.md")),
                language: "markdown".into(),
                content: format!("insight about topic-{i}"),
                namespace: namespace.to_string(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 1,
                doc_id: format!("insight-key-{i}"),
                content_hash: format!("hash-distinct-{i}"),
            };
            engine
                .index_docs(project_id, &[doc], &cancel)
                .await
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        let result = handle.await.unwrap();
        assert!(
            result.is_ok(),
            "D4/NS1: distinct-key writer {i} must not error; got: {:?}",
            result.unwrap_err()
        );
    }

    // Each unique doc_id must produce exactly one hit.
    let all_hits = engine
        .search(
            &HybridQuery {
                project_id: project_id.to_string(),
                namespace: namespace.to_string(),
                generation: 0,
                text: "insight topic".to_string(),
                top_k: 100,
                fts_mode: "loose".to_string(),
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                author_filter: None,
                date_after: None,
                date_before: None,
                use_mmr: false,
            },
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        all_hits.len(),
        n_writers,
        "D4/NS1: all {n_writers} distinct-key writes must produce {n_writers} hits; \
         got {} — some writes were lost or duplicated",
        all_hits.len()
    );
}

/// D4/NS1: Concurrent writes on the same doc_id must complete within a bounded
/// wall-clock window, proving no unbounded blocking or livelock.
///
/// Wraps the concurrent-write section in `tokio::time::timeout` with a generous
/// deadline (10 seconds for 8 concurrent writers). A deadlock or heavy contention
/// would cause the timeout to fire, surfacing the issue as a test failure rather
/// than an infinite hang in CI.
#[tokio::test]
async fn global_mutable_concurrent_writes_complete_within_deadline() {
    use std::time::Duration;

    const DEADLINE: Duration = Duration::from_secs(10);

    let tmp = tempfile::TempDir::new().unwrap();
    let engine = Arc::new(open_engine(&tmp).await);

    let project_id = "ns1-timing-proj";
    let namespace = "memory_bank";
    let doc_id = "timing-key-001";
    let n_writers = 8;

    let engine_clone = engine.clone();
    let result = tokio::time::timeout(DEADLINE, async move {
        let mut handles = Vec::new();
        for i in 0..n_writers {
            let engine = engine_clone.clone();
            let doc_id = doc_id.to_string();
            handles.push(tokio::spawn(async move {
                let cancel = CancellationToken::new();
                let doc = IndexDoc {
                    generation: 0,
                    chunk_id: 0,
                    path: RelPath::new("notes/timing.md"),
                    language: "markdown".into(),
                    content: format!("timing writer {i}"),
                    namespace: namespace.to_string(),
                    author: None,
                    timestamp: None,
                    start_line: 1,
                    end_line: 1,
                    doc_id: doc_id.clone(),
                    content_hash: format!("hash-timing-{i}"),
                };
                engine.index_docs(project_id, &[doc], &cancel).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "D4/NS1-a3p9: {n_writers} concurrent writes to the same doc_id must complete \
         within {DEADLINE:?}; timeout indicates deadlock or unbounded contention"
    );
}
