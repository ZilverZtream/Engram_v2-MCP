//! Pins `hybrid.rs`'s extension-to-extractor dispatch chain for `.ml` /
//! `.mlinc` files by driving the REAL indexing entry point,
//! `HybridSearchEngine::index_files`, end to end (real Tantivy dir, real
//! LanceDB dir, real files on disk).
//!
//! Why this is not covered by the existing MiniLang tests:
//! `ml_extractor_test.rs`, `ml_corpus_smoke_test.rs`, and
//! `ml_real_corpus_test.rs` all call `extract_ml` (or `SymbolExtractor::
//! extract`) directly. That proves the extractor itself is correct, but
//! it can never catch a wiring regression in `hybrid.rs`'s `index_files`
//! match chain: deleting the `Some("ml" | "mlinc")` arm, reordering it
//! behind an earlier arm, typoing the extension string, or swapping the
//! extractor's two path arguments would all leave every extractor-level
//! test green while MiniLang indexing silently produced nothing (or
//! corrupt edges) in the real pipeline.
//!
//! This test can only be defeated by actually exercising the dispatch
//! chain because it asserts on properties that depend on BOTH halves of
//! the arm's call, `crate::ml_extractor::extract_ml(p, arc_rel.as_str(),
//! &text)`, being wired in the right place and the right order:
//!
//!   1. `.ml` content produces MiniLang-shaped symbols at all (the
//!      generic tree-sitter fallback every other extension falls through
//!      to is a documented no-op for MiniLang -- see
//!      `ml_corpus_smoke_test.rs`'s
//!      `generic_extractor_never_produced_ml_symbols_before_wiring`).
//!   2. An `Include "..."` edge resolves relative to the file's
//!      PROJECT-RELATIVE directory -- only true if the second argument
//!      really is `rel_path`, not `abs_path`.
//!   3. A golden-sidecar (`test_oracle`) edge appears -- only true if the
//!      first argument really is the ABSOLUTE disk path (the extractor
//!      stats the sidecar from it), not `rel_path`.
//!
//! Swapping the two arguments at the call site changes neither the
//! compiled types (both are stringly-compatible with `Path::new`/
//! `&str`) nor anything `extract_ml`'s unit tests check (they pass their
//! own abs/rel pair per call, so a transposed pair there is just "a
//! different, still self-consistent input" and cannot fail). Only a test
//! that supplies a REAL project tree, where the absolute and relative
//! paths necessarily point at the same file but differ in shape, can
//! observe a swap.

use engram_core::namespaces;
use engram_index::HybridSearchEngine;
use tokio_util::sync::CancellationToken;

/// Build a real, temp-dir-backed engine -- the same construction
/// `generation_policy_test.rs` and `grep_vs_rg_test.rs` already use
/// elsewhere in this crate's test suite.
async fn new_engine(tmp: &std::path::Path) -> HybridSearchEngine {
    let cfg = engram_core::Config::default();
    HybridSearchEngine::new(tmp.join("tantivy"), tmp.join("lancedb"), &cfg)
        .await
        .expect("engine construction")
}

#[tokio::test]
async fn ml_files_are_routed_through_the_minilang_dispatch_arm() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!("engram_ml_dispatch_test_{now}"));
    let project_dir = tmp.join("project");
    let src_dir = project_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // The `Include` line resolves relative to the includer's own
    // directory ("src/") -- this is the probe for the rel_path half of
    // the dispatch arm's argument order.
    let ml_path = src_dir.join("Sample.ml");
    std::fs::write(
        &ml_path,
        "Include \"Other.mlinc\"\n\
         Function Add(a As Int, b As Int) As Int\n\
         \x20\x20\x20Return a + b\n\
         End Function\n",
    )
    .unwrap();

    // Golden sidecar. `extract_ml` stats this sibling from the ABSOLUTE
    // disk path -- this is the probe for the abs_path half of the
    // dispatch arm's argument order.
    std::fs::write(src_dir.join("Sample.expected"), "3\n").unwrap();

    let engine = new_engine(&tmp).await;
    let cancel = CancellationToken::new();

    let stats = engine
        .index_files(
            "ml-dispatch-proj",
            namespaces::NAMESPACE_MEMORY,
            1,
            &project_dir,
            vec![ml_path],
            4096,
            &cancel,
            |_, _| {},
        )
        .await
        .expect("index_files must succeed");

    // --- Proof 1: the arm exists and actually ran the MiniLang extractor.
    // If the arm is deleted, reordered behind an earlier arm, or the
    // extension string is typoed, this file falls through to the
    // generic tree-sitter extractor, which has no MiniLang grammar and
    // emits nothing.
    let symbol_debug = || {
        stats
            .symbols
            .iter()
            .map(|(p, s)| (p.as_str().to_string(), s.kind.clone(), s.name.clone()))
            .collect::<Vec<_>>()
    };
    let add_fn = stats
        .symbols
        .iter()
        .find(|(_, s)| s.kind == "function" && s.name == "Add");
    assert!(
        add_fn.is_some(),
        "expected a MiniLang 'Add' function symbol out of index_files — the \
         `.ml`/`.mlinc` dispatch arm did not run (or the generic fallback \
         extractor ran instead); got symbols: {:?}",
        symbol_debug()
    );

    // --- Proof 2: the `Include` edge target is project-relative
    // ("src/Other.mlinc"), which is only correct if the extractor
    // received rel_path — not the absolute disk path — as its `rel_path`
    // argument.
    let edge_debug = || {
        stats
            .edges
            .iter()
            .map(|(p, e)| {
                (
                    p.as_str().to_string(),
                    e.kind.clone(),
                    e.target_name.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let include_target = stats
        .edges
        .iter()
        .find(|(_, e)| e.kind == "includes_file")
        .map(|(_, e)| e.target_name.as_str());
    assert_eq!(
        include_target,
        Some("src/Other.mlinc"),
        "Include target must resolve relative to the project-relative source \
         directory; a wrong/missing target here means the extractor's two path \
         arguments were swapped at the call site. All edges: {:?}",
        edge_debug()
    );

    // --- Proof 3: the golden-sidecar `test_oracle` edge appears, which is
    // only possible if the extractor received the ABSOLUTE disk path —
    // not the project-relative path — as its `abs_path` argument (it
    // stats `Sample.expected` next to `Sample.ml` on disk).
    let has_oracle_edge = stats.edges.iter().any(|(_, e)| e.kind == "test_oracle");
    assert!(
        has_oracle_edge,
        "expected a test_oracle edge from the golden-sidecar disk stat; its \
         absence means the extractor's two path arguments were swapped at the \
         call site (abs_path must be the real disk path for the stat to \
         succeed). All edges: {:?}",
        edge_debug()
    );

    std::fs::remove_dir_all(&tmp).ok();
}
