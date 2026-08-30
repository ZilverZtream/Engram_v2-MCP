#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P0-4): "35/35 does not mean 35
//! correct answers". Live, "Which resource keys describe the main code
//! category workflow?" was ANSWERED with ten evidence items and no .resx;
//! "Which reports (.rdl) read rk_redovisningskategorier?" cited no .rdl;
//! "Which table stores reporting categories?" cited no .sql/.dbml. The
//! question names an evidence MODALITY; retrieval must run a modality arm
//! and an answer without evidence of the requested modality is at most
//! Partial, with the gap named. A mention that IS a file stem
//! ("api-installationsobjektprojekt") must resolve to that file.

use engram_core::config::Config;
use engram_server::models::AskCodebaseRequest;
use engram_server::services::ask_engine::plan::Modality;
use engram_server::services::ask_engine::planner;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

const RDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Report xmlns="http://schemas.microsoft.com/sqlserver/reporting/2016/01/reportdefinition">
  <DataSets>
    <DataSet Name="Kategorier">
      <Query>
        <DataSourceName>iFalt</DataSourceName>
        <CommandText>SELECT id, namn FROM rk_redovisningskategorier ORDER BY namn</CommandText>
      </Query>
    </DataSet>
  </DataSets>
</Report>
"#;

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

async fn build(with_report: bool) -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for d in [
        "Site/Reports/redovisning",
        "Site/App_GlobalResources",
        "Site/App_Code/api-json",
        "Site/App_Code/redovisning",
        "db-x.sql/dbo/Tables",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    if with_report {
        std::fs::write(root.join("Site/Reports/redovisning/redovisning.rdl"), RDL).unwrap();
    }
    std::fs::write(
        root.join("db-x.sql/dbo/Tables/rk_redovisningskategorier.sql"),
        "CREATE TABLE [dbo].[rk_redovisningskategorier] (\n    [id] INT NOT NULL,\n    [namn] NVARCHAR(200) NULL,\n    [huvudkategori_id] INT NULL\n);\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_GlobalResources/text.resx"),
        resx(&[
            ("main_code_category_title", "Main code category"),
            (
                "main_code_category_workflow_hint",
                "Pick the main code category for the workflow",
            ),
            ("Save", "Save"),
        ]),
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/api-json/api-installationsobjektprojekt.vb"),
        "Public Class api_installationsobjektprojekt\n    Public Function ioUpdateBaseTypeInBulk(qry As Object) As String\n        If Not CanUserBulkUpdate(qry) Then Return \"denied\"\n        Return \"ok\"\n    End Function\n    Private Function CanUserBulkUpdate(qry As Object) As Boolean\n        Return True\n    End Function\nEnd Class\n",
    )
    .unwrap();
    // Enough code chunks about the same words to fill every top-k on their own.
    for i in 0..25 {
        std::fs::write(
            root.join(format!("Site/App_Code/redovisning/redovisning{i:02}.vb")),
            format!(
                "Public Class redovisning{i:02}\n    ' reads rk_redovisningskategorier for the main code category workflow (reporting categories)\n    Public Function GetCategories{i}() As Object\n        Return db.rk_redovisningskategorier.Where(Function(k) k.main_code_category = True)\n    End Function\nEnd Class\n"
            ),
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
            project_name: "ModalityFixture".into(),
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

async fn ask(engram: &Engram, pid: &str, question: &str) -> Value {
    let req: AskCodebaseRequest = serde_json::from_value(json!({
        "project_id": pid,
        "question": question,
        "output_format": "json",
        "depth": "standard"
    }))
    .unwrap();
    let res = engram.handle_ask_codebase(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let start = t.find('{').unwrap_or(0);
    serde_json::from_str(&t[start..]).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

fn paths(v: &Value) -> Vec<String> {
    v["evidence"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e["path"].as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_planner_detects_the_requested_modality_from_the_question() {
    let p = planner::plan_query("Which reports (.rdl) read the rk_redovisningskategorier table?");
    assert!(
        p.modalities.contains(&Modality::Report),
        "{:?}",
        p.modalities
    );
    assert!(p.modalities.contains(&Modality::Sql), "{:?}", p.modalities);
    let p = planner::plan_query("Which resource keys describe the main code category workflow?");
    assert_eq!(p.modalities, vec![Modality::Resource]);
    let p =
        planner::plan_query("Which table stores reporting categories (redovisningskategorier)?");
    assert_eq!(
        p.modalities,
        vec![Modality::Sql],
        "'reporting' is not a report request"
    );
    let p = planner::plan_query("Where is Check_pr_id defined and who calls it?");
    assert!(p.modalities.is_empty(), "{:?}", p.modalities);
    assert!(Modality::Report.matches("Site/Reports/x/Y.RDL"));
    assert!(!Modality::Report.matches("Site/x.vb"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_report_question_cites_report_evidence_even_when_code_dominates() {
    let (_tmp, engram, pid) = build(true).await;
    let v = ask(
        &engram,
        &pid,
        "Which reports (.rdl) read the rk_redovisningskategorier table?",
    )
    .await;
    let ps = paths(&v);
    assert!(
        ps.iter().any(|p| p.ends_with(".rdl")),
        "a .rdl evidence item must be cited (status {}): {ps:?}",
        v["status"]
    );
    assert_ne!(v["status"], "unsupported", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_table_question_cites_schema_evidence() {
    let (_tmp, engram, pid) = build(true).await;
    let v = ask(
        &engram,
        &pid,
        "Which table stores reporting categories (redovisningskategorier)?",
    )
    .await;
    let ps = paths(&v);
    assert!(
        ps.iter()
            .any(|p| p.ends_with(".sql") || p.ends_with(".dbml")),
        "a schema evidence item must be cited: {ps:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resource_key_question_cites_resx_evidence() {
    let (_tmp, engram, pid) = build(true).await;
    let v = ask(
        &engram,
        &pid,
        "Which resource keys describe the main code category workflow?",
    )
    .await;
    let ps = paths(&v);
    assert!(
        ps.iter().any(|p| p.ends_with(".resx")),
        "a .resx evidence item must be cited: {ps:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_without_the_requested_modality_is_partial_and_names_the_gap() {
    let (_tmp, engram, pid) = build(false).await;
    let v = ask(
        &engram,
        &pid,
        "Which reports (.rdl) read the rk_redovisningskategorier table?",
    )
    .await;
    assert_ne!(
        v["status"], "answered",
        "no report exists in the index, so the answer cannot be full: {v}"
    );
    let unknowns = v["unknowns"].to_string().to_lowercase();
    assert!(
        unknowns.contains("report") && unknowns.contains(".rdl"),
        "the coverage gap names the missing modality: {unknowns}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_stem_mention_resolves_to_that_file_and_is_cited() {
    let (_tmp, engram, pid) = build(true).await;
    let v = ask(
        &engram,
        &pid,
        "How are permission checks done in api-installationsobjektprojekt, and which endpoints read a client-supplied project id?",
    )
    .await;
    let ps = paths(&v);
    assert!(
        ps.iter()
            .any(|p| p.ends_with("api-installationsobjektprojekt.vb")),
        "the named file is cited: {ps:?}"
    );
}
