#![allow(clippy::unwrap_used)]
//! Behavioral tests for HybridSearchEngine (Subsystem 5 — search/index pipeline).
//!
//! Uses `embedding_backend = "fts_only"` to avoid vector dependencies.
//! Tests call production HybridSearchEngine directly:
//!  - `new` construction
//!  - `index_docs` persistence
//!  - `count_docs` / `count_docs_by_namespace` / `count_docs_by_language`
//!  - `list_docs_for_project`
//!  - `get_doc_by_doc_id`
//!  - `lexical_search` — full FTS query path

use engram_core::Config;
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc};
use engram_core::RelPath;
use tokio_util::sync::CancellationToken;

// ── helpers ───────────────────────────────────────────────────────────────────

async fn open_engine(tmp: &tempfile::TempDir) -> HybridSearchEngine {
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).expect("create tantivy dir");
    std::fs::create_dir_all(&lance_dir).expect("create lance dir");

    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };

    HybridSearchEngine::new(tantivy_dir, lance_dir, &cfg)
        .await
        .expect("HybridSearchEngine::new must succeed with fts_only backend")
}

fn make_doc(doc_id: &str, path: &str, namespace: &str, content: &str) -> IndexDoc {
    IndexDoc {
        generation: 1,
        chunk_id: doc_id.len() as u64,
        path: RelPath::new(path),
        language: "rust".to_string(),
        content: content.to_string(),
        namespace: namespace.to_string(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 10,
        doc_id: doc_id.to_string(),
        content_hash: format!("hash_{doc_id}"),
    }
}

fn fts_query(project_id: &str, namespace: &str, text: &str) -> HybridQuery {
    HybridQuery {
        project_id: project_id.to_string(),
        namespace: namespace.to_string(),
        generation: 1,
        text: text.to_string(),
        top_k: 10,
        fts_mode: "loose".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    }
}

// ── construction ──────────────────────────────────────────────────────────────

/// HybridSearchEngine::new must succeed with fts_only backend on a fresh path.
#[tokio::test]
async fn hybrid_engine_new_fts_only_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).expect("mkdir tantivy");
    std::fs::create_dir_all(&lance_dir).expect("mkdir lance");

    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let result = HybridSearchEngine::new(tantivy_dir, lance_dir, &cfg).await;
    assert!(
        result.is_ok(),
        "HybridSearchEngine::new with fts_only must succeed; got: {:?}",
        result.err()
    );
}

// ── count_docs ────────────────────────────────────────────────────────────────

/// count_docs must return 0 before any docs are indexed.
#[tokio::test]
async fn hybrid_count_docs_zero_before_any_indexing() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;

    let count = engine.count_docs("proj-empty").expect("count_docs must not error");
    assert_eq!(count, 0, "empty project must have 0 docs");
}

/// count_docs must match the number of indexed docs.
#[tokio::test]
async fn hybrid_count_docs_matches_indexed_doc_count() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let docs = vec![
        make_doc("d1", "src/a.rs", "functions", "fn alpha() {}"),
        make_doc("d2", "src/b.rs", "functions", "fn beta() {}"),
        make_doc("d3", "src/c.rs", "functions", "fn gamma() {}"),
    ];
    engine
        .index_docs("proj-count", &docs, &cancel)
        .await
        .expect("index_docs must succeed");

    let count = engine.count_docs("proj-count").expect("count_docs");
    assert_eq!(count, 3, "count_docs must return 3 after indexing 3 docs; got {count}");
}

/// count_docs must not count docs from other projects.
#[tokio::test]
async fn hybrid_count_docs_project_isolation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    engine
        .index_docs("proj-A", &[make_doc("d1", "a.rs", "rust", "fn a() {}")], &cancel)
        .await
        .expect("index A");
    engine
        .index_docs(
            "proj-B",
            &[
                make_doc("d2", "b.rs", "rust", "fn b() {}"),
                make_doc("d3", "c.rs", "rust", "fn c() {}"),
            ],
            &cancel,
        )
        .await
        .expect("index B");

    let count_a = engine.count_docs("proj-A").expect("count A");
    let count_b = engine.count_docs("proj-B").expect("count B");

    assert_eq!(count_a, 1, "proj-A must have 1 doc; got {count_a}");
    assert_eq!(count_b, 2, "proj-B must have 2 docs; got {count_b}");
}

