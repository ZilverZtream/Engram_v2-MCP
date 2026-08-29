#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 — observability defect found while measuring the
//! Dream row: `get_codebase_overview` printed only the 15 most frequent edge
//! kinds and hid the rest behind "... and 15 more kinds", which made a live
//! graph with 180 fresh `co_occurrence` edges read as having none. A histogram
//! that hides kinds is not a histogram: every kind is listed, largest first.

use std::collections::HashMap;

use engram_server::handlers::cognitive_tools::render_edge_kind_histogram;

#[test]
fn every_edge_kind_is_listed_even_the_rare_ones() {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (i, kind) in [
        "temporal_coupling",
        "contains",
        "calls",
        "imports",
        "contains_ui",
        "queries_table",
        "reads_column",
        "dependency",
        "has_column",
        "reads_setting",
        "writes_state",
        "manipulates_dom",
        "data_binding",
        "ui_layout_neighbor",
        "implements_interface",
        "co_occurrence",
        "raises_event",
    ]
    .iter()
    .enumerate()
    {
        counts.insert((*kind).to_string(), 1_000_000 / (i + 1));
    }
    let out = render_edge_kind_histogram(&counts);
    assert!(out.contains("--- Edge Types ("), "{out}");
    for kind in counts.keys() {
        assert!(
            out.contains(&format!("  {kind}: ")),
            "kind {kind} must be listed:\n{out}"
        );
    }
    assert!(!out.contains("more kinds"), "nothing is hidden:\n{out}");
    // Largest first.
    let tc = out.find("temporal_coupling").unwrap();
    let co = out.find("co_occurrence").unwrap();
    assert!(tc < co);
}

#[test]
fn an_empty_histogram_renders_nothing() {
    assert_eq!(render_edge_kind_histogram(&HashMap::new()), "");
}
