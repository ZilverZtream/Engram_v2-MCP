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
    // and miss 400+ similar generic declarations across the stdlib. Also
    // covers Step 5's `generic_params`/`access` metadata on the
    // Function/Sub arm, which until now was only exercised on `Type`.
    let src = "\
Public Function BTreeMap_Get Of K As Ordered, V(tree As Int, key As K) As V
    Return key
End Function
";
    let (syms, _) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "BTreeMap_Get");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("generic_params").map(String::as_str),
        Some("K:Ordered||V")
    );
    assert_eq!(m.get("access").map(String::as_str), Some("Public"));
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

#[test]
fn type_with_field_rows_is_a_struct() {
    let src = "\
Namespace Demo
    Type Point
        X As Int
        Y As Int
    End Type
End Namespace
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct")
        .expect("struct symbol");
    assert_eq!(t.name, "Demo.Point");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("fields").map(String::as_str), Some("X:Int||Y:Int"));
}

#[test]
fn type_with_variant_rows_is_a_union() {
    let src = "\
Type Shape
    Circle(radius As Int)
    Rectangle(w As Int, h As Int)
    Point
End Type
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "union")
        .expect("union symbol");
    assert_eq!(t.name, "Shape");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("variants").map(String::as_str),
        Some("Circle/1||Rectangle/2||Point/0")
    );
}

#[test]
fn mixed_body_rows_fall_back_to_struct_not_union() {
    // Mixed bodies (both field AND variant-shaped rows) never occur in
    // valid MiniLang -- a corpus survey of 204 .ml files found zero such
    // cases. This pins the documented fallback: any field row forces
    // `kind: "struct"` even when variant-shaped rows are also present, and
    // those variant-shaped rows are still recorded in `variants` metadata
    // rather than silently dropped. This is NOT a majority/dominance rule
    // -- a single field row wins over any number of variant rows.
    let src = "\
Type Mixed
    Name As Str
    Circle(radius As Int)
End Type
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct")
        .expect("struct symbol");
    assert_eq!(t.name, "Mixed");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("fields").map(String::as_str), Some("Name:Str"));
    assert_eq!(m.get("variants").map(String::as_str), Some("Circle/1"));
}

#[test]
fn empty_body_type_defaults_to_struct_with_no_metadata() {
    // `Type Foo` / `End Type` with no rows never occurs in the corpus, but
    // it is a well-defined fallback: no field or variant rows means both
    // metadata strings are empty, `meta()` filters them out entirely, and
    // the symbol still gets a reasonable default kind (`struct`) rather
    // than panicking or being misclassified as `union` from nothing.
    let src = "\
Type Foo
End Type
";
    let (syms, _) = run(src);
    let t = syms.iter().find(|s| s.name == "Foo").expect("Foo symbol");
    assert_eq!(t.kind, "struct");
    assert!(t.metadata.is_none(), "got metadata {:?}", t.metadata);
}

#[test]
fn function_type_field_row_is_correctly_classified_as_a_field() {
    // `Mapper As Function(T) As R` is a live grammar trap that DOES occur
    // in the real stdlib (e.g. `Std.Collections.List.Core.ml`'s
    // `ListMapCursor`/`ListFilterCursor`). `split_once(" As ")` splits at
    // the FIRST " As ", so `lhs = "Mapper"`, `rhs = "Function(T) As R"`;
    // since `lhs` has no `(`, it is a field, not a union variant, and the
    // full `Function(T) As R` is recorded as its type. This also exercises
    // a parenthesised generic type on a field's RHS
    // (`Std.Collections.List(Of T)`), which must land as a field too.
    let src = "\
Type ListMapCursor Of T, R
    Items As Std.Collections.List(Of T)
    Index As Int
    Mapper As Function(T) As R
End Type
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct")
        .expect("struct symbol");
    assert_eq!(t.name, "ListMapCursor");
    assert!(
        !syms
            .iter()
            .any(|s| s.kind == "function" && s.name.contains("Mapper")),
        "Mapper field must not be misread as a function declaration: {syms:?}"
    );
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("fields").map(String::as_str),
        Some("Items:Std.Collections.List(Of T)||Index:Int||Mapper:Function(T) As R")
    );
    assert_eq!(m.get("generic_params").map(String::as_str), Some("T||R"));
}

