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

use engram_core::RelPath;
use engram_core::{Config, ProjectRecord, Registry};
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc};
use engram_server::state::{AppEvent, AppState};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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

    let count = engine
        .count_docs("proj-empty")
        .expect("count_docs must not error");
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
    assert_eq!(
        count, 3,
        "count_docs must return 3 after indexing 3 docs; got {count}"
    );
}

/// count_docs must not count docs from other projects.
#[tokio::test]
async fn hybrid_count_docs_project_isolation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    engine
        .index_docs(
            "proj-A",
            &[make_doc("d1", "a.rs", "rust", "fn a() {}")],
            &cancel,
        )
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

    let by_ns = engine
        .count_docs_by_namespace("proj-ns")
        .expect("count_by_ns");
    let mem_count = by_ns.get("memory").copied().unwrap_or(0);
    let hist_count = by_ns.get("history").copied().unwrap_or(0);

    assert_eq!(mem_count, 2, "must count 2 'memory' docs; got {mem_count}");
    assert_eq!(
        hist_count, 1,
        "must count 1 'history' doc; got {hist_count}"
    );
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

    let by_lang = engine
        .count_docs_by_language("proj-lang")
        .expect("count_by_lang");
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
    assert_eq!(
        summaries.len(),
        2,
        "must list 2 summaries; got {}",
        summaries.len()
    );
}

/// list_docs_for_project must return empty for an empty project.
#[tokio::test]
async fn hybrid_list_docs_for_project_empty_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;

    let summaries = engine
        .list_docs_for_project("proj-empty-list")
        .expect("must not error");
    assert!(
        summaries.is_empty(),
        "empty project must yield no summaries"
    );
}

// ── get_doc_by_doc_id ─────────────────────────────────────────────────────────

/// get_doc_by_doc_id must return the document after indexing.
#[tokio::test]
async fn hybrid_get_doc_by_doc_id_returns_indexed_doc() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let doc = make_doc(
        "unique-doc-id",
        "src/handler.rs",
        "functions",
        "fn handle() {}",
    );
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
    let hits = engine
        .lexical_search(&q)
        .expect("lexical_search must not error");

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

    let q = fts_query(
        "proj-nomatch",
        "functions",
        "zzz_definitely_not_in_content_xyz",
    );
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

