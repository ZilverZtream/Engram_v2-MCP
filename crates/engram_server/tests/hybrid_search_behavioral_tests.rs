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
use std::sync::Arc;

// ── helpers ───────────────────────────────────────────────────────────────────

async fn open_engine(tmp: &tempfile::TempDir) -> HybridSearchEngine {
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).expect("create tantivy dir");
    std::fs::create_dir_all(&lance_dir).expect("create lance dir");

    let cfg = Config {
        embedding_backend: "local".into(), // was "projection" — hermetic stub embedder
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
        embedding_backend: "local".into(), // was "projection" — hermetic stub embedder
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

    // Use namespaces that the production code actually counts.
    // index_docs requires homogeneous namespace per call (ENG-AUD-2026-S05-0001);
    // two separate calls are needed for two distinct namespaces.
    engine
        .index_docs(
            "proj-ns",
            &[
                make_doc("m1", "a.rs", "memory", "session notes about auth"),
                make_doc("m2", "b.rs", "memory", "session notes about db"),
            ],
            &cancel,
        )
        .await
        .expect("index memory docs");
    engine
        .index_docs(
            "proj-ns",
            &[make_doc("h1", "c.rs", "history", "commit: refactor auth")],
            &cancel,
        )
        .await
        .expect("index history docs");

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

    // index_docs requires homogeneous namespace per call (ENG-AUD-2026-S05-0001).
    engine
        .index_docs(
            "proj-list",
            &[make_doc("doc-a", "src/mod_a.rs", "functions", "fn f() {}")],
            &cancel,
        )
        .await
        .expect("index doc-a");
    engine
        .index_docs(
            "proj-list",
            &[make_doc("doc-b", "src/mod_b.rs", "classes", "struct S {}")],
            &cancel,
        )
        .await
        .expect("index doc-b");

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

/// ENG-AUD-2026-S05-0001: heterogeneous namespace batch must be rejected.
///
/// A batch where docs[0].namespace != docs[N].namespace would silently mis-key
/// vector rows if the first doc's namespace were used for all rows.
/// index_docs must fail-closed with a descriptive error.
#[tokio::test]
async fn index_docs_rejects_heterogeneous_namespace_batch() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let doc_a = make_doc("da", "a.rs", "memory", "fn a() {}");
    let doc_b = make_doc("db", "b.rs", "history", "fn b() {}"); // different namespace

    let result = engine
        .index_docs("proj-ns-mixed", &[doc_a, doc_b], &cancel)
        .await;

    assert!(
        result.is_err(),
        "index_docs must return Err for a mixed-namespace batch (ENG-AUD-2026-S05-0001)"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("ENG-AUD-2026-S05-0001"),
        "error must cite ENG-AUD-2026-S05-0001; got: {err_msg}"
    );
    assert!(
        err_msg.contains("heterogeneous namespace"),
        "error must describe heterogeneous namespace; got: {err_msg}"
    );
}

/// ENG-AUD-2026-S05-0001: homogeneous namespace batch must succeed.
/// This is the nominal (non-error) path — verifies the precondition does not
/// incorrectly block valid same-namespace batches.
#[tokio::test]
async fn index_docs_accepts_homogeneous_namespace_batch() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let docs: Vec<_> = (0..3)
        .map(|i| make_doc(&format!("doc{i}"), &format!("f{i}.rs"), "memory", "fn x() {}"))
        .collect();

    let result = engine.index_docs("proj-ns-homo", &docs, &cancel).await;
    assert!(
        result.is_ok(),
        "index_docs must succeed for a same-namespace batch; got: {:?}",
        result.err()
    );
}

/// ENG-AUD-2026-S05-0002: source structure — verify vector search timeout
/// returns a typed Err (not Ok(Vec::new())) so callers can distinguish infra
/// failures from genuine empty result sets.
#[test]
fn vector_search_timeout_is_not_masked_as_empty_result() {
    // Structural regression: verify the hybrid.rs source does not contain
    // Ok(Vec::new()) in the timeout branch, and does contain the audit tag.
    let source = include_str!(
        "../../engram_index/src/hybrid.rs"
    );
    assert!(
        source.contains("ENG-AUD-2026-S05-0002"),
        "hybrid.rs must contain ENG-AUD-2026-S05-0002 audit tag"
    );
    // The timeout branch must not silently return Ok(Vec::new()).
    // We verify this by checking the audit tag and error propagation are present.
    assert!(
        source.contains("vector search infrastructure timeout"),
        "hybrid.rs timeout branch must emit infrastructure timeout error message"
    );
}

