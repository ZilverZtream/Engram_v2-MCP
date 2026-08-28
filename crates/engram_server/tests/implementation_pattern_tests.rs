#![allow(clippy::unwrap_used)]
//! Row-5 audit (docs/audits/07-house-pattern-and-ui-conformance.md)
//! slice 1 — A1/A2/A3/A4: `find_implementation_pattern` must return
//! exemplars of the KIND the query implies (a TypeScript file cannot be
//! exemplar #1 for a WebForms page query), rank by structural fit, derive
//! each exemplar's SHAPE from the graph (ordered handler chains through
//! `Calls` edges, controls, data access) instead of echoing an FTS
//! snippet, name the common shapes, and report every cap.
//!
//! The fixture is a real mini project indexed through `index_project`
//! (VB extractor + FTS), not hand-seeded nodes.

use engram_core::config::Config;
use engram_server::models::FindImplementationPatternRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const PAGE_A_VB: &str = "Partial Class admin_categories\n\
    Inherits System.Web.UI.Page\n\
\n\
    Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load\n\
        If Page.IsPostBack Then Return\n\
        BindGrid()\n\
    End Sub\n\
\n\
    Private Sub BindGrid()\n\
        GridView1.DataSource = _rv.categories.GetAll(db)\n\
        GridView1.DataBind()\n\
    End Sub\n\
\n\
    Protected Sub btnSave_Click(sender As Object, e As EventArgs) Handles btnSave.Click\n\
        _rv.categories.Save(txtName.Text, db)\n\
        BindGrid()\n\
    End Sub\n\
End Class\n";

const PAGE_B_VB: &str = "Partial Class admin_units\n\
    Inherits System.Web.UI.Page\n\
\n\
    Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load\n\
        If Page.IsPostBack Then Return\n\
        BindGrid()\n\
    End Sub\n\
\n\
    Private Sub BindGrid()\n\
        GridView1.DataSource = _rv.units.GetAll(db)\n\
        GridView1.DataBind()\n\
    End Sub\n\
\n\
    Protected Sub btnSave_Click(sender As Object, e As EventArgs) Handles btnSave.Click\n\
        _rv.units.Save(txtName.Text, db)\n\
        BindGrid()\n\
    End Sub\n\
End Class\n";

const PAGE_ASPX: &str = "<%@ Page Language=\"VB\" AutoEventWireup=\"false\" %>\n\
<asp:GridView ID=\"GridView1\" runat=\"server\" />\n\
<asp:TextBox ID=\"txtName\" runat=\"server\" />\n\
<asp:Button ID=\"btnSave\" runat=\"server\" Text=\"Save\" />\n";

const RV_CATEGORIES_VB: &str = "Public Class categories\n\
    Public Function GetAll(db As Ctx) As List(Of String)\n\
        Return db.rk_categories.ToList()\n\
    End Function\n\
    Public Sub Save(name As String, db As Ctx)\n\
        db.rk_categories.InsertOnSubmit(name)\n\
    End Sub\n\
End Class\n";

const RV_UNITS_VB: &str = "Public Class units\n\
    Public Function GetAll(db As Ctx) As List(Of String)\n\
        Return db.rk_units.ToList()\n\
    End Function\n\
    Public Sub Save(name As String, db As Ctx)\n\
        db.rk_units.InsertOnSubmit(name)\n\
    End Sub\n\
End Class\n";

/// Lexically the strongest match for the query — and the wrong kind.
const DECOY_TS: &str = "// admin page GridView save button admin page GridView save button\n\
// categories admin page lists and edits categories GridView save button\n\
export class QtyManager {\n\
  save() { /* admin page GridView save button categories */ }\n\
  list() { /* lists and edits categories with a GridView */ }\n\
}\n";

async fn build() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for (rel, content) in [
        ("Site/modules/admin/categories.aspx.vb", PAGE_A_VB),
        ("Site/modules/admin/categories.aspx", PAGE_ASPX),
        ("Site/modules/admin/units.aspx.vb", PAGE_B_VB),
        ("Site/modules/admin/units.aspx", PAGE_ASPX),
        ("Site/App_Code/rv/categories.vb", RV_CATEGORIES_VB),
        ("Site/App_Code/rv/units.vb", RV_UNITS_VB),
        ("Site/ts/qty/qtyManager.ts", DECOY_TS),
    ] {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(50),
        max_project_bytes: Some(2 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "PatternFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

const QUERY: &str = "admin page that lists and edits categories with a GridView and a save button";

async fn run(engram: &Engram, pid: &str, body: Value) -> String {
    let mut body = body;
    body["project_id"] = json!(pid);
    let req: FindImplementationPatternRequest = serde_json::from_value(body).unwrap();
    let res = engram
        .handle_find_implementation_pattern(req)
        .await
        .unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_query_returns_page_exemplars_never_the_lexically_louder_script() {
    let (_tmp, _state, engram, pid) = build().await;
    let out = run(
        &engram,
        &pid,
        json!({"pattern_query": QUERY, "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}):\n{out}"));
    assert_eq!(v["inferred_kind"], "page", "{v}");
    let paths: Vec<String> = v["exemplars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_string())
        .collect();
    assert!(!paths.is_empty(), "{v}");
    assert!(
        paths[0].ends_with(".aspx.vb"),
        "exemplar #1 must be a page code-behind, got {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with(".ts")),
        "a TypeScript file is not an exemplar for a WebForms page query: {paths:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exemplar_shape_is_derived_from_the_graph_and_common_shapes_are_named() {
    let (_tmp, _state, engram, pid) = build().await;
    let out = run(
        &engram,
        &pid,
        json!({"pattern_query": QUERY, "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap();
    let first = &v["exemplars"][0];
    let handlers = first["shape"]["handlers"].to_string();
    assert!(
        handlers.contains("Page_Load") && handlers.contains("BindGrid"),
        "ordered handler chain expected (Page_Load → BindGrid): {handlers}"
    );
    assert!(
        handlers.contains("btnSave_Click"),
        "the save handler chain must be part of the shape: {handlers}"
    );
    let controls = first["shape"]["controls"].to_string();
    assert!(
        controls.contains("GridView1") && controls.contains("btnSave"),
        "controls from the .aspx side: {controls}"
    );
    let common = v["common_shapes"].to_string();
    assert!(
        common.contains("btnSave_Click") && common.contains("BindGrid"),
        "the shape both pages share must be named as the house pattern: {common}"
    );

    // Markdown carries the same shape, not a raw FTS snippet as the only content.
    let md = run(&engram, &pid, json!({"pattern_query": QUERY})).await;
    assert!(md.contains("Page_Load") && md.contains("BindGrid"), "{md}");
    assert!(
        md.contains("## Common shapes") || md.contains("house pattern"),
        "{md}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_cap_is_reported() {
    let (_tmp, _state, engram, pid) = build().await;
    let out = run(
        &engram,
        &pid,
        json!({"pattern_query": QUERY, "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap();
    let cov = &v["coverage"];
    for key in [
        "lexical_hits",
        "lexical_cap",
        "lexical_status",
        "exemplar_cap",
        "handlers_cap",
    ] {
        assert!(!cov[key].is_null(), "coverage.{key} missing: {cov}");
    }
    assert_eq!(cov["lexical_status"], "complete", "{cov}");
    assert!(
        cov["failures"].as_array().is_some(),
        "provider failures must be a list, even when empty: {cov}"
    );
    let md = run(&engram, &pid, json!({"pattern_query": QUERY})).await;
    assert!(md.contains("## Coverage"), "{md}");
}
