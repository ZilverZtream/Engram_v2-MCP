#![allow(clippy::unwrap_used)]
//! Listing docs for one namespace must not materialise the whole corpus.
//!
//! `get_change_set` and `ingest_merged_prs` both wanted the `history`
//! namespace's `pr:*` docs. Both called `list_docs_for_project`, which pages
//! through EVERY doc in the project and reads each one's stored fields, then
//! filtered in Rust — so a project with a large code corpus paid for all of
//! it to find a handful of PR records.

use engram_core::{RelPath, namespaces};
use engram_index::{HybridSearchEngine, IndexDoc};
use tokio_util::sync::CancellationToken;

fn doc(ns: &str, path: &str, n: usize) -> IndexDoc {
    IndexDoc {
        generation: 1,
        chunk_id: n as u64,
        doc_id: format!("doc_{ns}_{n}"),
        content_hash: format!("hash_{ns}_{n}"),
        path: RelPath::new(path),
        language: "text".into(),
        content: format!("content {n}"),
        namespace: ns.to_string(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
    }
}

#[tokio::test]
async fn scoped_listing_returns_only_the_requested_namespace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = engram_core::Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let engine = HybridSearchEngine::new(tmp.path().join("t"), tmp.path().join("l"), &cfg)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let pid = "scoped-test";

    let mut memory: Vec<IndexDoc> = (0..50)
        .map(|i| doc(namespaces::NAMESPACE_MEMORY, &format!("src/f{i}.rs"), i))
        .collect();
    memory.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
    engine.index_docs(pid, &memory, &cancel).await.unwrap();

    let history: Vec<IndexDoc> = (0..3)
        .map(|i| doc(namespaces::NAMESPACE_HISTORY, &format!("pr:{i}"), 100 + i))
        .collect();
    engine.index_docs(pid, &history, &cancel).await.unwrap();

    let scoped = engine
        .list_docs_in_namespace(pid, namespaces::NAMESPACE_HISTORY)
        .unwrap();

    assert_eq!(
        scoped.len(),
        3,
        "must return exactly the history docs, got {:?}",
        scoped.iter().map(|d| &d.path).collect::<Vec<_>>()
    );
    assert!(
        scoped
            .iter()
            .all(|d| d.namespace == namespaces::NAMESPACE_HISTORY),
        "no other namespace may leak in"
    );
    assert!(
        scoped.iter().all(|d| d.path.starts_with("pr:")),
        "paths must survive intact"
    );

    // The unscoped listing still sees everything — the scoped one is a
    // narrowing, not a replacement.
    assert_eq!(engine.list_docs_for_project(pid).unwrap().len(), 53);
}

/// A namespace with nothing in it returns empty rather than erroring.
#[tokio::test]
async fn scoped_listing_of_an_empty_namespace_is_empty() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = engram_core::Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let engine = HybridSearchEngine::new(tmp.path().join("t"), tmp.path().join("l"), &cfg)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    engine
        .index_docs(
            "p",
            &[doc(namespaces::NAMESPACE_MEMORY, "src/a.rs", 1)],
            &cancel,
        )
        .await
        .unwrap();

    assert!(
        engine
            .list_docs_in_namespace("p", namespaces::NAMESPACE_HISTORY)
            .unwrap()
            .is_empty()
    );
}
