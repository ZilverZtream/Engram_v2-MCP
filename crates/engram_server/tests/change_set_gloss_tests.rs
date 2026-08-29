#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 P0-3: the reference story names its domain
//! entity in a parenthesized gloss — "a main reporting category
//! (huvudredovisningskategori)" — and get_change_set still extracted
//! `main, reporting, category`, cut the API file by the per-layer tail cap,
//! and never reached the table's .sql or the .dbml that declares it.
//!
//! Contract: an explicit gloss is a DEFAULT concept (index-corroborated,
//! compound suffix split), files that match it are never tail-capped away,
//! and the data layer of the matched class (its table's .sql, the .dbml
//! declaring the table) rides along.

use engram_core::config::Config;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const STORY: &str = "As an admin I want to set a main reporting category (huvudredovisningskategori) on a production code list category so that time reports roll up to it";

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for d in [
        "Site/App_Code/redovisning/code",
        "Site/App_Code/redovisning/api-json",
        "Site/App_Code",
        "db-x.sql/dbo/Tables",
        "Site/App_Code/noise",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("Site/App_Code/redovisning/code/redovisningskategorier.vb"),
        "Public Class redovisningskategorier\n    Public Function GetByProjectId(pr_id As Integer) As Object\n        Return (From k In db.rk_redovisningskategorier Where k.pr_id = pr_id).ToList()\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/redovisning/api-json/api-redovisning.vb"),
        "Public Class api_redovisning\n    Public Function GetCategories(qry As Object) As String\n        Dim list = _rv.redovisningskategorier.GetByProjectId(1)\n        Return \"ok\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("db-x.sql/dbo/Tables/rk_redovisningskategorier.sql"),
        "CREATE TABLE [dbo].[rk_redovisningskategorier] (\n    [id] INT NOT NULL,\n    [namn] NVARCHAR(200) NULL,\n    [huvudkategori_id] INT NULL\n);\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/iFalt.dbml"),
        "<?xml version=\"1.0\"?>\n<Database Name=\"iFalt\">\n  <Table Name=\"dbo.rk_redovisningskategorier\" Member=\"rk_redovisningskategorier\">\n    <Type Name=\"rk_redovisningskategorier\">\n      <Column Name=\"id\" Type=\"System.Int32\" />\n    </Type>\n  </Table>\n</Database>\n",
    )
    .unwrap();
    // Enough single-signal "category" noise to fill a layer's tail cap (18).
    for i in 0..40 {
        std::fs::write(
            root.join(format!("Site/App_Code/noise/category_helper{i:02}.vb")),
            format!("Public Class category_helper{i:02}\n    Public Function MainReporting{i}() As String\n        Return \"category\"\n    End Function\nEnd Class\n"),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
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
            project_name: "GlossFixture".into(),
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

async fn change_set(engram: &Engram, pid: &str) -> Value {
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": pid, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

fn paths(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| f["path"].as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_gloss_is_a_default_concept_and_its_family_is_rendered() {
    let (_tmp, engram, pid) = build().await;
    let v = change_set(&engram, &pid).await;

    let concepts: Vec<String> = v["concepts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_lowercase())
        .collect();
    assert!(
        concepts.iter().any(|c| c == "redovisningskategori"),
        "the gloss `huvudredovisningskategori` must resolve to the index-corroborated concept `redovisningskategori` BY DEFAULT, got {concepts:?}"
    );

    let files = paths(&v, "files");
    let omissions = paths(&v, "omissions");
    for must in [
        "redovisning/code/redovisningskategorier.vb",
        "redovisning/api-json/api-redovisning.vb",
        "rk_redovisningskategorier.sql",
        "ifalt.dbml",
    ] {
        assert!(
            files.iter().any(|p| p.contains(must)),
            "{must} must be a RENDERED candidate (files: {}; omitted: {}):\n{}",
            files.len(),
            omissions.len(),
            omissions
                .iter()
                .filter(|o| o.contains(must))
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}
