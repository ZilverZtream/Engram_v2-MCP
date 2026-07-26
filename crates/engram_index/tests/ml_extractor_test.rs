use engram_index::ml_extractor::extract_ml;
use std::path::Path;

/// Helper: run the extractor on a source string with a stable fake path.
fn run(
    src: &str,
) -> (
    Vec<engram_index::parsing::ExtractedSymbol>,
    Vec<engram_index::parsing::ExtractedEdge>,
) {
    extract_ml(Path::new("C:/proj/Sample.ml"), "Sample.ml", src)
}

#[test]
fn strips_comments_but_not_string_contents() {
    // All three comment markers are stripped; markers inside string
    // literals are preserved. MiniLang has no character-literal syntax,
    // so a bare ' is always a comment.
    let src = "\
Namespace Demo
    Function Marker() As Str
        Return \"a ' b # c // d\"
    End Function
End Namespace
' trailing comment
# hash comment
// slash comment
";
    let (syms, _) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "Demo.Marker");
}

#[test]
fn nested_namespaces_build_fqn() {
    let src = "\
Namespace Std
    Namespace Collections
        Function ListCount() As Int
            Return 0
        End Function
    End Namespace
End Namespace
";
    let (syms, _) = run(src);
    let ns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "namespace")
        .map(|s| s.name.as_str())
        .collect();
    assert!(ns.contains(&"Std"), "got namespaces {ns:?}");
    assert!(ns.contains(&"Std.Collections"), "got namespaces {ns:?}");

    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "Std.Collections.ListCount");
}

#[test]
fn control_flow_blocks_do_not_corrupt_nesting() {
    // End If / End While / End Try must be balanced by the scanner but
    // must not close the enclosing function.
    let src = "\
Namespace Demo
    Function Complex(n As Int) As Int
        If n > 0
            While n > 0
                Set n To n - 1
            End While
        End If
        Try
            Return n
        Finally
            Say n
        End Try
    End Function
    Function After() As Int
        Return 1
    End Function
End Namespace
";
    let (syms, _) = run(src);
    let fns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(fns, vec!["Demo.Complex", "Demo.After"]);

    let complex = syms
        .iter()
        .find(|s| s.name == "Demo.Complex")
        .expect("Complex");
    assert_eq!(complex.start_line, 2);
    assert_eq!(complex.end_line, 13);
}

#[test]
fn public_namespace_modifier_does_not_corrupt_fqn() {
    // `block_opener` strips a leading `Public`/`Private` before matching
    // the keyword, so it already recognizes `Public Namespace` as opening
    // a Namespace block. The `Namespace` arm in `decls.rs` must strip the
    // same modifier when extracting the name (via `declaration_name`), or
    // it silently produces an empty FQN and every nested declaration
    // attaches to the wrong ancestor.
    let src = "\
Public Namespace Demo
    Function Marker() As Int
        Return 0
    End Function
End Namespace
";
    let (syms, _) = run(src);
    let ns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "namespace")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(ns, vec!["Demo"], "got namespaces {ns:?}");

    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "Demo.Marker");
}

#[test]
fn generic_function_name_captured_before_of_clause() {
    // Generic parameters sit BETWEEN the declared name and the parameter
    // list: `Function BTreeMap_Get Of K, V(tree As Int, key As K) As V`.
    // A pattern demanding `name(` would miss this declaration entirely —
    // and miss 400+ similar generic declarations across the stdlib.
    let src = "\
Function BTreeMap_Get Of K, V(tree As Int, key As K) As V
    Return key
End Function
";
    let (syms, _) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "BTreeMap_Get");
}

#[test]
fn type_annotated_field_is_not_a_declaration() {
    // `As Function(T) As R` is a type annotation on a field, never a
    // declaration. `block_opener` anchors on the keyword being the FIRST
    // significant token on the line, so a field line beginning with its
    // own identifier (`Mapper`) must never be mistaken for a `Function`
    // block opener merely because the word "Function" appears later in
    // the line.
    let src = "\
Type Mapper2
    Mapper As Function(T) As R
End Type
";
    let (syms, _) = run(src);
    assert!(
        !syms.iter().any(|s| s.kind == "function"),
        "type annotation must not produce a function symbol: {syms:?}"
    );
}

#[test]
fn escaped_quote_in_string_does_not_expose_trailing_marker() {
    // The string contains an escaped double quote (`\"`) immediately
    // followed by a bare `'`. This is a smoke test at the `extract_ml`
    // level: it pins that a Return statement built from exactly this
    // construct never disturbs the enclosing function's boundaries or a
    // sibling declaration after it. The actual escape-handling contract —
    // that `strip_comment` must not truncate this line at all — is
    // pinned precisely by the unit tests in
    // `ml_extractor/mod.rs::tests`, because `extract_ml` only ever reads
    // a line's FIRST token, so a `strip_comment` regression confined to
    // a line's tail (as this one is) cannot change any symbol this crate
    // produces today; it's provably invisible at this level.
    let src = "\
Function Marker() As Str
    Return \"a\\\" ' not a comment\"
End Function
Function After() As Int
    Return 1
End Function
";
    let (syms, _) = run(src);
    let fns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(fns, vec!["Marker", "After"], "got symbols {syms:?}");

    let marker = syms.iter().find(|s| s.name == "Marker").expect("Marker");
    assert_eq!(marker.start_line, 1);
    assert_eq!(marker.end_line, 3);
}
