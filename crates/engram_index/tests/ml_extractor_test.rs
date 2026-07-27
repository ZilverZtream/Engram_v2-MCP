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

#[test]
fn ui_block_produces_nested_containers_and_controls() {
    let src = "\
Ui Width 420 Height 160 Bg bg
  Panel
    Bg surface
    Rect 20 20 400 140 16
    Label
      Text \"Deployment status\"
      Rect 44 38 360 70 0
    End Label
    Badge
      Text \"Active\"
      Rect 44 84 134 112 0
    End Badge
  End Panel
End Ui
";
    let (syms, edges) = run(src);

    let root = syms
        .iter()
        .find(|s| s.kind == "ui_container" && s.name == "Sample.Ui")
        .expect("root Ui container");
    let m = root.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("width").map(String::as_str), Some("420"));
    assert_eq!(m.get("height").map(String::as_str), Some("160"));

    let label = syms
        .iter()
        .find(|s| s.kind == "control" && s.name.ends_with(".Label"))
        .expect("Label control");
    let lm = label.metadata.as_ref().expect("metadata");
    assert_eq!(
        lm.get("text").map(String::as_str),
        Some("Deployment status")
    );
    assert_eq!(lm.get("rect").map(String::as_str), Some("44 38 360 70 0"));
    assert_eq!(lm.get("element").map(String::as_str), Some("Label"));

    // Nesting is expressed as contains_ui edges parent -> child.
    assert!(
        edges.iter().any(|e| e.kind == "contains_ui"
            && e.source_name == "Sample.Ui"
            && e.target_name.ends_with(".Panel")),
        "Ui should contain Panel"
    );
    assert!(
        edges.iter().any(|e| e.kind == "contains_ui"
            && e.source_name.ends_with(".Panel")
            && e.target_name.ends_with(".Label")),
        "Panel should contain Label"
    );

    // Attribute folding must not bleed a child's own rows onto its
    // ancestor: Panel has no `Text` row of its own, so its metadata must
    // not pick up Label's "Deployment status" (or Badge's "Active").
    let panel = syms
        .iter()
        .find(|s| s.kind == "control" && s.name.ends_with(".Panel"))
        .expect("Panel control");
    let pm = panel.metadata.as_ref().expect("metadata");
    assert_eq!(pm.get("bg").map(String::as_str), Some("surface"));
    assert_eq!(pm.get("rect").map(String::as_str), Some("20 20 400 140 16"));
    assert_eq!(
        pm.get("text"),
        None,
        "Label's Text row must not bleed onto its parent Panel: {pm:?}"
    );

    // A purely declarative UI file must not fabricate a synthetic
    // `<module>` entry: every attribute row inside the Ui tree is folded
    // into its owning element and must never be misfiled as an orphaned
    // top-level statement.
    assert!(
        !syms.iter().any(|s| s.name.ends_with("<module>")),
        "UI-only file must not get a synthetic module entry, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );
}

#[test]
fn inline_asm_block_records_mnemonics_and_bindings() {
    // Real corpus grammar (verified against the compiler's own parser,
    // `Frontend/Parser.Statements.vb::ParseAsmLine`): `In`/`Out` rows are
    // BARE, comma-separated variable names with NO `As Type` clause --
    // the compiler hardcodes every asm in/out slot to `Int` itself
    // (`InlineAsmStmt` in `Core/AST/StatementNodes.Unsafe.vb` carries no
    // type field at all). This matches all 47 real `Asm`-bearing corpus
    // files, e.g. `tests/conformance/asm/test_asm_arithmetic.ml`'s
    // `In x, y` / `Out result`.
    let src = "\
Function Fast(x As Int, y As Int) As Int
    Asm
        In x, y
        Mov Rax, Rbx
        Add Rax, 1
        Out result
    End Asm
    Return x
End Function
";
    let (syms, _) = run(src);
    let a = syms
        .iter()
        .find(|s| s.kind == "inline_asm")
        .expect("inline_asm symbol");
    let m = a.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("mnemonics").map(String::as_str), Some("Mov||Add"));
    assert_eq!(m.get("inputs").map(String::as_str), Some("x||y"));
    assert_eq!(m.get("outputs").map(String::as_str), Some("result"));
    assert_eq!(m.get("owner").map(String::as_str), Some("Fast"));
    // The block's real span (End Asm is line 7), not a hand-counted
    // estimate -- the block contains no blank/comment lines here, but the
    // backfill mechanism must be exercised regardless.
    assert_eq!(a.start_line, 2);
    assert_eq!(a.end_line, 7);
}

