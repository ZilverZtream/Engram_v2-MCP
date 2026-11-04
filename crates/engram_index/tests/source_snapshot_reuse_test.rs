#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, RelPath};
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc};
use std::collections::BTreeSet;
use tokio_util::sync::CancellationToken;

fn document(path: &str, generation: u64, id: &str, text: &str) -> IndexDoc {
    IndexDoc { generation, chunk_id: generation, path: RelPath::new(path), language: "rust".into(),
        content: text.into(), namespace: "memory".into(), author: None, timestamp: None,
        start_line: 1, end_line: 1, doc_id: id.into(), content_hash: ContentHash::compute(text.as_bytes()).0 }
}

fn query(generation: u64, text: &str) -> HybridQuery {
    HybridQuery { project_id: "project".into(), namespace: "memory".into(), generation,
        text: text.into(), top_k: 20, fts_mode: "loose".into(), include_path_prefixes: None,
        exclude_path_prefixes: None, include_path_suffixes: None, language_filters: None,
        author_filter: None, date_after: None, date_before: None, use_mmr: false }
}

async fn engine(root: &std::path::Path, backend: &str) -> HybridSearchEngine {
    HybridSearchEngine::new(root.join("tantivy"), root.join("lancedb"), &Config {
        embedding_backend: backend.into(), ..Default::default()
    }).await.unwrap()
}

#[tokio::test]
async fn one_file_edit_reuses_unchanged_chunks_and_keeps_an_existing_reader_complete() {
    let temporary = tempfile::tempdir().unwrap();
    let index = engine(temporary.path(), "fts_only").await;
    let cancel = CancellationToken::new();
    index.index_docs("project", &[
        document("a.rs", 1, "old-a", "old_marker source"),
        document("b.rs", 1, "stable-b", "stable_marker source"),
        document("removed.rs", 1, "removed-c", "removed_marker source"),
    ], &cancel).await.unwrap();
    let old_reader = index.pin_source_generation("project", 1).unwrap();
    index.prepare_source_snapshot("project", 1, 2, &[RelPath::new("a.rs")], &[RelPath::new("removed.rs")], &cancel).await.unwrap();
    assert!(index.lexical_search(&query(2, "source")).is_err(), "an incomplete snapshot must be unavailable");
    assert_eq!(index.lexical_search(&query(1, "source")).unwrap().len(), 3);
    index.index_docs("project", &[document("a.rs", 2, "new-a", "new_marker source")], &cancel).await.unwrap();
    index.seal_source_snapshot("project", 2).unwrap();
    assert_eq!(index.count_docs_in_generation("project", "memory", 2).unwrap(), 2);
    assert_eq!(index.paths_in_generation("project", Some("memory"), 2).unwrap(), BTreeSet::from(["a.rs".into(), "b.rs".into()]));
    assert!(index.lexical_search(&query(2, "old_marker")).unwrap().is_empty());
    let stable = index.lexical_search(&query(2, "stable_marker")).unwrap();
    assert_eq!(stable.len(), 1);
    assert_eq!(stable[0].pk, engram_core::build_pk("project", "memory", 1, "stable-b"),
        "unchanged content must retain its original physical identity");
    assert!(index.get_doc_by_doc_id("project", "memory", 2, "stable-b").unwrap().is_some());
    index.purge_old_generations("project", 2).await.unwrap();
    assert!(index.pin_source_generation("project", 1).is_err(),
        "a fresh request must not extend a retired snapshot's lifetime");
    assert_eq!(index.lexical_search(&query(1, "source")).unwrap().len(), 3,
        "collection must preserve the complete snapshot held by an existing reader");
    assert_eq!(index.count_docs("project").unwrap(), 4);
    drop(old_reader);
    index.purge_old_generations("project", 2).await.unwrap();
    assert_eq!(index.count_docs("project").unwrap(), 2);
    assert!(index.lexical_search(&query(1, "source")).is_err());
    assert!(index.get_doc_by_doc_id("project", "memory", 2, "stable-b").unwrap().is_some());
    drop(index);
    let reopened = engine(temporary.path(), "fts_only").await;
    assert_eq!(reopened.lexical_search(&query(2, "source")).unwrap().len(), 2);
}

#[tokio::test]
async fn retry_after_partial_indexing_replaces_only_the_abandoned_attempt() {
    let temporary = tempfile::tempdir().unwrap();
    let index = engine(temporary.path(), "fts_only").await;
    let cancel = CancellationToken::new();
    index.index_docs("project", &[
        document("a.rs", 1, "original-a", "original_marker source"),
        document("b.rs", 1, "stable-b", "stable_marker source"),
    ], &cancel).await.unwrap();
    index.prepare_source_snapshot("project", 1, 2, &[RelPath::new("a.rs")], &[], &cancel).await.unwrap();
    index.index_docs("project", &[document("a.rs", 2, "abandoned-a", "abandoned_marker source")], &cancel).await.unwrap();
    assert!(index.lexical_search(&query(2, "source")).is_err());
    assert_eq!(index.lexical_search(&query(1, "source")).unwrap().len(), 2);
    drop(index);
    let restarted = engine(temporary.path(), "fts_only").await;
    restarted.prepare_source_snapshot("project", 1, 2, &[RelPath::new("b.rs")], &[], &cancel).await.unwrap();
    assert_eq!(restarted.count_docs("project").unwrap(), 2, "abandoned physical chunks must be removed before retry");
    restarted.index_docs("project", &[document("b.rs", 2, "new-b", "new_marker source")], &cancel).await.unwrap();
    restarted.seal_source_snapshot("project", 2).unwrap();
    assert!(restarted.lexical_search(&query(2, "abandoned_marker")).unwrap().is_empty());
    assert_eq!(restarted.lexical_search(&query(2, "original_marker")).unwrap().len(), 1);
    assert_eq!(restarted.lexical_search(&query(2, "new_marker")).unwrap().len(), 1);
}