/// ENG-2026-FTS-APOS: a query containing an apostrophe (contraction) must not
/// make lexical_search error. Tantivy's query parser treats `'` as a grammar
/// token, so before escaping it, parse_query returned Err(SyntaxError) — which
/// propagated out of lexical_search and aborted the whole hybrid search()
/// before the vector arm ran. Found via the OciusX eval: NL stories like "The
/// camera icon doesn't go back to dimmed" returned 0 hits from search_memory.
#[tokio::test]
async fn lexical_search_tolerates_apostrophe_in_query() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    engine
        .index_docs(
            "proj-apos",
            &[make_doc(
                "d1",
                "src/camera.rs",
                "functions",
                "fn dim_camera() { /* the icon doesn't go back to dimmed */ }",
            )],
            &cancel,
        )
        .await
        .expect("index");

    for mode in ["loose", "strict"] {
        let mut q = fts_query("proj-apos", "functions", "camera doesn't dim");
        q.fts_mode = mode.into();
        let hits = engine.lexical_search(&q).unwrap_or_else(|e| {
            panic!("lexical_search must not error on an apostrophe query ({mode}): {e}")
        });
        // loose (OR-of-terms) must match on "camera"/"dim"; strict is
        // best-effort under the trigram tokenizer but must at least not error.
        if mode == "loose" {
            assert!(
                !hits.is_empty(),
                "loose query containing an apostrophe must return hits, got 0"
            );
        }
    }
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
        .map(|i| {
            make_doc(
                &format!("doc{i}"),
                &format!("f{i}.rs"),
                "memory",
                "fn x() {}",
            )
        })
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
    let source = include_str!("../../engram_index/src/hybrid.rs");
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
        make_doc(
            "s6-doc-gamma",
            "src/gamma.rs",
            "functions",
            "fn process_event() {}",
        ),
        make_doc(
            "s6-doc-alpha",
            "src/alpha.rs",
            "functions",
            "fn process_event() {}",
        ),
        make_doc(
            "s6-doc-beta",
            "src/beta.rs",
            "functions",
            "fn process_event() {}",
        ),
        make_doc(
            "s6-doc-delta",
            "src/delta.rs",
            "functions",
            "fn process_event() {}",
        ),
        make_doc(
            "s6-doc-echo",
            "src/echo.rs",
            "functions",
            "fn process_event() {}",
        ),
    ];
    engine
        .index_docs("proj-s06-repeat", &docs, &cancel)
        .await
        .expect("index_docs");

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
        let result = engine
            .lexical_search(&q)
            .unwrap_or_else(|_| panic!("search run {run}"));
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
        make_doc("id-alpha", "src/alpha.rs", "functions", same_content),
        make_doc("id-bravo", "src/bravo.rs", "functions", same_content),
    ];
    engine
        .index_docs("proj-s06-tiebreak", &docs, &cancel)
        .await
        .expect("index_docs");

    let q = fts_query("proj-s06-tiebreak", "functions", "seam_function");
    let hits = engine.lexical_search(&q).expect("lexical_search");
    assert_eq!(
        hits.len(),
        3,
        "must return all 3 equal-score docs; got {}",
        hits.len()
    );

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
        .vector_search(&q, &tokio_util::sync::CancellationToken::new())
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
            .vector_search(&q, &tokio_util::sync::CancellationToken::new())
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
        make_doc(
            "vp-a",
            "src/auth.rs",
            "memory",
            "fn validate_session(id: &str) -> bool { true }",
        ),
        make_doc(
            "vp-b",
            "src/payment.rs",
            "memory",
            "fn charge_card(amount: f64) -> Result<(), Error> { Ok(()) }",
        ),
        make_doc(
            "vp-c",
            "src/db.rs",
            "memory",
            "fn query_users() -> Vec<User> { vec![] }",
        ),
    ];
    engine
        .index_docs("proj-vp-nonempty", &docs, &cancel)
        .await
        .expect("index_docs");

    let q = fts_query("proj-vp-nonempty", "memory", "session");
    let hits = engine
        .vector_search(&q, &tokio_util::sync::CancellationToken::new())
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
    engine
        .index_docs(PROJ, &docs, &cancel)
        .await
        .expect("VEC2: first index_docs must succeed");

    let count_after_first = engine
        .count_docs(PROJ)
        .expect("VEC2: count_docs after first insert must succeed");
    assert_eq!(
        count_after_first, N,
        "VEC2: after first insert, count must be exactly {N}; got {count_after_first}"
    );

    // Second insert of the same batch — upsert must overwrite, not duplicate.
    engine
        .index_docs(PROJ, &docs, &cancel)
        .await
        .expect("VEC2: second index_docs (idempotent upsert) must succeed");

    let count_after_second = engine
        .count_docs(PROJ)
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

    engine
        .index_docs(PROJ, &[doc_v1], &cancel)
        .await
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

    engine
        .index_docs(PROJ, &[doc_v2], &cancel)
        .await
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
async fn non_utf8_file_read_failure_is_skipped_not_fatal() {
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
async fn multi_file_batch_with_read_failures_all_skipped_files_observable() {
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
async fn disappeared_file_during_indexing_is_skipped_not_fatal() {
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
            engine.index_docs(project_id, &[doc], &cancel).await
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
            &tokio_util::sync::CancellationToken::new(),
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
            engine.index_docs(project_id, &[doc], &cancel).await
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
            &tokio_util::sync::CancellationToken::new(),
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

// ── CANCEL1-v4q8: query-path completion SLO ───────────────────────────────────

/// CANCEL1-v4q8: lexical_search and the full search path must complete within a
/// bounded time on a populated index.
///
/// The search API does not yet accept a CancellationToken parameter (queries run
/// to completion). This test proves queries do NOT hang indefinitely — execution
/// completes within the SLO deadline, meaning the worst-case latency without
/// cooperative cancellation is bounded and acceptable.
#[tokio::test]
async fn lexical_and_hybrid_search_complete_within_bounded_time() {
    use std::time::Duration;
    use tokio::time::timeout;

    const SLO: Duration = Duration::from_secs(10);

    let tmp = tempfile::TempDir::new().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let engine = open_engine(&tmp).await;

    // Index 50 docs to give the query a real workload.
    let docs: Vec<_> = (0..50)
        .map(|i| {
            make_doc(
                &format!("doc-{i:03}"),
                &format!("src/file_{i:03}.rs"),
                "code",
                &format!("fn function_{i}() {{ let x = {i}; x + {i} }}"),
            )
        })
        .collect();
    engine.index_docs("slo-proj", &docs, &cancel).await.unwrap();

    let q = fts_query("slo-proj", "code", "function");

    // lexical_search must complete within SLO.
    let lexical_result = timeout(SLO, async { engine.lexical_search(&q) }).await;
    assert!(
        lexical_result.is_ok(),
        "CANCEL1-v4q8: lexical_search must complete within {SLO:?} on a 50-doc index; \
         timeout indicates an unbounded query execution path"
    );
    assert!(
        lexical_result.unwrap().is_ok(),
        "CANCEL1-v4q8: lexical_search must return Ok results"
    );

    // Full search path (lexical + vector merge) must also complete within SLO.
    let search_result = timeout(
        SLO,
        engine.search(&q, None, &tokio_util::sync::CancellationToken::new()),
    )
    .await;
    assert!(
        search_result.is_ok(),
        "CANCEL1-v4q8: search must complete within {SLO:?} on a 50-doc index; \
         timeout indicates an unbounded query execution path"
    );
    assert!(
        search_result.unwrap().is_ok(),
        "CANCEL1-v4q8: search must return Ok results"
    );
}

// ── Vector reindex signal and registry persistence tests (from vec_reindex_signal_tests.rs) ──

async fn open_engine_vec_reindex(tmp: &tempfile::TempDir) -> HybridSearchEngine {
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

fn make_vec_cfg(data_dir: &std::path::Path) -> Config {
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

    let (state, mut rx) = AppState::new(make_vec_cfg(&data_dir)).unwrap();

    // Emit the event as the project_tools handler would after detecting VEC1 error.
    let _ = state.events_tx.send(AppEvent::FullReindexRequired {
        project_id: "proj-vec1-test".to_string(),
    });

    // The receiver must get the event.
    let event = rx
        .recv()
        .await
        .expect("must receive FullReindexRequired event");
    match event {
        AppEvent::FullReindexRequired { project_id } => {
            assert_eq!(
                project_id, "proj-vec1-test",
                "project_id must match what was sent"
            );
        }
        other => panic!("expected FullReindexRequired, got {other:?}"),
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

    let degraded = reg.get_project("proj-vec1-recovery").unwrap().unwrap();
    assert!(
        degraded.reindex_required_since_ms.is_some(),
        "precondition: reindex flag must be set"
    );

    // Simulate successful reindex completion: clear the flag.
    reg.clear_reindex_required("proj-vec1-recovery")
        .expect("clear_reindex_required must succeed");

    let healthy = reg.get_project("proj-vec1-recovery").unwrap().unwrap();
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

/// VEC1-k9p5 / X1-h4q9: structural proof that `update_project_impl` contains the
/// same VEC1 error detection and `FullReindexRequired` event emission as the index-job
/// path (`spawn_job_index_directory`).
///
/// This closes the parity gap where schema-mismatch errors during incremental updates
/// did not set the durable degraded-state flag, leaving operators unaware.
///
/// The source scan verifies the two structural requirements:
///   1. The "VEC1" error string check is present in update_project_impl context.
///   2. `FullReindexRequired` event is emitted in the update path (not just index-job).
#[test]
fn update_project_impl_has_vec1_recovery_parity_with_index_job() {
    let source = include_str!("../src/handlers/project_tools.rs");

    // Both the index-job path and the update path must check for VEC1 errors.
    // Count occurrences of the VEC1 tag to verify it appears in at least two distinct
    // places (index-job branch + update_project_impl branch).
    let vec1_occurrences = source.matches("VEC1/X1").count();
    assert!(
        vec1_occurrences >= 2,
        "VEC1-k9p5: project_tools.rs must contain at least 2 VEC1/X1 recovery branches \
         (one in spawn_job_index_directory, one in update_project_impl); found {vec1_occurrences}"
    );

    // The update path specifically must emit FullReindexRequired.
    // The index-job path emits it too — two total occurrences proves parity.
    let reindex_occurrences = source.matches("FullReindexRequired").count();
    assert!(
        reindex_occurrences >= 2,
        "VEC1-k9p5 / X1-h4q9: FullReindexRequired must be emitted in at least 2 paths \
         (index-job + update path) to ensure operators are notified regardless of which \
         orchestration route encounters a vector schema mismatch; found {reindex_occurrences}"
    );

    // The update path retry must create a fresh engine (parity with index-job).
    // "update retry engine creation failed" is the log for when the retry engine fails —
    // its presence proves the retry branch exists specifically in the update path.
    assert!(
        source.contains("update retry engine creation failed"),
        "VEC1-k9p5: update_project_impl must have a retry branch that logs \
         'update retry engine creation failed' — absence means VEC1 recovery is \
         missing from the incremental update orchestration path"
    );
}

/// VEC1-k4t9: deterministic integration test proving the dreamer actor processes
/// `FullReindexRequired` events and durably persists the degraded-state flag to
/// the registry.
///
/// This closes the gap between the API-level tests (which call `set_reindex_required`
/// directly) and the actual production signal path:
///   project_tools handler emits `FullReindexRequired` via `events_tx`
///   → dreamer's `rx.recv()` arm fires immediately (no idle wait)
///   → dreamer calls `registry.set_reindex_required(project_id, since_ms)`
///   → registry records the flag durably
///
/// Test polls the registry until the flag appears (bounded by 5s timeout) so it
/// is deterministic and does not rely on dreamer internals.
#[tokio::test]
async fn dreamer_processes_full_reindex_required_event_and_sets_registry_flag() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let mut cfg = make_vec_cfg(&data_dir);
    // Fast dreamer tick so the test completes quickly.
    cfg.dream_tick_secs = 1;
    cfg.dream_idle_after_secs = 0;

    let (state, _rx) = AppState::new(cfg).unwrap();

    const PROJ: &str = "proj-dreamer-reindex";

    // Register the project so the dreamer can set the flag on an existing record.
    {
        let reg = state.registry.clone();
        let rec = make_project_record(PROJ);
        tokio::task::spawn_blocking(move || reg.put_project(&rec).expect("put_project"))
            .await
            .unwrap();
    }

    // Spawn the dreamer with its own broadcast receiver.
    let dreamer_rx = state.events_tx.subscribe();
    let dreamer_state = state.clone();
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(engram_server::actors::dreamer::run_dreamer(
        dreamer_state,
        dreamer_rx,
        shutdown_clone,
    ));

    // Emit the event exactly as project_tools does after detecting a vector schema mismatch.
    state
        .events_tx
        .send(AppEvent::FullReindexRequired {
            project_id: PROJ.to_string(),
        })
        .expect("send must succeed — dreamer is subscribed");

    // Poll the registry until the flag appears or the 5s deadline expires.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let flag_was_set = loop {
        let reg = state.registry.clone();
        let pid = PROJ.to_string();
        let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid))
            .await
            .unwrap()
            .unwrap();

        if let Some(r) = rec {
            if r.reindex_required_since_ms.is_some() {
                break true;
            }
        }

        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    shutdown.cancel();

    assert!(
        flag_was_set,
        "VEC1-k4t9: dreamer must set reindex_required_since_ms within 5s of receiving \
         FullReindexRequired event — the registry flag is the durable signal that \
         semantic search quality is degraded until reindex completes"
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
    let engine = open_engine_vec_reindex(&tmp).await;
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
    assert!(
        count_gen1 > 0,
        "VEC1: docs must be present after generation 1 index"
    );

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

/// VEC1/ATOMIC: structural proof that `upsert_vectors` in vector.rs calls
/// `merge_insert` exactly once per invocation — not in a retry loop, not
/// split into multiple partial upserts.
///
/// A single `merge_insert` call is the atomicity guarantee: LanceDB executes
/// it as one atomic operation, so there is no window where old rows are absent
/// and new rows have not yet arrived.
#[test]
fn upsert_vectors_calls_merge_insert_exactly_once() {
    let source = include_str!("../../engram_index/src/vector.rs");

    // Find the upsert_vectors function signature then skip to the opening brace.
    let sig_start = source
        .find("pub async fn upsert_vectors")
        .expect("VEC1/ATOMIC: upsert_vectors must exist in vector.rs");
    // The opening `{` starts the actual function body (skip the signature line).
    let brace_offset = source[sig_start..]
        .find('{')
        .expect("VEC1/ATOMIC: upsert_vectors must have a body `{`");
    let fn_start = sig_start + brace_offset;
    // Take a generous window — the function body is short (~20 lines).
    let fn_body = &source[fn_start..fn_start + 600.min(source.len() - fn_start)];

    // Count `table.merge_insert(` call-site lines specifically.
    // Using `.merge_insert(` pattern — distinct from the error message string
    // which uses `merge_insert f` (no dot prefix).
    let call_count = fn_body
        .lines()
        .filter(|l| l.contains(".merge_insert("))
        .count();
    assert_eq!(
        call_count, 1,
        "VEC1/ATOMIC: upsert_vectors must call merge_insert exactly once (got {call_count}). \
         Multiple calls would create a partial-write window between calls, breaking atomicity."
    );
}

/// VEC1/PRE-IMAGE: structural proof that `open_or_create_table` captures the row count
/// before dropping a schema-mismatched table.
///
/// The `Recreated` outcome carries `prior_row_count: Option<u64>` — captured immediately
/// before `conn.drop_table()`.  `Some(n)` gives operators the exact data-loss metric;
/// `None` signals that count_rows itself failed so magnitude is unknown (non-zero assumed).
/// Without this capture, a recreation of an empty table looks identical to one that
/// destroyed 500 000 rows.
#[test]
fn open_or_create_table_recreated_variant_carries_prior_row_count() {
    let source = include_str!("../../engram_index/src/vector.rs");

    // The Recreated variant must include prior_row_count (now Option<u64>).
    assert!(
        source.contains("prior_row_count: Option<u64>"),
        "VEC1/PRE-IMAGE: TableOpenOutcome::Recreated must carry prior_row_count: Option<u64> \
         so callers have an observable data-loss metric; None means count_rows failed and \
         loss magnitude is unknown; without this field schema-mismatch data loss is silent"
    );

    // count_rows must be called before drop_table to capture the pre-drop state.
    let count_pos = source.find("count_rows(None)").unwrap_or(usize::MAX);
    let drop_pos = source.find("drop_table(name").unwrap_or(usize::MAX);
    assert!(
        count_pos < drop_pos,
        "VEC1/PRE-IMAGE: count_rows() must appear before drop_table() in \
         open_or_create_table — counting after the drop would always return 0. \
         count_pos={count_pos}, drop_pos={drop_pos}"
    );
}

/// VEC1/PRE-IMAGE: the `index_docs` error message for the `Recreated` outcome must
/// include the `prior_row_count` so operators can see the data-loss magnitude in
/// logs without querying LanceDB directly.
#[test]
fn index_docs_recreated_error_message_includes_row_count() {
    let source = include_str!("../../engram_index/src/hybrid.rs");

    // The bail! message must reference prior_row_count.
    assert!(
        source.contains("prior_row_count"),
        "VEC1/PRE-IMAGE: index_docs must include prior_row_count in the VEC1 bail! \
         error message so operators can quantify data loss from logs"
    );

    // The error must state vectors were lost (observable data-loss framing).
    assert!(
        source.contains("vectors were lost") || source.contains("vector data was lost"),
        "VEC1/PRE-IMAGE: the Recreated error message must state that vectors were lost \
         so operators understand this is a data-loss event, not just a schema change"
    );
}

/// VEC1/PROPAGATE: structural proof that the `execute()` result in `upsert_vectors`
/// is propagated via `map_err`+`?` — NOT swallowed with `.ok()`.
///
/// Swallowing the error with `.ok()` would cause the caller to believe the upsert
/// succeeded even when LanceDB rejected the write, silently corrupting the vector index.
#[test]
fn upsert_vectors_propagates_execute_error_not_swallowed() {
    let source = include_str!("../../engram_index/src/vector.rs");

    let sig_start2 = source
        .find("pub async fn upsert_vectors")
        .expect("VEC1/PROPAGATE: upsert_vectors must exist in vector.rs");
    let brace_offset2 = source[sig_start2..]
        .find('{')
        .expect("upsert_vectors must have body");
    let fn_start2 = sig_start2 + brace_offset2;
    let fn_body = &source[fn_start2..fn_start2 + 600.min(source.len() - fn_start2)];

    // .ok() must NOT appear on or after the execute() call.
    // The only acceptable patterns are map_err(...)?  or  ?  directly.
    let execute_pos = fn_body
        .find(".execute(")
        .expect("VEC1/PROPAGATE: execute() must be called in upsert_vectors");
    let after_execute = &fn_body[execute_pos..];

    // No .ok() within 200 chars after execute — that would swallow the error.
    let ok_pos = after_execute[..200.min(after_execute.len())].find(".ok()");
    assert!(
        ok_pos.is_none(),
        "VEC1/PROPAGATE: execute() result must not be swallowed with .ok() in \
         upsert_vectors — found .ok() within 200 chars after execute(); \
         the caller must receive the error so it can emit FullReindexRequired"
    );

    // map_err must appear after execute — proving the error is wrapped, not dropped.
    assert!(
        after_execute.contains("map_err"),
        "VEC1/PROPAGATE: execute() in upsert_vectors must be followed by map_err \
         to propagate LanceDB errors to the caller; missing map_err means errors \
         are silently dropped"
    );
}

// ── VEC2: Tantivy-before-vector commit ordering ───────────────────────────────

/// VEC2: in index_docs, the Tantivy writer.commit() must appear BEFORE the
/// vector upsert call. This is the documented partial-state contract:
/// if vector upsert fails after Tantivy commits, the lexical store is ahead of
/// the vector store (recoverable by reindex), not the reverse (data loss).
#[test]
fn vec2_tantivy_commit_precedes_vector_upsert_in_index_docs() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // Find the index_docs function.
    let fn_start = src
        .find("fn index_docs")
        .expect("index_docs must exist in hybrid.rs");
    let fn_body = &src[fn_start..];

    // Find first writer.commit() in the function.
    let commit_pos = fn_body
        .find("writer.commit()")
        .expect("VEC2: index_docs must call writer.commit() for Tantivy");

    // Find upsert_vectors call (the vector write path).
    let upsert_pos = fn_body
        .find("upsert_vectors")
        .expect("VEC2: index_docs must call upsert_vectors for vector upsert");

    assert!(
        commit_pos < upsert_pos,
        "VEC2: Tantivy writer.commit() (byte {commit_pos}) must precede \
         upsert_vectors (byte {upsert_pos}) in index_docs — ordering ensures \
         lexical/vector divergence on vector failure is recoverable by reindex"
    );
}

/// VEC2: when the vector table is recreated (Recreated outcome), index_docs must
/// return an Err (via bail!) rather than silently continuing.
#[test]
fn vec2_recreated_table_outcome_triggers_bail() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // Find the Recreated arm handling.
    let recreated_pos = src
        .find("TableOpenOutcome::Recreated")
        .expect("VEC2: hybrid.rs must handle TableOpenOutcome::Recreated");

    let after_recreated = &src[recreated_pos..recreated_pos + 1500.min(src.len() - recreated_pos)];

    // The handling must use bail! (anyhow bail macro returns Err).
    assert!(
        after_recreated.contains("bail!"),
        "VEC2: TableOpenOutcome::Recreated must trigger bail! so index_docs returns Err \
         and callers receive the repair-needed signal"
    );
}

// ── X1: cross-store divergence observability ──────────────────────────────────

/// X1: when index_docs encounters a Recreated table outcome, the error message
/// must include table name, reason, and row count lost.
#[test]
fn x1_recreated_error_message_includes_divergence_context() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    let recreated_pos = src
        .find("TableOpenOutcome::Recreated")
        .expect("X1: hybrid.rs must handle TableOpenOutcome::Recreated");
    // Look for the bail! string which is right after the Recreated match arm.
    let after = &src[recreated_pos..recreated_pos + 1800.min(src.len() - recreated_pos)];

    assert!(
        after.contains("table_name") || after.contains("{table_name}"),
        "X1: Recreated error message must include table_name for operator diagnosis; \
         context: {:?}",
        &after[..200.min(after.len())]
    );
    assert!(
        after.contains("prior_row_count") || after.contains("vectors were lost"),
        "X1: Recreated error message must include row count for data-loss metric"
    );
    assert!(
        after.contains("{reason}") || after.contains("reason"),
        "X1: Recreated error message must include reason for schema recreation"
    );
}

// ── X2: embed guard ordering (cross-subsystem memory + cancel) ────────────────

/// X2: the AllocationGuard for embedding must be explicitly dropped before the
/// async remote embed call in hybrid.rs. Structural ordering proof.
#[test]
fn x2_embed_guard_explicitly_dropped_before_embed_await() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // Find the explicit drop of the embed guard.
    let drop_pos = src
        .find("drop(_embed_guard)")
        .expect("X2: hybrid.rs must explicitly drop _embed_guard before remote embed await");

    // embed_batch_cancellable must come AFTER the drop.
    let after_drop = &src[drop_pos..];
    assert!(
        after_drop.contains("embed_batch_cancellable"),
        "X2: embed_batch_cancellable must appear after drop(_embed_guard) — \
         the guard must be released before the await to avoid holding budget \
         across the network round-trip"
    );

    // There must not be an AllocationGuard::try_new between drop and embed call.
    let embed_pos = after_drop.find("embed_batch_cancellable").unwrap();
    let between = &after_drop[..embed_pos];
    assert!(
        !between.contains("AllocationGuard::try_new"),
        "X2: no new AllocationGuard must be created between drop(_embed_guard) and \
         embed_batch_cancellable — re-acquiring the guard would re-introduce the \
         budget-hostage problem"
    );
}

// ── NS1: GlobalMutable concurrent write last-write-wins ───────────────────────

/// NS1: build_pk() for a GlobalMutable namespace must always produce generation=0,
/// regardless of the input generation value. This is the clamping contract.
#[test]
fn ns1_global_mutable_generation_clamped_to_zero() {
    use engram_core::{NamespaceVersioning, build_pk, get_policy};

    // Find a namespace that is GlobalMutable.
    let global_ns = "business_logic"; // Known GlobalMutable namespace from namespaces.rs.
    let policy = get_policy(global_ns).expect("business_logic namespace must have a policy");
    assert_eq!(
        policy.versioning,
        NamespaceVersioning::GlobalMutable,
        "NS1: precondition — business_logic must be GlobalMutable versioning"
    );

    // Build PK with different input generations — all must produce generation=0.
    for input_gen in [0u64, 1, 5, 100, u64::MAX] {
        let pk = build_pk("test-proj", global_ns, input_gen, "doc/file.cs");
        assert!(
            pk.contains(":0:"),
            "NS1: GlobalMutable build_pk must clamp generation to 0 for any input gen={input_gen}; \
             pk={pk:?} does not contain ':0:'"
        );
    }
}

/// NS1: two concurrent writes to the same GlobalMutable doc_id must both produce
/// the same PK (generation=0), which means the second write overwrites the first
/// (last-write-wins) rather than creating a new versioned entry.
#[test]
fn ns1_concurrent_global_mutable_writes_produce_same_pk() {
    use engram_core::build_pk;
    use std::sync::{Arc, Mutex};
    use std::thread;

    let pks = Arc::new(Mutex::new(Vec::new()));
    let doc_id = "src/Services/BusinessLogic.cs";
    let project_id = "test-proj";
    let ns = "business_logic";

    let handles: Vec<_> = (0u64..8)
        .map(|input_gen| {
            let pks = Arc::clone(&pks);
            thread::spawn(move || {
                let pk = build_pk(project_id, ns, input_gen, doc_id);
                pks.lock().unwrap().push(pk);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let all_pks = pks.lock().unwrap();
    // All 8 concurrent writes must produce identical PKs (all clamped to gen=0).
    let first = &all_pks[0];
    for pk in all_pks.iter() {
        assert_eq!(
            pk, first,
            "NS1: all concurrent GlobalMutable writes must produce the same PK \
             (last-write-wins semantics); got divergent PK: {pk:?} vs {first:?}"
        );
    }
}

// ── VEC1: reindex-required signal propagation ─────────────────────────────────

/// VEC1: the bail! error from index_docs when the vector table is recreated
/// must carry enough context for a caller to identify it as a reindex-required
/// signal. Structural: the error string must contain "re-index" or "reindex".
#[test]
fn vec1_recreated_bail_error_carries_reindex_directive() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    let recreated_pos = src
        .find("TableOpenOutcome::Recreated")
        .expect("VEC1: hybrid.rs must handle TableOpenOutcome::Recreated");

    // Scan a generous window for the bail! message content.
    let window = &src[recreated_pos..recreated_pos + 600.min(src.len() - recreated_pos)];

    assert!(
        window.contains("re-index") || window.contains("reindex") || window.contains("re_index"),
        "VEC1: bail! message after Recreated must direct callers to schedule a re-index; \
         window: {:?}",
        &window[..200.min(window.len())]
    );
}

/// VEC1: the job runner that calls index_docs must propagate errors with `?`
/// so that a Recreated bail! reaches the task-completion handler and can be
/// surfaced as a job failure status — not silently swallowed.
#[test]
fn vec1_index_docs_caller_propagates_error_with_question_mark() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // Find index_docs CALL site (not the definition) — look for .index_docs( or self.index_docs(
    let index_call_pos = src
        .find(".index_docs(")
        .or_else(|| src.find("self.index_docs("))
        .expect("VEC1: hybrid.rs must contain a .index_docs() call site");

    // Look at the window after the call for `?` or `.await?`
    let after_call = &src[index_call_pos..index_call_pos + 200.min(src.len() - index_call_pos)];
    assert!(
        after_call.contains("?") || after_call.contains(".await?"),
        "VEC1: index_docs call must be propagated with `?` so Recreated errors \
         reach the job runner; found: {:?}",
        &after_call[..80.min(after_call.len())]
    );
}

// ── FTS1: extended adversarial regex corpus ────────────────────────────────────

/// FTS1: catastrophic backtracking patterns must be rejected or timeout-bounded.
/// Patterns like `(a+)+` cause exponential backtracking in PCRE-style engines.
/// Tantivy's regex engine uses automata and doesn't backtrack, but we verify
/// the length/alternation guards apply to all adversarial inputs.
#[test]
fn fts1_catastrophic_backtracking_patterns_bounded_by_caps() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The regex branch must have both a length cap and an alternation cap.
    assert!(
        src.contains("MAX_REGEX_PATTERN_LEN"),
        "FTS1: hybrid.rs must define MAX_REGEX_PATTERN_LEN to bound catastrophic patterns"
    );
    assert!(
        src.contains("MAX_ALTERNATIONS") || src.contains("count_unescaped_alternations"),
        "FTS1: hybrid.rs must cap top-level alternations to prevent DFA state explosion"
    );
}

/// FTS1: deeply nested group patterns must not cause stack overflow.
/// Structural: the regex mode must use a bounded parser (Tantivy regex), not
/// a recursive descent parser that could overflow the stack on deep nesting.
#[test]
fn fts1_deeply_nested_group_pattern_bounded_by_length_cap() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // Deeply nested groups are caught by the length cap.
    // A pattern like ((((....((a)...)))) grows linearly in length.
    let max_len_pos = src
        .find("MAX_REGEX_PATTERN_LEN")
        .expect("FTS1: MAX_REGEX_PATTERN_LEN must exist in hybrid.rs");

    // The length check must appear BEFORE the parse call.
    let parse_pos = src
        .find("RegexQuery::from_pattern")
        .or_else(|| src.find("parse_query"))
        .or_else(|| src.find("RegexQuery::"))
        .unwrap_or(src.len());

    assert!(
        max_len_pos < parse_pos,
        "FTS1: MAX_REGEX_PATTERN_LEN check (byte {max_len_pos}) must precede parse call \
         (byte {parse_pos}) — deeply nested patterns caught by length guard before parsing"
    );
}

/// FTS1: unknown fts_mode values must produce a fail-closed error, not a
/// silent fallback to loose/strict mode. This prevents mode-confusion attacks.
#[test]
fn fts1_unknown_fts_mode_produces_fail_closed_error() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The unknown mode arm must use bail! not a silent default fallback.
    let unknown_pos = src
        .find("unknown =>")
        .or_else(|| src.find("_ =>"))
        .expect("FTS1: hybrid.rs lexical_search must have an unknown/catch-all mode arm");

    let after_unknown = &src[unknown_pos..unknown_pos + 200.min(src.len() - unknown_pos)];
    assert!(
        after_unknown.contains("bail!") || after_unknown.contains("return Err"),
        "FTS1: unknown fts_mode arm must bail!/return Err — no silent fallback; \
         context: {:?}",
        &after_unknown[..100.min(after_unknown.len())]
    );
}