#[test]
fn generic_type_records_parameters_and_constraints() {
    let src = "\
Namespace Std
    Public Type BTreeMap Of K As Ordered, V As Droppable
        Handle As Int
    End Type
End Namespace
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct")
        .expect("struct symbol");
    assert_eq!(t.name, "Std.BTreeMap");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("generic_params").map(String::as_str),
        Some("K:Ordered||V:Droppable")
    );
    assert_eq!(m.get("access").map(String::as_str), Some("Public"));
}

#[test]
fn implements_clause_emits_an_edge() {
    let src = "\
Namespace Std
    Namespace Collections
        Type ListError Implements Std.Error
            Operation As Str
        End Type
    End Namespace
End Namespace
";
    let (syms, edges) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct")
        .expect("struct symbol");
    assert_eq!(t.name, "Std.Collections.ListError");

    let e = edges
        .iter()
        .find(|e| e.kind == "implements_interface")
        .expect("implements_interface edge");
    assert_eq!(e.source_name, "Std.Collections.ListError");
    assert_eq!(e.target_name, "Std.Error");
}

#[test]
fn enum_and_interface_declarations() {
    let src = "\
Enum Status
    Idle = 0
    Running = 1
End Enum
Interface IStream
End Interface
";
    let (syms, _) = run(src);
    let e = syms.iter().find(|s| s.kind == "enum").expect("enum symbol");
    assert_eq!(e.name, "Status");
    let m = e.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("members").map(String::as_str),
        Some("Idle=0||Running=1")
    );

    let i = syms
        .iter()
        .find(|s| s.kind == "interface")
        .expect("interface symbol");
    assert_eq!(i.name, "IStream");
}

#[test]
fn ref_and_weak_fields_are_marked_by_strength() {
    let src = "\
Type Node
    Parent As Weak(Of Node)
    Child As Ref(Of Node)
    Count As Int
End Type
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct")
        .expect("struct symbol");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("fields").map(String::as_str),
        Some("Parent:Weak(Of Node):weak||Child:Ref(Of Node):strong||Count:Int")
    );
}

#[test]
fn function_signature_records_params_return_and_throws() {
    let src = "\
Namespace Std
    Function BTreeMap_Get Of K As Ordered, V(Borrow tree As Std.BTreeMap(Of K, V), key As K) As V Throws Std.BTreeLookupError
        Return key
    End Function
End Namespace
";
    let (syms, edges) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "Std.BTreeMap_Get");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("generic_params").map(String::as_str),
        Some("K:Ordered||V")
    );
    assert_eq!(
        m.get("params").map(String::as_str),
        Some("borrow tree||owned key")
    );
    assert_eq!(m.get("returns").map(String::as_str), Some("V"));
    assert_eq!(
        m.get("throws").map(String::as_str),
        Some("Std.BTreeLookupError")
    );

    let e = edges
        .iter()
        .find(|e| e.kind == "dependency" && e.target_name == "Std.BTreeLookupError")
        .expect("throws edge");
    assert_eq!(e.source_name, "Std.BTreeMap_Get");
    assert_eq!(
        e.metadata
            .as_ref()
            .and_then(|m| m.get("relation"))
            .map(String::as_str),
        Some("throws")
    );
}

#[test]
fn nullable_return_and_borrowmut_params() {
    let src = "\
Function FirstPositive(BorrowMut buf As Bytes, x As Int) As Int?
    Return x
End Function
";
    let (syms, _) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("params").map(String::as_str),
        Some("borrow_mut buf||owned x")
    );
    assert_eq!(m.get("returns").map(String::as_str), Some("Int"));
    assert_eq!(m.get("nullable_return").map(String::as_str), Some("true"));
}

