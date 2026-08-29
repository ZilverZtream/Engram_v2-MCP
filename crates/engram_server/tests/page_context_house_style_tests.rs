#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 5 v3 slice 2 (owner: "metric → exemplar →
//! enforce"). Two A/Bs of a catalog-style UI contract were negative: on a
//! Bootstrap WebForms app without a component layer the house style lives in
//! the NEAREST SIBLING PAGES of the page being edited (same territory, same
//! kind of page), the user controls they reuse and the idioms they share —
//! what an engineer opens next door before writing markup. `get_page_context`
//! now carries that as `house_style`.

use engram_core::config::Config;
use engram_server::models::GetPageContextRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};
use std::path::PathBuf;

const PID: &str = "house-style-test";

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    (tmp, state)
}

fn register_project(state: &AppState, tmp: &tempfile::TempDir) -> PathBuf {
    let project_dir = tmp.path().join("project");
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
    project_dir
}

fn write(dir: &std::path::Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// The page under edit (a bare form) and its territory: two admin siblings
/// that share the house idioms (a `<uc:files>` control, an `alert alert-info`
/// message Panel + Label, `Resources.text.*`), plus a page in another
/// territory that shares nothing.
fn seed(dir: &std::path::Path) {
    let head = "<%@ Page Language=\"VB\" MasterPageFile=\"~/modules/dashboard/dashboard.master\" AutoEventWireup=\"false\" CodeFile=\"{cb}\" Inherits=\"{cls}\" %>\n<asp:Content ID=\"c\" ContentPlaceHolderID=\"contentBody\" runat=\"server\">\n";
    let page = head
        .replace("{cb}", "edit.aspx.vb")
        .replace("{cls}", "admin_edit")
        + "<div class=\"row\"><div class=\"col-lg-10\"><asp:TextBox ID=\"txtName\" runat=\"server\" /><asp:Button ID=\"btnSave\" runat=\"server\" Text=\"<%$ Resources:control, Save %>\" /></div></div>\n</asp:Content>\n";
    write(
        dir,
        "Site/modules/dashboard/pages/admin/things/edit.aspx",
        &page,
    );
    write(
        dir,
        "Site/modules/dashboard/pages/admin/things/edit.aspx.vb",
        "Partial Class admin_edit\n    Inherits System.Web.UI.Page\n    Protected Sub Page_Load(sender As Object, e As EventArgs)\n    End Sub\nEnd Class\n",
    );
    let list = head
        .replace("{cb}", "list.aspx.vb")
        .replace("{cls}", "admin_list")
        + "<div class=\"row\"><div class=\"col-lg-10\"><div class=\"panel panel-default\"><div class=\"panel-heading\"><i class=\"fa fa-list fa-fw\"></i>&nbsp;<%=Resources.label.Things %></div>\n<asp:Panel ID=\"panMsg\" runat=\"server\" CssClass=\"alert alert-info\" Visible=\"false\"><asp:Label ID=\"lblMsg\" runat=\"server\" Text=\"<%$ Resources:text, Saved %>\" /></asp:Panel>\n<asp:TextBox ID=\"txtFilter\" runat=\"server\" /><asp:Button ID=\"btnGo\" runat=\"server\" Text=\"<%$ Resources:control, Search %>\" />\n<uc:files ID=\"ucFiles\" runat=\"server\" />\n</div></div></div>\n</asp:Content>\n";
    write(
        dir,
        "Site/modules/dashboard/pages/admin/things/list.aspx",
        &list,
    );
    let detail = head
        .replace("{cb}", "detail.aspx.vb")
        .replace("{cls}", "admin_detail")
        + "<div class=\"row\"><div class=\"col-lg-10\"><div class=\"panel panel-default\"><div class=\"panel-heading\"><i class=\"fa fa-cog fa-fw\"></i>&nbsp;<%=Resources.label.Thing %></div>\n<asp:Panel ID=\"panInfo\" runat=\"server\" CssClass=\"alert alert-info\"><asp:Label ID=\"lblInfo\" runat=\"server\" Text=\"<%$ Resources:text, Hint %>\" /></asp:Panel>\n<asp:TextBox ID=\"txtA\" runat=\"server\" /><asp:Button ID=\"btnSave\" runat=\"server\" Text=\"<%$ Resources:control, Save %>\" />\n<uc:files ID=\"ucFiles\" runat=\"server\" />\n</div></div></div>\n</asp:Content>\n";
    write(
        dir,
        "Site/modules/dashboard/pages/admin/things/detail.aspx",
        &detail,
    );
    // Another territory: a map page with none of the admin idioms.
    let map = head
        .replace("{cb}", "map.aspx.vb")
        .replace("{cls}", "public_map")
        + "<div id=\"map\" class=\"map-canvas\"></div><asp:HiddenField ID=\"hidLayer\" runat=\"server\" />\n</asp:Content>\n";
    write(
        dir,
        "Site/modules/dashboard/pages/public/map/map.aspx",
        &map,
    );
}

async fn page_context(engram: &Engram, body: Value) -> Value {
    let req: GetPageContextRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_get_page_context(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

async fn page_context_md(engram: &Engram, body: Value) -> String {
    let req: GetPageContextRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_get_page_context(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn house_style_names_the_nearest_siblings_and_the_idioms_they_share() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed(&dir);
    let engram = Engram::new(state);
    let v = page_context(
        &engram,
        json!({"project_id": PID, "aspx_file": "Site/modules/dashboard/pages/admin/things/edit.aspx", "output_json": true}),
    )
    .await;
    let hs = &v["house_style"];
    assert!(
        hs.is_object(),
        "house_style is part of the page context: {v}"
    );
    let sibs: Vec<String> = hs["siblings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["path"].as_str().map(|p| p.replace('\\', "/")))
        .collect();
    assert!(
        sibs.iter().any(|p| p.ends_with("admin/things/list.aspx"))
            && sibs.iter().any(|p| p.ends_with("admin/things/detail.aspx")),
        "the same-territory pages are the siblings: {sibs:?}"
    );
    assert!(
        !sibs.iter().any(|p| p.ends_with("map.aspx")),
        "a page from another territory is not a sibling: {sibs:?}"
    );
    let ucs = hs["user_controls"].to_string().to_lowercase();
    assert!(
        ucs.contains("uc:files"),
        "the user control the siblings reuse: {ucs}"
    );
    let fams = hs["resource_families"].to_string().to_lowercase();
    assert!(
        fams.contains("text") && fams.contains("control"),
        "resource families in use: {fams}"
    );
    let classes = hs["common_classes"].to_string().to_lowercase();
    assert!(
        classes.contains("alert-info") && classes.contains("panel-heading"),
        "the classes the siblings share: {classes}"
    );
    let missing = hs["missing_in_page"].to_string().to_lowercase();
    assert!(
        missing.contains("uc:files") && missing.contains("alert-info"),
        "idioms every sibling has that this page lacks are called out: {missing}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_markdown_carries_a_house_style_section_and_the_flag_turns_it_off() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed(&dir);
    let engram = Engram::new(state);
    let md = page_context_md(
        &engram,
        json!({"project_id": PID, "aspx_file": "Site/modules/dashboard/pages/admin/things/edit.aspx"}),
    )
    .await;
    assert!(md.contains("## House style"), "the section renders:\n{md}");
    assert!(
        md.contains("list.aspx") && md.contains("uc:files"),
        "siblings and idioms render:\n{md}"
    );
    let v = page_context(
        &engram,
        json!({"project_id": PID, "aspx_file": "Site/modules/dashboard/pages/admin/things/edit.aspx", "output_json": true, "include_house_style": false}),
    )
    .await;
    assert!(
        v["house_style"].is_null(),
        "include_house_style=false ignored: {}",
        v["house_style"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_alone_in_its_territory_reports_no_siblings_honestly() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed(&dir);
    let engram = Engram::new(state);
    let v = page_context(
        &engram,
        json!({"project_id": PID, "aspx_file": "Site/modules/dashboard/pages/public/map/map.aspx", "output_json": true}),
    )
    .await;
    let hs = &v["house_style"];
    assert!(hs.is_object(), "{v}");
    assert_eq!(
        hs["siblings"].as_array().map(|a| a.len()).unwrap_or(99),
        0,
        "{hs}"
    );
    assert!(
        hs["note"].as_str().unwrap_or("").contains("no sibling"),
        "the empty case says so: {hs}"
    );
}
