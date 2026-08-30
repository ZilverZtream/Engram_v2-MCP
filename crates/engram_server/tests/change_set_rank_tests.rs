#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P0-3): get_change_set PRECISION
//! and RANKING. The rejected r33 dossier for the reference story rendered
//! 200 candidates: 39 files reached tier 0 on `concept+lexicon` alone (a
//! generic .resx translation — `kategori` — paired with its own footprint),
//! the page and its code-behind sat at ranks 121/122 with a vector-only
//! signal although their file name is composed of five story words, and the
//! SQL/dbml rows sat at 176/198 because rendering was layer-first.
//!
//! The fixture reproduces that shape and the acceptance is RANK-based, as
//! the auditor demanded: every critical file in the top 30 of a PRIMARY set
//! of at most 40; a file whose only evidence is a broad translated term is
//! never primary.

use engram_core::config::Config;
use engram_server::handlers::planning_tools::{CHANGE_SET_PRIMARY_CAP, NAME_COVERAGE_MIN};
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As an admin I want to set a main reporting category (huvudredovisningskategori) for each production code list category so that time reports roll up to it";

const MUST: &[&str] = &[
    "productioncodelistmaincategory.aspx",
    "productioncodelistmaincategory.aspx.vb",
    "rk_redovisningskategorier.sql",
    "redovisningskategorier.vb",
    "api-redovisning.vb",
];