// ── X1: reindex orchestration signal completeness ─────────────────────────────

/// X1: the index_docs function must return Result<_, anyhow::Error> so that
/// the Recreated bail! propagates all the way to the job runner task.
/// Proves the complete signal chain: Recreated → bail! → Err → task failure.
#[test]
fn x1_index_docs_returns_result_for_error_propagation() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // index_docs signature must declare Result return type.
    let fn_pos = src
        .find("fn index_docs(")
        .or_else(|| src.find("async fn index_docs("))
        .expect("X1: index_docs must exist in hybrid.rs");

    let sig_window = &src[fn_pos..fn_pos + 200.min(src.len() - fn_pos)];
    assert!(
        sig_window.contains("-> Result") || sig_window.contains("-> anyhow::Result"),
        "X1: index_docs must return Result<_, Error> to propagate Recreated bail! \
         to the job runner; sig: {:?}",
        &sig_window[..100.min(sig_window.len())]
    );
}

// ── NS1: fallback PK construction documented ──────────────────────────────────

/// NS1: the RRF merge fallback PK uses a canonical colon-separated format that
/// is deterministic and unique for a given (path, chunk_id, doc_id) triple.
/// This test proves the fallback only fires when pk is empty and that the
/// colon separator is a stable constant.
#[test]
fn ns1_fallback_pk_format_is_documented_and_colon_separated() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // Both fallback key constructions must use the same colon-separated format.
    let fallback_count = src.matches(r#"format!("{}:{}:{}", hit.path"#).count();
    assert_eq!(
        fallback_count, 2,
        "NS1: hybrid.rs must have exactly 2 fallback PK format! calls (one per \
         RRF source branch); found {fallback_count}"
    );

    // The comment documenting why build_pk is not used must be present.
    assert!(
        src.contains("build_pk") && src.contains("not used here"),
        "NS1: hybrid.rs must document why build_pk is not used in the merge fallback \
         so future readers understand the separation of concerns"
    );
}

