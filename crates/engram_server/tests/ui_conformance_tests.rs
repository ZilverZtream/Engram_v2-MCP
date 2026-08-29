#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 5 (owner decision 09:32: build M1 + M2).
//! M2 — `get_ui_conformance(region)`, both directions of the spec's Layer 2:
//! * pull: region (file / dir prefix / glob) → the families that live there →
//!   the assembled contract (exemplar + typed axes), consumed BEFORE writing;
//! * check: a candidate's class list against the same contract → every
//!   deviation at once, ✓/✗ per axis, with the expected value.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ui_catalog::{
    build_families, check_classes, families_for_region, render_conformance,
};
use engram_server::state::AppState;
use serde_json::json;

const PID: &str = "ui-conformance-test";

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state)
}

fn container(
    path: &str,
    id: &str,
    container_type: &str,
    layout: &str,
    css: &str,
    line: u32,
) -> Node {
    Node {
        node_id: format!("control_layout:{path}:{id}"),
        node_type: "control_layout".into(),
        name: id.into(),
        namespace: "ui".into(),
        language: "aspx".into(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line + 4,
        generation: 1,
        metadata: Some(json!({
            "container_type": container_type,
            "layout_style": layout,
            "css_class": css,
        })),
    }
}

fn seed(state: &AppState) {
    let nodes = vec![
        container(
            "Site/pages/a.aspx",
            "grpName",
            "Panel",
            "Flow",
            "form-group row",
            10,
        ),
        container(
            "Site/pages/a.aspx",
            "grpDate",
            "Panel",
            "Flow",
            "row form-group",
            20,
        ),
        container(
            "Site/pages/b.aspx",
            "grpCat",
            "Panel",
            "Flow",
            "form-group row",
            5,
        ),
        container(
            "Site/pages/c.aspx",
            "grpMain",
            "Panel",
            "Flow",
            "form-group row mt-3",
            8,
        ),
        container(
            "Site/admin/d.aspx",
            "pnlInfo",
            "Panel",
            "Table",
            "panel panel-default",
            3,
        ),
        container(
            "Site/admin/e.aspx",
            "pnlWarn",
            "Panel",
            "Table",
            "panel-default panel",
            3,
        ),
    ];
    state.graph.upsert_nodes(PID, &nodes).unwrap();
}

#[test]
fn pull_returns_only_the_families_that_live_in_the_region() {
    let (_tmp, state) = state();
    seed(&state);
    // a file
    let f = families_for_region(&state.graph, PID, "Site/pages/a.aspx", 2).unwrap();
    assert_eq!(
        f.len(),
        1,
        "{:?}",
        f.iter().map(|x| &x.family_id).collect::<Vec<_>>()
    );
    assert_eq!(
        f[0].instances, 4,
        "the family is reported whole, not just its members in the file"
    );
    // a directory prefix
    let d = families_for_region(&state.graph, PID, "Site/admin/", 2).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].instances, 2);
    // a glob
    let g = families_for_region(&state.graph, PID, "Site/**/*.aspx", 2).unwrap();
    assert_eq!(g.len(), 2);
    // nothing there → nothing, not a default
    assert!(
        families_for_region(&state.graph, PID, "Site/nowhere/", 2)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn check_reports_every_axis_with_the_expected_value() {
    let (_tmp, state) = state();
    seed(&state);
    let families = build_families(&state.graph, PID, 2).unwrap();
    let form = families.iter().find(|f| f.instances == 4).unwrap();

    let ok = check_classes(form, "row form-group");
    assert!(ok.iter().all(|v| v.ok), "order-insensitive match: {ok:?}");

    let bad = check_classes(form, "form-group row mt-3");
    let classes = bad.iter().find(|v| v.axis == "style.classes").unwrap();
    assert!(!classes.ok, "{bad:?}");
    assert!(classes.expected.contains("form-group") && classes.expected.contains("row"));
    assert!(classes.found.contains("mt-3"));
    assert!(
        classes.detail.contains("mt-3"),
        "the deviation is named: {classes:?}"
    );

    let text = render_conformance(&[form.clone()], Some(&bad));
    assert!(
        text.contains("✗") && text.contains("style.classes") && text.contains("mt-3"),
        "{text}"
    );
    assert!(
        text.contains("exemplar:"),
        "the contract cites its exemplar: {text}"
    );
}
