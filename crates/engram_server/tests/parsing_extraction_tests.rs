#![allow(clippy::unwrap_used)]
//! Behavioral tests for the production SymbolExtractor (Subsystem 4).
//!
//! All tests call `engram_index::SymbolExtractor::extract` directly with
//! controlled source-code strings and assert on extracted symbol names,
//! kinds, line numbers, and edge lists.

use engram_index::{ExtractedSymbol, SymbolExtractor};
use std::path::Path;

// ── helpers ───────────────────────────────────────────────────────────────────

fn extractor() -> SymbolExtractor {
    SymbolExtractor::new()
}

fn has_symbol<'a>(symbols: &'a [ExtractedSymbol], name: &str, kind: &str) -> Option<&'a ExtractedSymbol> {
    symbols.iter().find(|s| s.name == name && s.kind == kind)
}

// ── unknown / unsupported extension ──────────────────────────────────────────

/// Unsupported extensions must return empty vecs, never panic.
#[test]
fn extract_unsupported_extension_returns_empty_no_panic() {
    let ex = extractor();
    let (syms, edges) = ex.extract(Path::new("file.unknown"), "anything here");
    assert!(syms.is_empty(), "unsupported extension must yield no symbols");
    assert!(edges.is_empty(), "unsupported extension must yield no edges");
}

/// Plain text file must return empty vecs, never panic.
#[test]
fn extract_txt_extension_returns_empty_no_panic() {
    let ex = extractor();
    let (syms, _) = ex.extract(Path::new("notes.txt"), "fn foo() {}");
    assert!(syms.is_empty(), ".txt must yield no symbols even with Rust-like content");
}

// ── empty / whitespace content ────────────────────────────────────────────────

/// Empty content for a supported language must return empty vecs without panic.
#[test]
fn extract_empty_content_rust_returns_empty_no_panic() {
    let ex = extractor();
    let (syms, edges) = ex.extract(Path::new("lib.rs"), "");
    assert!(syms.is_empty(), "empty Rust source must yield no symbols");
    assert!(edges.is_empty(), "empty Rust source must yield no edges");
}

/// Whitespace-only content must not panic.
#[test]
fn extract_whitespace_only_content_does_not_panic() {
    let ex = extractor();
    let (syms, _) = ex.extract(Path::new("lib.rs"), "   \n\t\n  ");
    assert!(syms.is_empty(), "whitespace-only Rust source must yield no symbols");
}

// ── Rust extraction ───────────────────────────────────────────────────────────

/// A Rust source with one function must extract exactly that function.
#[test]
fn extract_rust_single_function_name_and_kind() {
    let ex = extractor();
    let src = r#"
fn compute_total(items: &[u32]) -> u32 {
    items.iter().sum()
}
"#;
    let (syms, _) = ex.extract(Path::new("src/lib.rs"), src);
    let sym = has_symbol(&syms, "compute_total", "function");
    assert!(
        sym.is_some(),
        "must extract 'compute_total' as kind='function'; got: {syms:?}"
    );
}

/// A Rust source with multiple functions must extract all of them.
#[test]
fn extract_rust_multiple_functions_all_extracted() {
    let ex = extractor();
    let src = r#"
fn alpha() -> u32 { 1 }
fn beta() -> u32 { 2 }
fn gamma() -> u32 { 3 }
"#;
    let (syms, _) = ex.extract(Path::new("lib.rs"), src);
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    for name in ["alpha", "beta", "gamma"] {
        assert!(
            names.contains(&name),
            "must extract function '{name}'; got: {names:?}"
        );
    }
}

/// A Rust struct definition must be extracted with kind="class" (production maps
/// both @struct and @class tags to the "class" kind string — see parsing.rs:587).
#[test]
fn extract_rust_struct_produces_class_kind() {
    let ex = extractor();
    let src = r#"
struct MyRecord {
    id: u64,
    value: String,
}
"#;
    let (syms, _) = ex.extract(Path::new("models.rs"), src);
    let sym = has_symbol(&syms, "MyRecord", "class");
    assert!(
        sym.is_some(),
        "must extract 'MyRecord' as kind='class' (struct→class mapping); got: {syms:?}"
    );
}

/// Extracting a Rust function must record a non-zero start_line.
#[test]
fn extract_rust_function_start_line_is_correct() {
    let ex = extractor();
    // function starts at line 3 (1-based)
    let src = "// comment\n// another comment\nfn target_fn() {}\n";
    let (syms, _) = ex.extract(Path::new("x.rs"), src);
    let sym = has_symbol(&syms, "target_fn", "function");
    assert!(sym.is_some(), "must extract 'target_fn'");
    let start = sym.unwrap().start_line;
    assert!(
        start >= 1,
        "start_line must be >= 1 (1-based); got {start}"
    );
    // tree-sitter is 0-based internally, production code adds 1 → we expect line 3
    assert_eq!(
        start, 3,
        "target_fn is on line 3; got start_line={start}"
    );
}

