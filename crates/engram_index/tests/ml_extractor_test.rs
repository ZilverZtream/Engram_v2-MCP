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