#[test]
fn function_type_annotation_is_not_a_declaration() {
    // `Mapper As Function(T) As R` is a FIELD, not a function declaration.
    let src = "\
Type ListMapCursor Of T, R
    Index As Int
    Mapper As Function(T) As R
End Type
";
    let (syms, _) = run(src);
    assert!(
        syms.iter().all(|s| s.kind != "function"),
        "field of function type must not register as a declaration, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );
    let t = syms
        .iter()
        .find(|s| s.kind == "struct")
        .expect("struct symbol");
    assert_eq!(t.name, "ListMapCursor");
}

#[test]
fn method_convention_links_this_param_to_its_type() {
    let src = "\
Namespace Std
    Type ListError
        Operation As Str
    End Type
    Function Message(this As Std.ListError) As Str
        Return this.Operation
    End Function
End Namespace
";
    let (_, edges) = run(src);
    let e = edges
        .iter()
        .find(|e| e.kind == "contains" && e.target_name == "Std.Message")
        .expect("method containment edge");
    assert_eq!(e.source_name, "Std.ListError");
}

#[test]
fn include_emits_a_file_edge_resolved_relative_to_the_includer() {
    let src = "Include \"Std.Collections.Typed.HashMaps.ml\"\n";
    let (_, edges) = extract_ml(
        Path::new("C:/proj/src/Libraries/Std.Collections.Typed.ml"),
        "src/Libraries/Std.Collections.Typed.ml",
        src,
    );
    let e = edges
        .iter()
        .find(|e| e.kind == "includes_file")
        .expect("includes_file edge");
    assert_eq!(
        e.target_name,
        "src/Libraries/Std.Collections.Typed.HashMaps.ml"
    );
    assert_eq!(e.target_kind.as_deref(), Some("file"));
}

#[test]
fn ffi_bindings_record_library_and_binding_style() {
    let src = "\
Unsafe(Ffi)
    Declare Function GetTickCount Lib \"kernel32.dll\" () As Int
    Extern \"C\" Blocking Function SlowOp Lib \"mylib.dll\" (x As Int) As Int
End Unsafe
";
    let (syms, _) = run(src);
    let externs: Vec<&engram_index::parsing::ExtractedSymbol> = syms
        .iter()
        .filter(|s| s.kind == "extern_function")
        .collect();
    assert_eq!(
        externs.len(),
        2,
        "got {:?}",
        externs.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    let tick = externs
        .iter()
        .find(|s| s.name == "GetTickCount")
        .expect("GetTickCount");
    let m = tick.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("binding").map(String::as_str), Some("pinvoke"));
    assert_eq!(m.get("library").map(String::as_str), Some("kernel32.dll"));
    // No `Alias` clause on this line -- the key must be ABSENT, not an
    // empty string, so downstream consumers can distinguish "no alias"
    // from "empty alias".
    assert_eq!(m.get("alias"), None, "got metadata {m:?}");

    let slow = externs.iter().find(|s| s.name == "SlowOp").expect("SlowOp");
    let m = slow.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("binding").map(String::as_str), Some("c_ffi"));
    assert_eq!(m.get("library").map(String::as_str), Some("mylib.dll"));
    assert_eq!(m.get("blocking").map(String::as_str), Some("true"));
}

#[test]
fn ffi_alias_clause_is_recorded_when_present() {
    // 171 corpus occurrences use this shape: `Declare Function Copy Lib
    // "intrinsic" Alias "bytes_copy" (b As Bytes) As Bytes`.
    let src = "\
Unsafe(Ffi)
    Declare Function Copy Lib \"intrinsic\" Alias \"bytes_copy\" (b As Bytes) As Bytes
End Unsafe
";
    let (syms, _) = run(src);
    let copy = syms
        .iter()
        .find(|s| s.kind == "extern_function" && s.name == "Copy")
        .expect("Copy");
    let m = copy.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("alias").map(String::as_str), Some("bytes_copy"));
    assert_eq!(m.get("library").map(String::as_str), Some("intrinsic"));
}