#[test]
fn switch_is_a_ui_control_not_control_flow() {
    // MiniLang has no control-flow `Switch`/`Case`/`End Switch` construct
    // -- every real `Switch` in the corpus (7 `End Switch` occurrences
    // across 3 files, e.g. `examples/ui/declarative_switch_png.ml`) is a
    // toggle control. Before this fix `Switch` was tracked as inert
    // control flow (produced no symbol, no `contains_ui` edge), and --
    // more importantly -- correct parsing of the sibling `Label` after it
    // depended on `Switch` being on `ui::UI_ELEMENTS`'s nested-element
    // boundary list at all; this test also proves that boundary still
    // works once the sibling reopens.
    let src = "\
Ui Width 400 Height 220 Bg bg
  Panel
    Switch
      On 1
      Rect 300 84 356 112 0
    End Switch
    Label
      Text \"Dark mode\"
      Rect 44 86 250 110 0
    End Label
  End Panel
End Ui
";
    let (syms, edges) = run(src);

    let toggle = syms
        .iter()
        .find(|s| s.kind == "control" && s.name.ends_with(".Switch"))
        .expect("Switch control symbol");
    assert_eq!(
        toggle
            .metadata
            .as_ref()
            .and_then(|m| m.get("element"))
            .map(String::as_str),
        Some("Switch")
    );

    assert!(
        edges.iter().any(|e| e.kind == "contains_ui"
            && e.source_name.ends_with(".Panel")
            && e.target_name.ends_with(".Switch")),
        "Panel should contain Switch, got {edges:?}"
    );

    // The Label AFTER the Switch must still be correctly nested under the
    // same Panel -- proof the scanner's stack was never desynced by
    // Switch's `End Switch` line.
    let label = syms
        .iter()
        .find(|s| s.kind == "control" && s.name.ends_with(".Label"))
        .expect("Label control symbol");
    assert!(
        label.name.contains(".Panel."),
        "Label must nest under Panel, got {}",
        label.name
    );
    assert!(
        edges.iter().any(|e| e.kind == "contains_ui"
            && e.source_name.ends_with(".Panel")
            && e.target_name.ends_with(".Label")),
        "Panel should contain Label, got {edges:?}"
    );
}

#[test]
fn ui_header_window_modifier_does_not_shift_attribute_pairs() {
    // Real corpus shape (13 of the 58 files with a `Ui` header, e.g.
    // `examples/ui/declarative_window_png.ml`): a bare `Window` flag with
    // no value of its own sits between `Ui` and the first real key/value
    // pair. Naively pairing tokens two-at-a-time from `Ui` onward reads
    // `("window", "Width")`, `("360", "Height")`, ... -- garbage.
    let src = "Ui Window Width 360 Height 220 Bg bg\nEnd Ui\n";
    let (syms, _) = run(src);
    let root = syms
        .iter()
        .find(|s| s.kind == "ui_container")
        .expect("root Ui container");
    let m = root.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("width").map(String::as_str), Some("360"));
    assert_eq!(m.get("height").map(String::as_str), Some("220"));
    assert_eq!(m.get("bg").map(String::as_str), Some("bg"));
    // Windowed vs headless is real semantic content -- it must be
    // recorded, not silently discarded.
    assert_eq!(m.get("window").map(String::as_str), Some("true"));
}

#[test]
fn define_style_block_inside_ui_does_not_corrupt_sibling_nesting() {
    // `Define Style <name> ... End Style` (the "mcss" style-bundle
    // feature, real corpus construct: 8 files under
    // `tests/conformance/ui/` and `tests/drafts/`) is untracked by
    // `BLOCK_KEYWORDS`. Its `End Style` line would otherwise pop
    // whatever's actually on top of the scanner's stack -- corrupting
    // every UI element that follows it in the same file. This test does
    // not assert anything about the style bundle's own properties (out of
    // scope); it only proves the scanner survives the construct and keeps
    // nesting the real UI elements correctly.
    let src = "\
Ui Width 240 Height 240 Bg bg
  Define Style boxy
    MinSize 150 80
    Margin 5 6 7 8
  End Style
  Panel
    Bg surface
    Rect 10 10 230 230 8
  End Panel
End Ui
";
    let (syms, edges) = run(src);

    let root = syms
        .iter()
        .find(|s| s.kind == "ui_container" && s.name == "Sample.Ui")
        .expect("root Ui container");
    assert_eq!(root.end_line, 10, "Ui's real End Ui is line 10");

    let panel = syms
        .iter()
        .find(|s| s.kind == "control" && s.name.ends_with(".Panel"))
        .expect("Panel must still be extracted");
    assert_eq!(panel.name, "Sample.Ui.Panel", "Panel must nest under Ui");

    assert!(
        edges.iter().any(|e| e.kind == "contains_ui"
            && e.source_name == "Sample.Ui"
            && e.target_name.ends_with(".Panel")),
        "Ui should still contain Panel despite the intervening Define Style block, got {edges:?}"
    );
}

#[test]
fn struct_field_named_label_does_not_fabricate_a_ui_control() {
    // Real corpus shape (8 files under `tests/conformance/` --
    // `abi/`, `arc/` (x5), `optimizer/`, `syntax/` -- e.g.
    // `tests/conformance/arc/test_ref_nullable_arc.ml`): a `Type` field is
    // literally named `Label`, colliding with the `Ui` DSL's `Label`
    // element keyword. `block_opener("Label As Str")` matches `Label`
    // (the field NAME is the line's first token), so without a guard this
    // is indistinguishable from a genuine `Label` element opener -- it
    // would fabricate a `control` symbol that doesn't exist, and
    // `ui::ui_own_rows` would find no matching `End Label` and walk to
    // EOF hoovering up every remaining line in the file as a candidate
    // attribute row.
    let src = "\
Type Box
    Label As Str
End Type
Function After() As Int
    Return 1
End Function
";
    let (syms, _) = run(src);

    assert!(
        !syms
            .iter()
            .any(|s| s.kind == "control" || s.kind == "ui_container"),
        "a Type field named Label must never fabricate a UI symbol, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );

    let t = syms
        .iter()
        .find(|s| s.kind == "struct" && s.name == "Box")
        .expect("Box struct symbol");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("fields").map(String::as_str), Some("Label:Str"));

    // The sibling Function after the Type must still be parsed correctly
    // -- proof the scanner's stack was never desynced by the phantom
    // `Label` block that the old fallthrough would have pushed (and that
    // `ui::ui_own_rows`, if it had run, never ran away to EOF instead of
    // stopping at `End Type`).
    let f = syms
        .iter()
        .find(|s| s.kind == "function" && s.name == "After")
        .expect("After function symbol");
    assert_eq!(f.start_line, 4);
    assert_eq!(f.end_line, 6);
}

