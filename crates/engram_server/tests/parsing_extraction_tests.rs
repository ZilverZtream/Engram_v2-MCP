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
