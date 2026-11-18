//! Recorded review decisions must be usable through the ordinary question tool.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const PATH: &str = "Site/Reports/Invoice.aspx.vb";
const QUESTION: &str = "What prior review decisions apply to `Site/Reports/Invoice.aspx.vb`?";
const RATIONALE: &str = "Preserve the existing preview response for reporting clients. The recorded exception covers read-only previews of approved invoices. It does not authorize edits, exports or bypassing the server permission check. Reassess the exact current implementation and the complete conditions before treating the old review as relevant to a new change.";
const CONDITIONS: &str = "Approved reports remain read-only. Only the internal draft preview response is covered.";

struct Fixture {
    // Drop application/database handles before the owned directory on every exit.
    server: Engram,
    owned: tempfile::TempDir,
    project: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("ard-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        let target = root.join(PATH);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(target, "Public Class Invoice\nPublic Function ReadPreview() As Boolean\nReturn True\nEnd Function\nEnd Class\n")?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(),
            advertise_all_tools: true, watch_debounce_secs: 3600, ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned review question","project_type":"general","wait":true
        }))?).await?;
        let project = indexed.content[0].as_text().unwrap().text.split("project_id: ")
            .nth(1).and_then(|text| text.lines().next()).unwrap().trim().to_owned();
        Ok(Self { server, owned, project })
    }

    async fn record(&self, events: Vec<Value>) -> Result<()> {
        let result = self.server.handle_record_review_decisions(serde_json::from_value(json!({
            "project_id":self.project,"review_id":"PR-17","decisions":events
        }))?).await?;
        ensure!(result.is_error != Some(true));
        Ok(())
    }

    async fn ask(&self, question: &str, format: &str) -> Result<String> {
        let result = self.server.handle_ask_codebase(serde_json::from_value(json!({
            "project_id":self.project,"question":question,"output_format":format,"deadline_ms":2000,"max_tokens":16384
        }))?).await?;
        ensure!(result.is_error != Some(true));
        Ok(result.content[0].as_text().unwrap().text.clone())
    }

    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

fn event(identity: &str, finding: &str, kind: &str, supersedes: Option<&str>, path: &str, rationale: &str) -> Value {
    json!({"event_id":identity,"finding_id":finding,"supersedes":supersedes,"kind":kind,
        "source_url":format!("https://example.invalid/pr/17#{identity}"),"author":"Owned external reviewer",
        "recorded_at":"2026-11-17T12:00:00Z","rationale":rationale,"verification":null,
        "scope":{"reviewed_commit":"1111111111111111111111111111111111111111","paths":[path],"conditions":CONDITIONS}})
}

fn recorded(report: &Value) -> (&Value, Value) {
    let item = report["evidence"].as_array().unwrap().iter()
        .find(|row| row["provider"] == "review_decisions").expect("Missing recorded review evidence");
    let content = serde_json::from_str(item["content"].as_str().unwrap()).unwrap();
    (item, content)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_review_question_preserves_complete_scoped_event_and_advisory_authority() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.record(vec![
        event("kept", "preview", "accepted_exception", None, PATH, RATIONALE),
        event("other", "other-area", "accepted_exception", None, "Site/Other/Invoice.aspx.vb", &format!("See {PATH}; unrelated scope.")),
    ]).await?;
    let report: Value = serde_json::from_str(&fixture.ask(QUESTION, "json").await?)?;
    let (item, content) = recorded(&report);
    ensure!(item["authority"] == "agent_memory" && item["kind"] == "memory_note");
    ensure!(report["status"] != "answered" && report["mode"] == "retrieval_only");
    let candidates = content["candidates"].as_array().unwrap();
    ensure!(candidates.len() == 1 && candidates[0]["event"]["event_id"] == "kept");
    ensure!(candidates[0]["event"]["rationale"] == RATIONALE);
    ensure!(candidates[0]["event"]["scope"]["conditions"] == CONDITIONS);
    ensure!(candidates[0]["record_identity"]["blake3"].as_str().unwrap().len() == 64);
    ensure!(candidates[0]["event_history"]["tool"] == "get_review_decisions");
    ensure!(content["automatic_finding_suppression"] == false && content["current_head_fix_verification_performed"] == false);
    let markdown = fixture.ask(QUESTION, "markdown").await?;
    ensure!(markdown.contains(RATIONALE) && markdown.contains(CONDITIONS));
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_ruling_is_used_and_ordinary_code_questions_do_not_read_decisions() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.record(vec![event("old", "preview", "accepted_exception", None, PATH, RATIONALE)]).await?;
    fixture.record(vec![event("reopened", "preview", "open", Some("old"), PATH, "The exception needs renewed review against current source.")]).await?;
    let report: Value = serde_json::from_str(&fixture.ask(QUESTION, "json").await?)?;
    let (_, content) = recorded(&report);
    let candidates = content["candidates"].as_array().unwrap();
    ensure!(candidates.len() == 1 && candidates[0]["event"]["event_id"] == "reopened");
    ensure!(candidates[0]["event"]["kind"] == "open");
    let ordinary: Value = serde_json::from_str(&fixture.ask("Explain ReadPreview", "json").await?)?;
    ensure!(!ordinary["providers"].as_array().unwrap().iter().any(|row| row["provider"] == "review_decisions"));
    fixture.close()
}
