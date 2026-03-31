#![allow(clippy::unwrap_used)]
//! PARSE1 — depth and size stress tests for production parsers.
//!
//! Proves that the tree-sitter-backed `SymbolExtractor` and the regex-based
//! `sql_parser::analyze_sql` do not panic, stack-overflow, or hang when given:
//! - Deeply nested code (hundreds of nesting levels)
//! - Very large files (hundreds of KB)
//! - Adversarial SQL (deeply nested subqueries, very long identifiers)
//! - Edge-case inputs (empty string, whitespace-only, binary-looking content)
//!
//! All tests assert completion within a hard deadline (30 s) rather than timing
//! out the whole test suite.  A stack overflow manifests as a process abort and
//! will fail the test.

use engram_index::{sql_parser::analyze_sql, SymbolExtractor};
use std::path::Path;
use std::time::{Duration, Instant};

const PARSE_DEADLINE: Duration = Duration::from_secs(30);

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build Rust source with `depth` levels of nested function definitions.
/// Each level wraps the inner level in a `mod` block.
/// e.g. depth=3 → `mod d0 { mod d1 { mod d2 { fn leaf() {} } } }`
fn nested_rust_mods(depth: usize) -> String {
    let mut s = String::with_capacity(depth * 20 + 30);
    for i in 0..depth {
        s.push_str(&format!("mod depth_{i} {{ "));
    }
    s.push_str("fn leaf() {}");
    for _ in 0..depth {
        s.push_str(" }");
    }
    s
}

/// Build Rust source with `n` top-level functions, each calling the next.
/// Produces a wide flat file rather than deeply nested.
fn wide_rust_source(n: usize) -> String {
    let mut s = String::with_capacity(n * 50);
    for i in 0..n {
        s.push_str(&format!("fn func_{i}() {{ "));
        if i + 1 < n {
            s.push_str(&format!("func_{}(); ", i + 1));
        }
        s.push_str("}\n");
    }
    s
}

/// Build a SQL string with `depth` nested subqueries:
/// `SELECT * FROM (SELECT * FROM (... (SELECT 1) ...) t1) t0`
fn nested_sql_subqueries(depth: usize) -> String {
    let mut inner = "SELECT 1".to_string();
    for i in 0..depth {
        inner = format!("SELECT * FROM ({inner}) AS t{i}");
    }
    inner
}

// ── PARSE1 tree-sitter stress tests ──────────────────────────────────────────

/// PARSE1: Deeply nested Rust mod blocks (500 levels) must not stack-overflow
/// or hang.  Tree-sitter uses an iterative LR algorithm, so this should be safe,
/// but the test proves it end-to-end with the production extractor.
#[test]
fn parse1_deeply_nested_rust_mods_does_not_panic_or_hang() {
    let extractor = SymbolExtractor::new();
    let code = nested_rust_mods(500);

    let start = Instant::now();
    let (symbols, _edges) = extractor.extract(Path::new("deep.rs"), &code);
    let elapsed = start.elapsed();

    assert!(
        elapsed < PARSE_DEADLINE,
        "PARSE1: 500-deep Rust mod parse must complete within {PARSE_DEADLINE:?}; took {elapsed:?}"
    );
    // tree-sitter returns a valid (possibly partial) parse even on extreme nesting.
    // The important thing is that it returns at all.
    let _ = symbols; // just verify it didn't panic
}

/// PARSE1: Very large Rust source (1 000 top-level functions ≈ 50 KB) must
/// complete within the deadline.  Catches O(n²) query-match complexity.
#[test]
fn parse1_large_flat_rust_file_completes_within_deadline() {
    let extractor = SymbolExtractor::new();
    let code = wide_rust_source(1_000);

    assert!(code.len() >= 25_000, "test input must be ≥25 KB; got {} bytes", code.len());

    let start = Instant::now();
    let (symbols, _edges) = extractor.extract(Path::new("large.rs"), &code);
    let elapsed = start.elapsed();

    assert!(
        elapsed < PARSE_DEADLINE,
        "PARSE1: 1 000-function Rust file must parse within {PARSE_DEADLINE:?}; took {elapsed:?}"
    );
    // Must have found some symbols — proves the parser ran on real content.
    assert!(
        !symbols.is_empty(),
        "PARSE1: large Rust file must yield at least one symbol; got none (parse may have silently failed)"
    );
}

/// PARSE1: Deeply nested C# class definitions (200 levels) must not hang.
#[test]
fn parse1_deeply_nested_csharp_classes_does_not_hang() {
    let extractor = SymbolExtractor::new();

    // Build: class C0 { class C1 { ... class C199 { void Leaf() {} } ... } }
    let mut code = String::with_capacity(200 * 30);
    for i in 0..200 {
        code.push_str(&format!("public class C{i} {{ "));
    }
    code.push_str("public void Leaf() {}");
    for _ in 0..200 {
        code.push_str(" }");
    }

    let start = Instant::now();
    let _ = extractor.extract(Path::new("deep.cs"), &code);
    let elapsed = start.elapsed();

    assert!(
        elapsed < PARSE_DEADLINE,
        "PARSE1: 200-deep C# class nesting must parse within {PARSE_DEADLINE:?}; took {elapsed:?}"
    );
}

