//! Ordinary questions retain authored WebForms actions and executable recovery.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::services::ask_engine::request_trace;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const PAGE: &str = "<%@ Page Language='VB' CodeFile='Entry.aspx.vb' Inherits='Entry' AutoEventWireup='false' %>\n<asp:Button ID='btnSave' runat='server' OnClick='Save' />\n<asp:DropDownList ID='ddlType' runat='server' OnSelectedIndexChanged='Choose' />\n<asp:Button ID='btnDelete' runat='server' OnClick='Delete' />\n";
const SOURCE: &str = "Public Class Entry\nInherits System.Web.UI.Page\nPublic Sub LoadPage() Handles Me.Load\nEnd Sub\nPublic Sub Save()\nDim db As New DataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Preferences SET Plan=1\")\nEnd Sub\nPublic Sub Choose()\nEnd Sub\nPublic Sub Delete()\nEnd Sub\nEnd Class\n";
const GUIDE: &str = "Preserve owner approval. Exception: read-only previews never submit.\n";

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("aw-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site/Area"))?;
        std::fs::create_dir_all(root.join("Other/Area"))?;
        for (path, body) in [
            ("Site/Area/Entry.aspx", PAGE.to_owned()),
            ("Site/Area/Entry.aspx.vb", SOURCE.to_owned()),
            (
                "Site/Global.asax",
                "<%@ Application Language='VB' %>\n".into(),
            ),
            (
                "Other/Area/Note.vb",
                "Public Class Note\nEnd Class\n".into(),
            ),
            (
                "AGENTS.md",
                format!(
                    "{GUIDE}{}",
                    "Preserve the accepted preview exception.\n".repeat(200)
                ),
            ),
        ] {
            std::fs::write(root.join(path), body)?;
        }
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: true,
            watch_debounce_secs: 3600,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned WebForms questions","project_type":"general","wait":true
        }))?).await?;
        let project = body(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            server,
            owned,
            project,
        })
    }
    async fn ask(&self, question: &str, budget: usize) -> Result<Value> {
        let result = self
            .server
            .handle_ask_codebase(serde_json::from_value(json!({
                "project_id":self.project,"question":question,"output_format":"json",
                "deadline_ms":5000,"max_tokens":budget
            }))?)
            .await?;
        ensure!(result.is_error != Some(true));
        Ok(serde_json::from_str(body(&result))?)
    }
    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}
fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn facts(answer: &Value) -> Vec<Value> {
    answer["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["provider"] == "request_trace")
        .map(|row| serde_json::from_str(row["content"].as_str().unwrap()).unwrap())
        .collect()
}
fn handlers(trace: &Value) -> Vec<String> {
    trace["pages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|page| page["handlers"].as_array().unwrap())
        .map(|handler| handler["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn action_mentions_require_both_control_identity_and_page_and_share_one_cap() {
    for question in [
        "Explain the Save button",
        "What happens when I click btnSave?",
        "Explain event ddlType on /Entry.aspx",
        "Explain btnSave on /Entry.aspx",
    ] {
        assert!(request_trace::requests(question).is_empty(), "{question}");
    }
    let queries = request_trace::requests(
        "click `btnSave` on `/Entry.aspx`, click btnSave on /Entry.aspx, click btnSave on /Entry.aspx, event ddlType.SelectedIndexChanged in /Entry.aspx, GET /Entry.aspx, GET /other.aspx",
    );
    assert_eq!(queries.len(), 3);
    assert_eq!(queries[0].action.as_deref(), Some("click btnSave"));
    assert_eq!(
        queries[1].action.as_deref(),
        Some("event ddlType.SelectedIndexChanged")
    );
    assert_eq!(queries[2].entry_point, "GET /Entry.aspx");
    assert_eq!(queries[2].action, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn click_and_declared_event_questions_return_selected_handlers_sql_and_action_recovery()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let answer = fixture
        .ask(
            "What happens when I click btnSave on /Area/Entry.aspx under Site/Area?",
            65536,
        )
        .await?;
    let rows = facts(&answer);
    ensure!(rows.len() == 1, "{answer}");
    let trace = &rows[0]["trace"];
    let names = handlers(trace);
    ensure!(names.iter().any(|name| name == "Save") && names.iter().any(|name| name == "LoadPage"));
    ensure!(
        !names
            .iter()
            .any(|name| name == "Delete" || name == "Choose")
    );
    ensure!(
        trace["pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|page| page["paths"].as_array().unwrap())
            .any(|hop| hop["kind"] == "inline_sql_writes_table"
                && hop["target_name"] == "Preferences")
    );
    let hash = blake3::hash(SOURCE.as_bytes()).to_hex().to_string();
    ensure!(
        trace["pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|page| page["source_members"].as_array().unwrap())
            .any(|member| member["file"] == "Site/Area/Entry.aspx.vb"
                && member["source_blake3"] == hash)
    );
    ensure!(
        trace["project_policy_sources"]
            .to_string()
            .contains("read-only previews never submit")
    );
    ensure!(
        trace["coverage"]["runtime_execution_proven"] == false && answer["status"] != "answered"
    );
    let args = rows[0]["recovery"]["arguments"].clone();
    ensure!(
        args["action"] == "click btnSave"
            && args["path_prefixes"] == trace["coverage"]["path_prefixes"]
    );
    let recovered = fixture
        .server
        .handle_trace_user_request(serde_json::from_value(args)?)
        .await?;
    ensure!(handlers(&serde_json::from_str::<Value>(body(&recovered))?) == names);
    let events = fixture
        .ask(
            "Explain event ddlType.SelectedIndexChanged in /Area/Entry.aspx under Site/Area",
            65536,
        )
        .await?;
    let rows = facts(&events);
    ensure!(rows.len() == 1);
    let names = handlers(&rows[0]["trace"]);
    ensure!(
        names.iter().any(|name| name == "Choose")
            && !names.iter().any(|name| name == "Save" || name == "Delete")
    );
    let compared = fixture.ask("Compare click btnSave on /Area/Entry.aspx and event ddlType.SelectedIndexChanged on /Area/Entry.aspx", 65536).await?;
    ensure!(
        facts(&compared).len() == 2,
        "Different actions on the same page must survive: {compared}"
    );
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_action_evidence_recovers_immutably_and_fresh_requests_withhold_changed_handlers()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let disjoint = fixture
        .ask(
            "Explain click btnSave on /Area/Entry.aspx under Other/Area",
            65536,
        )
        .await?;
    ensure!(facts(&disjoint).is_empty());
    let question = "Explain click btnSave on /Area/Entry.aspx under Site/Area";
    let deferred = fixture.ask(question, 4096).await?;
    let id = deferred["response_delivery"]["response_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Expected bounded deferred report: {deferred}"))?;
    std::fs::write(
        fixture.owned.path().join("r/AGENTS.md"),
        "Changed after report capture.\n",
    )?;
    std::fs::write(
        fixture.owned.path().join("r/Site/Area/Entry.aspx.vb"),
        format!("{SOURCE}\n' Changed after capture\n"),
    )?;
    let mut saved = String::new();
    let mut offset = 0;
    loop {
        let page = fixture
            .server
            .handle_ask_codebase(serde_json::from_value(json!({
                "project_id":fixture.project,"question":"","response_id":id,
                "response_offset":offset,"max_tokens":4096
            }))?)
            .await?;
        let page: Value = serde_json::from_str(body(&page))?;
        ensure!(page["start_byte"] == offset);
        saved.push_str(page["content"].as_str().unwrap());
        match page["next_offset"].as_u64() {
            Some(next) => {
                ensure!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    ensure!(blake3::hash(saved.as_bytes()).to_hex().as_str() == id);
    let saved: Value = serde_json::from_str(&saved)?;
    let rows = facts(&saved);
    ensure!(rows[0]["recovery"]["arguments"]["action"] == "click btnSave");
    ensure!(
        handlers(&rows[0]["trace"])
            .iter()
            .any(|name| name == "Save")
    );
    ensure!(
        saved
            .to_string()
            .contains("read-only previews never submit")
    );
    let current = fixture.ask(question, 65536).await?;
    ensure!(
        facts(&current)
            .iter()
            .all(|row| handlers(&row["trace"]).is_empty())
    );
    fixture.close()
}