#[test]
fn define_style_bundle_rows_do_not_bleed_onto_the_enclosing_element() {
    // Real corpus shape (`tests/conformance/ui/test_ui_class_bg_headless.ml`
    // and 7 other files): a `Define Style` bundle's own attribute-shaped
    // rows (`Bg`, `Border`, ...) must not fold into the metadata of the
    // UI element it happens to be nested inside. `Border` is the
    // discriminating row here (unlike `Bg`, no `Ui` header ever sets
    // `border`, so a bleed would silently attach a bogus `border` value
    // to the root container as if it were the window's own).
    let src = "\
Ui Width 320 Height 200 Bg bg
  Define Style filled
    Bg accent
    Border accent 2
    MinSize 120 40
  End Style
  Panel
    Rect 10 10 300 180 8
  End Panel
End Ui
";
    let (syms, _) = run(src);
    let root = syms
        .iter()
        .find(|s| s.kind == "ui_container" && s.name == "Sample.Ui")
        .expect("root Ui container");
    let m = root.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("bg").map(String::as_str),
        Some("bg"),
        "got metadata {m:?}"
    );
    assert_eq!(
        m.get("border"),
        None,
        "the Define Style bundle's own Border row must not bleed onto Ui, got metadata {m:?}"
    );
}

#[test]
fn same_type_sibling_elements_get_distinct_identities() {
    // Real corpus shape (`examples/ui/declarative_switch_png.ml`: a
    // single `Panel` holds 4 `Label`s and 3 `Switch`es). FQNs built purely
    // from ancestry (`{parent}.{keyword}`) collapse every same-type
    // sibling into ONE name, and every `contains_ui` edge to them into
    // ONE (source, target) pair.
    let src = "\
Ui Width 300 Height 200 Bg bg
  Panel
    Label
      Text \"One\"
    End Label
    Label
      Text \"Two\"
    End Label
    Label
      Text \"Three\"
    End Label
  End Panel
End Ui
";
    let (syms, edges) = run(src);

    let labels: Vec<&engram_index::parsing::ExtractedSymbol> = syms
        .iter()
        .filter(|s| {
            s.kind == "control"
                && s.metadata
                    .as_ref()
                    .and_then(|m| m.get("element"))
                    .map(String::as_str)
                    == Some("Label")
        })
        .collect();
    assert_eq!(
        labels.len(),
        3,
        "expected 3 Label symbols, got {:?}",
        labels.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    let distinct_names: std::collections::HashSet<&str> =
        labels.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        distinct_names.len(),
        3,
        "same-type siblings must get distinct names, got {distinct_names:?}"
    );

    // Each Label's own Text must still fold correctly despite the
    // ordinal-suffixed fqn.
    let texts: std::collections::HashSet<Option<&str>> = labels
        .iter()
        .map(|s| {
            s.metadata
                .as_ref()
                .and_then(|m| m.get("text"))
                .map(String::as_str)
        })
        .collect();
    assert!(texts.contains(&Some("One")), "got {texts:?}");
    assert!(texts.contains(&Some("Two")), "got {texts:?}");
    assert!(texts.contains(&Some("Three")), "got {texts:?}");

    let label_edges: Vec<&engram_index::parsing::ExtractedEdge> = edges
        .iter()
        .filter(|e| e.kind == "contains_ui" && e.target_name.contains("Label"))
        .collect();
    assert_eq!(
        label_edges.len(),
        3,
        "expected 3 distinct contains_ui edges to Label children, got {label_edges:?}"
    );
    let distinct_targets: std::collections::HashSet<&str> =
        label_edges.iter().map(|e| e.target_name.as_str()).collect();
    assert_eq!(
        distinct_targets.len(),
        3,
        "contains_ui edges must target distinct Label fqns, got {distinct_targets:?}"
    );
}

#[test]
fn inline_type_method_gets_its_own_frame_not_swallowed_by_the_type() {
    // MLH-2080: `Type` bodies may declare methods with full bodies (real
    // corpus shape, `tests/negative/syntax/inline_type_method_reserved_this.ml`).
    // Round-1's fix for the `Label As Str` field-name collision (CRITICAL 1)
    // over-corrected: it unconditionally skipped EVERY block-opener match
    // inside a Type body, including a genuine `Function` declaration. Since
    // nothing was pushed for it, its own `End Function` popped the
    // enclosing `Type` frame instead -- truncating the Type's `end_line`
    // and desyncing the stack into the following `End Type`.
    let src = "\
Type Job
    value As Int

    Function Bad(this As Int) As Int
        Return this
    End Function
End Type
";
    let (syms, _) = run(src);

    let f = syms
        .iter()
        .find(|s| s.kind == "function" && s.name == "Job.Bad")
        .expect("inline method must produce its own function symbol");
    assert_eq!(f.start_line, 4);
    assert_eq!(f.end_line, 6, "the method's own End Function, not End Type");

    let t = syms
        .iter()
        .find(|s| s.name == "Job" && (s.kind == "struct" || s.kind == "union"))
        .expect("Type Job symbol");
    assert_eq!(
        t.end_line, 7,
        "the Type's real End Type line, not truncated by the inline method's End Function"
    );
}

