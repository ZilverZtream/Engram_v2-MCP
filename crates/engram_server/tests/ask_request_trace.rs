//! HTTP questions compose the ordinary source-bound tracer without a model.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::services::ask_engine::request_trace;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const SOURCE: &str = "Public Class language\nInherits System.Web.UI.Page\n<System.Web.Services.WebMethod()>\nPublic Shared Function change() As String\nReturn \"accepted\"\nEnd Function\nEnd Class\n";
const GUIDE: &str = "Preserve owner approval. Exception: read-only previews never submit.\n";

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        Self::in_directory("Site").await
    }
    async fn in_directory(directory: &str) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("ar-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join(directory))?;
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join("Other/Area"))?;
        for (path, body) in [
            (
                format!("{directory}/language.aspx"),
                "<%@ Page Language=\"VB\" CodeFile=\"language.aspx.vb\" Inherits=\"language\" %>\n",
            ),
            (format!("{directory}/language.aspx.vb"), SOURCE),
            ("Site/Global.asax".into(), "<%@ Application Language=\"VB\" %>\n"),
            ("Other/Area/Note.vb".into(), "Public Class Note\nEnd Class\n"),
            ("AGENTS.md".into(), GUIDE),
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
            "directory":root,"project_name":"Owned HTTP question","project_type":"general","wait":true
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
    async fn ask(&self, question: &str) -> Result<Value> {
        let result = self
            .server
            .handle_ask_codebase(serde_json::from_value(json!({
                "project_id":self.project,"question":question,"output_format":"json",
                "deadline_ms":5000,"max_tokens":65536
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
fn traces(answer: &Value) -> Vec<Value> {
    answer["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["provider"] == "request_trace")
        .map(|row| serde_json::from_str::<Value>(row["content"].as_str().unwrap()).unwrap())
        .collect()
}
fn handlers(trace: &Value) -> Vec<&Value> {
    trace["pages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|page| page["handlers"].as_array().unwrap())
        .collect()
}

#[test]
fn only_explicit_http_requests_activate_tracing_with_deduplication_and_cap_disclosure() {
    assert!(
        request_trace::queries("Explain the language change handler and its POST behavior")
            .is_empty()
    );
    assert!(request_trace::queries("Explain /language.aspx/change").is_empty());
    assert_eq!(
        request_trace::queries(
            "Explain `post /language.aspx/change`, then POST /language.aspx/change"
        ),
        vec!["POST /language.aspx/change"]
    );
    assert_eq!(
        request_trace::queries("GET /a.aspx GET /b.aspx GET /c.aspx GET /d.aspx").len(),
        3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_http_question_retains_exact_handler_source_guidance_and_executable_trace_recovery()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let direct = fixture.server.handle_trace_user_request(serde_json::from_value(json!({
        "project_id":fixture.project,"entry_point":"POST /language.aspx/change","max_tokens":16384
    }))?).await?;
    let direct: Value = serde_json::from_str(body(&direct))?;
    ensure!(handlers(&direct).len() == 1);
    let answer = fixture.ask("Explain POST /language.aspx/change").await?;
    let facts = traces(&answer);
    ensure!(facts.len() == 1, "{answer}");
    let trace = &facts[0]["trace"];
    ensure!(trace["active_generation"] == direct["active_generation"]);
    let found = handlers(trace);
    ensure!(
        found.len() == 1
            && found[0]["name"]
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case("change")
    );
    ensure!(
        trace["coverage"]["runtime_execution_proven"] == false && answer["status"] != "answered"
    );
    ensure!(
        trace["project_policy_sources"]
            .to_string()
            .contains("read-only previews never submit")
    );
    let expected_hash = blake3::hash(SOURCE.as_bytes()).to_hex().to_string();
    ensure!(
        trace["pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|page| page["source_members"].as_array().unwrap())
            .any(|member| member["file"] == "Site/language.aspx.vb"
                && member["source_blake3"] == expected_hash)
    );
    let recovered = fixture
        .server
        .handle_trace_user_request(serde_json::from_value(
            facts[0]["recovery"]["arguments"].clone(),
        )?)
        .await?;
    let recovered: Value = serde_json::from_str(body(&recovered))?;
    ensure!(handlers(&recovered) == found);
    let paired = fixture
        .ask("Compare POST /language.aspx/change and GET /language.aspx")
        .await?;
    ensure!(
        traces(&paired).len() == 2,
        "Distinct requests on one page must survive: {paired}"
    );
    let markdown = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(json!({
            "project_id":fixture.project,"question":"Explain POST /language.aspx/change",
            "output_format":"markdown","deadline_ms":5000,"max_tokens":65536
        }))?)
        .await?;
    ensure!(
        body(&markdown).contains("request_trace")
            && body(&markdown).contains("read-only previews never submit")
    );
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_or_unmatched_questions_do_not_invent_traces_and_changed_source_withholds_handlers()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let scoped = fixture
        .ask("Explain POST /language.aspx/change under Other/Area")
        .await?;
    ensure!(
        !scoped["plan"]["qualifiers"]["path_prefixes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    ensure!(traces(&scoped).is_empty());
    let provider = scoped["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["provider"] == "request_trace")
        .unwrap();
    ensure!(provider["status"] == "empty" && provider["examined"] == 1);
    let unmatched = fixture.ask("Explain GET /missing-owned.aspx").await?;
    ensure!(traces(&unmatched).is_empty());
    ensure!(
        unmatched["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["provider"] == "request_trace" && row["count"] == 0)
    );
    std::fs::write(
        fixture.owned.path().join("r/Site/language.aspx.vb"),
        format!("{SOURCE}\n' Changed owned source\n"),
    )?;
    let changed = fixture.ask("Explain POST /language.aspx/change").await?;
    ensure!(
        traces(&changed)
            .iter()
            .all(|fact| handlers(&fact["trace"]).is_empty())
    );
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_http_question_and_recovery_read_only_the_area_and_withhold_stale_handlers()
-> Result<()> {
    let fixture = Fixture::in_directory("Site/Area").await?;
    // The application marker is outside the requested source scope. Membership
    // establishes its root; its body and unrelated source must not be read.
    std::fs::remove_file(fixture.owned.path().join("r/Site/Global.asax"))?;
    std::fs::remove_file(fixture.owned.path().join("r/Other/Area/Note.vb"))?;
    let answer = fixture.ask("Explain POST /Area/language.aspx/change under Site/Area").await?;
    let facts = traces(&answer);
    ensure!(facts.len() == 1, "{answer}");
    let trace = &facts[0]["trace"];
    ensure!(handlers(trace).len() == 1, "{trace}");
    ensure!(trace["coverage"]["source_files_read"] == 2);
    ensure!(trace["coverage"]["source_bytes_read"].as_u64() == Some(
        SOURCE.len() as u64 + std::fs::metadata(fixture.owned.path().join("r/Site/Area/language.aspx"))?.len()));
    ensure!(trace["coverage"]["gaps"].as_array().unwrap().is_empty(), "{trace}");
    ensure!(trace["coverage"]["application_root_path_markers"] == json!(["Site/Global.asax"]));
    ensure!(trace["coverage"]["path_prefixes"].as_array().unwrap().iter().any(|scope| scope.as_str().is_some_and(|scope| scope.eq_ignore_ascii_case("Site/Area"))));
    ensure!(trace["project_policy_sources"].to_string().contains("read-only previews never submit"));
    let args = &facts[0]["recovery"]["arguments"];
    ensure!(args["path_prefixes"] == trace["coverage"]["path_prefixes"]);
    let recovered = fixture.server.handle_trace_user_request(serde_json::from_value(args.clone())?).await?;
    let recovered: Value = serde_json::from_str(body(&recovered))?;
    ensure!(handlers(&recovered) == handlers(trace));
    ensure!(recovered["coverage"]["source_files_read"] == 2);
    std::fs::write(fixture.owned.path().join("r/Site/Area/language.aspx.vb"), format!("{SOURCE}\n' Changed scoped source\n"))?;
    let changed = fixture.ask("Explain POST /Area/language.aspx/change under Site/Area").await?;
    ensure!(!traces(&changed).is_empty(), "The page candidate remains: {changed}");
    ensure!(traces(&changed).iter().all(|fact| handlers(&fact["trace"]).is_empty()));
    fixture.close()
}