#[test]
fn public_declare_and_extern_ffi_bindings_are_still_extracted() {
    // 4 real files carry `Public Declare Function ...` /
    // `Public Extern "C" Function ...`. Neither `parse_ffi_binding` nor
    // `parse_const` stripped a leading access modifier, so these lines
    // silently produced no symbol at all.
    let src = "\
Unsafe(Ffi)
    Public Declare Function GetTickCount Lib \"kernel32.dll\" () As Int
    Public Extern \"C\" Function SlowOp Lib \"mylib.dll\" (x As Int) As Int
End Unsafe
";
    let (syms, _) = run(src);
    let externs: Vec<&engram_index::parsing::ExtractedSymbol> = syms
        .iter()
        .filter(|s| s.kind == "extern_function")
        .collect();
    assert_eq!(
        externs.len(),
        2,
        "got {:?}",
        externs.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    for e in &externs {
        let m = e.metadata.as_ref().expect("metadata");
        assert_eq!(
            m.get("access").map(String::as_str),
            Some("Public"),
            "{}: got metadata {m:?}",
            e.name
        );
    }
}

#[test]
fn const_declarations_record_their_ctfe_expression() {
    let src = "\
Namespace Demo
    Const WIDTH = 5 * 2
End Namespace
";
    let (syms, _) = run(src);
    let c = syms
        .iter()
        .find(|s| s.kind == "constant")
        .expect("constant symbol");
    assert_eq!(c.name, "Demo.WIDTH");
    assert_eq!(
        c.metadata
            .as_ref()
            .and_then(|m| m.get("value"))
            .map(String::as_str),
        Some("5 * 2")
    );
}

#[test]
fn const_inside_a_function_body_is_not_promoted_to_a_symbol() {
    // MiniLang allows a `Const` inside a function body for CTFE-friendly
    // local declarations (real corpus file:
    // `tests/conformance/generics/test_mlh2380_const_ctfe.ml`'s
    // `Const LOCAL = 6 * 7`). It is local to the function's execution, not
    // a project-level declaration, so it must never be promoted to a
    // `constant` symbol under any enclosing scope.
    let src = "\
Namespace Demo
    Function Compute() As Int
        Const LOCAL = 6 * 7
        Return LOCAL
    End Function
End Namespace
";
    let (syms, _) = run(src);
    assert!(
        !syms.iter().any(|s| s.kind == "constant"),
        "Const inside a function body must not emit a constant symbol, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );
}

#[test]
fn include_that_escapes_the_project_root_emits_no_edge() {
    // `tests/negative/includes/` in the target corpus exists precisely to
    // prove the MiniLang compiler REJECTS root-escaping includes. Before
    // the fix, `parts.pop()` on an empty Vec was a silent no-op, so extra
    // leading `..`s were simply dropped, resolving to a plausible-looking
    // but fabricated in-project target instead of no edge at all.
    let src = "Include \"../../../../../../../../Windows/win.ini\"\n";
    let (_, edges) = extract_ml(
        Path::new("C:/proj/src/Libraries/Std.Collections.Typed.ml"),
        "src/Libraries/Std.Collections.Typed.ml",
        src,
    );
    assert!(
        !edges.iter().any(|e| e.kind == "includes_file"),
        "root-escaping include must not fabricate a project-relative edge, got {edges:?}"
    );
}

#[test]
fn include_of_an_absolute_or_unc_path_emits_no_edge() {
    // Covers the drive-letter-absolute (also the alternate-data-stream
    // shape, `C:\...:hidden`, which is textually indistinguishable from a
    // plain absolute path at this parser's level) and UNC/device-namespace
    // adversarial cases from `tests/negative/includes/`.
    let src = "\
Include \"C:\\Windows\\win.ini\"
Include \"\\\\attacker.example\\share\\payload.ml\"
";
    let (_, edges) = extract_ml(
        Path::new("C:/proj/src/Libraries/Std.Collections.Typed.ml"),
        "src/Libraries/Std.Collections.Typed.ml",
        src,
    );
    assert!(
        !edges.iter().any(|e| e.kind == "includes_file"),
        "absolute/UNC includes must not fabricate a project-relative edge, got {edges:?}"
    );
}

#[test]
fn call_edges_attribute_to_the_enclosing_function() {
    let src = "\
Namespace Demo
    Function Helper(n As Int) As Int
        Return n
    End Function
    Function Main() As Int
        Return Helper(3)
    End Function
End Namespace
";
    let (_, edges) = run(src);
    let e = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Helper")
        .expect("calls edge");
    assert_eq!(e.source_name, "Demo.Main");
    // `source_start_line` is the STATEMENT's own line (the `Return
    // Helper(3)` line, line 6), not a sentinel and not the enclosing
    // function's start line -- house convention per `asp_classic_extractor.rs`.
    assert_eq!(e.source_start_line, 6);
}

#[test]
fn spawn_and_detached_are_flagged_on_the_call_edge() {
    let src = "\
Function Worker(n As Int) As Int
    Return n
End Function
Function Boot() As Int
    Spawn Call Worker(42)
    Spawn Detached Hi Call Worker(7)
    Return 0
End Function
";
    let (_, edges) = run(src);
    let spawns: Vec<&engram_index::parsing::ExtractedEdge> = edges
        .iter()
        .filter(|e| e.kind == "calls" && e.target_name == "Worker")
        .collect();
    assert_eq!(spawns.len(), 2, "expected two spawn call edges");

    assert!(spawns.iter().any(|e| {
        let m = e.metadata.as_ref().expect("metadata");
        m.get("spawn").map(String::as_str) == Some("true") && m.get("detached").is_none()
    }));
    assert!(spawns.iter().any(|e| {
        let m = e.metadata.as_ref().expect("metadata");
        m.get("detached").map(String::as_str) == Some("true")
            && m.get("priority").map(String::as_str) == Some("Hi")
    }));
}

#[test]
fn channel_and_simd_calls_carry_domain_metadata() {
    let src = "\
Function Pipe() As Int
    Var ch As Channel(Of Int) = NewChannel(Of Int)(4)
    Send(ch, 42)
    Var v As Vector256(Of Int32) = Std.Vector.Splat256 Of Int32(7i32)
    Return 0
End Function
";
    let (_, edges) = run(src);

    let send = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Send")
        .expect("Send call edge");
    assert_eq!(
        send.metadata
            .as_ref()
            .and_then(|m| m.get("concurrency"))
            .map(String::as_str),
        Some("channel")
    );

    let splat = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Std.Vector.Splat256")
        .expect("SIMD call edge");
    let m = splat.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("simd_width").map(String::as_str), Some("256"));
    assert_eq!(m.get("lane_type").map(String::as_str), Some("Int32"));
}

#[test]
fn unsafe_capability_block_emits_a_capability_edge() {
    let src = "\
Function Poke(p As Int) As Int
    Unsafe(RawPtr, Alloc)
        Set p^ To 42
    End Unsafe
    Return 0
End Function
";
    let (_, edges) = run(src);
    let e = edges
        .iter()
        .find(|e| e.kind == "dependency" && e.target_name == "Unsafe(RawPtr, Alloc)")
        .expect("capability edge");
    assert_eq!(e.source_name, "Poke");
    // `source_start_line` is the `Unsafe(...)` statement's own line (2),
    // not the enclosing function's start line (1).
    assert_eq!(e.source_start_line, 2);
    let m = e.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("relation").map(String::as_str), Some("capability"));
    assert_eq!(
        m.get("capabilities").map(String::as_str),
        Some("RawPtr||Alloc")
    );
}