#[test]
fn type_with_inline_methods_classifies_as_struct_with_methods_as_own_symbols() {
    // Real corpus shape (`tests/conformance/interfaces/test_mlh2080_type_inline_methods.ml`):
    // `Type BuildJob` declares 2 fields and 3 inline methods (MLH-2080).
    // Before this fix, `collect_block_body` handed the Type classifier
    // EVERY line inside the block, including each method's declaration
    // and statement body -- `Function Cost(extra As Int) As Int` was
    // misread as a variant (its `lhs` contains `(`) and `Return weight *
    // 2 + extra` as another, polluting `variants` with junk like
    // `Function/1`/`Return/0`. `collect_type_member_rows` now excludes a
    // method's declaration line and its whole body from the member rows
    // handed to the classifier.
    let src = "\
Namespace InlineMethodModel
    Interface IInlineJob
        Function Cost(extra As Int) As Int
        Function Label() As Str
    End Interface

    Type BuildJob Implements IInlineJob
        name As Str
        weight As Int

        Function Cost(extra As Int) As Int
            Return weight * 2 + extra
        End Function

        Function Label() As Str
            Return name
        End Function

        Function Shadow(weight As Int) As Int
            Return weight + this.weight
        End Function
    End Type
End Namespace
";
    let (syms, _) = run(src);

    // The bare Interface signature rows must not fabricate anything:
    // they collide with the same `Function` keyword but have no body and
    // no matching `End Function` of their own.
    let iface = syms
        .iter()
        .find(|s| s.kind == "interface" && s.name == "InlineMethodModel.IInlineJob")
        .expect("Interface symbol");
    assert_eq!(iface.end_line, 5);

    let t = syms
        .iter()
        .find(|s| s.name == "InlineMethodModel.BuildJob")
        .expect("BuildJob Type symbol");
    assert_eq!(t.kind, "struct");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("fields").map(String::as_str),
        Some("name:Str||weight:Int")
    );
    assert_eq!(
        m.get("variants"),
        None,
        "no method row must survive into variants metadata, got {m:?}"
    );

    for method in ["Cost", "Label", "Shadow"] {
        let fqn = format!("InlineMethodModel.BuildJob.{method}");
        assert!(
            syms.iter().any(|s| s.kind == "function" && s.name == fqn),
            "expected inline method symbol {fqn}, got {:?}",
            syms.iter()
                .filter(|s| s.kind == "function")
                .map(|s| &s.name)
                .collect::<Vec<_>>()
        );
    }
}

// ---------------------------------------------------------------------
// Task 6b: scanner keyword completeness (Union, Repeat, Func, For/Next,
// the Asm/Sub mnemonic collision, and the field access-modifier leak).
// ---------------------------------------------------------------------

#[test]
fn explicit_union_keyword_declares_a_union_with_generic_constraint() {
    // Real corpus shape (`tests/conformance/generics/test_mlh2020_generic_type_constraints.ml`):
    // MiniLang's EXPLICIT `Union` declaration keyword was completely
    // unmodeled -- only the `Type`-with-variant-rows fallback was. Before
    // this fix `Union` was not a block opener at all, so this block's own
    // body rows ("Some(value As T)", "None") fell through and were
    // misfiled as top-level program statements (fabricating a phantom
    // `Some` call edge on a synthetic `<module>` entry), and `End Union`
    // closed nothing (silently ignored on an empty stack) -- desyncing
    // every declaration after it in the file.
    let src = "\
Union OrderedChoice Of T As Ordered
    Some(value As T)
    None
End Union
Function After() As Int
    Return 1
End Function
";
    let (syms, _) = run(src);
    let u = syms
        .iter()
        .find(|s| s.kind == "union" && s.name == "OrderedChoice")
        .expect("union symbol");
    let m = u.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("variants").map(String::as_str),
        Some("Some/1||None/0")
    );
    assert_eq!(
        m.get("generic_params").map(String::as_str),
        Some("T:Ordered")
    );

    // No phantom top-level `<module>` entry or spurious call edge to the
    // variant constructor.
    assert!(
        !syms.iter().any(|s| s.name.ends_with("<module>")),
        "an explicit Union body must not fabricate a synthetic module entry, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );

    // The sibling Function after it must still parse correctly -- proof
    // the scanner's stack was never desynced by the Union block.
    let f = syms
        .iter()
        .find(|s| s.kind == "function" && s.name == "After")
        .expect("After function symbol");
    assert_eq!(f.start_line, 5);
    assert_eq!(f.end_line, 7);
}

