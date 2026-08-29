#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 5 — owner decision 15:23: refine the catalog
//! before the A/B. Live OciusX showed the slice-1 key (container type +
//! layout) lumping 6,475 class-less `div`s into one "family" whose exemplar
//! sat outside the requested region. A family is a container type + its BASE
//! class set (utility/spacing classes stripped); class-less instances are
//! orphans; a region pull cites an exemplar inside the region when one exists.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::services::ui_catalog::{
    Consistency, base_class_set, build_families, families_for_region,
};
use engram_server::state::AppState;
use serde_json::json;

const PID: &str = "ui-catalog-refine-test";

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
        node_id: format!("ui_container:{path}:{id}"),
        node_type: "ui_container".into(),
        name: id.into(),
        namespace: "ui".into(),
        language: "aspx".into(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line + 4,
        generation: 1,
        metadata: Some(
            json!({"container_type": container_type, "layout_style": layout, "css_class": css}),
        ),
    }
}

fn seed(state: &AppState) {
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                // one form-group family: the base set is `form-group row`; mt-3 is a spacing deviation
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
                // class-less panels: orphans, not a family (the live 6,475-div trap)
                container("Site/pages/d.aspx", "pnl1", "Panel", "Flow", "", 3),
                container("Site/pages/e.aspx", "pnl2", "Panel", "Flow", "", 3),
                container("Site/pages/f.aspx", "pnl3", "Panel", "Flow", "", 3),
                // two DIFFERENT div families inside the same container type
                container("Site/admin/g.aspx", "divRow1", "div", "Flow", "row", 1),
                container("Site/admin/h.aspx", "divRow2", "div", "Flow", "row mb-2", 1),
                container(
                    "Site/admin/i.aspx",
                    "divAlert1",
                    "div",
                    "Flow",
                    "alert alert-danger",
                    1,
                ),
                container(
                    "Site/admin/j.aspx",
                    "divAlert2",
                    "div",
                    "Flow",
                    "alert alert-danger",
                    1,
                ),
            ],
        )
        .unwrap();
}

#[test]
fn utility_classes_do_not_carry_family_identity() {
    let b = base_class_set("form-group row mt-3 col-md-6 text-right d-none");
    let v: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    assert_eq!(v, vec!["form-group", "row"], "{v:?}");
    assert!(base_class_set("mt-3 mb-2").is_empty());
}

#[test]
fn families_are_container_plus_base_class_set_and_classless_instances_are_orphans() {
    let (_tmp, state) = state();
    seed(&state);
    let fams = build_families(&state.graph, PID, 2).unwrap();
    let ids: Vec<&str> = fams.iter().map(|f| f.family_id.as_str()).collect();
    assert_eq!(
        fams.len(),
        3,
        "form-group panels, row divs, alert divs — and NO class-less family: {ids:?}"
    );
    assert!(
        fams.iter()
            .all(|f| !f.family_name.trim_end().ends_with("(Flow)")),
        "every family carries a base class set: {ids:?}"
    );
    let form = fams.iter().find(|f| f.instances == 4).unwrap();
    let classes = form
        .axes
        .iter()
        .find(|a| a.axis == "style.classes")
        .unwrap();
    assert_eq!(
        classes.consistency,
        Consistency::Chaotic,
        "mt-3 is a listed deviation inside the family: {classes:?}"
    );
    assert!(classes.alternatives.iter().any(|a| a.contains("mt-3")));
    let rows = fams
        .iter()
        .find(|f| f.family_name.contains(".row") && f.instances == 2)
        .unwrap();
    let rc = rows
        .axes
        .iter()
        .find(|a| a.axis == "style.classes")
        .unwrap();
    assert!(
        rc.alternatives.iter().any(|a| a.contains("mb-2")),
        "spacing deviation listed: {rc:?}"
    );
    assert!(fams.iter().any(|f| f.family_name.contains("alert")));
}

#[test]
fn a_region_pull_cites_an_exemplar_inside_the_region_when_one_exists() {
    let (_tmp, state) = state();
    seed(&state);
    let f = families_for_region(&state.graph, PID, "Site/pages/b.aspx", 2).unwrap();
    assert_eq!(
        f.len(),
        1,
        "{:?}",
        f.iter().map(|x| &x.family_id).collect::<Vec<_>>()
    );
    assert!(
        f[0].exemplar.path.ends_with("b.aspx"),
        "exemplar inside the region: {:?}",
        f[0].exemplar
    );
    assert!(
        f[0].carriers.len() >= 3,
        "carriers of the canonical set are listed: {:?}",
        f[0].carriers
    );
    // a region whose only member deviates: the global exemplar stands, and the deviation is still listed
    let g = families_for_region(&state.graph, PID, "Site/pages/c.aspx", 2).unwrap();
    assert_eq!(g.len(), 1);
    assert!(
        !g[0].exemplar.path.ends_with("c.aspx"),
        "c.aspx carries the deviation, not the canonical set: {:?}",
        g[0].exemplar
    );
}