// ── count_docs_by_namespace ───────────────────────────────────────────────────

/// count_docs_by_namespace must break down doc counts by the production-defined
/// namespace list (memory, history, antipattern, vfs).
#[tokio::test]
async fn hybrid_count_docs_by_namespace_correct_breakdown() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    // Use namespaces that the production code actually counts
    engine
        .index_docs(
            "proj-ns",
            &[
                make_doc("m1", "a.rs", "memory", "session notes about auth"),
                make_doc("m2", "b.rs", "memory", "session notes about db"),
                make_doc("h1", "c.rs", "history", "commit: refactor auth"),
            ],
            &cancel,
        )
        .await
        .expect("index docs");

    let by_ns = engine.count_docs_by_namespace("proj-ns").expect("count_by_ns");
    let mem_count = by_ns.get("memory").copied().unwrap_or(0);
    let hist_count = by_ns.get("history").copied().unwrap_or(0);

    assert_eq!(mem_count, 2, "must count 2 'memory' docs; got {mem_count}");
    assert_eq!(hist_count, 1, "must count 1 'history' doc; got {hist_count}");
}

// ── count_docs_by_language ────────────────────────────────────────────────────

/// count_docs_by_language must report correct language breakdown.
#[tokio::test]
async fn hybrid_count_docs_by_language_correct_breakdown() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let mut doc_rs1 = make_doc("r1", "a.rs", "memory", "fn a() {}");
    doc_rs1.language = "rust".into();
    let mut doc_rs2 = make_doc("r2", "b.rs", "memory", "fn b() {}");
    doc_rs2.language = "rust".into();
    let mut doc_cs = make_doc("c1", "A.cs", "memory", "class A {}");
    doc_cs.language = "csharp".into();

    engine
        .index_docs("proj-lang", &[doc_rs1, doc_rs2, doc_cs], &cancel)
        .await
        .expect("index");

    let by_lang = engine.count_docs_by_language("proj-lang").expect("count_by_lang");
    let rust_count = by_lang.get("rust").copied().unwrap_or(0);
    let cs_count = by_lang.get("csharp").copied().unwrap_or(0);

    assert_eq!(rust_count, 2, "must count 2 rust docs; got {rust_count}");
    assert_eq!(cs_count, 1, "must count 1 csharp doc; got {cs_count}");
}

// ── list_docs_for_project ─────────────────────────────────────────────────────

/// list_docs_for_project must return one summary per indexed doc.
#[tokio::test]
async fn hybrid_list_docs_for_project_returns_all_docs() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    engine
        .index_docs(
            "proj-list",
            &[
                make_doc("doc-a", "src/mod_a.rs", "functions", "fn f() {}"),
                make_doc("doc-b", "src/mod_b.rs", "classes", "struct S {}"),
            ],
            &cancel,
        )
        .await
        .expect("index");

    let summaries = engine
        .list_docs_for_project("proj-list")
        .expect("list_docs_for_project must not error");
    assert_eq!(summaries.len(), 2, "must list 2 summaries; got {}", summaries.len());
}

/// list_docs_for_project must return empty for an empty project.
#[tokio::test]
async fn hybrid_list_docs_for_project_empty_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;

    let summaries = engine
        .list_docs_for_project("proj-empty-list")
        .expect("must not error");
    assert!(summaries.is_empty(), "empty project must yield no summaries");
}

// ── get_doc_by_doc_id ─────────────────────────────────────────────────────────

