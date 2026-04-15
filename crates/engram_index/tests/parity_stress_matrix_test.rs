#![allow(clippy::unwrap_used)]
//! PARITY matrix tests for VB/C# stress categories.
//!
//! This suite is intentionally table-driven so parity regressions are easy to
//! pinpoint by category + language in CI logs.

use engram_core::paths::RelPath;
use engram_index::{
    vb_extractor::extract_vb, webforms::extract_webforms, ExtractedEdge, ExtractedSymbol,
    SymbolExtractor,
};
use std::path::Path;

#[derive(Clone, Copy)]
struct LanguageExpectation {
    min_symbols: usize,
    key_edge: Option<(&'static str, &'static str, &'static str)>,
}

#[derive(Clone, Copy)]
struct SyntaxCase {
    category: &'static str,
    vb_source: &'static str,
    cs_source: &'static str,
    vb_expect: LanguageExpectation,
    cs_expect: LanguageExpectation,
}

fn has_edge(
    edges: &[ExtractedEdge],
    kind: &str,
    source_contains: &str,
    target_contains: &str,
) -> bool {
    edges.iter().any(|edge| {
        edge.kind == kind
            && edge.source_name.contains(source_contains)
            && edge.target_name.contains(target_contains)
    })
}

fn assert_language_case(
    category: &str,
    language: &str,
    symbols: &[ExtractedSymbol],
    edges: &[ExtractedEdge],
    expect: LanguageExpectation,
) {
    assert!(
        symbols.len() >= expect.min_symbols,
        "parity matrix [{category}:{language}] expected >= {} symbols, got {}\nsymbols={symbols:?}",
        expect.min_symbols,
        symbols.len()
    );

    if let Some((kind, src, dst)) = expect.key_edge {
        assert!(
            has_edge(edges, kind, src, dst),
            "parity matrix [{category}:{language}] missing edge kind='{kind}' src~'{src}' dst~'{dst}'\nedges={edges:?}"
        );
    }
}

fn run_syntax_case(case: SyntaxCase) {
    let extractor = SymbolExtractor::new();

    let vb = std::panic::catch_unwind(|| extract_vb(Path::new("Matrix.aspx.vb"), case.vb_source));
    assert!(
        vb.is_ok(),
        "parity matrix [{}:vb] extraction panicked",
        case.category
    );
    let (vb_symbols, vb_edges) = vb.unwrap();
    assert_language_case(case.category, "vb", &vb_symbols, &vb_edges, case.vb_expect);

    let cs = std::panic::catch_unwind(|| extractor.extract(Path::new("Matrix.cs"), case.cs_source));
    assert!(
        cs.is_ok(),
        "parity matrix [{}:cs] extraction panicked",
        case.category
    );
    let (cs_symbols, cs_edges) = cs.unwrap();
    assert_language_case(case.category, "cs", &cs_symbols, &cs_edges, case.cs_expect);
}

#[test]
fn vb_cs_stress_parity_matrix() {
    let long_vb = Box::leak(
        format!(
            "Public Class Big\n  Public Sub Runner()\n    Dim x As Integer = 0\n    {}\n  End Sub\nEnd Class",
            "x = x + 1\n".repeat(10_000)
        )
        .into_boxed_str(),
    );
    let long_cs = Box::leak(
        format!(
            "public class Big {{ public void Runner() {{ int x = 0; {} }} }}",
            "x = x + 1;".repeat(10_000)
        )
        .into_boxed_str(),
    );

    let cases = [
        SyntaxCase {
            category: "malformed_incomplete_syntax",
            vb_source: "Public Class Recover\n  Public Sub Caller()\n    Callee()\n  End Sub\n  Public Sub Callee()\n  End Sub\n  Public Sub Broken(\nEnd Class",
            cs_source: "public class Recover { public void Caller() { Callee(); } public void Callee() {} public void Broken( }",
            vb_expect: LanguageExpectation {
                min_symbols: 1,
                key_edge: None,
            },
            cs_expect: LanguageExpectation {
                min_symbols: 1,
                key_edge: None,
            },
        },
        SyntaxCase {
            category: "deeply_nested_constructs",
            vb_source: "Public Class Nest\n  Public Sub Outer()\n    If True Then\n      For i = 0 To 2\n        While i >= 0\n          If i = 1 Then\n            Inner()\n          End If\n          Exit While\n        End While\n      Next\n    End If\n  End Sub\n  Private Sub Inner()\n  End Sub\nEnd Class",
            cs_source: "public class Nest { public void Outer() { if (true) { for (int i = 0; i < 3; i++) { while (i >= 0) { if (i == 1) { Inner(); } break; } } } } private void Inner() {} }",
            vb_expect: LanguageExpectation {
                min_symbols: 2,
                key_edge: Some(("contains", "Nest", "Outer")),
            },
            cs_expect: LanguageExpectation {
                min_symbols: 2,
                key_edge: Some(("calls", "Outer", "Inner")),
            },
        },
        SyntaxCase {
            category: "unicode_identifiers",
            vb_source: "Public Class Pàge\n  Public Sub Càller()\n    Héלper()\n  End Sub\n  Private Sub Héלper()\n  End Sub\nEnd Class",
            cs_source: "public class Pàge { public void Càller() { Héלper(); } private void Héלper() {} }",
            vb_expect: LanguageExpectation {
                min_symbols: 2,
                key_edge: Some(("contains", "Pàge", "Càller")),
            },
            cs_expect: LanguageExpectation {
                min_symbols: 2,
                key_edge: Some(("calls", "Càller", "Héלper")),
            },
        },
        SyntaxCase {
            category: "very_large_files_long_lines",
            vb_source: long_vb,
            cs_source: long_cs,
            vb_expect: LanguageExpectation {
                min_symbols: 1,
                key_edge: None,
            },
            cs_expect: LanguageExpectation {
                min_symbols: 1,
                key_edge: None,
            },
        },
        SyntaxCase {
            category: "call_edge_extraction_fidelity",
            vb_source: "Public Class Calls\n  Public Sub btnSave_Click(sender As Object, e As EventArgs) Handles btnSave.Click\n  End Sub\nEnd Class",
            cs_source: "public class Calls { public void Caller() { Callee(); } private void Callee() {} }",
            vb_expect: LanguageExpectation {
                min_symbols: 1,
                key_edge: Some(("event_wiring", "btnSave", "btnSave_Click")),
            },
            cs_expect: LanguageExpectation {
                min_symbols: 2,
                key_edge: Some(("calls", "Caller", "Callee")),
            },
        },
    ];

    for case in cases {
        run_syntax_case(case);
    }
}

#[derive(Clone, Copy)]
struct WebFormsCase {
    category: &'static str,
    aspx: &'static str,
    min_symbols: usize,
    expected_edges: &'static [(&'static str, &'static str, &'static str)],
}