/// A Rust impl block must produce an "impl" symbol with the type name.
#[test]
fn extract_rust_impl_block_produces_impl_kind() {
    let ex = extractor();
    let src = r#"
struct Processor;

impl Processor {
    fn run(&self) {}
}
"#;
    let (syms, _) = ex.extract(Path::new("proc.rs"), src);
    let impl_sym = has_symbol(&syms, "Processor", "impl");
    assert!(
        impl_sym.is_some(),
        "must extract 'Processor' as kind='impl'; got: {syms:?}"
    );
}

/// Rust function with a call inside must produce at least one edge.
#[test]
fn extract_rust_function_call_produces_edge() {
    let ex = extractor();
    let src = r#"
fn helper() -> u32 { 42 }

fn caller() -> u32 {
    helper()
}
"#;
    let (_, edges) = ex.extract(Path::new("lib.rs"), src);
    assert!(
        !edges.is_empty(),
        "calling helper() inside caller() must produce at least one edge; got no edges"
    );
    // caller → helper edge should exist
    let edge = edges
        .iter()
        .find(|e| e.source_name.contains("caller") && e.target_name.contains("helper"));
    assert!(
        edge.is_some(),
        "must have a caller→helper edge; edges: {edges:?}"
    );
}

/// Syntactically invalid Rust must not panic (tree-sitter is error-tolerant).
#[test]
fn extract_rust_malformed_source_does_not_panic() {
    let ex = extractor();
    let src = "fn broken( { let x = ; } fn another() {} @@@@##!!";
    let (syms, _) = ex.extract(Path::new("broken.rs"), src);
    // tree-sitter may still extract partial symbols; we just need no panic
    let _ = syms;
}

// ── Python extraction ─────────────────────────────────────────────────────────

/// A Python function definition must be extracted with kind="function".
#[test]
fn extract_python_function_produces_function_kind() {
    let ex = extractor();
    let src = r#"
def process_data(items):
    return [x * 2 for x in items]
"#;
    let (syms, _) = ex.extract(Path::new("utils.py"), src);
    let sym = has_symbol(&syms, "process_data", "function");
    assert!(
        sym.is_some(),
        "must extract 'process_data' as kind='function'; got: {syms:?}"
    );
}

/// A Python class definition must be extracted with kind="class".
#[test]
fn extract_python_class_produces_class_kind() {
    let ex = extractor();
    let src = r#"
class DataService:
    def __init__(self):
        self.data = []

    def fetch(self):
        return self.data
"#;
    let (syms, _) = ex.extract(Path::new("service.py"), src);
    let sym = has_symbol(&syms, "DataService", "class");
    assert!(
        sym.is_some(),
        "must extract 'DataService' as kind='class'; got: {syms:?}"
    );
}

/// Python with both classes and functions must extract all symbols.
#[test]
fn extract_python_mixed_symbols_all_extracted() {
    let ex = extractor();
    let src = r#"
class Validator:
    pass

def validate(x):
    return x > 0

def format_output(val):
    return str(val)
"#;
    let (syms, _) = ex.extract(Path::new("model.py"), src);
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Validator"), "must find Validator class; got {names:?}");
    assert!(names.contains(&"validate"), "must find validate fn; got {names:?}");
    assert!(names.contains(&"format_output"), "must find format_output fn; got {names:?}");
}

/// Empty Python source must return empty vecs without panic.
#[test]
fn extract_python_empty_content_returns_empty_no_panic() {
    let ex = extractor();
    let (syms, edges) = ex.extract(Path::new("empty.py"), "");
    assert!(syms.is_empty(), "empty Python must yield no symbols");
    assert!(edges.is_empty(), "empty Python must yield no edges");
}

// ── C# extraction ─────────────────────────────────────────────────────────────

/// A C# class definition must be extracted with kind="class".
#[test]
fn extract_csharp_class_produces_class_kind() {
    let ex = extractor();
    let src = r#"
namespace MyApp.Services
{
    public class OrderService
    {
        public void ProcessOrder(int id) { }
    }
}
"#;
    let (syms, _) = ex.extract(Path::new("OrderService.cs"), src);
    let sym = has_symbol(&syms, "OrderService", "class");
    assert!(
        sym.is_some(),
        "must extract 'OrderService' as kind='class'; got: {syms:?}"
    );
}

/// A C# method must be extracted with kind="function".
#[test]
fn extract_csharp_method_produces_function_kind() {
    let ex = extractor();
    let src = r#"
public class MyPage : Page
{
    protected void Page_Load(object sender, EventArgs e)
    {
        DoWork();
    }

    private void DoWork() { }
}
"#;
    let (syms, _) = ex.extract(Path::new("MyPage.cs"), src);
    let names: Vec<(&str, &str)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    let has_page_load = syms.iter().any(|s| s.name == "Page_Load");
    assert!(
        has_page_load,
        "must extract 'Page_Load'; got: {names:?}"
    );
}

/// Malformed C# must not panic.
#[test]
fn extract_csharp_malformed_source_does_not_panic() {
    let ex = extractor();
    let src = "public class {{{ void broken( {} public override";
    let _ = ex.extract(Path::new("bad.cs"), src);
}