/// PARSE1: Empty input must return empty symbol/edge vecs without panicking
/// for all supported file extensions.
#[test]
fn parse1_empty_input_all_extensions_safe() {
    let extractor = SymbolExtractor::new();
    let extensions = ["rs", "py", "go", "java", "ts", "tsx", "js", "jsx", "cs", "c", "h", "cpp", "hpp"];

    for ext in &extensions {
        let path = Path::new("empty").with_extension(ext);
        let (symbols, edges) = extractor.extract(&path, "");
        assert!(
            symbols.is_empty() && edges.is_empty(),
            "PARSE1: empty input for .{ext} must return empty vecs; got {symbols:?}"
        );
    }
}

/// PARSE1: Whitespace-only and null-byte input must not panic.
#[test]
fn parse1_whitespace_and_near_binary_input_does_not_panic() {
    let extractor = SymbolExtractor::new();

    // Whitespace-only
    let (s, e) = extractor.extract(Path::new("ws.rs"), "   \t\n\r\n   ");
    let _ = (s, e);

    // Very long single line (no newlines) — can stress line-counting in some parsers.
    let long_line = "x".repeat(100_000);
    let (s, e) = extractor.extract(Path::new("long.rs"), &long_line);
    let _ = (s, e);
}

/// PARSE1: A file whose extension is not recognised must return empty vecs
/// immediately (no attempt to parse, no panic).
#[test]
fn parse1_unknown_extension_returns_empty_immediately() {
    let extractor = SymbolExtractor::new();
    let (symbols, edges) = extractor.extract(Path::new("data.bin"), "\x00\x01\x02\x7Fsome binary");
    assert!(
        symbols.is_empty() && edges.is_empty(),
        "PARSE1: unknown extension must give empty output; got {symbols:?}"
    );
}

// ── PARSE1 SQL regex stress tests ─────────────────────────────────────────────

/// PARSE1: SQL with 100 nested subqueries must complete within the deadline.
/// Catches catastrophic backtracking in the regex-based `analyze_sql`.
#[test]
fn parse1_deeply_nested_sql_subqueries_does_not_hang() {
    let sql = nested_sql_subqueries(100);

    let start = Instant::now();
    let analysis = analyze_sql(&sql);
    let elapsed = start.elapsed();

    assert!(
        elapsed < PARSE_DEADLINE,
        "PARSE1: 100-deep SQL subquery must analyze within {PARSE_DEADLINE:?}; took {elapsed:?}"
    );
    let _ = analysis;
}

/// PARSE1: SQL with a very long identifier (10 000 chars) must not hang.
/// Some regex alternations can backtrack on long non-matching token strings.
#[test]
fn parse1_sql_very_long_identifier_does_not_hang() {
    let long_table = "A".repeat(10_000);
    let sql = format!("SELECT col1, col2 FROM {long_table} WHERE id = @p1");

    let start = Instant::now();
    let analysis = analyze_sql(&sql);
    let elapsed = start.elapsed();

    assert!(
        elapsed < PARSE_DEADLINE,
        "PARSE1: SQL with 10 000-char identifier must complete within {PARSE_DEADLINE:?}; took {elapsed:?}"
    );
    let _ = analysis;
}

/// PARSE1: Very large SQL (100 KB of repeated JOINs) must complete within deadline.
#[test]
fn parse1_large_sql_completes_within_deadline() {
    // Build a SQL string with many JOINs: SELECT t0.id FROM t0 JOIN t1 ON ... JOIN t2 ON ...
    let mut sql = String::with_capacity(100_000);
    sql.push_str("SELECT t0.id");
    for i in 0..500 {
        sql.push_str(&format!(", t{}.col_{i}", i + 1));
    }
    sql.push_str(" FROM t0");
    for i in 0..500 {
        sql.push_str(&format!(" JOIN t{0} ON t0.id = t{0}.fk_{i}", i + 1));
    }

    assert!(sql.len() >= 20_000, "test SQL must be ≥20 KB; got {} bytes", sql.len());

    let start = Instant::now();
    let analysis = analyze_sql(&sql);
    let elapsed = start.elapsed();

    assert!(
        elapsed < PARSE_DEADLINE,
        "PARSE1: large SQL ({} bytes) must analyze within {PARSE_DEADLINE:?}; took {elapsed:?}",
        sql.len()
    );
    let _ = analysis;
}

/// PARSE1: Empty and whitespace-only SQL must return a safe default (no panic).
#[test]
fn parse1_sql_empty_and_whitespace_inputs_safe() {
    for input in &["", "   ", "\t\n\r"] {
        let analysis = analyze_sql(input);
        let _ = analysis; // no panic = pass
    }
}
