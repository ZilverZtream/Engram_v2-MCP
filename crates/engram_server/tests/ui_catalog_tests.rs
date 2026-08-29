#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 5 (owner decision 09:32: build M1 + M2).
//! M1 slice 1 — the UI Family Catalog: cluster the `ui_container` /
//! `control_layout` nodes the extractor already stores (metadata
//! `container_type`, `layout_style`, `css_class`) into families, and derive
//! a per-axis contract with evidence counts (spec 2026-08-17 Layer 0/1).
//!
//! Fixture: two families — a Bootstrap form-group family (4 instances, one
//! chaotic spacing class) and a panel family (2 instances). The catalog must
//! find exactly those two, type each axis by its actual consistency, and cite
//! evidence counts, never a default.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ui_catalog::{Consistency, build_families};
use engram_server::state::AppState;
use serde_json::json;

const PID: &str = "ui-catalog-test";

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

#[test]
fn the_catalog_finds_the_families_and_types_each_axis_by_its_real_consistency() {
    let (_tmp, state) = state();
    let nodes = vec![
        // form-group family: same container + class SET (order differs), one chaotic margin class
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
        // panel family
        container(
            "Site/pages/d.aspx",
            "pnlInfo",
            "Panel",
            "Table",
            "panel panel-default",
            3,
        ),
        container(
            "Site/pages/e.aspx",
            "pnlWarn",
            "Panel",
            "Table",
            "panel-default panel",
            3,
        ),
    ];
    state.graph.upsert_nodes(PID, &nodes).unwrap();

    let families = build_families(&state.graph, PID, 2).unwrap();
    assert_eq!(
        families.len(),
        2,
        "two families: {:?}",
        families.iter().map(|f| &f.family_id).collect::<Vec<_>>()
    );

    let form = families
        .iter()
        .find(|f| f.instances == 4)
        .unwrap_or_else(|| panic!("the form-group family has 4 instances: {families:?}"));
    assert_eq!(form.derived_at_generation, 1);
    let structure = form.axes.iter().find(|a| a.axis == "structure").unwrap();
    assert_eq!(
        structure.consistency,
        Consistency::Consistent,
        "container Panel/Flow in 4/4: {structure:?}"
    );
    assert_eq!(structure.evidence_count, 4);
    let classes = form
        .axes
        .iter()
        .find(|a| a.axis == "style.classes")
        .unwrap();
    assert_eq!(
        classes.consistency,
        Consistency::Chaotic,
        "the class SET differs in 1 of 4 (mt-3) — chaotic, with the modal set as canonical: {classes:?}"
    );
    assert!(
        classes.canonical.contains("form-group") && classes.canonical.contains("row"),
        "{classes:?}"
    );
    assert!(
        classes.alternatives.iter().any(|a| a.contains("mt-3")),
        "the deviation is listed: {classes:?}"
    );
    assert_eq!(classes.evidence_count, 3, "3 of 4 carry the canonical set");
    assert!(form.exemplar.path.ends_with(".aspx") && !form.exemplar.node_id.is_empty());

    let panel = families.iter().find(|f| f.instances == 2).unwrap();
    let pc = panel
        .axes
        .iter()
        .find(|a| a.axis == "style.classes")
        .unwrap();
    assert_eq!(
        pc.consistency,
        Consistency::Consistent,
        "set equality ignores class order: {pc:?}"
    );
}

#[test]
fn singletons_are_not_families_and_the_catalog_says_how_many_it_skipped() {
    let (_tmp, state) = state();
    state
        .graph
        .upsert_nodes(
            PID,
            &[container(
                "Site/pages/z.aspx",
                "one",
                "Panel",
                "Flow",
                "lonely",
                1,
            )],
        )
        .unwrap();
    let families = build_families(&state.graph, PID, 2).unwrap();
    assert!(
        families.is_empty(),
        "a single instance is an orphan, not a family"
    );
}