#[test]
fn public_union_with_multi_variant_records_access_and_variants() {
    // Real corpus shape (`tests/conformance/integration/ultimate_showcase.ml`
    // line 43: `Public Union JobResult`, nested under `Namespace
    // Showcase.Model`). `Union` reuses the same
    // `access_modifier`/`generic_params`/`meta` helpers as every other
    // declaration kind, and emits `kind: "union"` -- the same kind the
    // `Type`-with-variant-rows fallback path already produces. This test
    // uses a single-segment namespace name (`Showcase`, not the real
    // file's dotted `Showcase.Model`): `declaration_name`'s
    // `take_while(is_alphanumeric || '_')` stops at `.`, so a DOTTED
    // namespace name is a separate, pre-existing gap unrelated to Union --
    // out of scope for this task, and not exercised here so it cannot
    // mask a Union-specific failure.
    let src = "\
Namespace Showcase
    Public Union JobResult
        Ok(value As Int)
        Failed(reason As Str)
    End Union
End Namespace
";
    let (syms, _) = run(src);
    let u = syms
        .iter()
        .find(|s| s.kind == "union" && s.name == "Showcase.JobResult")
        .expect("JobResult union symbol");
    let m = u.metadata.as_ref().expect("metadata");
    assert_eq!(m.get("access").map(String::as_str), Some("Public"));
    assert_eq!(
        m.get("variants").map(String::as_str),
        Some("Ok/1||Failed/1")
    );
}

#[test]
fn field_access_modifier_is_stripped_from_the_field_name() {
    // Real corpus shape (`tests/conformance/integration/ultimate_showcase.ml`
    // line 31): the field-row parser in `decls.rs` used a bare
    // `lhs.trim()`, so `Public name As Str` yielded the field `Public
    // name:Str` -- the access modifier leaked straight into the field
    // NAME -- instead of `name:Str`.
    let src = "\
Type BuildJob
    Public name As Str
    weight As Int
End Type
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct" && s.name == "BuildJob")
        .expect("BuildJob struct symbol");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("fields").map(String::as_str),
        Some("name:Str||weight:Int"),
        "the Public modifier must not leak into the field name, got {m:?}"
    );
}

#[test]
fn repeat_times_loop_is_balanced_and_produces_no_symbol() {
    // Real corpus shape (`tests/conformance/control-flow/test_repeat_counter.ml`):
    // `Repeat N Times ... End Repeat` was not tracked by `BLOCK_KEYWORDS`
    // at all (45 real `End Repeat` occurrences), so its `End Repeat`
    // closed nothing (silently ignored on an empty stack) while
    // everything declared after it in the enclosing scope desynced.
    let src = "\
Function Counter() As Int
    Var count As Int
    Set count To 0
    Repeat 10 Times
        Set count To count + 1
    End Repeat
    Return count
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
    assert_eq!(fns, vec!["Counter", "After"], "got symbols {syms:?}");
    assert!(
        !syms.iter().any(|s| s.name.contains("Repeat")),
        "Repeat must emit no symbol of its own, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );
}

#[test]
fn func_keyword_alternate_syntax_declares_a_function_with_arrow_return() {
    // Real corpus shape (`tests/drafts/seh_phase5_test.ml`): MiniLang's
    // alternate `Func Name(...) -> Type ... End Func` declaration syntax
    // (7 occurrences, all in `tests/drafts/`) was completely unmodeled --
    // `Func` was not a block opener, so its body statements fell through
    // as top-level statements and `End Func` closed nothing, desyncing
    // every declaration after it in the file. Also covers the colon-typed
    // parameter shape (`x: Int` rather than `x As Int`) used by this same
    // draft dialect: `parse_params` only ever reads the NAME, so it
    // already tolerates this without changes.
    let src = "\
Func DivideByZero(x: Int) -> Int
    Throw 999
    Return x
End Func
Func TestNestedTryCatch() -> Int
    Return 0
End Func
";
    let (syms, _) = run(src);
    let fns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        fns,
        vec!["DivideByZero", "TestNestedTryCatch"],
        "got symbols {syms:?}"
    );

    let f = syms
        .iter()
        .find(|s| s.name == "DivideByZero")
        .expect("DivideByZero function symbol");
    let m = f.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("params").map(String::as_str),
        Some("owned x"),
        "the colon-typed param name must still be captured, got {m:?}"
    );
    assert_eq!(
        m.get("returns").map(String::as_str),
        Some("Int"),
        "the arrow return clause must be captured, got {m:?}"
    );
}

#[test]
fn for_next_loops_including_nested_ones_stay_balanced() {
    // Real corpus shape (thousands of occurrences, e.g.
    // `benchmarks/bench_loop_sum.ml`'s `For i = 1 To 1000 ... Next`) --
    // MiniLang's dominant loop form. Before this fix `For` was not a block
    // opener and `Next` was not recognized as a closer at all, so the
    // common case "balanced by accident" (nothing pushed, nothing
    // popped). Naively adding `For` as an opener WITHOUT also making
    // `Next` close it would turn every one of these loops into a
    // permanently unclosed frame -- this test also covers nesting, which
    // would be the first thing to break from an off-by-one in that fix.
    let src = "\
Function SumGrid() As Int
    Var total As Int
    Set total To 0
    For i = 0 To 9
        For j = 0 To 9
            Set total To total + i * j
        Next
    Next
    Return total
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
    assert_eq!(fns, vec!["SumGrid", "After"], "got symbols {syms:?}");
    let sum = syms.iter().find(|s| s.name == "SumGrid").expect("SumGrid");
    assert_eq!(sum.start_line, 1);
    assert_eq!(sum.end_line, 10);
}

