//! Refresh results must tell agents about failures and preserve late warnings.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{
    AppState, Engram, read_report_delivery as delivery, services::corpus_refresh::Report,
};
use rmcp::{
    ErrorData,
    model::{CallToolRequestParam, CallToolResult, Content},
};
use serde_json::{Value, json};

fn owned() -> Result<tempfile::TempDir> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(tempfile::Builder::new().prefix("cr-").tempdir_in(parent)?)
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn prepare(args: Value) -> Result<delivery::Report> {
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":"refresh_corpora","arguments":args}))?;
    Ok(delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[test]
fn complete_stage_errors_and_late_warnings_survive_budgeted_recovery() -> Result<()> {
    let owned = owned()?;
    let warning = format!(
        "{}\nLATE_WARNING: skipped embeddings; source rows remain lexical only",
        "Unchanged file observation\n".repeat(2200)
    );
    let success = CallToolResult::success(vec![
        Content::text(&warning),
        Content::text("Second original text part"),
    ]);
    let failed = CallToolResult::error(vec![Content::text(
        "Provider rejected the history operation",
    )]);
    let mut report = Report::default();
    report.record("files", Ok(success.clone()));
    report.record("git history", Ok(failed.clone()));
    report.record(
        "merged-PR corpus",
        Err(ErrorData::internal_error("Repository unavailable", None)),
    );
    report.skip("code-review corpora", "No credentials supplied");
    let original = report.finish();
    ensure!(original.is_error == Some(true));
    let body: Value = serde_json::from_str(text(&original))?;
    ensure!(body["stage_errors"] == 2);
    ensure!(body["stages"][0]["tool_result"] == serde_json::to_value(success)?);
    ensure!(body["stages"][1]["tool_result"] == serde_json::to_value(failed)?);
    ensure!(body["stages"][2]["error"]["message"] == "Repository unavailable");
    ensure!(body["stages"][3]["status"] == "skipped");
    let delivered = prepare(json!({"project_id":"owned-refresh","max_tokens":2048}))?
        .finish(owned.path(), original.clone())?;
    ensure!(delivered.is_error == Some(true) && text(&delivered).len() <= 8192);
    let manifest: Value = serde_json::from_str(text(&delivered))?;
    ensure!(manifest["operation"]["handler_returned"] == true);
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut bytes = String::new();
    while !action.is_null() {
        ensure!(action["tool"] == "refresh_corpora");
        let page = prepare(action["arguments"].clone())?
            .recover(owned.path())?
            .unwrap();
        ensure!(text(&page).len() <= 8192);
        let page: Value = serde_json::from_str(text(&page))?;
        ensure!(page["start_byte"] == bytes.len());
        bytes.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(
        blake3::hash(bytes.as_bytes()).to_hex().as_str()
            == manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    ensure!(serde_json::from_str::<Value>(&bytes)? == serde_json::to_value(&original)?);
    ensure!(bytes.contains("LATE_WARNING"));
    let json_result =
        prepare(json!({"project_id":"owned-refresh","output_json":true,"max_tokens":65536}))?
            .finish(owned.path(), original.clone())?;
    ensure!(json_result.is_error == Some(true));
    let envelope: Value = serde_json::from_str(text(&json_result))?;
    ensure!(envelope["result"] == serde_json::to_value(&original)?);
    let blocked = owned.path().join("not-a-directory");
    std::fs::write(&blocked, "Owned retention failure")?;
    let fallback = prepare(json!({"project_id":"owned-refresh","max_tokens":2048}))?
        .finish(&blocked, original.clone())?;
    ensure!(
        fallback.is_error == Some(true)
            && serde_json::to_value(&fallback.content[0])?
                == serde_json::to_value(&original.content[0])?
    );
    let warning: Value =
        serde_json::from_str(fallback.content[1].as_text().unwrap().text.as_str())?;
    ensure!(
        warning["tool"] == "refresh_corpora"
            && warning["response_delivery"]["budget_honored"] == false
    );
    ensure!(
        warning["response_delivery"]["instruction"]
            .as_str()
            .unwrap()
            .contains("Do not repeat")
    );
    owned.close()?;
    Ok(())
}

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let owned = owned()?;
        let root = owned.path().join("r");
        std::fs::create_dir(&root)?;
        std::fs::write(
            root.join("Orders.js"),
            "export const refreshOriginal = 1;\n",
        )?;
        std::fs::write(
            root.join("Orders.sql"),
            "CREATE TABLE dbo.Orders (Id int);\n",
        )?;
        {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let author = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &author,
                &author,
                "Owned refresh source",
                &tree,
                &[],
            )?;
        }
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            watch_debounce_secs: 3600,
            ..Default::default()
        })?;
        Ok(Self {
            server: Engram::new(state),
            owned,
        })
    }
    fn close(self) -> Result<()> {
        drop(self.server);
        self.owned.close()?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_refresh_reports_missing_projects_and_preserves_successful_stage_outcomes()
-> Result<()> {
    let fixture = Fixture::new()?;
    let outcome = async {
        let missing = fixture.server.handle_refresh_corpora(serde_json::from_value(json!({"project_id":"d0632809-a9bb-4a6e-b19d-18bb92a51d95"}))?).await?;
        ensure!(missing.is_error == Some(true));
        let body: Value = serde_json::from_str(text(&missing))?;
        ensure!(body["stage_errors"] == 3 && body["stages"].as_array().unwrap().len() == 4);
        let indexed = fixture.server.handle_index_project(serde_json::from_value(json!({"directory":fixture.owned.path().join("r"),"project_name":"Owned refresh","project_type":"general","wait":true}))?).await?;
        ensure!(indexed.is_error != Some(true), "{}", text(&indexed));
        let project = text(&indexed).split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        std::fs::write(fixture.owned.path().join("r/Orders.js"), "export const refreshReplacement = 2;\n")?;
        let result = fixture.server.handle_refresh_corpora(serde_json::from_value(json!({"project_id":project}))?).await?;
        ensure!(result.is_error != Some(true), "{}", text(&result));
        let body: Value = serde_json::from_str(text(&result))?;
        ensure!(body["stage_errors"] == 0 && body["status"] == "no_stage_errors_reported");
        for stage in body["stages"].as_array().unwrap().iter().take(3) {
            ensure!(stage["status"] == "returned");
            ensure!(!stage["tool_result"]["content"].as_array().unwrap().is_empty());
        }
        ensure!(body["stages"][3]["status"] == "skipped");
        let grep = fixture.server.handle_grep_project(serde_json::from_value(json!({"project_id":project,"pattern":"refreshReplacement","output_json":true}))?).await?;
        ensure!(text(&grep).contains("refreshReplacement"), "Refresh did not publish changed JS source");
        Ok(())
    }.await;
    let cleanup = fixture.close();
    outcome.and(cleanup)
}