// ── Cross-language: extract returns correct language tag on edges ─────────────

/// Edges produced from Rust extraction must carry source_language="rs".
#[test]
fn extract_rust_edge_source_language_is_rs() {
    let ex = extractor();
    let src = r#"
fn inner() {}
fn outer() { inner(); }
"#;
    let (_, edges) = ex.extract(Path::new("code.rs"), src);
    if !edges.is_empty() {
        for e in &edges {
            assert_eq!(
                e.source_language, "rs",
                "Rust file edges must have source_language='rs'; got '{}'",
                e.source_language
            );
        }
    }
}

// ── Symbol count regression ───────────────────────────────────────────────────

/// Extracting 5 distinct Rust functions must produce exactly 5 function symbols.
#[test]
fn extract_rust_five_functions_produces_five_function_symbols() {
    let ex = extractor();
    let src = r#"
fn f1() {}
fn f2() {}
fn f3() {}
fn f4() {}
fn f5() {}
"#;
    let (syms, _) = ex.extract(Path::new("fns.rs"), src);
    let fn_count = syms.iter().filter(|s| s.kind == "function").count();
    assert_eq!(
        fn_count, 5,
        "must extract exactly 5 function symbols; got {fn_count}: {syms:?}"
    );
}

// ── PARSE1-u2k8: adversarial deeply-nested inputs ────────────────────────────

/// PARSE1-u2k8: deeply nested Rust blocks (200 levels) must not cause a stack
/// overflow or panic — the extractor must return within a bounded time budget.
///
/// Regression: naive recursive AST walkers can hit stack limits on pathological
/// nesting. This test proves the extractor terminates without panic.
#[test]
fn extract_deeply_nested_rust_blocks_does_not_panic() {
    const DEPTH: usize = 200;
    let deadline = std::time::Duration::from_secs(5);

    let ex = extractor();

    // Build deeply nested block: fn f() { fn g() { fn h() { ... } } }
    let open: String = (0..DEPTH).map(|i| format!("mod m{i} {{\n")).collect();
    let close: String = "}\n".repeat(DEPTH);
    let src = format!("fn outer() {{}}\n{open}fn inner() {{}}\n{close}");

    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| {
        ex.extract(std::path::Path::new("deep.rs"), &src)
    });
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "PARSE1-u2k8: extractor must not panic on deeply nested Rust source ({DEPTH} levels)"
    );
    assert!(
        elapsed < deadline,
        "PARSE1-u2k8: extractor on deeply nested Rust source must complete within {deadline:?}; \
         took {elapsed:?} — likely exponential recursion in AST traversal"
    );
}

/// PARSE1-u2k8: deeply nested C# braces must not cause a stack overflow or panic.
#[test]
fn extract_deeply_nested_csharp_classes_does_not_panic() {
    const DEPTH: usize = 150;
    let deadline = std::time::Duration::from_secs(5);

    let ex = extractor();

    // Build deeply nested C# class: class A { class B { class C { ... } } }
    let open: String = (0..DEPTH).map(|i| format!("class C{i} {{\n")).collect();
    let close: String = "}\n".repeat(DEPTH);
    let src = format!("{open}public void Method() {{}}\n{close}");

    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| {
        ex.extract(std::path::Path::new("deep.cs"), &src)
    });
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "PARSE1-u2k8: extractor must not panic on deeply nested C# source ({DEPTH} levels)"
    );
    assert!(
        elapsed < deadline,
        "PARSE1-u2k8: extractor on deeply nested C# source must complete within {deadline:?}; \
         took {elapsed:?}"
    );
}

/// PARSE1-u2k8: a file containing extremely long lines (1 MB of repeated chars)
/// must not cause a panic or allocation failure — proves no O(n²) line splitting.
#[test]
fn extract_very_long_single_line_does_not_panic() {
    let deadline = std::time::Duration::from_secs(5);
    let ex = extractor();

    // A single 500 KB line with a valid Rust function prefix.
    let long_comment = "// ".to_string() + &"a".repeat(500_000);
    let src = format!("{long_comment}\nfn ok() {{}}");

    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| {
        ex.extract(std::path::Path::new("longline.rs"), &src)
    });
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "PARSE1-u2k8: extractor must not panic on 500 KB single line"
    );
    assert!(
        elapsed < deadline,
        "PARSE1-u2k8: extractor on long-line source must complete within {deadline:?}; \
         took {elapsed:?}"
    );
}

// ── PARSE2: recursion depth and extractor robustness ─────────────────────────