#[test]
fn for_each_loop_and_the_end_for_terminator_both_close_a_for_frame() {
    // `For Each value As T In collection ... Next` is the collection-loop
    // variant (`benchmarks/collections/bench_list_cache_scan.ml`).
    // Separately, `End For` (3 real occurrences, all under
    // `tests/negative/syntax/` and `tests/fuzz/` -- the compiler itself
    // REJECTS `End For` in favor of `Next`, e.g. `test_end_for.ml`'s own
    // comment: "Developer writes End For instead of Next") must still be
    // recognized as closing a `For` frame here: this scanner is a lenient
    // text scanner over real files on disk, not a validating parser, and
    // these files exist in the corpus regardless of what the compiler
    // itself accepts.
    let src = "\
Function ScanAll() As Int
    For Each value As Int In values.AsSlice()
        Say value
    Next
    For i = 1 To 5
        Say i
    End For
    Return 0
End Function
";
    let (syms, _) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "ScanAll");
    assert_eq!(f.start_line, 1);
    assert_eq!(f.end_line, 9);
}

#[test]
fn parallel_for_is_recognized_as_a_for_opener() {
    // Real corpus shape (`tests/conformance/optimizer/test_mlh2270_parallel_for_syntax.ml`,
    // 60 occurrences across 9 files): `Parallel For …`/`Parallel For Each
    // …` is MiniLang's explicit MIMD parallel-loop form, closed by the
    // same bare `Next` as an ordinary `For`. This was found DURING this
    // task's own corpus desync measurement, not in the original spec: a
    // naive fix that makes `Next` unconditionally close a `For` frame
    // (this task's item 4) is not enough on its own -- `block_opener`
    // must also recognize `Parallel For` as an opener, or nothing gets
    // pushed for it and its `Next` wrongly pops whatever real frame (here,
    // the enclosing `Function`) happens to be on top of the stack instead,
    // corrupting the function's `end_line` and desyncing the real `End
    // Function` after it into a silent no-op.
    let src = "\
Function RunLocalParallel() As Int
    Var output As Int[512]
    For index = 0 To 511
        Set output[index] To index
    Next
    Parallel For index = 0 To 511
        Set output[index] To output[index] Xor 3
    Next
    Return output[0]
End Function
";
    let (syms, _) = run(src);
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "RunLocalParallel");
    assert_eq!(f.start_line, 1);
    assert_eq!(f.end_line, 10, "the real End Function line");
}

#[test]
fn asm_sub_mnemonic_does_not_collide_with_the_sub_block_keyword() {
    // Real corpus shape (`tests/conformance/asm/test_asm_two_blocks.ml`):
    // `Sub Rbx, Rax` inside an `Asm` block matches `block_opener` as a
    // `Sub` DECLARATION (the mnemonic operand row starts with the exact
    // block keyword `Sub` followed by a space). Before this fix that
    // pushed a phantom frame, fabricated a `function` symbol named from
    // the operand, and desynced the following `End Asm`/`End Unsafe`.
    let src = "\
Unsafe
    Asm
        In a, b
        Out r2
        Sub Rbx, Rax
        Mov Rax, Rbx
    End Asm
End Unsafe
Function After() As Int
    Return 1
End Function
";
    let (syms, _) = run(src);
    assert!(
        !syms
            .iter()
            .any(|s| s.kind == "function" && s.name != "After"),
        "the Sub mnemonic row must not fabricate a function symbol, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );
    let asm = syms
        .iter()
        .find(|s| s.kind == "inline_asm")
        .expect("inline_asm symbol");
    let m = asm.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("mnemonics").map(String::as_str),
        Some("Sub||Mov"),
        "Sub must still be recorded as a mnemonic, got {m:?}"
    );

    // The sibling Function after the Unsafe/Asm block must still parse
    // correctly -- proof the scanner's stack was never desynced.
    let f = syms
        .iter()
        .find(|s| s.kind == "function" && s.name == "After")
        .expect("After function symbol");
    assert_eq!(f.start_line, 9);
    assert_eq!(f.end_line, 11);
}

// ---------------------------------------------------------------------
// Task 6b, fix round 1: the Try/Try-Call collision (in scope after
// review -- it accounted for ~98% of the corpus's residual desyncs), plus
// the two safety properties `closes_block` depends on.
// ---------------------------------------------------------------------

#[test]
fn try_call_single_statement_does_not_open_a_block() {
    // Real corpus shape (`src/MiniLangCompiler/Libraries/Std.Collections.Deque.ml:676`):
    // `Try Call X(...)` is a single-line fallible-call STATEMENT with no
    // body and no `End Try` of its own -- 187 corpus occurrences total
    // (186 `Try Call X(...)` + 1 bare `Try X(...)` with no `Call`).
    // Before this fix, `block_opener` treated ANY `Try` followed by a
    // space as opening a block (the same boundary check used for
    // `Unsafe(...)`/etc.), pushing a frame that nothing ever closed and
    // desyncing the very next real `End` line in the enclosing scope --
    // 22 real corpus files, all `Std.*` library files making heavy use of
    // `Throws`/fallible-call propagation.
    let src = "\
Sub Deque_Put(items As Int, index As Int, value As Int) Throws Std.Collections.DequeError
    Try Call Deque_SetRaw(items, index, value)
End Sub
Sub After() As Int
    Return 1
End Sub
";
    let (syms, edges) = run(src);
    let fns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(fns, vec!["Deque_Put", "After"], "got symbols {syms:?}");

    // The stack must not desync: After's own span must be its real one,
    // not corrupted by a phantom, never-closed Try frame from the
    // preceding Sub.
    let after = syms.iter().find(|s| s.name == "After").expect("After");
    assert_eq!(after.start_line, 4);
    assert_eq!(after.end_line, 6);

    // The fix must not silently swallow the statement -- its call edge
    // must still be extracted and attributed to the enclosing Sub.
    let e = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "Deque_SetRaw")
        .expect("calls edge to Deque_SetRaw");
    assert_eq!(e.source_name, "Deque_Put");
}