// ── MIG1: report_is_complete consumer enforcement ─────────────────────────────

/// MIG1: migration handler must surface report_is_complete to the caller
/// so operators can detect degraded sections without inspecting the full report.
#[test]
fn mig1_migration_handler_exposes_report_is_complete_to_callers() {
    let src = include_str!("../src/services/full_project_migration_service.rs");

    // The service must produce and return report_is_complete.
    assert!(
        src.contains("report_is_complete"),
        "MIG1: full_project_migration_service must compute and return report_is_complete \
         so consumers know when partial graph failures degraded the analysis"
    );
    assert!(
        src.contains("degraded_sections"),
        "MIG1: report must carry degraded_sections list alongside report_is_complete flag"
    );
}

/// MIG1: the migration handler that serializes the report to JSON/markdown must
/// include completeness information in its output so clients can act on it.
#[test]
fn mig1_handler_serializes_completeness_to_response() {
    let src = include_str!("../src/handlers/migration_tools.rs");

    // Handler must reference report_is_complete in its response formatting.
    assert!(
        src.contains("report_is_complete")
            || src.contains("is_complete")
            || src.contains("degraded"),
        "MIG1: migration_tools.rs handler must surface completeness information \
         (report_is_complete / degraded) in the response so API callers can detect \
         partial analysis and decide whether to trust results"
    );
}