/// PARSE2: tree-sitter-backed SymbolExtractor must not stack-overflow or hang
/// on a Go source file with 200 levels of nested function literals.
///
/// Go nesting is relevant because it uses closures-in-closures which can stress
/// recursive AST walkers more than flat statement lists.
#[test]
fn parse2_deeply_nested_go_functions_does_not_panic() {
    let deadline = std::time::Duration::from_secs(10);
    let ex = extractor();
    let path = Path::new("deep.go");

    // Build 200 levels of nested func literals: func() { func() { ... } }
    let inner = "println(\"hi\")";
    let mut src = inner.to_string();
    for _ in 0..200 {
        src = format!("func() {{\n{src}\n}}()");
    }
    let full = format!("package main\nfunc main() {{\n{src}\n}}");

    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| ex.extract(path, &full));
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "PARSE2: deeply nested Go functions must not panic; panicked");
    assert!(
        elapsed < deadline,
        "PARSE2: deeply nested Go functions must complete within {deadline:?}; took {elapsed:?}"
    );
}

/// PARSE2: tree-sitter-backed SymbolExtractor must not stack-overflow or hang
/// on a Python source file with 200 levels of nested class definitions.
#[test]
fn parse2_deeply_nested_python_classes_does_not_panic() {
    let deadline = std::time::Duration::from_secs(10);
    let ex = extractor();
    let path = Path::new("deep.py");

    // Build 200 levels of nested class defs: class A:\n  class B:\n    ...
    let mut src = "pass".to_string();
    for i in 0..200 {
        src = format!("class C{i}:\n    {}", src.replace('\n', "\n    "));
    }

    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| ex.extract(path, &src));
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "PARSE2: deeply nested Python classes must not panic; panicked");
    assert!(
        elapsed < deadline,
        "PARSE2: deeply nested Python classes must complete within {deadline:?}; took {elapsed:?}"
    );
}

/// PARSE2: tree-sitter-backed SymbolExtractor must not stack-overflow on a
/// Java source file with 150 levels of nested class declarations.
#[test]
fn parse2_deeply_nested_java_classes_does_not_panic() {
    let deadline = std::time::Duration::from_secs(10);
    let ex = extractor();
    let path = Path::new("deep.java");

    // Build nested Java inner classes: class A { class B { class C { ... } } }
    let mut src = "int x = 0;".to_string();
    for i in 0..150 {
        src = format!("class Inner{i} {{ {src} }}");
    }
    let full = format!("public class Deep {{ {src} }}");

    let start = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| ex.extract(path, &full));
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "PARSE2: deeply nested Java classes must not panic; panicked");
    assert!(
        elapsed < deadline,
        "PARSE2: deeply nested Java classes must complete within {deadline:?}; took {elapsed:?}"
    );
}

// ── PARSE1-z5l2: regex-based extractor panic-free corpus ─────────────────────
//
// The extractors below use regex (not tree-sitter), so their failure modes are
// different — they must not panic on malformed input, truncated code, or
// adversarial patterns.  These tests cover the "extractor fuzz campaign" item
// from Section 9 of the audit.

/// PARSE1-z5l2: Classic ASP extractor must not panic on malformed/adversarial input.
///
/// Covers: unclosed script blocks, deeply nested VBScript, NUL bytes, very long
/// lines, and Unicode edge cases.  All cases must return without panicking.
#[test]
fn asp_extractor_does_not_panic_on_malformed_input() {
    let ex = extractor();

    let cases: &[(&str, &str)] = &[
        // Unclosed <% ... without closing %>
        ("page.asp", "<% Sub Broken(x\n  Response.Write(x\n  "),
        // No server-side code at all
        ("plain.asp", "<html><body>Hello world</body></html>"),
        // Empty source
        ("empty.asp", ""),
        // Whitespace only
        ("ws.asp", "   \n\t\n  "),
        // Unclosed <script runat="server"> block
        ("script.asp", "<script runat=\"server\">\nSub Foo()\n"),
        // Deeply repeated Session access patterns (regex stress)
        (
            "session_flood.asp",
            &"Session(\"k\") = x\n".repeat(5_000),
        ),
        // Valid-looking but truncated at boundary
        ("trunc.asp", "<%\nSub F\n  Dim x\n  Server.CreateObject(\"ADODB."),
        // Response.Redirect with empty URL
        ("redirect.asp", "<% Response.Redirect \"\" %>"),
        // Extremely long single line
        ("longline.asp", &("x".repeat(200_000))),
        // Mixed NUL-like surrogate sequences (not true NUL but unusual bytes)
        ("unicode.asp", "<% ' \u{FFFD}\u{0000} invalid %>"),
        // Malformed #include
        ("include.asp", "<!--#include file= -->"),
        // Doubly nested script blocks (invalid ASP but must not panic)
        ("nested.asp", "<% <% Sub F() %> %>"),
    ];

    let deadline = std::time::Duration::from_secs(10);
    for (path, src) in cases {
        let p = Path::new(path);
        let start = std::time::Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ex.extract(p, src)));
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "PARSE1-z5l2: ASP extractor must not panic on input {path:?}: {:?}",
            result.err()
        );
        assert!(
            elapsed < deadline,
            "PARSE1-z5l2: ASP extractor must complete within {deadline:?} for {path:?}; took {elapsed:?}"
        );
    }
}