#[test]
fn bare_next_against_an_empty_stack_is_a_safe_no_op() {
    // Pins one of the two safety properties `closes_block` depends on: a
    // `Next` with nothing open on the stack must be silently ignored,
    // exactly like every other closer hitting an empty stack -- not panic,
    // not corrupt state, not fabricate a top-level statement entry.
    let src = "\
Next
Function After() As Int
    Return 1
End Function
";
    let (syms, _) = run(src);
    assert!(
        !syms.iter().any(|s| s.name.ends_with("<module>")),
        "an unmatched Next must not be misfiled as a top-level statement, got {:?}",
        syms.iter().map(|s| (&s.kind, &s.name)).collect::<Vec<_>>()
    );
    let f = syms
        .iter()
        .find(|s| s.kind == "function")
        .expect("function symbol");
    assert_eq!(f.name, "After");
    assert_eq!(f.start_line, 2);
    assert_eq!(f.end_line, 4);
}

#[test]
fn next_as_type_field_row_is_not_treated_as_a_for_closer() {
    // Real corpus shape (`Std.Collections.Map.Core.ml`'s `Next As Int`, a
    // linked-list node's own `Next` pointer field). Pins the second safety
    // property: `closes_block` matches the bare literal `"Next"` only (an
    // exact match, not a `starts_with("Next")` prefix check) specifically
    // so this field row is never mistaken for a `For`-loop closer -- a
    // future "simplification" to a prefix check would silently corrupt
    // every file with this field name.
    let src = "\
Type Node
    Value As Int
    Next As Int
End Type
Function After() As Int
    Return 1
End Function
";
    let (syms, _) = run(src);
    let t = syms
        .iter()
        .find(|s| s.kind == "struct" && s.name == "Node")
        .expect("Node struct symbol");
    let m = t.metadata.as_ref().expect("metadata");
    assert_eq!(
        m.get("fields").map(String::as_str),
        Some("Value:Int||Next:Int"),
        "the Next field must be classified as a field, not consumed as a For closer"
    );

    let f = syms
        .iter()
        .find(|s| s.kind == "function" && s.name == "After")
        .expect("After function symbol");
    assert_eq!(f.start_line, 5);
    assert_eq!(f.end_line, 7);
}