fn resx(entries: &[(&str, &str)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root>\n");
    for (k, v) in entries {
        s.push_str(&format!(
            "  <data name=\"{k}\" xml:space=\"preserve\">\n    <value>{v}</value>\n  </data>\n"
        ));
    }
    s.push_str("</root>\n");
    s
}

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for d in [
        "Site/App_GlobalResources",
        "Site/App_Code/redovisning/code",
        "Site/App_Code/redovisning/api-json",
        "Site/modules/dashboard/pages/admin/production",
        "db-x.sql/dbo/Tables",
        "Site/App_Code/noise",
        "Site/App_Code/generic",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("Site/App_GlobalResources/text.resx"),
        resx(&[
            ("Category", "Kategori"),
            ("Reports", "Rapporter"),
            ("Code_list", "Kodlista"),
            ("Save", "Spara"),
        ]),
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_GlobalResources/text.en.resx"),
        resx(&[
            ("Category", "Category"),
            ("Reports", "Reports"),
            ("Code_list", "Code list"),
            ("Save", "Save"),
        ]),
    )
    .unwrap();
    // The critical files. The page pair carries NO story word in its content —
    // only its NAME is composed of story words (production/code/list/main/
    // category), exactly the live shape (the gloss never appears in it).
    std::fs::write(
        root.join("Site/modules/dashboard/pages/admin/production/productioncodelistmaincategory.aspx"),
        "<%@ Page Language=\"VB\" AutoEventWireup=\"false\" CodeFile=\"productioncodelistmaincategory.aspx.vb\" Inherits=\"pclmc\" %>\n<asp:GridView ID=\"GridView1\" runat=\"server\" />\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/modules/dashboard/pages/admin/production/productioncodelistmaincategory.aspx.vb"),
        "Partial Class pclmc\n    Inherits System.Web.UI.Page\n    Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load\n        GridView1.DataBind()\n    End Sub\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/redovisningskategorier.vb"),
        "Public Class redovisningskategorier\n    ' huvudredovisningskategori per kodlista\n    Public Function GetByProjectId(pr_id As Integer) As Object\n        Return Nothing\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/redovisning/api-json/api-redovisning.vb"),
        "Public Class api_redovisning\n    ' returns the huvudredovisningskategori for a category\n    Public Function GetCategories(qry As Object) As String\n        Return \"\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("db-x.sql/dbo/Tables/rk_redovisningskategorier.sql"),
        "CREATE TABLE [dbo].[rk_redovisningskategorier] (\n    [id] INT NOT NULL,\n    [huvudredovisningskategori_id] INT NULL,\n    [namn] NVARCHAR(200) NULL\n);\n",
    )
    .unwrap();
    // 60 files whose ONLY relation to the story is the generic translated
    // term `kategori` (and `rapporter`): the broad-term noise.
    for i in 0..60 {
        std::fs::write(
            root.join(format!("Site/App_Code/noise/kategori_helper{i:02}.vb")),
            format!(
                "Public Class kategori_helper{i:02}\n    ' kategori och rapporter\n    Public Function Hamta{i}() As String\n        Return \"kategori\"\n    End Function\nEnd Class\n"
            ),
        )
        .unwrap();
    }
    // 30 files matching the plain English concept `category` only.
    for i in 0..30 {
        std::fs::write(
            root.join(format!("Site/App_Code/generic/category_helper{i:02}.vb")),
            format!(
                "Public Class category_helper{i:02}\n    Public Function Describe{i}() As String\n        Return \"category\"\n    End Function\nEnd Class\n"
            ),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(300),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "RankFixture".into(),
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

async fn change_set(engram: &Engram, pid: &str, json_out: bool) -> String {
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": json_out}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

fn files(v: &Value) -> Vec<Value> {
    v["files"].as_array().cloned().unwrap_or_default()
}

fn path_of(f: &Value) -> String {
    f["path"].as_str().unwrap_or("").to_lowercase()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_critical_file_ranks_in_the_top_30_of_a_primary_set_of_at_most_40() {
    let (_tmp, engram, pid) = build().await;
    let t = change_set(&engram, &pid, true).await;
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    let fs = files(&v);
    let rendered: Vec<String> = fs.iter().map(path_of).collect();
    let primary: Vec<&Value> = fs.iter().filter(|f| f["set"] == "primary").collect();
    assert!(
        !primary.is_empty() && primary.len() <= CHANGE_SET_PRIMARY_CAP,
        "a PRIMARY set of 1..={CHANGE_SET_PRIMARY_CAP} candidates: got {} (files: {rendered:?})",
        primary.len()
    );
    for must in MUST {
        let pos = rendered.iter().position(|p| p.ends_with(must));
        let f = pos.map(|i| &fs[i]);
        assert!(
            pos.is_some_and(|i| i < 30) && f.is_some_and(|f| f["set"] == "primary"),
            "critical file `{must}` must be PRIMARY within the top 30 — position {pos:?}, row {f:?}\nfiles: {rendered:?}"
        );
    }
    // Rank numbering is the render order and starts at 1.
    assert_eq!(fs[0]["rank"], 1, "{:?}", fs[0]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generic_translated_term_alone_never_reaches_the_primary_set() {
    let (_tmp, engram, pid) = build().await;
    let t = change_set(&engram, &pid, true).await;
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    let all = files(&v);
    let noise: Vec<&Value> = all
        .iter()
        .filter(|f| path_of(f).contains("/noise/kategori_helper"))
        .collect();
    // Most of the noise is cut by the per-layer weak-signal tail cap (omitted
    // rows are not rendered and cannot be primary); the rows that do render
    // must all be labelled companions.
    assert!(
        !noise.is_empty(),
        "some broad-term noise renders as companions"
    );
    for f in &noise {
        assert!(
            f["set"] == "companion" && f["tier"].as_u64().unwrap_or(0) >= 3,
            "a file whose only evidence is the broad translation `kategori` is a COMPANION at tier >= 3: {f}"
        );
        let sig = f["signals"].to_string();
        assert!(
            sig.contains("broad") && !sig.contains("lexicon"),
            "the broad term is labelled `broad`, never `lexicon`: {f}"
        );
    }
    // The plain English concept `category` (30 files, below the broad
    // threshold) is concept-only evidence: tier 3, companion.
    for f in files(&v)
        .iter()
        .filter(|f| path_of(f).contains("/generic/category_helper"))
    {
        assert!(
            f["set"] == "companion" && f["tier"].as_u64().unwrap_or(0) >= 3,
            "single-arm concept evidence is not primary: {f}"
        );
    }
    let cov = v["coverage"]["concept"].to_string();
    assert!(
        cov.contains("broad") && cov.contains("kategori"),
        "coverage names the broad terms it refused to count: {cov}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_page_pair_is_found_by_story_word_coverage_of_its_name() {
    let (_tmp, engram, pid) = build().await;
    let t = change_set(&engram, &pid, true).await;
    let v: Value = serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"));
    for must in [
        "productioncodelistmaincategory.aspx",
        "productioncodelistmaincategory.aspx.vb",
    ] {
        let f = files(&v)
            .into_iter()
            .find(|f| path_of(f).ends_with(must))
            .unwrap_or_else(|| panic!("`{must}` must render: {t}"));
        assert!(
            f["signals"].to_string().contains("name"),
            "`{must}` carries the `name` signal (>= {NAME_COVERAGE_MIN} story words compose its name): {f}"
        );
        let why = f["why"].to_string().to_lowercase();
        assert!(
            why.contains("production") && why.contains("category"),
            "the provenance names the covering story words: {f}"
        );
        assert!(
            f["tier"].as_u64().unwrap_or(9) <= 1,
            "name coverage is golden evidence: {f}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_markdown_renders_a_ranked_primary_set_before_the_companions() {
    let (_tmp, engram, pid) = build().await;
    let t = change_set(&engram, &pid, false).await;
    let p = t
        .find("## Primary candidates")
        .unwrap_or_else(|| panic!("primary heading: {t}"));
    let c = t
        .find("## Possible companions")
        .unwrap_or_else(|| panic!("companions heading: {t}"));
    assert!(p < c, "primary before companions: {t}");
    let primary_block = &t[p..c];
    assert!(
        primary_block.contains("1. `"),
        "the primary set is numbered (rank): {primary_block}"
    );
    for must in MUST {
        assert!(
            primary_block.contains(must),
            "`{must}` is listed in the PRIMARY block:\n{primary_block}"
        );
    }
    assert!(
        !primary_block.contains("kategori_helper"),
        "broad-term noise is not primary:\n{primary_block}"
    );
}