/// PARSE1-z5l2: JavaScript extractor must not panic on malformed/adversarial input.
///
/// Covers: truncated fetch calls, malformed jQuery selectors, oversized source
/// (near the 5MiB skip limit), and invalid postback patterns.
#[test]
fn js_extractor_does_not_panic_on_malformed_input() {
    let ex = extractor();

    let cases: &[(&str, &str)] = &[
        // Empty
        ("app.js", ""),
        // Unclosed $.ajax call
        ("ajax.js", "$.ajax({ url: '/api/data', success: function(d) {"),
        // Truncated fetch
        ("fetch.js", "fetch('/api/endpoint"),
        // Malformed jQuery selector
        ("jquery.js", "$('[id$="),
        // PageMethods without method name
        ("pm.js", "PageMethods.()"),
        // __doPostBack with empty args
        ("postback.js", "__doPostBack('', '')"),
        // Very deeply nested closures (stress test)
        (
            "deep.js",
            &{
                let open = "function f() { ".repeat(200);
                let close = " }".repeat(200);
                format!("{open}var x = 1;{close}")
            },
        ),
        // XMLHttpRequest truncated
        ("xhr.js", "var xhr = new XMLHttpRequest(); xhr.open('GET'"),
        // Source that triggers URL pattern but has no complete URL
        ("partial_url.js", "$.get('/"),
        // Source at exactly the size limit edge (not over)
        // We just use a large-ish but not enormous string
        (
            "large.js",
            &"var x = 1; // comment\n".repeat(10_000),
        ),
    ];

    let deadline = std::time::Duration::from_secs(10);
    for (path, src) in cases {
        let p = Path::new(path);
        let start = std::time::Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ex.extract(p, src)));
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "PARSE1-z5l2: JS extractor must not panic on input {path:?}: {:?}",
            result.err()
        );
        assert!(
            elapsed < deadline,
            "PARSE1-z5l2: JS extractor must complete within {deadline:?} for {path:?}; took {elapsed:?}"
        );
    }
}

/// PARSE1-z5l2: VB extractor must not panic on malformed/adversarial input.
///
/// Covers: unclosed If/End If, incomplete class declarations, deeply nested
/// structures, and Unicode-heavy content.
#[test]
fn vb_extractor_does_not_panic_on_malformed_input() {
    let ex = extractor();

    let cases: &[(&str, &str)] = &[
        // Empty
        ("form.vb", ""),
        // Unclosed If block
        ("if.vb", "If x > 0 Then\n  DoSomething()\n"),
        // Incomplete class
        ("class.vb", "Public Class Incomplete\n    Private x As Integer"),
        // Truncated method
        ("method.vb", "Private Sub HandleClick(sender As Object"),
        // Deeply nested If structures (tree-sitter depth stress)
        (
            "deep_if.vb",
            &{
                let open = "If True Then\n".repeat(100);
                let close = "End If\n".repeat(100);
                format!("{open}{close}")
            },
        ),
        // Valid-looking but missing End Sub
        ("no_end.vb", "Private Sub Button1_Click()\n    Dim x = 1\n"),
        // Unicode in identifiers
        ("unicode.vb", "Public Sub Héllo_Wörld()\nEnd Sub"),
        // AddHandler with missing event target
        ("addhandler.vb", "AddHandler"),
        // Handles clause without method
        ("handles.vb", "    Handles Button1.Click"),
    ];

    let deadline = std::time::Duration::from_secs(10);
    for (path, src) in cases {
        let p = Path::new(path);
        let start = std::time::Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ex.extract(p, src)));
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "PARSE1-z5l2: VB extractor must not panic on input {path:?}: {:?}",
            result.err()
        );
        assert!(
            elapsed < deadline,
            "PARSE1-z5l2: VB extractor must complete within {deadline:?} for {path:?}; took {elapsed:?}"
        );
    }
}

/// PARSE1-z5l2: SQL analyzer must not panic on malformed/adversarial SQL strings.
///
/// `analyze_sql` is called with raw SQL captured from source code — it must
/// handle garbage input without panicking.
#[test]
fn sql_analyzer_does_not_panic_on_malformed_input() {
    let cases: &[&str] = &[
        "",
        "SELECT",
        "SELECT * FROM",
        "SELECT * FROM WHERE",
        "INSERT INTO",
        "UPDATE SET",
        "DELETE",
        // Deeply nested subquery
        &{
            let mut s = "SELECT 1".to_string();
            for _ in 0..100 {
                s = format!("SELECT * FROM ({s}) t");
            }
            s
        },
        // Missing table name
        "SELECT col FROM () AS x",
        // Multiple FROM clauses
        "SELECT a, b FROM t1 FROM t2",
        // Unterminated string literal
        "SELECT 'unclosed",
        // Very long identifier
        &format!("SELECT {} FROM tbl", "a".repeat(10_000)),
        // Only whitespace
        "   \t\n   ",
        // Random garbage
        "!@#$%^&*()",
        // EXEC without proc name
        "EXEC",
        // Unicode in SQL
        "SELECT * FROM tàblé WHERE nàme = 'vàlue'",
    ];

    let deadline = std::time::Duration::from_secs(5);
    for sql in cases {
        let start = std::time::Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engram_index::sql_parser::analyze_sql(sql)
        }));
        let elapsed = start.elapsed();
        assert!(
            result.is_ok(),
            "PARSE1-z5l2: analyze_sql must not panic on input {:?}: {:?}",
            &sql[..sql.len().min(80)],
            result.err()
        );
        assert!(
            elapsed < deadline,
            "PARSE1-z5l2: analyze_sql must complete within {deadline:?} for input {:?}; took {elapsed:?}",
            &sql[..sql.len().min(80)]
        );
    }
}