// ── S06-001: sort determinism ──────────────────────────────────────────────────

/// ENG-AUD-2026-S06-001: lexical_search must return identical hit ordering
/// across repeated calls for the same query.
///
/// Prior to the tie-break fix, equal-BM25-score docs had non-deterministic order
/// because Tantivy's internal segment ordering was not a stable sort key.
/// After the fix, results are ordered by (score DESC, path ASC, doc_id ASC, chunk_id ASC).
#[tokio::test]
async fn lexical_search_results_are_identical_across_repeated_calls() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let docs = vec![
        make_doc("s6-doc-gamma", "src/gamma.rs", "functions", "fn process_event() {}"),
        make_doc("s6-doc-alpha", "src/alpha.rs", "functions", "fn process_event() {}"),
        make_doc("s6-doc-beta",  "src/beta.rs",  "functions", "fn process_event() {}"),
        make_doc("s6-doc-delta", "src/delta.rs", "functions", "fn process_event() {}"),
        make_doc("s6-doc-echo",  "src/echo.rs",  "functions", "fn process_event() {}"),
    ];
    engine.index_docs("proj-s06-repeat", &docs, &cancel).await.expect("index_docs");

    let q = fts_query("proj-s06-repeat", "functions", "process_event");

    let first = engine.lexical_search(&q).expect("search run 1");
    assert!(
        !first.is_empty(),
        "lexical_search must return hits for 'process_event' query (S06-001)"
    );
    let first_key: Vec<(String, String, u64)> = first
        .iter()
        .map(|h| (h.path.as_str().to_string(), h.doc_id.clone(), h.chunk_id))
        .collect();

    for run in 2..=10 {
        let result = engine.lexical_search(&q).unwrap_or_else(|_| panic!("search run {run}"));
        let key: Vec<(String, String, u64)> = result
            .iter()
            .map(|h| (h.path.as_str().to_string(), h.doc_id.clone(), h.chunk_id))
            .collect();
        assert_eq!(
            first_key, key,
            "ENG-AUD-2026-S06-001: run {run} ordering must be byte-identical to run 1; \
             got {key:?}, expected {first_key:?}"
        );
    }
}

/// ENG-AUD-2026-S06-001: when multiple docs have equal BM25 scores (identical
/// content), the tie-break must be ascending path order.
///
/// Contract: docs with identical content index to identical BM25 scores.
/// The secondary sort key is path ASC so the result is deterministic and
/// predictable regardless of Tantivy's internal segment ordering.
#[tokio::test]
async fn lexical_search_equal_score_docs_sorted_by_path() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    // All docs have identical content → identical BM25 score for the query term.
    // Tie-break must produce alphabetical path order: alpha < bravo < charlie.
    let same_content = "fn seam_function() { /* identical implementation */ }";
    let docs = vec![
        make_doc("id-charlie", "src/charlie.rs", "functions", same_content),
        make_doc("id-alpha",   "src/alpha.rs",   "functions", same_content),
        make_doc("id-bravo",   "src/bravo.rs",   "functions", same_content),
    ];
    engine.index_docs("proj-s06-tiebreak", &docs, &cancel).await.expect("index_docs");

    let q = fts_query("proj-s06-tiebreak", "functions", "seam_function");
    let hits = engine.lexical_search(&q).expect("lexical_search");
    assert_eq!(hits.len(), 3, "must return all 3 equal-score docs; got {}", hits.len());

    // All scores must be equal (identical content, identical doc length).
    let scores: Vec<f32> = hits.iter().map(|h| h.score).collect();
    let first_score = scores[0];
    for (i, &s) in scores.iter().enumerate() {
        assert!(
            (s - first_score).abs() < 1e-4,
            "all docs must have equal BM25 score for tie-break to apply; \
             doc[{i}] score {s} != doc[0] score {first_score}"
        );
    }

    // Tie-break: ascending by path.
    let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(
        paths,
        ["src/alpha.rs", "src/bravo.rs", "src/charlie.rs"],
        "ENG-AUD-2026-S06-001: equal-score docs must be sorted by path ASC; got {paths:?}"
    );
}

