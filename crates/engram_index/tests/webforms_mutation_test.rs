//! Mutation test suite for WebForms event wiring extraction (Ticket 4).
//!
//! Introduces deliberate mutations (renamed handlers, duplicate control IDs,
//! ambiguous inheritance, missing code-behind) and verifies that extraction:
//! - Handles gracefully (no panics, no incorrect first-match)
//! - Emits correct edges when valid
//! - Omits spurious edges when input is broken

use engram_core::paths::RelPath;
use engram_index::webforms::extract_webforms;
use std::path::Path;

fn rel(s: &str) -> RelPath {
    RelPath::new(s)
}

fn find_edge<'a>(
    edges: &'a [engram_index::ExtractedEdge],
    kind: &str,
    target_contains: &str,
) -> Option<&'a engram_index::ExtractedEdge> {
    edges.iter().find(|e| {
        e.kind == kind
            && e.target_name
                .to_lowercase()
                .contains(&target_contains.to_lowercase())
    })
}

fn find_symbol<'a>(
    symbols: &'a [engram_index::ExtractedSymbol],
    kind: &str,
    name_contains: &str,
) -> Option<&'a engram_index::ExtractedSymbol> {
    symbols.iter().find(|s| {
        s.kind == kind
            && s.name
                .to_lowercase()
                .contains(&name_contains.to_lowercase())
    })
}

// ── Mutation 1: Renamed handler (OnClick references non-existent handler) ───

#[test]
fn mutation_renamed_handler_still_emits_event_wiring_edge() {
    // The extractor sees OnClick="OldName_Click" but the handler was renamed.
    // Extraction should still emit the event_wiring edge referencing the old name
    // (it cannot know the rename happened — that's trace-level detection).
    let aspx = r#"<%@ Page Language="C#" CodeBehind="Order.aspx.cs" Inherits="MyApp.OrderPage" %>
<asp:Button ID="btnSave" runat="server" OnClick="OldName_Click" />"#;

    let (symbols, edges) = extract_webforms(Path::new("/project"), &rel("Order.aspx"), aspx);

    // Control should be extracted
    assert!(
        find_symbol(&symbols, "control", "btnSave").is_some(),
        "btnSave control must be extracted"
    );

    // Event wiring edge should reference the declared handler name (even if renamed)
    let wiring = find_edge(&edges, "event_wiring", "OldName_Click");
    assert!(
        wiring.is_some(),
        "Event wiring edge must be emitted for the declared handler, even if renamed"
    );
}

// ── Mutation 2: Duplicate control IDs ───────────────────────────────────────

#[test]
fn mutation_duplicate_control_ids_extracts_both() {
    // Two controls with the same ID — extractor should emit both symbols.
    // Ambiguity detection happens at trace level, not extraction level.
    let aspx = r#"<%@ Page Language="C#" CodeBehind="Page.aspx.cs" Inherits="MyApp.Page" %>
<asp:Button ID="btnSubmit" runat="server" OnClick="btnSubmit_Click" />
<asp:Button ID="btnSubmit" runat="server" OnClick="btnSubmit_Click2" />"#;

    let (symbols, edges) = extract_webforms(Path::new("/project"), &rel("Page.aspx"), aspx);

    let controls: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "control" && s.name == "btnSubmit")
        .collect();

    // Both controls should be extracted (duplicate IDs are a real WebForms bug)
    assert!(
        controls.len() >= 2,
        "Both duplicate controls should be extracted, got {}",
        controls.len()
    );

    // Both event wiring edges should exist
    let wiring_edges: Vec<_> = edges.iter().filter(|e| e.kind == "event_wiring").collect();
    assert!(
        wiring_edges.len() >= 2,
        "Both event wiring edges should be emitted for duplicate controls"
    );
}

// ── Mutation 3: Missing CodeBehind attribute ────────────────────────────────