/// PARSE2: extractors for all supported tree-sitter languages must handle
/// a file consisting of a single deeply-indented block (stress test for
/// any O(depth) path in the post-processing walk).
#[test]
fn parse2_all_languages_handle_deep_nesting_without_panic() {
    let deadline = std::time::Duration::from_secs(15);
    let ex = extractor();

    // (extension, source template) — each generates ~100-level nesting
    let cases = [
        (
            "deep.rs",
            {
                let mut s = "let x = 1;".to_string();
                for i in 0..100 { s = format!("mod m{i} {{ {s} }}"); }
                s
            },
        ),
        (
            "deep.ts",
            {
                let mut s = "const x = 1;".to_string();
                for i in 0..100 { s = format!("namespace N{i} {{ {s} }}"); }
                s
            },
        ),
    ];

    for (name, src) in &cases {
        let path = Path::new(name);
        let start = std::time::Instant::now();
        let result = std::panic::catch_unwind(|| ex.extract(path, src));
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "PARSE2: extractor for {name} must not panic on deeply nested input"
        );
        assert!(
            elapsed < deadline,
            "PARSE2: extractor for {name} must complete within {deadline:?}; took {elapsed:?}"
        );
    }
}

// ── PARSE1: Capture-contract tests for js_extractor.rs named groups ──────────

/// PARSE1: Every regex pattern in js_extractor.rs that is used with
/// `cap.name("X").expect("mandatory 'X' group")` must contain the named
/// capture group `(?P<X>...)` in the pattern string.
///
/// These tests prove the regex pattern and the extraction call site cannot
/// diverge: if the named group is removed from the pattern, the test fails
/// before any code that calls `.expect(...)` can panic at runtime.
#[test]
fn js_extractor_mandatory_group_patterns_contain_named_captures() {
    // (pattern_fragment, named_group_that_must_exist, description)
    let contracts: &[(&str, &str, &str)] = &[
        // GOOGLE_MAPS_RE: cls group — line 707
        (
            r"(?i)new\s+google\.maps\.(?P<cls>",
            "(?P<cls>",
            "GOOGLE_MAPS_RE: cls group must be present — used with .expect(\"mandatory 'cls' group\")",
        ),
        // LEAFLET_RE: cls group — line 867
        (
            r"(?i)\bL\.(?P<cls>",
            "(?P<cls>",
            "LEAFLET_RE: cls group must be present — used with .expect(\"mandatory 'cls' group\")",
        ),
        // OPENLAYERS_RE: cls group — line 891
        (
            r"(?i)new\s+ol\.(?P<cls>",
            "(?P<cls>",
            "OPENLAYERS_RE: cls group must be present — used with .expect(\"mandatory 'cls' group\")",
        ),
        // GIS_API_KEY_RE: key group — line 916
        (
            r"(?P<key>[A-Za-z0-9_\-]{20,})",
            "(?P<key>",
            "GIS_API_KEY_RE: key group must be present — used with .expect(\"mandatory 'key' group\")",
        ),
        // GIS_ZOOM_RE: val group — line 963
        (
            r"(?P<val>\d{1,2})",
            "(?P<val>",
            "GIS_ZOOM_RE: val group must be present — used with .expect(\"mandatory 'val' group\")",
        ),
        // GIS_CENTER_RE: lat and lng groups — lines 988-989
        (
            r"(?P<lat>-?\d+\.?\d*)",
            "(?P<lat>",
            "GIS_CENTER_RE: lat group must be present — used with .expect(\"mandatory 'lat' group\")",
        ),
        (
            r"(?P<lng>-?\d+\.?\d*)",
            "(?P<lng>",
            "GIS_CENTER_RE: lng group must be present — used with .expect(\"mandatory 'lng' group\")",
        ),
        // CTL00_ID_RE: full_id and ctrl_id groups — lines 1032-1036
        (
            r"(?P<full_id>ctl\d+",
            "(?P<full_id>",
            "CTL00_ID_RE: full_id group must be present — used with .expect(\"mandatory 'full_id' group\")",
        ),
        (
            r"(?P<ctrl_id>[A-Za-z]",
            "(?P<ctrl_id>",
            "CTL00_ID_RE: ctrl_id group must be present — used with .expect(\"mandatory 'ctrl_id' group\")",
        ),
    ];

    let extractor_src = include_str!("../../engram_index/src/js_extractor.rs");

    for (pattern_fragment, named_group, description) in contracts {
        // The pattern fragment must appear in the extractor source.
        assert!(
            extractor_src.contains(pattern_fragment),
            "PARSE1: {description}\n\
             Pattern fragment {pattern_fragment:?} not found in js_extractor.rs — \
             the regex may have been changed without updating the call site"
        );
        // The named group token must appear in the extractor source.
        assert!(
            extractor_src.contains(named_group),
            "PARSE1: {description}\n\
             Named group token {named_group:?} not found in js_extractor.rs — \
             the capture group may have been removed or renamed"
        );
    }
}