/// get_doc_by_doc_id must return the document after indexing.
#[tokio::test]
async fn hybrid_get_doc_by_doc_id_returns_indexed_doc() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let doc = make_doc("unique-doc-id", "src/handler.rs", "functions", "fn handle() {}");
    engine
        .index_docs("proj-getdoc", &[doc], &cancel)
        .await
        .expect("index");

    // get_doc_by_doc_id returns (path, language, content, start_line, end_line)
    let result = engine
        .get_doc_by_doc_id("proj-getdoc", "functions", 1, "unique-doc-id")
        .expect("get_doc_by_doc_id must not error");
    assert!(result.is_some(), "must find the indexed doc by doc_id");

    let (path, language, content, _start, _end) = result.unwrap();
    assert_eq!(path.as_str(), "src/handler.rs", "path must match");
    assert_eq!(language, "rust", "language must match");
    assert!(content.contains("handle"), "content must contain 'handle'");
}

/// get_doc_by_doc_id for unknown doc_id must return None, not Err.
#[tokio::test]
async fn hybrid_get_doc_by_doc_id_unknown_returns_none() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;

    let result = engine
        .get_doc_by_doc_id("proj-x", "functions", 1, "no-such-doc")
        .expect("must not error");
    assert!(result.is_none(), "unknown doc_id must return None");
}

// ── lexical_search ────────────────────────────────────────────────────────────

/// lexical_search must return hits containing the search term.
#[tokio::test]
async fn hybrid_lexical_search_returns_matching_docs() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    engine
        .index_docs(
            "proj-fts",
            &[
                make_doc(
                    "doc-payment",
                    "src/payment.rs",
                    "functions",
                    "fn process_payment(amount: f64) -> Result<(), Error> { unimplemented!() }",
                ),
                make_doc(
                    "doc-auth",
                    "src/auth.rs",
                    "functions",
                    "fn authenticate_user(token: &str) -> bool { false }",
                ),
            ],
            &cancel,
        )
        .await
        .expect("index docs");

    let q = fts_query("proj-fts", "functions", "payment");
    let hits = engine.lexical_search(&q).expect("lexical_search must not error");

    assert!(
        !hits.is_empty(),
        "lexical_search('payment') must return at least one hit"
    );
    let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains("payment")),
        "results must include the payment doc; got: {paths:?}"
    );
}

/// lexical_search with no matching term must return empty results, not Err.
#[tokio::test]
async fn hybrid_lexical_search_no_match_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    engine
        .index_docs(
            "proj-nomatch",
            &[make_doc("d1", "src/lib.rs", "functions", "fn alpha() {}")],
            &cancel,
        )
        .await
        .expect("index");

    let q = fts_query("proj-nomatch", "functions", "zzz_definitely_not_in_content_xyz");
    let hits = engine
        .lexical_search(&q)
        .expect("lexical_search with no match must not error");
    assert!(
        hits.is_empty(),
        "no-match query must return empty hits, not Err; got {} hits",
        hits.len()
    );
}

/// lexical_search on empty project must return empty results.
#[tokio::test]
async fn hybrid_lexical_search_empty_project_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;

    let q = fts_query("proj-empty-fts", "functions", "anything");
    let hits = engine
        .lexical_search(&q)
        .expect("lexical_search on empty project must not error");
    assert!(hits.is_empty(), "empty project must return 0 hits");
}

/// lexical_search must not return docs from a different project.
#[tokio::test]
async fn hybrid_lexical_search_project_isolation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    // index "processOrder" only in proj-B
    engine
        .index_docs(
            "proj-B-iso",
            &[make_doc(
                "d1",
                "order.rs",
                "functions",
                "fn processOrder(id: u64) {}",
            )],
            &cancel,
        )
        .await
        .expect("index proj-B");

    // search proj-A — must not see proj-B's doc
    let q = fts_query("proj-A-iso", "functions", "processOrder");
    let hits = engine.lexical_search(&q).expect("search proj-A");
    assert!(
        hits.is_empty(),
        "proj-A must not see proj-B's documents; got {} hits",
        hits.len()
    );
}

/// index_docs with empty slice must succeed (no-op).
#[tokio::test]
async fn hybrid_index_docs_empty_slice_is_noop() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let result = engine.index_docs("proj-noop", &[], &cancel).await;
    assert!(result.is_ok(), "index_docs([]) must succeed as no-op");

    let count = engine.count_docs("proj-noop").expect("count");
    assert_eq!(count, 0, "empty index must have 0 docs");
}
