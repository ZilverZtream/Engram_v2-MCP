#![allow(clippy::unwrap_used)]
//! `Internal` is a MiniLang access modifier and the extractor did not know it.
//!
//! `block_opener` and the declaration parsers strip only `Public ` and
//! `Private `, so `Internal Function Foo()` was not recognised as a
//! declaration OR as a block opener. Two consequences, both silent:
//!
//!   1. the function produces no symbol — it is invisible to the graph,
//!      to find_symbol_references, and to get_full_method_body;
//!   2. nothing is pushed for it, so its `End Function` pops the enclosing
//!      `Namespace` frame instead. Every declaration after the first
//!      `Internal` one in the file is then at the wrong nesting depth.
//!
//! Live evidence (engram.log, 2026-07-29 → 2026-08-17): 20,944
//! "block mismatch: closer does not match the innermost open block"
//! warnings, overwhelmingly `opened=Namespace closed=Function`, alongside
//! `-32603 "No method 'X' found in '<file>'"` responses for methods that
//! plainly exist.
//!
//! `Internal` is not a corner case: in MiniLangCompiler's stdlib
//! (src/MiniLangCompiler/Libraries, 17 files) there are 117 `Internal`
//! declarations against 41 `Public` ones.

use engram_index::ml_extractor::extract_ml;
use std::path::Path;

/// Shape taken from Std.Collections.BTree.Arena.ml, the file the log names
/// first: nested namespaces, an `Internal Function` with a `Requires`
/// clause, then an `Internal Sub`.
const SOURCE: &str = r#"' Arena ownership
Namespace Std
    Namespace Collections
        Internal Function BTreeArena_New() As Int Requires Unsafe(Alloc.Allocate)
            Unsafe(Alloc, RawPtr)
                Var owners As Int
                Return owners
            End Unsafe
        End Function

        Internal Sub BTreeArena_AddOwner(owners As Int)
            Set owners To owners + 1
        End Sub

        Public Function BTreeArena_Count() As Int
            Return 0
        End Function
    End Namespace
End Namespace
"#;

fn symbol_names(source: &str) -> Vec<String> {
    let (syms, _) = extract_ml(Path::new("C:/proj/src/Arena.ml"), "src/Arena.ml", source);
    syms.iter().map(|s| s.name.clone()).collect()
}

/// The declarations themselves must be extracted.
#[test]
fn internal_functions_and_subs_produce_symbols() {
    let names = symbol_names(SOURCE);

    assert!(
        names.iter().any(|n| n.ends_with("BTreeArena_New")),
        "an Internal Function must produce a symbol; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("BTreeArena_AddOwner")),
        "an Internal Sub must produce a symbol; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("BTreeArena_Count")),
        "the Public control case must still work; got {names:?}"
    );
}

/// The block stack must stay in sync, so declarations AFTER an `Internal`
/// one keep their real namespace scope. This is the half that corrupts a
/// whole file rather than just dropping one symbol.
#[test]
fn an_internal_declaration_does_not_desync_the_block_stack() {
    let names = symbol_names(SOURCE);

    let public_fn = names
        .iter()
        .find(|n| n.ends_with("BTreeArena_Count"))
        .unwrap_or_else(|| panic!("public function missing entirely; got {names:?}"));

    assert!(
        public_fn.contains("Std") && public_fn.contains("Collections"),
        "a declaration following an Internal one must keep its namespace \
         scope — got {public_fn:?}, which means the End Function of the \
         Internal declaration popped a Namespace frame"
    );
}

/// The modifier must not leak into the symbol name.
#[test]
fn the_modifier_is_not_part_of_the_name() {
    let names = symbol_names(SOURCE);
    assert!(
        !names.iter().any(|n| n.contains("Internal")),
        "the access modifier is not part of the identifier; got {names:?}"
    );
}

/// `Unique Type` is the second missing modifier — 95 corpus occurrences,
/// all in the standard library's ownership-carrying collection types. It
/// opens a real `Type … End Type` block, so missing it desyncs the stack
/// exactly the way a missing `Internal Function` does.
const UNIQUE_SOURCE: &str = r#"Namespace Std
    Namespace Collections
        Unique Type BTreeMap Of K As Droppable, V As Droppable Drop With BTreeMap_Dispose
            Internal Nodes As Std.Collections.List(Of Int)
            Internal Root As Int
        End Type

        Public Function BTreeMap_Count() As Int
            Return 0
        End Function
    End Namespace
End Namespace
"#;

#[test]
fn unique_types_are_extracted_and_keep_the_stack_balanced() {
    let names = symbol_names(UNIQUE_SOURCE);

    assert!(
        names.iter().any(|n| n.ends_with("BTreeMap")),
        "a Unique Type must produce a symbol; got {names:?}"
    );

    let after = names
        .iter()
        .find(|n| n.ends_with("BTreeMap_Count"))
        .unwrap_or_else(|| panic!("function after the Unique Type is missing; got {names:?}"));
    assert!(
        after.contains("Std") && after.contains("Collections"),
        "a declaration following a Unique Type must keep its namespace \
         scope — got {after:?}"
    );
}

/// `BorrowMut` is deliberately NOT treated as a strippable modifier: it
/// appears only on Interface method signatures, which are single-line rows
/// with no `End Function`. Treating it as one would turn each into an
/// unbalanced block opener and desync the Interface.
#[test]
fn borrowmut_interface_rows_do_not_open_blocks() {
    const SRC: &str = r#"Namespace Std
    Namespace IO
        Interface IStream
            Function CanRead() As Bool
            BorrowMut Function SeekResult(position As Int) As Int
            BorrowMut Function FlushResult() As Int
        End Interface

        Public Function OpenStream() As Int
            Return 0
        End Function
    End Namespace
End Namespace
"#;
    let names = symbol_names(SRC);
    let after = names
        .iter()
        .find(|n| n.ends_with("OpenStream"))
        .unwrap_or_else(|| panic!("function after the interface is missing; got {names:?}"));
    assert!(
        after.contains("Std") && after.contains("IO"),
        "the BorrowMut rows must not unbalance the Interface block — \
         got {after:?}"
    );
}

/// `Internal` is an access LEVEL, so it belongs in the `access` metadata
/// alongside Public/Private. `Unique` is an ownership qualifier on a type,
/// not a visibility level, and must not land there.
#[test]
fn access_metadata_records_internal_but_not_unique() {
    let (syms, _) = extract_ml(Path::new("C:/proj/src/Arena.ml"), "src/Arena.ml", SOURCE);
    let internal_fn = syms
        .iter()
        .find(|s| s.name.ends_with("BTreeArena_New"))
        .expect("internal function symbol");
    assert_eq!(
        internal_fn
            .metadata
            .as_ref()
            .and_then(|m| m.get("access"))
            .map(String::as_str),
        Some("Internal"),
        "metadata: {:?}",
        internal_fn.metadata
    );

    let (usyms, _) = extract_ml(
        Path::new("C:/proj/src/Core.ml"),
        "src/Core.ml",
        UNIQUE_SOURCE,
    );
    let unique_ty = usyms
        .iter()
        .find(|s| s.name.ends_with("BTreeMap"))
        .expect("unique type symbol");
    assert_ne!(
        unique_ty
            .metadata
            .as_ref()
            .and_then(|m| m.get("access"))
            .map(String::as_str),
        Some("Unique"),
        "Unique is ownership, not visibility: {:?}",
        unique_ty.metadata
    );
}