#[tokio::test]
async fn revision_selection_does_not_change_equal_content_relevance() {
    let temporary = tempfile::tempdir().unwrap();
    let index = engine(temporary.path(), "fts_only").await;
    let cancel = CancellationToken::new();
    index.index_docs("project", &[
        document("a.rs", 1, "old-a", "obsolete source"),
        document("b.rs", 1, "stable-b", "sharedneedle source"),
    ], &cancel).await.unwrap();
    index.prepare_source_snapshot("project", 1, 2,
        &[RelPath::new("a.rs")], &[], &cancel).await.unwrap();
    index.index_docs("project", &[document("a.rs", 2, "new-a", "sharedneedle source")], &cancel).await.unwrap();
    index.seal_source_snapshot("project", 2).unwrap();
    let hits = index.lexical_search(&query(2, "sharedneedle")).unwrap();
    assert_eq!(hits.len(), 2);
    assert!((hits[0].score - hits[1].score).abs() < 0.000001,
        "equal content must not gain relevance merely from its physical revision: {:?}",
        hits.iter().map(|hit| (&hit.doc_id, hit.score)).collect::<Vec<_>>());
}

#[tokio::test]
async fn conservative_physical_cleanup_keeps_the_published_logical_head() {
    let temporary = tempfile::tempdir().unwrap();
    let index = engine(temporary.path(), "fts_only").await;
    let cancel = CancellationToken::new();
    index.index_docs("project", &[
        document("a.rs", 1, "old-a", "old_marker source"),
        document("b.rs", 1, "stable-b", "stable_marker source"),
    ], &cancel).await.unwrap();
    index.prepare_source_snapshot("project", 1, 2,
        &[RelPath::new("a.rs")], &[], &cancel).await.unwrap();
    index.index_docs("project", &[document("a.rs", 2, "new-a", "new_marker source")], &cancel).await.unwrap();
    index.seal_source_snapshot("project", 2).unwrap();

    index.purge_old_generations_with_cutoff("project", 2, 1).await.unwrap();
    assert_eq!(index.count_docs("project").unwrap(), 3,
        "a conservative exclusive cutoff must retain physical generation one");
    assert!(index.pin_source_generation("project", 1).is_err(),
        "an older physical cutoff must not be mistaken for the published head");
    assert_eq!(index.lexical_search(&query(2, "source")).unwrap().len(), 2);
    assert!(index.purge_old_generations_with_cutoff("project", 2, 3).await.is_err(),
        "future cutoffs must be rejected before mutating the live corpus");
    assert_eq!(index.lexical_search(&query(2, "stable_marker")).unwrap().len(), 1);
    index.purge_old_generations("project", 2).await.unwrap();
    assert_eq!(index.count_docs("project").unwrap(), 2);
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn vector_selection_and_collection_keep_inherited_revisions_and_hide_superseded_ones() {
    let temporary = tempfile::tempdir().unwrap();
    let index = engine(temporary.path(), "local").await;
    let cancel = CancellationToken::new();
    index.index_docs("project", &[
        document("a.rs", 1, "old-a", "old_marker source"),
        document("b.rs", 1, "stable-b", "stable_marker source"),
        document("removed.rs", 1, "removed-c", "removed_marker source"),
    ], &cancel).await.unwrap();
    let old = index.pin_source_generation("project", 1).unwrap();
    index.prepare_source_snapshot("project", 1, 2, &[RelPath::new("a.rs")], &[RelPath::new("removed.rs")], &cancel).await.unwrap();
    index.index_docs("project", &[document("a.rs", 2, "new-a", "new_marker source")], &cancel).await.unwrap();
    index.seal_source_snapshot("project", 2).unwrap();
    assert_eq!(index.vector_paths_in_generation("project", Some("memory"), 2).await.unwrap(), BTreeSet::from(["a.rs".into(), "b.rs".into()]));
    assert_eq!(index.count_vectors_in_generation("project", 2).await.unwrap(), 2);
    let hits = index.vector_search(&query(2, "source"), &cancel).await.unwrap();
    assert_eq!(hits.iter().map(|hit| hit.doc_id.as_str()).collect::<BTreeSet<_>>(), BTreeSet::from(["new-a", "stable-b"]));
    index.purge_old_generations("project", 2).await.unwrap();
    assert_eq!(index.vector_search(&query(1, "source"), &cancel).await.unwrap().len(), 3);
    drop(old);
    index.purge_old_generations("project", 2).await.unwrap();
    assert_eq!(index.count_vectors("project").await.unwrap(), 2);
    assert_eq!(index.vector_search(&query(2, "source"), &cancel).await.unwrap().len(), 2);
}
