//! Regression floor for the MiniLang extractor over a representative
//! composite of shapes drawn from the MiniLang standard library.
//!
//! This test calls `extract_ml` directly, so it exercises Tasks 2-7 (the
//! extractor itself) but NOT the Task 8 dispatch wiring in `hybrid.rs` --
//! there is no dispatch to bypass here. `generic_extractor_never_produced_ml_symbols_before_wiring`
//! below is the evidence for the wiring's effect: it shows what the
//! indexing pipeline's fallback extractor (the one every `.ml` file hit
//! before this task's dispatch arm existed) produces for the exact same
//! source -- nothing.

use engram_index::ml_extractor::extract_ml;
use engram_index::parsing::SymbolExtractor;
use std::path::Path;

const CORPUS: &str = r#"
' Representative MiniLang shapes: namespaces, generics, ADTs, methods,
' interfaces, FFI, concurrency, and a UI block.
Namespace Std
    Namespace Collections
        Public Type MapEntry Of K, V
            Key As K
            Value As V
        End Type

        Type ListError Implements Std.Error
            Operation As Str
            Index As Int
        End Type

        Function Message(this As Std.Collections.ListError) As Str
            Return this.Operation
        End Function

        Sub BTreeMap_Install Of K, V(BorrowMut target As Std.Collections.BTreeMap(Of K, V), Borrow replacement As Std.Collections.BTreeMap(Of K, V))
            Call Std.Memory.Copy(target, replacement)
        End Sub

        Function BTreeMap_Get Of K As Ordered, V(Borrow tree As Std.Collections.BTreeMap(Of K, V), key As K) As V Throws Std.Collections.BTreeLookupError
            Return Std.Collections.BTreeCore_Lookup Of K, V(tree, key)
        End Function
    End Namespace

    Interface Error
    End Interface
End Namespace

Type Shape
    Circle(radius As Int)
    Rectangle(w As Int, h As Int)
    Point
End Type

Enum Status
    Idle = 0
    Running = 1
End Enum

Unsafe(Ffi)
    Declare Function GetTickCount Lib "kernel32.dll" () As Int
End Unsafe

Function Producer(Borrow out As Channel(Of Int)) As Int
    Var i As Int
    For i = 1 To 5
        Send(out, i * i)
    Next
    Close(out)
    Return 0
End Function

Function Boot() As Int
    Unsafe(RawPtr)
        Say 1
    End Unsafe
    Spawn Detached Call Producer(0)
    Return 0
End Function

Ui Width 360 Height 184 Bg bg
  Panel
    Label
      Text "Ready"
    End Label
  End Panel
End Ui

Say Boot()
"#;

#[test]
fn corpus_composite_extracts_every_construct_family() {
    let (syms, edges) = extract_ml(Path::new("C:/proj/src/Corpus.ml"), "src/Corpus.ml", CORPUS);

    let count = |kind: &str| syms.iter().filter(|s| s.kind == kind).count();

    // Namespaces: Std, Std.Collections.
    assert_eq!(count("namespace"), 2, "namespaces");
    // Message, BTreeMap_Install, BTreeMap_Get, Producer, Boot, <module>.
    // BTreeMap_Install is a `Sub`, but `Sub` is emitted with kind
    // "function" (see decls.rs's `"Function" | "Sub" | "Func" =>` arm,
    // which is_sub-tags it in metadata rather than using a distinct
    // "sub" symbol kind) -- confirmed against the live extractor, not
    // assumed from the brief.
    assert_eq!(count("function"), 6, "functions");
    // MapEntry, ListError.
    assert_eq!(count("struct"), 2, "structs");
    // Shape -- via the implicit Type-with-variant-rows fallback, NOT the
    // explicit `Union` keyword (the corpus fixture has no `Union ... End
    // Union` block, so that construct family, real as it is, is not
    // exercised here). See the test-8 report for the fixture-coverage
    // note.
    assert_eq!(count("union"), 1, "unions (Shape)");
    assert_eq!(count("enum"), 1, "enums");
    assert_eq!(count("interface"), 1, "interfaces");
    assert_eq!(count("extern_function"), 1, "extern functions");
    assert_eq!(count("ui_container"), 1, "ui containers");
    assert_eq!(count("control"), 2, "ui controls (Panel, Label)");

    let ekind = |k: &str| edges.iter().filter(|e| e.kind == k).count();
    assert_eq!(ekind("implements_interface"), 1, "implements edges");
    assert_eq!(ekind("contains_ui"), 2, "contains_ui edges");
    assert!(ekind("calls") >= 5, "calls edges, got {}", ekind("calls"));

    // The Throws clause on BTreeMap_Get.
    assert!(
        edges.iter().any(|e| e.kind == "dependency"
            && e.target_name == "Std.Collections.BTreeLookupError"
            && e.metadata
                .as_ref()
                .and_then(|m| m.get("relation"))
                .map(String::as_str)
                == Some("throws")),
        "throws edge"
    );

    // Generic Sub declaration must be found despite the Of clause.
    assert!(
        syms.iter()
            .any(|s| s.name == "Std.Collections.BTreeMap_Install"),
        "generic Sub declaration must be extracted"
    );
}

/// Task-8 wiring evidence: this test exercises the SAME fallback path
/// every `.ml`/`.mlinc` file hit inside `hybrid.rs`'s `index_files`
/// BEFORE this task added the `Some("ml" | "mlinc")` dispatch arm --
/// `SymbolExtractor::extract`, the generic tree-sitter dispatcher, which
/// `hybrid.rs` still falls through to today for any extension with no
/// dedicated arm (its own `match ext { ... _ => return (vec![], vec![]) }`
/// has no "ml"/"mlinc" case and never will, since MiniLang has no
/// tree-sitter grammar -- see `ml_extractor/mod.rs`'s module doc).
///
/// Run against the exact same `CORPUS` fixture the composite test above
/// proves `extract_ml` populates richly, this demonstrates the "zero
/// symbols" half of the task brief's claim directly, rather than asserting
/// it from documentation. It is not itself a test of the dispatch arm
/// (that lives deep inside a rayon `par_iter` closure in a large async
/// indexing routine with no unit-test seam); it is the "before" side of
/// the before/after the dispatch arm's addition changes.
#[test]
fn generic_extractor_never_produced_ml_symbols_before_wiring() {
    let extractor = SymbolExtractor::new();
    let (syms, edges) = extractor.extract(Path::new("src/Corpus.ml"), CORPUS);
    assert!(
        syms.is_empty() && edges.is_empty(),
        "the generic fallback extractor has no MiniLang grammar and must \
         produce nothing for .ml content -- got {} symbols, {} edges",
        syms.len(),
        edges.len()
    );
}