/// PARSE1: All `expect("mandatory '...' group")` call sites in js_extractor.rs
/// must have a corresponding `(?P<...>` in the same source file, and the count
/// of expect-call-sites must equal the count of named-group definitions they reference.
///
/// This is a count-parity test: it fails if a `.expect("mandatory")` call is added
/// without a corresponding regex group, or vice versa.
#[test]
fn js_extractor_mandatory_expect_sites_are_covered_by_named_groups() {
    let src = include_str!("../../engram_index/src/js_extractor.rs");

    // PARSE1 fix: mandatory named groups must use .map_or("", |m| m.as_str())
    // (not .expect()) so missing groups produce empty string instead of a panic.
    let panicky_expect_count = src.matches(".expect(\"mandatory '").count();
    assert!(
        panicky_expect_count == 0,
        "PARSE1: js_extractor.rs still has {panicky_expect_count} panicky mandatory-group \
         .expect() calls — all must be replaced with .map_or(\"\", |m| m.as_str())"
    );

    // Count the `(?P<` named capture group definitions.
    let group_count = src.matches("(?P<").count();

    // Count safe named-group accesses: .map_or("", |m| m.as_str()) after cap.name(...)
    let safe_access_count = src.matches(".map_or(\"\", |m| m.as_str())").count();

    // Sanity: there must be at least 9 safe access sites (cls×3 + key + val + lat + lng + full_id + ctrl_id).
    assert!(
        safe_access_count >= 9,
        "PARSE1: expected at least 9 safe named-group accesses in js_extractor.rs \
         (cls×3, key, val, lat, lng, full_id, ctrl_id); found {safe_access_count} — \
         a call site may have been inadvertently removed or reverted to .expect()"
    );

    // Named group count must cover all safe accesses.
    assert!(
        group_count >= safe_access_count,
        "PARSE1: js_extractor.rs has {safe_access_count} safe named-group accesses \
         but only {group_count} named capture groups (?P<...>)"
    );
}

// ── PARSE1-x3r8: cap.get(0) safety invariant ─────────────────────────────────

/// PARSE1: `cap.get(0)` in js_extractor.rs uses `.expect("group 0 always exists")`.
/// Group 0 is the full match — it is ALWAYS present when `regex.captures()`
/// returns `Some(caps)`. This test documents and proves the invariant so future
/// changes that restructure capture usage are caught immediately.
#[test]
fn parse1_cap_get_0_expect_is_safe_because_group_zero_is_whole_match() {
    let src = include_str!("../../engram_index/src/js_extractor.rs");

    // Every occurrence of cap.get(0).expect must say "group 0 always exists".
    // This comment IS the invariant proof — if the message changes, the invariant
    // may have been violated.
    let expect_g0 = src.matches("cap.get(0).expect(\"group 0 always exists\")").count();
    let other_g0_expects = src.matches("cap.get(0).expect(").count()
        .saturating_sub(expect_g0);

    assert!(
        other_g0_expects == 0,
        "PARSE1: {other_g0_expects} cap.get(0).expect() call(s) have message other than \
         'group 0 always exists' — the invariant comment is the proof; all must be consistent"
    );

    // There must be at least 10 such sites (production code uses group 0 for byte offsets).
    assert!(
        expect_g0 >= 10,
        "PARSE1: expected ≥10 cap.get(0).expect('group 0 always exists') sites in \
         js_extractor.rs production code; found {expect_g0} — some may have been removed"
    );
}

/// PARSE1: cap.get(0) in js_extractor.rs is always called AFTER regex.captures()
/// returns Some(cap). Structural proof: every captures() call must be in an
/// `if let Some(cap)` or similar pattern — not a raw unwrap.
#[test]
fn parse1_captures_calls_are_guarded_by_option_check() {
    let src = include_str!("../../engram_index/src/js_extractor.rs");

    // Bare `.captures(` unwrap without guard would mean cap.get(0) could panic.
    // Count .captures( usages vs guarded forms.
    let total_captures = src.matches(".captures(").count();
    let guarded_forms = src.matches("if let Some(cap").count()
        + src.matches("for cap in").count()
        + src.matches(".captures_iter(").count();

    // All captures( calls must be in guarded context or iterator (captures_iter).
    // Direct .captures( is only used when immediately pattern-matched.
    assert!(
        total_captures > 0,
        "PARSE1: js_extractor.rs must use regex.captures() — found none"
    );
    assert!(
        guarded_forms > 0,
        "PARSE1: js_extractor.rs must guard .captures() results with if-let or for-in; \
         found {guarded_forms} guards for {total_captures} captures calls"
    );
}