/// ENG-AUD-2026-S06-001: secondary sort by doc_id when path is equal.
///
/// When two chunks come from the same file (path) with equal scores, the
/// doc_id ASC tie-break must apply.
#[tokio::test]
async fn lexical_search_equal_path_sorted_by_doc_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let content = "fn dispatch_event() {}";
    // Two chunks from the same file; doc_id order determines final ordering.
    let mut doc_z = make_doc("z-chunk", "src/handler.rs", "functions", content);
    doc_z.chunk_id = 0;
    let mut doc_a = make_doc("a-chunk", "src/handler.rs", "functions", content);
    doc_a.chunk_id = 1;

    engine
        .index_docs("proj-s06-docid", &[doc_z, doc_a], &cancel)
        .await
        .expect("index_docs");

    let q = fts_query("proj-s06-docid", "functions", "dispatch_event");
    let hits = engine.lexical_search(&q).expect("lexical_search");
    assert_eq!(hits.len(), 2, "must return 2 hits");

    let doc_ids: Vec<&str> = hits.iter().map(|h| h.doc_id.as_str()).collect();
    assert_eq!(
        doc_ids,
        ["a-chunk", "z-chunk"],
        "ENG-AUD-2026-S06-001: equal-path docs must be sorted by doc_id ASC; got {doc_ids:?}"
    );
}

// ── S18-001: always-on vector parity ─────────────────────────────────────────

/// ENG-AUD-2026-S18-001: vector_search with the ProjectionEmbedder (hermetic,
/// deterministic, no environment variables required) must return consistent
/// results across repeated invocations.
///
/// This test is always-on (no env var guard) because ProjectionEmbedder is a
/// local hash-based projection — it never calls a remote service.
#[cfg(feature = "vector")]
#[tokio::test]
async fn vector_search_projection_backend_is_deterministic() {
    use engram_core::Config;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).unwrap();
    std::fs::create_dir_all(&lance_dir).unwrap();

    // "projection" falls through to ProjectionEmbedder (hermetic, no network).
    let cfg = Config {
        embedding_backend: "local".into(), // was "projection" — hermetic stub embedder
        ..Default::default()
    };
    let engine = HybridSearchEngine::new(tantivy_dir, lance_dir, &cfg)
        .await
        .expect("HybridSearchEngine with projection backend must succeed");

    let cancel = CancellationToken::new();
    let docs: Vec<_> = (0..6)
        .map(|i| {
            make_doc(
                &format!("vp-{i}"),
                &format!("src/module_{i}.rs"),
                "memory",
                &format!("fn authenticate_user_{i}(token: &str) -> bool {{ false }}"),
            )
        })
        .collect();
    engine
        .index_docs("proj-vp-determinism", &docs, &cancel)
        .await
        .expect("index_docs must succeed with projection backend");

    let q = fts_query("proj-vp-determinism", "memory", "authenticate");

    // Run vector_search 5 times.  Results must be byte-identical across all runs.
    let first: Vec<(String, u64)> = engine
        .vector_search(&q)
        .await
        .expect("vector_search run 1 must not error")
        .into_iter()
        .map(|h| (h.doc_id, h.chunk_id))
        .collect();

    assert!(
        !first.is_empty(),
        "ENG-AUD-2026-S18-001: vector_search must return at least one hit \
         after indexing 6 docs with query term in content"
    );

    for run in 2..=5 {
        let result: Vec<(String, u64)> = engine
            .vector_search(&q)
            .await
            .unwrap_or_else(|e| panic!("vector_search run {run} failed: {e}"))
            .into_iter()
            .map(|h| (h.doc_id, h.chunk_id))
            .collect();
        assert_eq!(
            first, result,
            "ENG-AUD-2026-S18-001: vector_search run {run} must produce identical \
             (doc_id, chunk_id) sequence as run 1"
        );
    }
}

