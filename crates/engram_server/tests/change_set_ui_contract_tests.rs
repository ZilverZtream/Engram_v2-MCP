#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 5 (owner 2026-08-29: "build the gated,
//! relevance-filtered section and re-run the 8-story A/B"). The n=8 A/B of a
//! REGION-pulled get_ui_conformance contract was NEGATIVE (−4.1 F1): the region
//! came from the dossier's first .aspx mention, the families were unrelated to
//! the story, and the prompt invited invented UI work. The contract now rides
//! INSIDE get_change_set: emitted only when a top-tier candidate is page
//! markup, listing only the families that markup already belongs to, framed
//! as shaping HOW markup already in the plan is written — never adding files.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As an admin I want to set a main reporting category (huvudredovisningskategori) for each production code list category so that time reports roll up correctly.";
const PAGE: &str =
    "Site/modules/dashboard/pages/admin/production/productioncodelistmaincategory.aspx";
const OTHER: &str = "Site/modules/dashboard/pages/other/unrelated.aspx";

fn container(
    path: &str,
    id: &str,
    container_type: &str,
    layout: &str,
    css: &str,
    generation: u64,
) -> Node {
    Node {
        node_id: format!("control_layout:{path}:{id}"),
        node_type: "control_layout".into(),
        name: id.into(),
        namespace: "ui".into(),
        language: "aspx".into(),
        file_path: RelPath::new(path),
        start_line: 7,
        end_line: 11,
        generation,
        metadata: Some(json!({
            "container_type": container_type,
            "layout_style": layout,
            "css_class": css,
        })),
    }
}