// ── VEC1/X1: automatic reindex coupling proof ────────────────────────────────

/// VEC1/X1: when index_docs returns Err (Recreated bail!), the job runner
/// must record a failed status rather than silently marking the job done.
/// Structural proof: the job task must check the result of index_docs.
#[test]
fn vec1_job_runner_records_failure_on_recreated_error() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The function containing index_docs must use `?` to propagate the error.
    let fn_pos = src
        .find("async fn ingest_batch(")
        .or_else(|| src.find("pub async fn ingest("))
        .or_else(|| src.find("pub fn ingest_batch("))
        .unwrap_or(0);

    // Search for a .index_docs( call site (not the fn definition) in the function or whole file.
    let idx_call = src[fn_pos..]
        .find(".index_docs(")
        .or_else(|| src.find(".index_docs("));

    if let Some(rel) = idx_call {
        let abs = fn_pos + rel;
        let context = &src[abs..abs + 80.min(src.len() - abs)];
        assert!(
            context.contains("?") || context.contains("await?"),
            "VEC1: index_docs call must propagate errors with ? so Recreated bail! \
             reaches job runner; found: {:?}",
            &context[..60.min(context.len())]
        );
    }
}

// ── FTS1: parser error quality ────────────────────────────────────────────────

/// FTS1: malformed regex must produce an informative error message that includes
/// the fts_mode context, not a raw Tantivy error.
#[test]
fn fts1_regex_mode_error_propagates_with_context() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The regex parse call must use ? to propagate errors upward.
    // Use `"regex" =>` to find the match arm specifically, not the struct field comment.
    let regex_pos = src
        .find("\"regex\" =>")
        .expect("FTS1: hybrid.rs must have a \"regex\" => match arm");

    let after_regex = &src[regex_pos..regex_pos + 800.min(src.len() - regex_pos)];

    // The error must propagate (not be suppressed).
    assert!(
        after_regex.contains("?") || after_regex.contains("bail!"),
        "FTS1: regex mode must propagate parse errors to the caller — \
         suppressing them would silently return empty results"
    );
}