fn run_webforms_case(case: WebFormsCase) {
    let parsed = std::panic::catch_unwind(|| {
        extract_webforms(
            Path::new("/matrix"),
            &RelPath::new("Orders.aspx"),
            case.aspx,
        )
    });

    assert!(
        parsed.is_ok(),
        "parity matrix [{}:webforms_cs] extraction panicked",
        case.category
    );

    let (symbols, edges): (Vec<ExtractedSymbol>, Vec<ExtractedEdge>) = parsed.unwrap();
    assert!(
        symbols.len() >= case.min_symbols,
        "parity matrix [{}:webforms_cs] expected >= {} symbols, got {}\nsymbols={symbols:?}",
        case.category,
        case.min_symbols,
        symbols.len()
    );

    for (kind, src, dst) in case.expected_edges {
        assert!(
            has_edge(&edges, kind, src, dst),
            "parity matrix [{}:webforms_cs] missing edge kind='{kind}' src~'{src}' dst~'{dst}'\nedges={edges:?}",
            case.category
        );
    }
}

#[test]
fn webforms_cs_event_wiring_parity_matrix() {
    let cases = [
        WebFormsCase {
            category: "webforms_event_wiring_standard",
            aspx: r#"<%@ Page Language="C#" CodeBehind="Orders.aspx.cs" Inherits="MyApp.Orders" %>
<asp:Button ID="btnSave" runat="server" OnClick="btnSave_Click" />"#,
            min_symbols: 1,
            expected_edges: &[
                ("event_wiring", "btnSave", "btnSave_Click"),
                ("codebehind_file", "Orders.aspx", "Orders.aspx.cs"),
            ],
        },
        WebFormsCase {
            category: "webforms_event_wiring_duplicate_controls",
            aspx: r#"<%@ Page Language="C#" CodeBehind="Orders.aspx.cs" Inherits="MyApp.Orders" %>
<asp:Button ID="btnSave" runat="server" OnClick="btnSave_Click" />
<asp:Button ID="btnSave" runat="server" OnClick="btnSave_Secondary" />"#,
            min_symbols: 2,
            expected_edges: &[
                ("event_wiring", "btnSave", "btnSave_Click"),
                ("event_wiring", "btnSave", "btnSave_Secondary"),
            ],
        },
        WebFormsCase {
            category: "webforms_event_wiring_malformed_directive",
            aspx: r#"<%@ Page Language="C#" CodeBehind="Orders.aspx.cs" Inherits=
<asp:Button ID="btnSave" runat="server" OnClick="btnSave_Click" />"#,
            min_symbols: 1,
            expected_edges: &[("event_wiring", "btnSave", "btnSave_Click")],
        },
    ];

    for case in cases {
        run_webforms_case(case);
    }
}