/// The markup-family fixture (the reference story's page + code-behind + noise)
/// with UI containers: the layout extractor indexes the pages' real markup, so
/// the candidate page carries a `form-group row` Panel (family member) or, in
/// the no-family variant, only class-less controls; the unrelated page carries
/// a Table family and extra synthetic Panel instances.
async fn build(candidate_page_has_family: bool) -> (tempfile::TempDir, Engram, AppState, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for d in [
        "Site/App_Code/redovisning/code",
        "Site/modules/dashboard/pages/admin/production",
        "Site/modules/dashboard/pages/other",
        "Site/App_Code/noise",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/redovisningskategorier.vb"),
        "Public Class redovisningskategorier\n    Public Function GetByProjectId(pr_id As Integer) As Object\n        Return Nothing\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join(format!("{PAGE}.vb")),
        "Partial Class productioncodelistmaincategory\n    Inherits System.Web.UI.Page\n    Protected Sub Page_Load(sender As Object, e As EventArgs)\n        ' main reporting category (huvudredovisningskategori) for the production code list category\n        Dim mainReportingCategory = redovisningskategorier.GetByProjectId(1)\n    End Sub\nEnd Class\n",
    )
    .unwrap();
    let page_body = if candidate_page_has_family {
        "<asp:Panel ID=\"grpMain\" CssClass=\"form-group row\" runat=\"server\"></asp:Panel>"
    } else {
        "<asp:Label ID=\"lblMain\" runat=\"server\" Text=\"main\" />"
    };
    std::fs::write(
        root.join(PAGE),
        format!(
            "<%@ Page Language=\"VB\" AutoEventWireup=\"false\" CodeFile=\"productioncodelistmaincategory.aspx.vb\" Inherits=\"productioncodelistmaincategory\" %>\n<asp:Content ID=\"c\" ContentPlaceHolderID=\"main\" runat=\"server\">\n{page_body}\n</asp:Content>\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join(OTHER),
        "<%@ Page Language=\"VB\" AutoEventWireup=\"false\" CodeFile=\"unrelated.aspx.vb\" Inherits=\"unrelated\" %>\n<asp:Content ID=\"c\" ContentPlaceHolderID=\"main\" runat=\"server\">\n<asp:Table ID=\"tblList\" CssClass=\"table table-striped\" runat=\"server\"></asp:Table>\n</asp:Content>\n",
    )
    .unwrap();
    for i in 0..20 {
        std::fs::write(
            root.join(format!("Site/App_Code/noise/category_helper{i:02}.vb")),
            format!(
                "Public Class category_helper{i:02}\n    Public Function MainReporting{i}() As String\n        Return \"category\"\n    End Function\nEnd Class\n"
            ),
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
            project_name: "UiContractFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let generation: u64 = state
        .registry
        .get_meta(&pid, "active_generation")
        .ok()
        .flatten()
        .and_then(|g| g.parse().ok())
        .unwrap_or(1);
    // Extra instances on the UNRELATED page only (the shape the layout
    // extractor stores): the Panel family gets members elsewhere, the Table
    // family lives nowhere near a candidate.
    let nodes = vec![
        container(OTHER, "grpB", "Panel", "Flow", "form-group row", generation),
        container(OTHER, "grpC", "Panel", "Flow", "row form-group", generation),
        container(
            OTHER,
            "tblA",
            "Table",
            "Grid",
            "table table-striped",
            generation,
        ),
        container(
            OTHER,
            "tblB",
            "Table",
            "Grid",
            "table table-striped",
            generation,
        ),
    ];
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    (tmp, engram, state, pid)
}

/// The section is OPT-IN (`include_ui_contract`): both A/Bs measured it
/// negative on file-F1 (region-pulled −4.1, in-dossier −9.7 at n = 8), so a
/// default change set stays clean; the v3 program (owner 2026-08-29 22:50)
/// measures markup conformance on the implementation instead.
async fn change_set_with(
    engram: &Engram,
    pid: &str,
    output_json: bool,
    include: Option<bool>,
) -> String {
    let mut body = json!({"project_id": pid, "story": STORY, "output_json": output_json});
    if let Some(i) = include {
        body["include_ui_contract"] = json!(i);
    }
    let req: GetChangeSetRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

async fn change_set(engram: &Engram, pid: &str, output_json: bool) -> String {
    change_set_with(engram, pid, output_json, Some(true)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_section_is_opt_in_a_default_change_set_never_carries_it() {
    let (_tmp, engram, _state, pid) = build(true).await;
    let t = change_set_with(&engram, &pid, true, None).await;
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    assert!(
        v.get("ui_contract").is_some_and(|c| c.is_null()),
        "without include_ui_contract the key is null even though a family lives on the candidate page: {}",
        v["ui_contract"]
    );
    let md = change_set_with(&engram, &pid, false, None).await;
    assert!(
        !md.contains("## UI contract"),
        "a default change set carries no UI contract section:\n{md}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_contract_lists_only_the_families_the_candidate_markup_belongs_to() {
    let (_tmp, engram, _state, pid) = build(true).await;
    let t = change_set(&engram, &pid, true).await;
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
            .any(|p| p.ends_with("productioncodelistmaincategory.aspx")),
        "precondition: the page markup is a candidate: {files:?}"
    );
    let c = &v["ui_contract"];
    assert!(
        c.is_object(),
        "a markup candidate opens the UI contract: {}",
        v["ui_contract"]
    );
    assert!(
        c["framing"]
            .as_str()
            .unwrap_or("")
            .contains("never adds or removes files"),
        "the framing says what the section is for: {c}"
    );
    let cands: Vec<String> = c["markup_candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.as_str().map(|s| s.to_lowercase()))
        .collect();
    assert!(
        cands
            .iter()
            .any(|p| p.ends_with("productioncodelistmaincategory.aspx")),
        "the markup the contract is about is named: {cands:?}"
    );
    let fams = c["families"].as_array().unwrap();
    assert_eq!(
        fams.len(),
        1,
        "ONLY the family the candidate page belongs to (the table family lives on an unrelated page): {fams:?}"
    );
    let name = fams[0]["family_name"].as_str().unwrap_or("").to_lowercase();
    assert!(name.contains("form-group"), "{name}");
    assert!(
        fams[0]["exemplar"]["path"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .ends_with("productioncodelistmaincategory.aspx"),
        "the exemplar is the candidate page itself, not a page elsewhere: {}",
        fams[0]["exemplar"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_markdown_section_carries_the_framing_and_only_the_relevant_family() {
    let (_tmp, engram, _state, pid) = build(true).await;
    let md = change_set(&engram, &pid, false).await;
    assert!(md.contains("## UI contract"), "the section renders:\n{md}");
    assert!(
        md.contains("never adds or removes files"),
        "the framing is in the markdown:\n{md}"
    );
    assert!(
        md.contains("form-group"),
        "the relevant family renders:\n{md}"
    );
    assert!(
        !md.contains("table-striped"),
        "a family from an unrelated page is NOT in the section:\n{md}"
    );
    let ui = md.find("## UI contract").unwrap();
    let cov = md.find("## Coverage").unwrap_or(md.len());
    assert!(
        ui < cov,
        "the contract sits with the candidates, before the coverage report"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_family_touching_a_candidate_means_no_section_at_all() {
    let (_tmp, engram, _state, pid) = build(false).await;
    let t = change_set(&engram, &pid, true).await;
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    assert!(
        v.get("ui_contract").is_some_and(|c| c.is_null()),
        "the key is present and null — no family lives on any candidate page: {}",
        v["ui_contract"]
    );
    let md = change_set(&engram, &pid, false).await;
    assert!(
        !md.contains("## UI contract"),
        "nothing to conform to → no section (never an empty or generic one):\n{md}"
    );
}