#[test]
fn bare_unsafe_grants_all_capability() {
    // Every `Unsafe` block in the pre-existing tests is either the
    // parenthesized form or a top-level `Unsafe(Ffi)` wrapper with no
    // enclosing function (so the capability-edge owner lookup never
    // fires). The bare-`Unsafe` -> `All` branch was correct by inspection
    // but had zero test coverage before this.
    let src = "\
Function Poke2(p As Int) As Int
    Unsafe
        Set p^ To 42
    End Unsafe
    Return 0
End Function
";
    let (_, edges) = run(src);
    let e = edges
        .iter()
        .find(|e| e.kind == "dependency" && e.target_name == "Unsafe")
        .expect("bare capability edge");
    assert_eq!(e.source_name, "Poke2");
    let m = e.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("relation").map(String::as_str), Some("capability"));
    assert_eq!(m.get("capabilities").map(String::as_str), Some("All"));
}

#[test]
fn new_expression_type_constructors_do_not_emit_phantom_calls() {
    // Real corpus shape: `New Ref(Of T)(...)` places the type name right
    // after `New`, not after `As` -- the type-annotation-position guard
    // cannot fire there, so `TYPE_CONSTRUCTORS` is the ONLY thing
    // suppressing the phantom edge. `Ref` is also exercised in ordinary
    // type-annotation position on the same line (`Var first As Ref(Of
    // Node)`), and `Slice` pins the newly-added TYPE_CONSTRUCTORS entry
    // (corpus has 3 real `New Slice(...)` occurrences).
    let src = "\
Function Build() As Int
    Var first As Ref(Of Node) = New Ref(Of Node)(firstValue)
    Var buf As Slice(Of Byte) = New Slice(Of Byte)(16)
    Return 0
End Function
";
    let (_, edges) = run(src);
    assert!(
        !edges
            .iter()
            .any(|e| e.kind == "calls" && e.target_name == "Ref"),
        "New Ref(...) construction must not emit a phantom calls edge, got {edges:?}"
    );
    assert!(
        !edges
            .iter()
            .any(|e| e.kind == "calls" && e.target_name == "Slice"),
        "New Slice(...) construction must not emit a phantom calls edge, got {edges:?}"
    );
}