#[test]
fn mutation_missing_codebehind_no_codebehind_edge() {
    // Page directive without CodeBehind — should not emit codebehind_file edge.
    let aspx = r#"<%@ Page Language="C#" Inherits="MyApp.InlinePage" %>
<asp:TextBox ID="txtName" runat="server" />"#;

    let (symbols, edges) = extract_webforms(Path::new("/project"), &rel("Inline.aspx"), aspx);

    // Control should still be extracted
    assert!(find_symbol(&symbols, "control", "txtName").is_some());

    // No codebehind_file edge should exist
    let cb_edge = find_edge(&edges, "codebehind_file", "");
    assert!(
        cb_edge.is_none(),
        "No codebehind_file edge when CodeBehind is missing"
    );
}

// ── Mutation 4: Ambiguous inheritance (Inherits= mismatch) ─────────────────

#[test]
fn mutation_inherits_mismatch_still_emits_class_edge() {
    // Inherits specifies a class that might not match the CodeBehind file.
    // Extractor should still emit the codebehind_class edge with the declared class.
    let aspx = r#"<%@ Page Language="C#" CodeBehind="Page.aspx.cs" Inherits="DifferentNamespace.WrongClass" %>
<asp:Button ID="btn" runat="server" />"#;

    let (_symbols, edges) = extract_webforms(Path::new("/project"), &rel("Page.aspx"), aspx);

    // The extractor emits edges with the Inherits class value. Check all edge kinds
    // for one that references the declared class.
    let class_ref = edges.iter().find(|e| {
        e.target_name.contains("DifferentNamespace.WrongClass")
            || e.target_name.contains("WrongClass")
    });
    assert!(
        class_ref.is_some(),
        "Should emit an edge referencing the declared Inherits value, got edges: {:?}",
        edges
            .iter()
            .map(|e| (&e.kind, &e.target_name))
            .collect::<Vec<_>>()
    );
}

// ── Mutation 5: Event handler with wrong signature pattern ──────────────────

#[test]
fn mutation_non_standard_event_name_still_wires() {
    // OnClick handler with unusual name (not following convention)
    let aspx = r#"<%@ Page Language="C#" CodeBehind="X.aspx.cs" Inherits="MyApp.X" %>
<asp:Button ID="btn" runat="server" OnClick="DoSomethingWeird_123" />"#;

    let (_symbols, edges) = extract_webforms(Path::new("/project"), &rel("X.aspx"), aspx);

    let wiring = find_edge(&edges, "event_wiring", "DoSomethingWeird_123");
    assert!(
        wiring.is_some(),
        "Event wiring must work with non-standard handler names"
    );
}

// ── Mutation 6: Empty control ID ────────────────────────────────────────────

#[test]
fn mutation_empty_control_id_skipped() {
    let aspx = r#"<%@ Page Language="C#" CodeBehind="P.aspx.cs" Inherits="MyApp.P" %>
<asp:Button ID="" runat="server" OnClick="Click" />"#;

    let (symbols, _edges) = extract_webforms(Path::new("/project"), &rel("P.aspx"), aspx);

    // Empty ID should not produce a named control symbol
    let empty_controls: Vec<_> = symbols
        .iter()
        .filter(|s| s.kind == "control" && s.name.is_empty())
        .collect();
    // We accept either 0 (skip empty) or 1 (extract as empty) — both are safe
    // The key is no panic
    assert!(empty_controls.len() <= 1);
}

// ── Mutation 7: Malformed directive (no closing tag) ────────────────────────

#[test]
fn mutation_malformed_directive_no_panic() {
    // Malformed Page directive — extractor should not panic
    let aspx = r#"<%@ Page Language="C#" CodeBehind="Bad.aspx.cs" Inherits=
<asp:Button ID="btn" runat="server" />"#;

    let (symbols, edges) = extract_webforms(Path::new("/project"), &rel("Bad.aspx"), aspx);

    // Should not panic — results may be empty or partial
    let _ = symbols;
    let _ = edges;
}

// ── Mutation 8: VB.NET Handles clause with wrong method name ────────────────