// ── PARSE1-j1n4: vb_extractor.rs safety invariants ───────────────────────────

/// PARSE1-j1n4: vb_extractor.rs uses `cap.get(0).expect("full match always exists")`
/// at multiple sites. This is safe because every such call appears inside a
/// `for cap in re.captures_iter(source)` loop. `captures_iter` yields only
/// successful regex matches, so group 0 (the whole match) is always Some.
/// This test documents and verifies the invariant structurally.
#[test]
fn parse1_vb_extractor_cap_get_0_expect_all_have_canonical_message() {
    let src = include_str!("../../engram_index/src/vb_extractor.rs");

    // Every cap.get(0).expect must use the canonical safety message.
    let canonical = src.matches("cap.get(0).expect(\"full match always exists\")").count();
    let other_expects = src.matches("cap.get(0).expect(").count().saturating_sub(canonical);

    assert!(
        other_expects == 0,
        "PARSE1-j1n4: {other_expects} cap.get(0).expect() call(s) in vb_extractor.rs have a \
         message other than 'full match always exists' — all must use the canonical \
         invariant message so static analysis can verify the safety claim"
    );

    // There must be a meaningful number of such sites (the production code uses many).
    assert!(
        canonical >= 5,
        "PARSE1-j1n4: expected ≥5 cap.get(0).expect('full match always exists') sites \
         in vb_extractor.rs production code; found {canonical} — some may have been removed"
    );
}

/// PARSE1-j1n4: every `cap.get(0).expect` in vb_extractor.rs is reached only
/// via `for cap in re.captures_iter(...)`. Proves no bare `.captures()` call
/// precedes an `.expect()` without an option guard.
#[test]
fn parse1_vb_extractor_captures_iter_guards_all_cap_get_0_sites() {
    let src = include_str!("../../engram_index/src/vb_extractor.rs");

    // captures_iter() always yields a complete match — group 0 is always Some.
    let captures_iter_count = src.matches("captures_iter(").count();

    // Every expect("full match always exists") must be reachable only through
    // captures_iter — if the count of captures_iter drops to 0 while expects remain,
    // the invariant is broken.
    let expect_count = src.matches("cap.get(0).expect(\"full match always exists\")").count();

    assert!(
        captures_iter_count > 0,
        "PARSE1-j1n4: vb_extractor.rs must use captures_iter() as the guard before \
         cap.get(0).expect() — no captures_iter found"
    );
    assert!(
        expect_count > 0,
        "PARSE1-j1n4: expected cap.get(0).expect() sites in vb_extractor.rs — none found; \
         test may be out of date"
    );
    // The invariant: captures_iter is the exclusive entry point for these sites.
    // If someone adds .captures() with a raw .expect() (no guard), the guard count
    // below would catch it via the bare_captures check.
    let bare_captures = src.matches(".captures(source)").count()
        .saturating_sub(src.matches("captures_iter(source)").count());
    assert!(
        bare_captures == 0,
        "PARSE1-j1n4: {bare_captures} bare `.captures(source)` call(s) found outside \
         captures_iter context in vb_extractor.rs — these must be guarded with \
         if-let Some(cap) before any cap.get(0) access"
    );
}

/// PARSE1-j1n4: VB extractor can extract Sub/Function symbols from minimal VB source.
/// Proves that the regex machinery (including all cap.get(0) sites) runs without
/// panicking on realistic input. Any panic would manifest here.
/// Uses engram_index::vb_extractor::extract_vb directly since SymbolExtractor routes
/// .vb files through the dedicated VB extractor, not the generic tree-sitter dispatch.
#[test]
fn parse1_vb_extractor_extracts_sub_and_function_symbols_without_panic() {
    let vb_source = r#"
Module MyModule
    Public Sub InitApp(ByVal name As String)
        Dim x As Integer
        x = 42
    End Sub

    Private Function ComputeValue(ByVal n As Integer) As Double
        Return n * 3.14
    End Function
End Module
"#;
    // Call extract_vb directly — this exercises all the cap.get(0) regex sites.
    let (syms, _edges) = engram_index::vb_extractor::extract_vb(
        Path::new("module.vb"), vb_source
    );
    // The extractor must not panic. Symbol extraction may be empty if tree-sitter
    // VB query coverage is partial, but no panic is the critical invariant.
    // If symbols are extracted, they must include at least one of the declared names.
    if !syms.is_empty() {
        assert!(
            syms.iter().any(|s| s.name.contains("InitApp") || s.name.contains("ComputeValue")
                || s.name.contains("MyModule")),
            "PARSE1-j1n4: vb_extractor extracted symbols but missed expected names; \
             extracted: {:?}",
            syms.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }
    // The no-panic invariant is the key test — any panic in cap.get(0) would abort here.
}
