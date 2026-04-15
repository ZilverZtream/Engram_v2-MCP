use engram_index::SymbolExtractor;
use std::path::Path;

#[test]
fn test_cpp_parsing() {
    let code = r#"
    class MyClass {
    public:
        void myMethod() {
            otherFunction();
        }
    };

    void otherFunction() {}
    "#;

    let extractor = SymbolExtractor::new();
    let (symbols, edges) = extractor.extract(Path::new("main.cpp"), code);

    println!("Symbols: {:?}", symbols);
    println!("Edges: {:?}", edges);

    assert!(
        symbols
            .iter()
            .any(|s| s.name == "MyClass" && s.kind == "class")
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "myMethod" && s.kind == "function")
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "otherFunction" && s.kind == "function")
    );

    // Check contains edge
    assert!(
        edges.iter().any(|e| e.source_name == "MyClass"
            && e.target_name == "myMethod"
            && e.kind == "contains"),
        "Should find contains edge from MyClass to myMethod. Found: {:?}",
        edges
    );
}

#[test]
fn test_cpp_calls_capture_direct_and_member_calls_across_extensions() {
    let code = r#"
class Worker {
public:
    void helper() {}
    void run() {
        helper();
        this->helper();
    }
};

void free_fn() {}
void drive() {
    free_fn();
}
"#;

    for ext in ["cpp", "hpp", "cc", "cxx", "hh"] {
        let path = format!("sample.{ext}");
        let (_symbols, edges) = SymbolExtractor::new().extract(Path::new(&path), code);

        let call_edges: Vec<_> = edges.iter().filter(|e| e.kind == "calls").collect();
        assert!(
            call_edges
                .iter()
                .any(|e| e.source_name == "run" && e.target_name == "helper"),
            "expected run -> helper calls edge for .{ext}; edges: {:?}",
            edges
        );
        assert!(
            call_edges
                .iter()
                .any(|e| e.source_name == "drive" && e.target_name == "free_fn"),
            "expected drive -> free_fn calls edge for .{ext}; edges: {:?}",
            edges
        );
    }
}

#[test]
fn test_literal_escaping() {
    use engram_index::escape_tantivy_literal;
    let code_query = r#"unsafe { *const char: &str }"#;
    let escaped = escape_tantivy_literal(code_query);
    // Tantivy QueryParser should NOT throw an error on this escaped string
    // and should treat it as a sequence of terms.
    assert!(escaped.contains(r"\{"));
    assert!(escaped.contains(r"\*"));
    assert!(escaped.contains(r"\:"));
    assert!(escaped.contains(r"\}"));
}
