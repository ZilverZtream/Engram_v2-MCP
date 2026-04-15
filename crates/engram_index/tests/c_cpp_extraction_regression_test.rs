use engram_index::SymbolExtractor;
use std::path::Path;

#[test]
fn test_cpp_in_class_special_member_patterns() {
    let code = r#"class Widget {
public:
    Widget() {}
    ~Widget() {}
    Widget& touch() { return *this; }
    Widget* build() { return nullptr; }
    int operator+(const Widget& rhs) { return 0; }
};
"#;

    let extractor = SymbolExtractor::new();
    let (symbols, edges) = extractor.extract(Path::new("widget.cpp"), code);

    assert!(
        symbols
            .iter()
            .any(|s| s.name == "Widget" && s.kind == "class" && s.start_line == 1),
        "expected class symbol on line 1. symbols={symbols:?}"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "touch" && s.kind == "function" && s.start_line == 5),
        "expected reference-declarator method extraction. symbols={symbols:?}"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "build" && s.kind == "function" && s.start_line == 6),
        "expected pointer-declarator method extraction. symbols={symbols:?}"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name.contains("operator") && s.kind == "function" && s.start_line == 7),
        "expected operator method extraction. symbols={symbols:?}"
    );
    assert!(
        edges.iter().any(|e| {
            e.kind == "contains"
                && e.source_name == "Widget"
                && e.target_name == "touch"
                && e.target_kind.as_deref() == Some("function")
        }),
        "expected contains edge Widget -> touch. edges={edges:?}"
    );
    assert!(
        edges.iter().any(|e| {
            e.kind == "contains"
                && e.source_name == "Widget"
                && e.target_name == "build"
                && e.target_kind.as_deref() == Some("function")
        }),
        "expected contains edge Widget -> build. edges={edges:?}"
    );
}

#[test]
fn test_cpp_template_qualified_method_definition_and_calls() {
    let code = r#"namespace ns {
template <typename T>
class Box {
public:
    void work();
};
}

void helper() {}

template <typename T>
void ns::Box<T>::work() {
    helper();
}
"#;

    let extractor = SymbolExtractor::new();
    let (symbols, edges) = extractor.extract(Path::new("box.cpp"), code);

    assert!(
        symbols
            .iter()
            .any(|s| s.name == "Box" && s.kind == "class" && s.start_line == 3),
        "expected class extraction inside namespace. symbols={symbols:?}"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "helper" && s.kind == "function" && s.start_line == 9),
        "expected free function with stable line. symbols={symbols:?}"
    );
    assert!(
        symbols
            .iter()
            .any(|s| s.name == "work" && s.kind == "function" && s.start_line == 12),
        "expected template-qualified method extraction. symbols={symbols:?}"
    );
    assert!(
        edges.iter().any(|e| {
            e.kind == "calls"
                && e.source_name == "work"
                && e.target_name == "helper"
                && e.target_kind.is_none()
        }),
        "expected calls edge from work to helper. edges={edges:?}"
    );
}