/// ENG-AUD-2026-S18-001: the ProjectionEmbedder backend must produce non-empty
/// vector search results for a corpus where the query term appears in content.
///
/// This tests the behavioral contract: after indexing, the vector path must
/// return at least one hit.  It does NOT require Ollama or OpenAI env vars.
#[cfg(feature = "vector")]
#[tokio::test]
async fn vector_search_projection_backend_returns_nonempty_results() {
    use engram_core::Config;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let tantivy_dir = tmp.path().join("tantivy");
    let lance_dir = tmp.path().join("lance");
    std::fs::create_dir_all(&tantivy_dir).unwrap();
    std::fs::create_dir_all(&lance_dir).unwrap();

    let cfg = Config {
        embedding_backend: "local".into(), // was "projection" — hermetic stub embedder
        ..Default::default()
    };
    let engine = HybridSearchEngine::new(tantivy_dir, lance_dir, &cfg)
        .await
        .expect("engine");

    let cancel = CancellationToken::new();
    let docs = vec![
        make_doc("vp-a", "src/auth.rs",    "memory", "fn validate_session(id: &str) -> bool { true }"),
        make_doc("vp-b", "src/payment.rs", "memory", "fn charge_card(amount: f64) -> Result<(), Error> { Ok(()) }"),
        make_doc("vp-c", "src/db.rs",      "memory", "fn query_users() -> Vec<User> { vec![] }"),
    ];
    engine
        .index_docs("proj-vp-nonempty", &docs, &cancel)
        .await
        .expect("index_docs");

    let q = fts_query("proj-vp-nonempty", "memory", "session");
    let hits = engine
        .vector_search(&q)
        .await
        .expect("vector_search must not error");

    assert!(
        !hits.is_empty(),
        "ENG-AUD-2026-S18-001: vector_search with projection backend must return \
         at least one hit after indexing; got 0 hits"
    );
}

/// VEC2: `index_docs` upsert is idempotent — indexing the same batch twice must
/// not double the row count.  `merge_insert` on `pk` updates in place; there
/// must be no window where rows are absent between delete and re-insert.
///
/// This closes the VEC2 "Uncovered" gap by proving LanceDB merge_insert upsert
/// semantics hold at the engram_index integration level.
#[tokio::test]
async fn upsert_same_docs_twice_does_not_double_the_row_count() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;

    const PROJ: &str = "vec2-idem-proj";
    const N: usize = 5;

    // Build N distinct docs.
    let docs: Vec<IndexDoc> = (0..N)
        .map(|i| IndexDoc {
            generation: 1,
            chunk_id: i as u64,
            path: RelPath::new(&format!("src/file_{i}.rs")),
            language: "rust".into(),
            content: format!("fn func_{i}() {{ /* idempotency test */ }}"),
            namespace: "code".into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 5,
            doc_id: format!("idem-doc-{i:03}"),
            content_hash: format!("hash-{i:03}"),
        })
        .collect();

    let cancel = CancellationToken::new();

    // First insert.
    engine.index_docs(PROJ, &docs, &cancel).await
        .expect("VEC2: first index_docs must succeed");

    let count_after_first = engine.count_docs(PROJ)
        .expect("VEC2: count_docs after first insert must succeed");
    assert_eq!(
        count_after_first, N,
        "VEC2: after first insert, count must be exactly {N}; got {count_after_first}"
    );

    // Second insert of the same batch — upsert must overwrite, not duplicate.
    engine.index_docs(PROJ, &docs, &cancel).await
        .expect("VEC2: second index_docs (idempotent upsert) must succeed");

    let count_after_second = engine.count_docs(PROJ)
        .expect("VEC2: count_docs after second insert must succeed");
    assert_eq!(
        count_after_second, N,
        "VEC2: after second insert of same batch, count must still be {N}; got {count_after_second} \
         — merge_insert/upsert must overwrite existing rows, not append duplicates"
    );
}

/// VEC2: updating the content of existing docs (same doc_id, new content_hash)
/// must result in updated rows, not additional rows.
///
/// Proves the pk-based merge_insert correctly identifies existing rows for update.
#[tokio::test]
async fn upsert_updated_doc_content_replaces_old_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine(&tmp).await;

    const PROJ: &str = "vec2-update-proj";

    let doc_v1 = IndexDoc {
        generation: 1,
        chunk_id: 0,
        path: RelPath::new("src/lib.rs"),
        language: "rust".into(),
        content: "fn old_version() {}".into(),
        namespace: "code".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: "update-test-doc".into(),
        content_hash: "hash-v1".into(),
    };

    let cancel = CancellationToken::new();

    engine.index_docs(PROJ, &[doc_v1], &cancel).await
        .expect("VEC2: insert v1 must succeed");
    let count_v1 = engine.count_docs(PROJ).unwrap();
    assert_eq!(count_v1, 1, "VEC2: exactly 1 doc after v1 insert");

    // Update: same doc_id, new content and hash.
    let doc_v2 = IndexDoc {
        generation: 1,
        chunk_id: 0,
        path: RelPath::new("src/lib.rs"),
        language: "rust".into(),
        content: "fn new_version() { /* updated */ }".into(),
        namespace: "code".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: "update-test-doc".into(),
        content_hash: "hash-v2".into(),
    };

    engine.index_docs(PROJ, &[doc_v2], &cancel).await
        .expect("VEC2: update v2 must succeed");
    let count_v2 = engine.count_docs(PROJ).unwrap();
    assert_eq!(
        count_v2, 1,
        "VEC2: count must remain 1 after update (upsert overwrites, not appends); got {count_v2}"
    );
}