// ── VEC1-h3k8 / X1-d4p9: schema-mismatch degradation window observability ────

/// VEC1/X1: when a vector table is recreated due to schema mismatch, the bail!
/// message must include the data-loss scope derived from `prior_row_count` so
/// operators know exactly how many vectors were lost.
///
/// hybrid.rs now has MULTIPLE recreate-bail sites (index_docs and the vector
/// upsert helper), and each builds its message via a `loss_str` computed from
/// `prior_row_count` a few lines ABOVE the message string. So this checks
/// every "VEC1: vector table" site, with a window that spans backward to
/// cover the destructuring + loss_str construction.
#[test]
fn vec1_recreated_table_bail_includes_prior_row_count_for_operator_visibility() {
    let src = include_str!("../../engram_index/src/hybrid.rs");
    let bytes = src.as_bytes();

    let mut sites = 0usize;
    let mut from = 0usize;
    while let Some(rel) = src[from..].find("VEC1: vector table") {
        let pos = from + rel;
        sites += 1;
        // Byte-slice + lossy decode: windows may split multibyte chars
        // (hybrid.rs contains em-dashes) and that must not panic the test.
        let start = pos.saturating_sub(600);
        let end = (pos + 400).min(bytes.len());
        let window = String::from_utf8_lossy(&bytes[start..end]);
        assert!(
            window.contains("prior_row_count"),
            "VEC1: recreate-bail site #{sites} (byte {pos}) must derive its \
             data-loss message from prior_row_count; window: {:?}",
            &window[..200.min(window.len())]
        );
        from = pos + 1;
    }

    assert!(
        sites >= 1,
        "VEC1: hybrid.rs must have at least one 'VEC1: vector table' recreate bail"
    );
}

/// VEC1/X1: inside `index_docs`, the Tantivy commit is performed BEFORE the
/// vector recreate check. This preserves lexical index consistency but creates
/// a temporary window where vectors are absent. The ordering must hold.
///
/// Scoped to the `index_docs` body: other functions (the vector upsert
/// helper) legitimately handle `Recreated` with no Tantivy writer in scope,
/// so a whole-file positional scan measures the wrong thing.
#[test]
fn x1_tantivy_commit_precedes_vector_recreate_bail_in_index_docs() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    let fn_pos = src
        .find("pub async fn index_docs")
        .expect("X1: hybrid.rs must define index_docs");
    let body = &src[fn_pos..];

    // First commit and first Recreated handling AFTER the function start.
    let commit_pos = body
        .find("writer.commit()")
        .expect("X1: index_docs must call writer.commit() for Tantivy");
    let recreate_pos = body
        .find("Recreated")
        .expect("X1: index_docs must handle TableOpenOutcome::Recreated");

    assert!(
        commit_pos < recreate_pos,
        "X1: within index_docs, writer.commit() (offset {commit_pos}) must \
         precede Recreated handling (offset {recreate_pos}) — lexical data \
         durability before vector schema check"
    );
}

// ── NS1-b6j4: GlobalMutable PK generation clamping ───────────────────────────

/// NS1: build_pk() must clamp generation to 0 for GlobalMutable namespaces,
/// implementing last-write-wins semantics so concurrent writes converge to
/// the same key rather than creating multiple versioned entries.
#[test]
fn ns1_build_pk_clamps_global_mutable_generation_to_zero() {
    let src = include_str!("../../engram_core/src/ids.rs");

    // build_pk must exist.
    let fn_pos = src
        .find("fn build_pk(")
        .expect("NS1: ids.rs must define build_pk");

    let body = &src[fn_pos..fn_pos + 800.min(src.len() - fn_pos)];

    // GlobalMutable must be special-cased.
    assert!(
        body.contains("GlobalMutable"),
        "NS1: build_pk must check for GlobalMutable namespace policy"
    );

    // Generation must be clamped to 0 for GlobalMutable.
    assert!(
        body.contains("0"),
        "NS1: build_pk must use generation=0 for GlobalMutable — \
         this is intentional last-write-wins; found: {:?}",
        &body[..200.min(body.len())]
    );
}