#[test]
fn golden_siblings_that_exist_produce_oracle_edges() {
    // The pairing stats the disk, so build a real temp layout.
    let dir = std::env::temp_dir().join("engram_ml_oracle_test");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let ml = dir.join("abi_deep_recursion.ml");
    std::fs::write(&ml, "Say 1\n").expect("write ml");
    std::fs::write(dir.join("abi_deep_recursion.expected"), "610\n").expect("write expected");

    let (_, edges) = extract_ml(
        &ml,
        "tests/conformance/abi/abi_deep_recursion.ml",
        "Say 1\n",
    );

    let e = edges
        .iter()
        .find(|e| e.kind == "test_oracle")
        .expect("test_oracle edge");
    assert_eq!(e.source_name, "tests/conformance/abi/abi_deep_recursion.ml");
    assert_eq!(
        e.target_name,
        "tests/conformance/abi/abi_deep_recursion.expected"
    );
    assert_eq!(e.target_kind.as_deref(), Some("file"));
    assert_eq!(
        e.metadata
            .as_ref()
            .and_then(|m| m.get("oracle"))
            .map(String::as_str),
        Some("expected")
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_golden_siblings_produce_no_phantom_edges() {
    let dir = std::env::temp_dir().join("engram_ml_oracle_none_test");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let ml = dir.join("lonely.ml");
    std::fs::write(&ml, "Say 1\n").expect("write ml");

    let (_, edges) = extract_ml(&ml, "tests/lonely.ml", "Say 1\n");
    assert!(
        edges.iter().all(|e| e.kind != "test_oracle"),
        "a .ml file with no golden must not mint a phantom oracle target"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn self_closing_function_on_one_line_does_not_leave_a_dangling_frame() {
    // Real corpus shape (`Std.UI.Events.ml:72-74`): a WHOLE `Function`
    // declaration written on one line, including its own `End Function`.
    // Before this fix, `block_opener` saw the `Function` opener and pushed
    // a frame; the mid-line `End Function` was invisible to `block_closer`
    // (which only recognizes a closer at the START of a trimmed line), so
    // the frame was never popped -- every subsequent declaration nested
    // under it, and the eventual `End Namespace` closed the WRONG frame (a
    // `debug_assert_eq!` catches this mismatch in test builds; release
    // builds silently corrupted the symbol graph instead).
    let src = "\
Namespace Demo
    Function FLAG_HOVER() As Int     Return 1 End Function ' Bit 0
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
    assert_eq!(
        fns,
        vec!["Demo.FLAG_HOVER", "Demo.After"],
        "got symbols {syms:?}"
    );

    let flag_hover = syms
        .iter()
        .find(|s| s.name == "Demo.FLAG_HOVER")
        .expect("FLAG_HOVER");
    assert_eq!(flag_hover.start_line, 2);
    assert_eq!(
        flag_hover.end_line, 2,
        "a self-closing block's span is just its own line"
    );

    // The stack must not desync: After's own span must be its real one,
    // not corrupted by a phantom, never-closed FLAG_HOVER frame.
    let after = syms.iter().find(|s| s.name == "Demo.After").expect("After");
    assert_eq!(after.start_line, 3);
    assert_eq!(after.end_line, 5);

    // The enclosing Namespace itself must close on the real `End
    // Namespace` line, not get popped early by the phantom frame.
    let ns = syms
        .iter()
        .find(|s| s.kind == "namespace" && s.name == "Demo")
        .expect("Demo namespace");
    assert_eq!(ns.end_line, 6);
}

#[test]
fn self_closing_function_body_call_is_not_swallowed() {
    // Real corpus shape (`Std.UI.Render.ml:796-798`): a self-closing
    // `Function`'s embedded body can itself contain a call
    // (`MakeColor(...)`). The fix must not just make the frame stack
    // balance -- it must not silently drop the body's own call edge
    // either, and it must not fabricate a bogus self-call from the
    // declaration's OWN `Name(...)` signature (`COLOR_NORMAL(` looks
    // exactly like a call to `COLOR_NORMAL` if the header were scanned
    // along with the body).
    let src = "\
Namespace Demo
    Function COLOR_NORMAL() As Int   Return MakeColor(200, 200, 200) End Function
End Namespace
";
    let (syms, edges) = run(src);
    let f = syms
        .iter()
        .find(|s| s.name == "Demo.COLOR_NORMAL")
        .expect("COLOR_NORMAL");
    assert_eq!(f.start_line, 2);
    assert_eq!(f.end_line, 2);

    let calls: Vec<&str> = edges
        .iter()
        .filter(|e| e.kind == "calls")
        .map(|e| e.target_name.as_str())
        .collect();
    assert_eq!(calls, vec!["MakeColor"], "got edges {edges:?}");
    assert_eq!(
        edges
            .iter()
            .find(|e| e.kind == "calls")
            .unwrap()
            .source_name,
        "Demo.COLOR_NORMAL"
    );
}

#[test]
fn if_then_one_liner_does_not_open_a_block() {
    // Real corpus shape (`Std.UI.ParserV2.ml:95-103`): MiniLang's one-line
    // conditional `If <cond> Then <statement>` (no `End If` of its own) is
    // mixed, inside the SAME function, with the ordinary multi-line block
    // form `If <cond> Then` (nothing after `Then` -- body and `End If` on
    // following lines). Before this fix, `block_opener` could not tell
    // them apart: it saw `If` at the start of EVERY one of these lines and
    // pushed a frame for the one-liner too, so the next real `End If`
    // closed the WRONG frame, leaving the outer block's `If` dangling
    // until the enclosing `End Function` mis-popped it instead.
    let src = "\
Namespace Demo
    Function IsAlpha(ch As Int) As Bool
        If ch >= 65 Then
            If ch <= 90 Then Return True
        End If
        If IsDigit(ch) == True Then Return True
        Return False
    End Function
    Function After() As Int
        Return 1
    End Function
End Namespace
";
    let (syms, edges) = run(src);
    let is_alpha = syms
        .iter()
        .find(|s| s.name == "Demo.IsAlpha")
        .expect("IsAlpha");
    assert_eq!(is_alpha.start_line, 2);
    assert_eq!(is_alpha.end_line, 8);

    // The stack must not desync: After's own span must be its real one,
    // not corrupted by a dangling `If` frame from the one-line form above.
    let after = syms.iter().find(|s| s.name == "Demo.After").expect("After");
    assert_eq!(after.start_line, 9);
    assert_eq!(after.end_line, 11);

    // The one-liner's own text must not be swallowed: its call is still
    // extracted and attributed to the enclosing function, exactly like
    // any other statement line.
    let call = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name == "IsDigit")
        .expect("calls edge to IsDigit");
    assert_eq!(call.source_name, "Demo.IsAlpha");
}

#[test]
fn bare_end_closes_whatever_is_innermost() {
    // Real corpus shape (`tests/drafts/seh_test.ml`): a pre-`End Try` SEH
    // draft closes a `Try` block with a lone `End` line instead of `End
    // Try`. `block_closer` cannot recognize a bare `End` at all --
    // `trimmed.strip_prefix("End")` leaves an empty remainder, and
    // `.strip_prefix(' ')` on an empty string returns `None` -- so before
    // this fix it was silently swallowed as an unrecognized statement, the
    // `Try` frame was never popped, and the real `End Sub` wrongly closed
    // IT instead.
    let src = "\
Sub TestException()
    Try
        Throw 42
    Catch ex
        Say 1
    End
    Say 2
End Sub
Sub Main()
    Say 3
End Sub
";
    let (syms, _) = run(src);
    let fns: Vec<&str> = syms
        .iter()
        .filter(|s| s.kind == "function")
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(fns, vec!["TestException", "Main"], "got symbols {syms:?}");

    let test_exception = syms
        .iter()
        .find(|s| s.name == "TestException")
        .expect("TestException");
    assert_eq!(test_exception.start_line, 1);
    assert_eq!(test_exception.end_line, 8);

    let main = syms.iter().find(|s| s.name == "Main").expect("Main");
    assert_eq!(main.start_line, 9);
    assert_eq!(main.end_line, 11);
}