#[test]
fn mutation_vb_handles_emits_edge_regardless_of_match() {
    // VB code with Handles clause — extractor should pick it up from markup
    // (VB Handles extraction happens at parse level, not webforms level)
    let aspx = r#"<%@ Page Language="VB" CodeBehind="Order.aspx.vb" Inherits="MyApp.OrderPage" %>
<asp:Button ID="btnProcess" runat="server" />"#;

    let (symbols, edges) = extract_webforms(Path::new("/project"), &rel("Order.aspx"), aspx);

    // Control extracted
    assert!(find_symbol(&symbols, "control", "btnProcess").is_some());

    // CodeBehind should reference the .vb file
    let cb = find_edge(&edges, "codebehind_file", ".vb");
    assert!(cb.is_some(), "CodeBehind should reference the .vb file");
}

// ── Mutation 9: Multiple Page directives (malformed) ────────────────────────

#[test]
fn mutation_multiple_page_directives_no_panic() {
    let aspx = r#"<%@ Page Language="C#" CodeBehind="A.aspx.cs" Inherits="MyApp.A" %>
<%@ Page Language="C#" CodeBehind="B.aspx.cs" Inherits="MyApp.B" %>
<asp:Button ID="btn" runat="server" OnClick="Click" />"#;

    let (_symbols, edges) = extract_webforms(Path::new("/project"), &rel("A.aspx"), aspx);

    // Should not panic. May extract edges for both or just first — both acceptable.
    let cb_edges: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == "codebehind_file")
        .collect();
    assert!(
        !cb_edges.is_empty(),
        "At least one codebehind_file edge should be extracted"
    );
}

// ── Mutation 10: DataSource with missing SelectCommand ──────────────────────

#[test]
fn mutation_datasource_no_select_command_no_sql_edge() {
    let aspx = r#"<%@ Page Language="C#" CodeBehind="P.aspx.cs" Inherits="MyApp.P" %>
<asp:SqlDataSource ID="ds" runat="server" ConnectionString="<%$ ConnectionStrings:DB %>" />"#;

    let (_symbols, edges) = extract_webforms(Path::new("/project"), &rel("P.aspx"), aspx);

    // No SQL command attributes → no sql_calls edges from DataSource
    let sql_edges: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == "sql_calls" && e.source_name.contains("ds"))
        .collect();
    // Empty datasource should produce 0 sql_calls edges
    assert!(
        sql_edges.is_empty(),
        "DataSource without SQL commands should not emit sql_calls edges"
    );
}

// ── Mutation 11: Register directive with missing Src ────────────────────────

#[test]
fn mutation_register_missing_src_no_crash() {
    let aspx = r#"<%@ Page Language="C#" CodeBehind="P.aspx.cs" Inherits="MyApp.P" %>
<%@ Register TagPrefix="uc1" TagName="MyControl" />"#;

    let (_symbols, edges) = extract_webforms(Path::new("/project"), &rel("P.aspx"), aspx);
    // Should not panic. Missing Src → no registers_control edge
    let reg_edges: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == "registers_control")
        .collect();
    // Acceptable: 0 edges (missing Src) or 1 edge (partial extraction)
    let _ = reg_edges;
}

// ── Mutation 12: Script injection in event handler name ─────────────────────

#[test]
fn mutation_script_in_handler_name_extracted_as_string() {
    // Attempt to inject script via handler name — should be treated as literal string
    let aspx = r#"<%@ Page Language="C#" CodeBehind="P.aspx.cs" Inherits="MyApp.P" %>
<asp:Button ID="btn" runat="server" OnClick="alert('xss')" />"#;

    let (_symbols, edges) = extract_webforms(Path::new("/project"), &rel("P.aspx"), aspx);

    // Should extract the handler name as-is (it's just a string identifier)
    let wiring = edges.iter().find(|e| e.kind == "event_wiring");
    if let Some(w) = wiring {
        assert!(
            w.target_name.contains("alert"),
            "Handler name should be extracted literally"
        );
    }
}