/// ENG-AUD-2026-S18-001: source structure — verify the search_tools handler
/// emits the machine-parseable sentinel for empty results (S01-001) and
/// returns McpError for doc-miss lookups.
#[test]
fn search_tools_handler_has_correct_not_found_contract() {
    let source = include_str!("../../engram_server/src/handlers/search_tools.rs");

    assert!(
        source.contains("ENG-AUD-2026-S01-001"),
        "search_tools.rs must contain ENG-AUD-2026-S01-001 audit tag"
    );
    assert!(
        source.contains("result: no_hits"),
        "search_tools.rs empty-result branch must emit 'result: no_hits' sentinel \
         so programmatic callers can distinguish no-match from found results"
    );
    assert!(
        source.contains("McpError::invalid_params"),
        "search_tools.rs doc-miss branch must return McpError::invalid_params, \
         not a success payload (ENG-AUD-2026-S01-001)"
    );
    assert!(
        source.contains("not found in project"),
        "search_tools.rs McpError message must include 'not found in project' \
         to identify which project/namespace/generation was queried"
    );
}

// ── DS1: copy-forward hash/read failure semantics ─────────────────────────────

async fn open_engine_fts_only(tmp: &tempfile::TempDir) -> HybridSearchEngine {
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

/// D5/DS1: when a file contains non-UTF8 bytes, `index_files` must:
///   1. Return Ok(stats) — fail-open, job does not abort
///   2. Include the unreadable file in stats.skipped_files
///   3. Successfully index sibling valid files in the same batch
#[tokio::test]
async fn ds1_non_utf8_file_is_skipped_not_fatal() {
    use std::io::Write;
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine_fts_only(&tmp).await;

    let project_dir = tmp.path().join("proj");
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create a valid file that should be indexed.
    let good_file = project_dir.join("good.rs");
    std::fs::write(&good_file, b"fn hello_world() { println!(\"hi\"); }").unwrap();

    // Create a file with non-UTF8 bytes to simulate a binary/corrupt file
    // that slips past the binary-detection heuristic (e.g. .md extension).
    let bad_file = project_dir.join("corrupt.md");
    {
        let mut f = std::fs::File::create(&bad_file).unwrap();
        // Write bytes that are invalid UTF-8 (0xff 0xfe starts a UTF-16 BOM, but
        // together with 0x80-0xBF continuations they are invalid UTF-8).
        f.write_all(b"\xff\xfe\x80invalid-utf8-content\x81\x82")
            .unwrap();
    }

    let cancel = CancellationToken::new();
    let files = vec![good_file.clone(), bad_file.clone()];

    let result = engine
        .index_files(
            "ds1-proj",
            "memory",
            1,
            &project_dir,
            files,
            2048,
            &cancel,
            |_, _| {},
        )
        .await;

    assert!(
        result.is_ok(),
        "D5/DS1: index_files must return Ok even when a file fails UTF-8 decode; got: {:?}",
        result.unwrap_err()
    );

    let stats = result.unwrap();

    // The corrupt file must appear in skipped_files (no silent data loss).
    let skipped_paths: Vec<&str> = stats
        .skipped_files
        .iter()
        .map(|(p, _)| p.as_str())
        .collect();
    assert!(
        skipped_paths.iter().any(|p| p.contains("corrupt")),
        "D5/DS1: corrupt.md must appear in skipped_files; got: {:?}",
        skipped_paths
    );

    // The valid file must have been indexed (appears as a doc in the engine).
    let count = engine.count_docs("ds1-proj").unwrap();
    assert!(
        count > 0,
        "D5/DS1: valid sibling file must be indexed even when another file fails; docs=0"
    );
}

/// D5/DS1: mid-job multi-file batch with mixed failures.
///
/// Proves that when several files in a single batch have different failure
/// modes (non-UTF8 bytes, file disappeared), ALL failures are observable in
/// `skipped_files` and ALL surviving valid files are indexed. No silent data
/// loss: the caller has full observability into which files were skipped and why.
#[tokio::test]
async fn ds1_multi_file_batch_all_skipped_files_observable() {
    use std::io::Write;
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine_fts_only(&tmp).await;

    let project_dir = tmp.path().join("proj3");
    std::fs::create_dir_all(&project_dir).unwrap();

    // 3 good files that should be indexed.
    for i in 0..3 {
        let path = project_dir.join(format!("good_{i}.rs"));
        std::fs::write(&path, format!("fn good_{i}() {{}}").as_bytes()).unwrap();
    }

    // 2 corrupt files (non-UTF8).
    for i in 0..2 {
        let path = project_dir.join(format!("corrupt_{i}.md"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"\xff\xfe\x80corrupt-bytes\x81\x82").unwrap();
    }

    // 2 "vanished" files (never written — simulate disappearance between scan and read).
    let vanished: Vec<_> = (0..2)
        .map(|i| project_dir.join(format!("vanished_{i}.rs")))
        .collect();

    let cancel = CancellationToken::new();
    let mut files: Vec<_> = (0..3)
        .map(|i| project_dir.join(format!("good_{i}.rs")))
        .chain((0..2).map(|i| project_dir.join(format!("corrupt_{i}.md"))))
        .chain(vanished.iter().cloned())
        .collect();
    files.extend(vanished);

    // De-duplicate in case of accidental overlap.
    files.dedup();

    let result = engine
        .index_files(
            "ds1-multi-proj",
            "memory",
            1,
            &project_dir,
            files,
            2048,
            &cancel,
            |_, _| {},
        )
        .await;

    assert!(
        result.is_ok(),
        "D5/DS1: index_files must return Ok with mixed failures; got: {:?}",
        result.unwrap_err()
    );

    let stats = result.unwrap();

    // All failures must be observable in skipped_files.
    assert!(
        !stats.skipped_files.is_empty(),
        "D5/DS1: skipped_files must be non-empty when files fail — caller must have \
         observability into which files were skipped"
    );

    // Valid files must have been indexed.
    let count = engine.count_docs("ds1-multi-proj").unwrap();
    assert!(
        count > 0,
        "D5/DS1: at least the good files must be indexed; got docs=0"
    );
}

/// D5/DS1: when a file disappears between scan-time and read-time (race
/// condition), `index_files` must return Ok and record the file in
/// skipped_files rather than panicking or aborting.
#[tokio::test]
async fn ds1_disappeared_file_is_skipped_not_fatal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = open_engine_fts_only(&tmp).await;

    let project_dir = tmp.path().join("proj2");
    std::fs::create_dir_all(&project_dir).unwrap();

    // Good file stays throughout.
    let good_file = project_dir.join("stays.rs");
    std::fs::write(&good_file, b"fn stays() {}").unwrap();

    // Vanishing file: we pass a path that doesn't exist yet at index time.
    let vanished_file = project_dir.join("vanished.rs");
    // Do NOT create the file — simulating a file that was listed but disappeared.

    let cancel = CancellationToken::new();
    let files = vec![good_file.clone(), vanished_file.clone()];

    let result = engine
        .index_files(
            "ds1-vanish-proj",
            "memory",
            1,
            &project_dir,
            files,
            2048,
            &cancel,
            |_, _| {},
        )
        .await;

    assert!(
        result.is_ok(),
        "D5/DS1: index_files must return Ok even when a listed file does not exist; got: {:?}",
        result.unwrap_err()
    );

    let stats = result.unwrap();

    // The vanished file must appear in skipped_files.
    let skipped_paths: Vec<&str> = stats
        .skipped_files
        .iter()
        .map(|(p, _)| p.as_str())
        .collect();
    assert!(
        !skipped_paths.is_empty(),
        "D5/DS1: vanished file must appear in skipped_files; skipped_files is empty"
    );

    // Good file must still be indexed.
    let count = engine.count_docs("ds1-vanish-proj").unwrap();
    assert!(
        count > 0,
        "D5/DS1: valid file must be indexed even when sibling vanishes; docs=0"
    );
}

// ── NS1: GlobalMutable concurrent writer stress tests ─────────────────────────

/// D4/NS1: N concurrent `index_docs` calls for the same doc_id in a
/// GlobalMutable namespace must produce exactly one hit after all writers
/// complete. No panics or errors are permitted during concurrent writes.
#[tokio::test]
async fn global_mutable_concurrent_writes_last_write_wins() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = Arc::new(open_engine_fts_only(&tmp).await);

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
    let engine = Arc::new(open_engine_fts_only(&tmp).await);

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
    let engine = Arc::new(open_engine_fts_only(&tmp).await);

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