#[test]
fn as_position_guard_suppresses_a_name_outside_type_constructors() {
    // `Std.BTreeMap` (also used in `function_signature_records_params_return_and_throws`
    // and `generic_type_records_parameters_and_constraints`) is a real
    // generic stdlib type, but its last segment, "BTreeMap", is NOT in
    // `TYPE_CONSTRUCTORS`. Only the type-annotation-position guard (`… As
    // Foo(`) can suppress a phantom call edge to it here, which isolates
    // that guard: deleting it while leaving `TYPE_CONSTRUCTORS` untouched
    // would make this test fail.
    let src = "\
Function UseMap() As Int
    Var m As Std.BTreeMap(Of Int, Int)
    Return 0
End Function
";
    let (_, edges) = run(src);
    assert!(
        !edges
            .iter()
            .any(|e| e.kind == "calls" && e.target_name == "Std.BTreeMap"),
        "type annotation must not emit a phantom calls edge, got {edges:?}"
    );
}

#[test]
fn top_level_statements_get_a_module_entry_symbol() {
    let src = "\
Function Fib(n As Int) As Int
    Return n
End Function

Say Fib(15)
Say Fib(16)
";
    let (syms, edges) = run(src);
    let m = syms
        .iter()
        .find(|s| s.name == "Sample.<module>")
        .expect("module entry symbol");
    assert_eq!(m.kind, "function");
    assert_eq!(
        m.metadata
            .as_ref()
            .and_then(|x| x.get("synthetic"))
            .map(String::as_str),
        Some("module_entry")
    );
    // The synthetic symbol's line span must cover exactly the top-level
    // statements (lines 5-6), not the whole file and not just the first
    // top-level line.
    assert_eq!(
        m.start_line, 5,
        "expected span to start at the first top-level statement"
    );
    assert_eq!(
        m.end_line, 6,
        "expected span to end at the last top-level statement"
    );

    // The top-level call attributes to the module entry, not to nothing.
    let e = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Fib")
        .expect("top-level call edge");
    assert_eq!(e.source_name, "Sample.<module>");
}

#[test]
fn pure_declaration_files_get_no_module_entry() {
    let src = "\
Namespace Std
    Function Helper() As Int
        Return 0
    End Function
End Namespace
";
    let (syms, _) = run(src);
    assert!(
        syms.iter().all(|s| !s.name.ends_with("<module>")),
        "stdlib-style declaration-only files must not get a module entry"
    );
}

#[test]
fn function_records_local_binding_modes_and_fallible_regions() {
    let src = "\
Function Risky() As Int
    Dim fixed As Int
    Var counter As Int
    Mut total As Int
    Try
        Set total To counter
    Catch
        Set total To 0
    End Try
    Return total
End Function
";
    let (syms, _) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("immutable_locals").map(String::as_str), Some("fixed"));
    assert_eq!(
        m.get("mutable_locals").map(String::as_str),
        Some("counter||total")
    );
    assert_eq!(m.get("has_catch").map(String::as_str), Some("true"));
}