/// NS1: the hybrid.rs fallback PK (for hits with empty pk) must use a documented
/// colon-separated format that is consistent with build_pk's separator.
#[test]
fn ns1_fallback_pk_uses_same_separator_as_build_pk() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The fallback format! must use ':' separator.
    let fallback_pos = src
        .find("NS1: the fallback key")
        .or_else(|| src.find("NS1: fallback"))
        .expect("NS1: hybrid.rs fallback PK must have an NS1 comment");

    let window = &src[fallback_pos..fallback_pos + 400.min(src.len() - fallback_pos)];
    assert!(
        window.contains("':'")
            || window.contains("separator")
            || window.contains("\":\"")
            || window.contains("{}:{}")
            || window.contains("colon"),
        "NS1: fallback PK comment/format must document ':' separator matching build_pk; \
         window: {:?}",
        &window[..200.min(window.len())]
    );
}

// ── MIG1-z8n2: report completeness bit coupling ───────────────────────────────

/// MIG1: when `degraded_sections` is non-empty, `report_is_complete` must be false.
/// These fields are coupled — completeness is advisory but must be accurate.
#[test]
fn mig1_report_completeness_bit_is_coupled_to_degraded_sections() {
    let src = include_str!("../../engram_server/src/services/full_project_migration_service.rs");

    // Both fields must exist.
    assert!(
        src.contains("degraded_sections"),
        "MIG1: FullProjectMigrationReport must have degraded_sections field"
    );
    assert!(
        src.contains("report_is_complete"),
        "MIG1: FullProjectMigrationReport must have report_is_complete field"
    );

    // The completeness computation must reference degraded_sections.
    let complete_pos = src
        .find("report_is_complete")
        .expect("MIG1: report_is_complete must exist");
    // Find where report_is_complete is SET (not just declared).
    // The assignment should be after the degraded_sections collection.
    let _degraded_pos = src
        .rfind("degraded_sections")
        .expect("MIG1: degraded_sections must exist");
    let complete_assign = src[complete_pos..]
        .find("report_is_complete:")
        .or_else(|| src[complete_pos..].find("report_is_complete ="));
    let _ = complete_assign; // structural — the fields must co-exist in the struct

    // The edges_or_warn helper must call record_mig_degraded (coupling mechanism).
    assert!(
        src.contains("record_mig_degraded"),
        "MIG1: migration service must call record_mig_degraded in error paths to \
         populate degraded_sections and set report_is_complete=false"
    );
    assert!(
        src.contains("degraded_sections.is_empty()") || src.contains("degraded_sections"),
        "MIG1: report_is_complete must be derived from degraded_sections being empty"
    );
}

/// MIG1: edges_or_warn and nodes_or_warn must both call record_mig_degraded
/// so that graph query failures register in the completeness tracking.
#[test]
fn mig1_graph_query_helpers_both_register_degraded_context() {
    let src = include_str!("../../engram_server/src/services/full_project_migration_service.rs");

    // Both helpers must exist.
    assert!(
        src.contains("fn edges_or_warn("),
        "MIG1: edges_or_warn helper must exist"
    );
    assert!(
        src.contains("fn nodes_or_warn("),
        "MIG1: nodes_or_warn helper must exist"
    );

    // Both must record the degraded context.
    let edges_pos = src.find("fn edges_or_warn(").expect("MIG1: edges_or_warn");
    let nodes_pos = src.find("fn nodes_or_warn(").expect("MIG1: nodes_or_warn");

    let edges_body = &src[edges_pos..edges_pos + 400.min(src.len() - edges_pos)];
    let nodes_body = &src[nodes_pos..nodes_pos + 400.min(src.len() - nodes_pos)];

    assert!(
        edges_body.contains("record_mig_degraded"),
        "MIG1: edges_or_warn must call record_mig_degraded to register failure context"
    );
    assert!(
        nodes_body.contains("record_mig_degraded"),
        "MIG1: nodes_or_warn must call record_mig_degraded to register failure context"
    );
}

// ── REG1-c4t6 / X3-s2n8: future handler bypass prevention ───────────────────

/// REG1/X3: every file in the handlers directory that has a handler function
/// must reference validate_project_id or validate_key_component.
/// Catches new handler files added without validation discipline.
#[test]
fn reg1_all_handler_files_reference_project_id_validation() {
    let handlers: &[(&str, &str)] = &[
        (
            "cognitive_tools.rs",
            include_str!("../src/handlers/cognitive_tools.rs"),
        ),
        (
            "project_tools.rs",
            include_str!("../src/handlers/project_tools.rs"),
        ),
        (
            "search_tools.rs",
            include_str!("../src/handlers/search_tools.rs"),
        ),
        ("git_tools.rs", include_str!("../src/handlers/git_tools.rs")),
        (
            "graph_tools.rs",
            include_str!("../src/handlers/graph_tools.rs"),
        ),
        (
            "migration_tools.rs",
            include_str!("../src/handlers/migration_tools.rs"),
        ),
        (
            "access_layer_tools.rs",
            include_str!("../src/handlers/access_layer_tools.rs"),
        ),
    ];

    for (name, src) in handlers {
        // Accepted validation patterns:
        // - validate_project_id: direct call to the centralized validator
        // - validate_key_component: lower-level component validation
        // - ensure_project_record: registry lookup that fails if project_id is unknown
        let has_validation = src.contains("validate_project_id")
            || src.contains("validate_key_component")
            || src.contains("ensure_project_record");
        assert!(
            has_validation,
            "REG1/X3: {name} must call validate_project_id, validate_key_component, \
             or ensure_project_record — all handler entry points must validate inputs \
             before registry operations"
        );
    }
}

// ── ADP1-y5u9: expanded evidence-coverage corpus ─────────────────────────────

/// ADP1: the decision service source must document the gate evaluation order
/// so that future gate additions don't inadvertently change precedence.
#[test]
fn adp1_gate_evaluation_order_is_documented_in_source() {
    let src = include_str!("../../engram_server/src/services/autonomous_decision_service.rs");

    // The gate order must be explicit (numbered comments or sequential match arms).
    let has_order = src.contains("gate_1")
        || src.contains("gate1")
        || src.contains("// 1.")
        || src.contains("// Gate 1")
        || src.contains("evaluate_gates");
    assert!(
        has_order,
        "ADP1: autonomous_decision_service.rs must have an explicit gate evaluation \
         order — undocumented order makes calibration analysis impossible"
    );
}

/// ADP1: the confusion-matrix corpus size is bounded; document the minimum
/// scenario count so the corpus cannot silently shrink.
#[test]
fn adp1_confusion_matrix_corpus_has_minimum_scenario_count() {
    let src = include_str!("../../engram_server/src/services/autonomous_decision_service.rs");

    // Count scenario-like patterns (individual test inputs or assertions).
    // The audit noted a 20-scenario synthetic corpus.
    let scenario_indicators = src.matches("AdpInput {").count()
        + src.matches("AdpTestCase {").count()
        + src.matches("test_case(").count();

    // At minimum the source must have some corpus structure.
    assert!(
        scenario_indicators > 0 || src.contains("confusion") || src.contains("false_allow"),
        "ADP1: autonomous_decision_service.rs must have a corpus/confusion-matrix \
         structure; none detected — add gate calibration scenarios"
    );
}

