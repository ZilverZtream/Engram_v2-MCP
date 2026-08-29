#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3: on OciusX the reference story's page markup
//! `productioncodelistmaincategory.aspx` never rendered — only its code-behind
//! (found by the vector arm alone). The family expansion pulled code-behind for
//! a markup hit but never the markup for a code-behind hit. A WebForms page is
//! one unit: whichever half any arm finds, the other half renders with it.

use engram_core::config::Config;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As an admin I want to set a main reporting category (huvudredovisningskategori) for each production code list category so that time reports roll up to it";

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for d in [
        "Site/App_Code/redovisning/code",
        "Site/modules/dashboard/pages/admin/production",
        "Site/App_Code/noise",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/redovisningskategorier.vb"),
        "Public Class redovisningskategorier\n    Public Function GetByProjectId(pr_id As Integer) As Object\n        Return Nothing\n    End Function\nEnd Class\n",
    )
    .unwrap();
    // The page: the concept lives ONLY in the code-behind; the markup names nothing the story says.
    std::fs::write(
        root.join("Site/modules/dashboard/pages/admin/production/productioncodelistmaincategory.aspx.vb"),
        "Partial Class productioncodelistmaincategory\n    Inherits System.Web.UI.Page\n    Protected Sub Page_Load(sender As Object, e As EventArgs)\n        Dim cats = redovisningskategorier.GetByProjectId(pr_id)\n        ddlHuvud.DataSource = cats\n    End Sub\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/modules/dashboard/pages/admin/production/productioncodelistmaincategory.aspx"),
        "<%@ Page Language=\"VB\" AutoEventWireup=\"false\" CodeFile=\"productioncodelistmaincategory.aspx.vb\" Inherits=\"productioncodelistmaincategory\" %>\n<asp:Panel ID=\"pnlMain\" runat=\"server\" CssClass=\"form-group row\">\n  <asp:DropDownList ID=\"ddlHuvud\" runat=\"server\" />\n</asp:Panel>\n",
    )
    .unwrap();
    for i in 0..20 {
        std::fs::write(
            root.join(format!("Site/App_Code/noise/category_helper{i:02}.vb")),
            format!("Public Class category_helper{i:02}\n    Public Function MainReporting{i}() As String\n        Return \"category\"\n    End Function\nEnd Class\n"),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(100),
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
            project_name: "MarkupFamilyFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_code_behind_hit_renders_its_page_markup_too() {
    let (_tmp, engram, pid) = build().await;
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    let files: Vec<String> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["path"].as_str().map(|s| s.to_lowercase()))
        .collect();
    assert!(
        files
            .iter()
            .any(|p| p.ends_with("productioncodelistmaincategory.aspx.vb")),
        "the code-behind renders: {files:?}"
    );
    assert!(
        files
            .iter()
            .any(|p| p.ends_with("productioncodelistmaincategory.aspx")),
        "the page MARKUP renders with its code-behind (family expansion in both directions): {files:?}"
    );
    // `family` is a cap-exemption marker the renderer hides from the signal
    // list; the provenance line is the user-facing evidence.
    let markup = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| {
            f["path"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .ends_with(".aspx")
        })
        .unwrap();
    let why = markup["why"].to_string().to_lowercase();
    assert!(
        why.contains("page markup of"),
        "the markup's provenance names its code-behind: {markup}"
    );
}
