//! Agents must recover the generation they requested without publishing again.
use anyhow::{Result, ensure};
use engram_core::{Config, RepoRule};
use engram_server::{AppState, Engram, read_report_delivery as delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};

const HUMAN: &str = "Human guidance: preserve authorized cross-view saves.\n";

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

fn prepare(args: Value) -> Result<(delivery::Report, Value)> {
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":"produce_claude_md","arguments":args}))?;
    let report = delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap();
    Ok((report, Value::Object(request.arguments.unwrap())))
}

fn recover(data: &std::path::Path, initial: &CallToolResult, budget: usize) -> Result<Value> {
    ensure!(text(initial).len() <= budget);
    let manifest: Value = serde_json::from_str(text(initial))?;
    ensure!(manifest["operation"]["handler_returned"] == true);
    ensure!(
        manifest["operation"]["instruction"]
            .as_str()
            .unwrap()
            .contains("Do not repeat produce_claude_md")
    );
    let id = manifest["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut restored = String::new();
    while !action.is_null() {
        ensure!(action["tool"] == "produce_claude_md");
        let (report, _) = prepare(action["arguments"].clone())?;
        let result = report.recover(data)?.unwrap();
        ensure!(text(&result).len() <= budget);
        let page: Value = serde_json::from_str(text(&result))?;
        ensure!(page["start_byte"] == restored.len());
        restored.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(blake3::hash(restored.as_bytes()).to_hex().as_str() == id);
    Ok(serde_json::from_str(&restored)?)
}

struct Fixture {
    // Close runtime/database handles before the owned directory on unwinding.
    server: Option<Engram>,
    owned: tempfile::TempDir,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("ig-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("Orders.js"), "export const total = 1;\n")?;
        std::fs::write(root.join("CLAUDE.md"), HUMAN)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server
            .handle_index_project(serde_json::from_value(json!({
                "directory":root,"project_name":"Owned instruction generation",
                "project_type":"general","wait":true
            }))?)
            .await?;
        ensure!(indexed.is_error != Some(true), "{}", text(&indexed));
        let project = text(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        for index in 0..20 {
            server.state.registry.put_repo_rule(&project, &RepoRule {
                rule_id: format!("accepted-{index:02}"), file_pattern: "Orders.js".into(),
                rule_text: format!("Preserve scope {index}. {} Exception: authorized saves from other accessible views remain allowed.", "Source-bound condition; ".repeat(60)),
                priority: 5, updated_at_ms: 1, introduced_at: None,
                provenance: Some("manual repository rule".into()),
            })?;
        }
        Ok(Self {
            server: Some(server),
            owned,
            project,
        })
    }
    async fn generate(&self, write: bool) -> Result<CallToolResult> {
        self.server
            .as_ref()
            .unwrap()
            .handle_produce_claude_md(serde_json::from_value(json!({
                "project_id":self.project,"merge_existing":false,"generate_agents_md":true,
                "write_to_disk":write,"use_llm":false
            }))?)
            .await
            .map_err(Into::into)
    }
    fn close(mut self) -> Result<()> {
        drop(self.server.take());
        self.owned.close()?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preview_recovers_every_rule_and_exception_after_rule_changes_and_runtime_close()
-> Result<()> {
    let mut fixture = Fixture::new().await?;
    let result = async {
        let original = fixture.generate(false).await?;
        ensure!(text(&original).len() > 40960);
        let (default, _) = prepare(json!({"project_id":fixture.project}))?;
        let default_reply = default.finish(&fixture.owned.path().join("data"), original.clone())?;
        let (explicit, _) = prepare(json!({"project_id":fixture.project,"max_tokens":2048}))?;
        let explicit_reply =
            explicit.finish(&fixture.owned.path().join("data"), original.clone())?;
        let ledger_text = text(&original)
            .split("```json\n")
            .nth(1)
            .unwrap()
            .split("\n```")
            .next()
            .unwrap();
        let ledger: Value = serde_json::from_str(ledger_text)?;
        ensure!(ledger["inputs"].as_array().unwrap().len() == 20);
        ensure!(ledger["inputs"].as_array().unwrap().iter().all(
            |rule| rule["original"]["file_pattern"] == "Orders.js"
                && rule["original"]["rule_text"].as_str().unwrap().contains(
                    "Exception: authorized saves from other accessible views remain allowed."
                )
        ));
        let server = fixture.server.as_ref().unwrap();
        for mut rule in server.state.registry.list_repo_rules(&fixture.project)? {
            rule.rule_text = "New evidence must not replace the saved preview".into();
            server
                .state
                .registry
                .put_repo_rule(&fixture.project, &rule)?;
        }
        drop(fixture.server.take());
        let expected = serde_json::to_value(&original)?;
        ensure!(recover(&fixture.owned.path().join("data"), &default_reply, 40960)? == expected);
        ensure!(recover(&fixture.owned.path().join("data"), &explicit_reply, 8192)? == expected);
        ensure!(std::fs::read_to_string(fixture.owned.path().join("r/CLAUDE.md"))? == HUMAN);
        ensure!(
            !fixture
                .owned
                .path()
                .join("r/.engram/instruction-generation.json")
                .exists()
        );
        Ok(())
    }
    .await;
    result.and(fixture.close())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_outcome_survives_recovery_and_failed_retention_without_rewriting_files()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let result = async {
        let original = fixture.generate(true).await?;
        ensure!(text(&original).contains("# Written files") && text(&original).len() > 40960);
        let artifact = fixture.owned.path().join("r/CLAUDE.engram.md");
        ensure!(
            artifact.is_file(),
            "Owned publication did not write its diverted root"
        );
        let data = fixture.owned.path().join("data");
        let (report, _) = prepare(json!({"project_id":fixture.project,"max_tokens":2048}))?;
        let delivered = report.finish(&data, original.clone())?;
        std::fs::write(
            &artifact,
            "A subsequent authored edit must survive recovery\n",
        )?;
        ensure!(recover(&data, &delivered, 8192)? == serde_json::to_value(&original)?);
        ensure!(
            std::fs::read_to_string(&artifact)?
                == "A subsequent authored edit must survive recovery\n"
        );
        ensure!(std::fs::read_to_string(fixture.owned.path().join("r/CLAUDE.md"))? == HUMAN);
        let unavailable = fixture.owned.path().join("not-a-directory");
        std::fs::write(
            &unavailable,
            "Retained operation outcome must survive this storage failure",
        )?;
        let (report, _) = prepare(json!({"project_id":fixture.project,"max_tokens":2048}))?;
        let fallback = report.finish(&unavailable, original.clone())?;
        ensure!(
            serde_json::to_value(&fallback.content[0])?
                == serde_json::to_value(&original.content[0])?
        );
        let warning: Value =
            serde_json::from_str(&fallback.content.last().unwrap().as_text().unwrap().text)?;
        ensure!(
            warning["tool"] == "produce_claude_md"
                && warning["response_delivery"]["budget_honored"] == false
        );
        ensure!(
            warning["response_delivery"]["instruction"]
                .as_str()
                .unwrap()
                .contains("Do not repeat produce_claude_md")
        );
        Ok(())
    }
    .await;
    result.and(fixture.close())
}
