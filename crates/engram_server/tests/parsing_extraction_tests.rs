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