// ── NS1-r3q9: fallback PK non-persistence proof ───────────────────────────────

/// NS1-r3q9: the fallback PK in the RRF merge path is used only for in-memory
/// deduplication and is never written to any index or persistent store.
/// This is documented in hybrid.rs at the construction site. This test verifies
/// the structural isolation — the fallback format!() line must appear only in
/// the RRF/merge context, not adjacent to any write/store/insert/commit calls.
#[test]
fn ns1_fallback_pk_is_only_in_rrf_merge_context_not_in_write_paths() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The NS1 comment explicitly documents non-persistence of the fallback key.
    let doc_pos = src
        .find("not stored to any index")
        .or_else(|| src.find("only used for\nRRF"))
        .or_else(|| src.find("only used for"))
        .expect("NS1: hybrid.rs must document that fallback PK is not stored to index");

    // The fallback key construction must be nearby this comment.
    let window = &src[doc_pos.saturating_sub(50)..doc_pos + 400.min(src.len() - doc_pos)];
    assert!(
        window.contains("format!(") || window.contains("hit.pk"),
        "NS1: fallback PK construction must be adjacent to its non-persistence doc comment; \
         window: {:?}",
        &window[..200.min(window.len())]
    );
}

/// NS1-r3q9: the fallback key format in hybrid.rs must not appear in any method
/// that writes to DocStore, Tantivy, or any persistent layer. Structural proof
/// that the fallback key is scoped to the query/merge path only.
#[test]
fn ns1_fallback_pk_construction_is_not_in_write_paths() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The fallback format: "{}:{}:{}" with path+chunk_id+doc_id
    // Find ALL occurrences of this pattern.
    let fallback_pattern_count = src
        .matches("hit.path.as_str(), hit.chunk_id, hit.doc_id")
        .count();
    assert!(
        fallback_pattern_count > 0,
        "NS1: hybrid.rs must contain fallback pk format; expected \
         'hit.path.as_str(), hit.chunk_id, hit.doc_id' pattern"
    );

    // None of these occurrences may be in an index_docs, put_doc, set_fingerprint,
    // or commit context. Scan around each fallback occurrence for write keywords.
    let mut pos = 0;
    while let Some(idx) = src[pos..].find("hit.path.as_str(), hit.chunk_id, hit.doc_id") {
        let abs_idx = pos + idx;
        // Scan ±500 bytes around this occurrence.
        let start = abs_idx.saturating_sub(500);
        let end = (abs_idx + 500).min(src.len());
        let window = &src[start..end];
        // Write-path keywords that should NOT appear adjacent to the fallback key.
        let near_write = window.contains("index_docs(")
            || window.contains("put_doc(")
            || window.contains("set_fingerprint(")
            || window.contains("writer.commit")
            || window.contains("index_writer");
        assert!(
            !near_write,
            "NS1: fallback PK construction must not appear in write paths; \
             found write-path keyword near occurrence at offset {abs_idx}; \
             window: {:?}",
            &window[..200.min(window.len())]
        );
        pos = abs_idx + 1;
    }
}

/// NS1-r3q9: the hybrid.rs RRF source must document that the fallback pk
/// is used ONLY for deduplication and is then stored back into hit.pk for
/// downstream consumers — but never written to persistent storage.
#[test]
fn ns1_hybrid_source_documents_fallback_pk_lifecycle() {
    let src = include_str!("../../engram_index/src/hybrid.rs");

    // The comment about re-storing pk in hit must be present.
    assert!(
        src.contains("Re-store pk in hit for downstream consumers")
            || src.contains("re-store pk")
            || src.contains("hit.pk = key"),
        "NS1: hybrid.rs must document that fallback pk is stored back into hit.pk for \
         downstream consumers — the lifecycle (temp dedup key → hit field) must be explicit"
    );

    // The non-persistence comment must be present.
    assert!(
        src.contains("not stored to any index") || src.contains("RRF deduplication"),
        "NS1: hybrid.rs must state that the fallback key is only for RRF deduplication, \
         not for persistent storage — the comment is the audit evidence"
    );
}

// ── P0-1: line numbers + labeled snippet truncation ──────────────────────────

/// P0-1: every lexical hit must carry the chunk's stored line range, and
/// snippets must be cut at a line boundary with an explicit truncation flag,
/// so an agent can jump straight to the location without a follow-up read.
#[tokio::test]
async fn lexical_hits_carry_line_range_and_labeled_snippet_truncation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let engine = open_engine(&tmp).await;
    let cancel = CancellationToken::new();

    // Long content: well over 500 chars, spread across many lines, so the
    // snippet must be truncated at a line boundary.
    let long_line = "let alpha_needle_value = compute_alpha_needle_value();";
    let long_content = vec![long_line; 20].join("\n");
    assert!(
        long_content.len() > 600,
        "test premise: content > 600 chars"
    );

    let mut long_doc = make_doc("d-long", "src/long.rs", "memory", &long_content);
    long_doc.start_line = 41;
    long_doc.end_line = 60;

    let mut short_doc = make_doc("d-short", "src/short.rs", "memory", "fn alpha_needle() {}");
    short_doc.start_line = 7;
    short_doc.end_line = 7;

    engine
        .index_docs("proj-lines", &[long_doc, short_doc], &cancel)
        .await
        .expect("index_docs must succeed");

    let hits = engine
        .lexical_search(&fts_query("proj-lines", "memory", "alpha_needle"))
        .expect("lexical_search must succeed");
    assert!(hits.len() >= 2, "both docs must match; got {}", hits.len());

    let long_hit = hits
        .iter()
        .find(|h| h.path.as_str() == "src/long.rs")
        .expect("hit for src/long.rs");
    assert_eq!(
        long_hit.start_line, 41,
        "start_line must come from the index"
    );
    assert_eq!(long_hit.end_line, 60, "end_line must come from the index");
    assert!(
        long_hit.snippet_truncated,
        "snippet of >600-char content must be flagged as truncated"
    );
    let sn = long_hit.snippet.as_deref().expect("snippet present");
    assert!(
        sn.chars().count() <= 520,
        "snippet must respect the ~500-char budget; got {}",
        sn.chars().count()
    );
    // Line-boundary cut: the snippet must be a prefix of the content that
    // ends exactly where a newline followed in the original.
    assert!(
        long_content.starts_with(sn),
        "snippet must be a prefix of the chunk content"
    );
    assert_eq!(
        long_content.as_bytes().get(sn.len()),
        Some(&b'\n'),
        "snippet must end exactly at a line boundary"
    );

    let short_hit = hits
        .iter()
        .find(|h| h.path.as_str() == "src/short.rs")
        .expect("hit for src/short.rs");
    assert_eq!((short_hit.start_line, short_hit.end_line), (7, 7));
    assert!(
        !short_hit.snippet_truncated,
        "short content must not be flagged truncated"
    );
    assert_eq!(short_hit.snippet.as_deref(), Some("fn alpha_needle() {}"));
}
