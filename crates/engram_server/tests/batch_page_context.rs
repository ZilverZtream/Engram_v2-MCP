//! Multi-page orientation preserves ordinary evidence, options and recovery.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const GUIDE: &str = "Preserve owner approval. Exception: read-only previews never submit.\n";
const CODE_GUIDE: &str =
    "---\napplyTo: Site/First.aspx.vb\n---\nKeep server authorization before writes.\n";

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("bp-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        let mut markup =
            "<%@ Page Language=\"VB\" CodeFile=\"First.aspx.vb\" Inherits=\"First\" %>\n"
                .to_owned();
        for number in 0..120 {
            markup.push_str(&format!(
                "<asp:TextBox ID=\"Value{number}\" runat=\"server\" />\n"
            ));
        }
        for (path, body) in [
            ("Site/First.aspx", markup.as_str()),
            (
                "Site/First.aspx.vb",
                "Public Class First\nPublic Sub Save()\nEnd Sub\nEnd Class\n",
            ),
            (
                "Site/Second.aspx",
                "<%@ Page Language=\"VB\" %>\n<asp:Label ID=\"Preview\" runat=\"server\" />\n",
            ),
            ("Site/web.config", "<configuration />\n"),
            ("AGENTS.md", GUIDE),
            (".github/instructions/code.instructions.md", CODE_GUIDE),
        ] {
            std::fs::write(root.join(path), body)?;
        }
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let recorded_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        state.registry.put_project(&ProjectRecord {
            project_id: "batch-page-owned".into(),
            project_name: "Owned multi-page orientation".into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into_owned(),
            created_at_ms: recorded_at,
            updated_at_ms: recorded_at,
            reindex_required_since_ms: None,
        })?;
        Ok(Self {
            server: Engram::new(state),
            owned,
        })
    }
    fn args(&self, page: &str, output_json: bool) -> Value {
        json!({"project_id":"batch-page-owned","aspx_file":page,"output_json":output_json,
            "include_master_page":false,"include_house_style":false})
    }
    async fn read(&self, args: Value) -> Result<CallToolResult> {
        let result = self
            .server
            .handle_get_page_context(serde_json::from_value(args)?)
            .await?;
        ensure!(result.is_error != Some(true));
        Ok(result)
    }
    async fn batch(&self, reads: Vec<(&str, Value)>, tokens: usize) -> Result<(String, Value)> {
        let requests: Vec<Value> = reads.into_iter().map(|(id,args)|
            json!({"request_id":id,"request":{"tool":"get_page_context","arguments":args}})
        ).collect();
        let result = self
            .server
            .handle_batch_read(serde_json::from_value(
                json!({"requests":requests,"max_tokens":tokens}),
            )?)
            .await?;
        let text = body(&result).to_owned();
        let parsed = serde_json::from_str(&text)?;
        Ok((text, parsed))
    }
    fn close(self) -> Result<()> {
        let Self { server, owned } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}
fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn json_response(value: &Value) -> Result<Value> {
    let mut value = value.clone();
    for block in value["content"].as_array_mut().unwrap() {
        if block["type"] == "text" {
            block["text"] = serde_json::from_str(block["text"].as_str().unwrap())?;
        }
    }
    Ok(value)
}
fn row_body(row: &Value) -> &str {
    row["response"]["content"][0]["text"].as_str().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_pages_match_ordinary_json_markdown_options_and_keep_errors_independent()
-> Result<()> {
    let fixture = Fixture::new()?;
    let first = fixture.args("Site/First.aspx", true);
    let mut second = fixture.args("Site/Second.aspx", false);
    second["include_codebehind"] = json!(false);
    let singles = [
        fixture.read(first.clone()).await?,
        fixture.read(second.clone()).await?,
    ];
    ensure!(body(&singles[0]).contains("Keep server authorization before writes"));
    ensure!(body(&singles[1]).contains("read-only previews never submit"));
    ensure!(!body(&singles[1]).contains("Keep server authorization before writes"));
    let missing = fixture.args("Site/Missing.aspx", true);
    let (_, batch) = fixture
        .batch(
            vec![("first", first), ("missing", missing), ("second", second)],
            65536,
        )
        .await?;
    ensure!(batch["complete"] == true);
    ensure!(
        batch["results"][0]["request_id"] == "first"
            && batch["results"][2]["request_id"] == "second"
    );
    ensure!(json_response(&batch["results"][0]["response"])? == json_response(&serde_json::to_value(&singles[0])?)?);
    ensure!(batch["results"][2]["response"] == serde_json::to_value(&singles[1])?);
    ensure!(
        batch["results"][1]["status"] == "request_error"
            || batch["results"][1]["status"] == "tool_error"
    );
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_page_recovery_and_inner_continuations_preserve_policy_and_reject_changed_evidence()
-> Result<()> {
    let fixture = Fixture::new()?;
    let mut first = fixture.args("Site/First.aspx", true);
    first["max_tokens"] = json!(4096);
    let ordinary = fixture.read(first.clone()).await?;
    let inner: Value = serde_json::from_str(body(&ordinary))?;
    ensure!(
        !inner["continuation"].is_null(),
        "The owned control inventory must page"
    );
    let (text, omitted) = fixture.batch(vec![("first", first.clone())], 1024).await?;
    ensure!(
        text.len() <= 4096 && omitted["results"][0]["status"] == "omitted_shared_response_budget"
    );
    let row = &omitted["results"][0];
    ensure!(row["response"].is_null() && row["recovery"]["tool"] == "get_page_context");
    let recovered = fixture.read(row["recovery"]["arguments"].clone()).await?;
    ensure!(json_response(&serde_json::to_value(recovered)?)? == json_response(&serde_json::to_value(&ordinary)?)?);
    let (_, complete) = fixture.batch(vec![("first", first)], 65536).await?;
    ensure!(json_response(&complete["results"][0]["response"])? == json_response(&serde_json::to_value(&ordinary)?)?);
    let next = inner["continuation"]["arguments"].clone();
    let (_, continued) = fixture.batch(vec![("next", next.clone())], 65536).await?;
    ensure!(continued["results"][0]["status"] == "ok");
    let continued_inner: Value = serde_json::from_str(row_body(&continued["results"][0]))?;
    ensure!(
        continued_inner["required_context"]["project_policy_sources"]
            .to_string()
            .contains("read-only previews never submit")
    );
    std::fs::write(
        fixture.owned.path().join("r/AGENTS.md"),
        format!("{GUIDE}Keep the updated owner decision.\n"),
    )?;
    let second = fixture.args("Site/Second.aspx", false);
    let (_, changed) = fixture
        .batch(vec![("old-cursor", next), ("fresh", second)], 65536)
        .await?;
    ensure!(changed["results"][0]["status"] == "request_error");
    ensure!(
        changed["results"][0]["response"]["message"]
            .as_str()
            .unwrap()
            .contains("context_changed")
    );
    ensure!(
        changed["results"][1]["status"] == "ok"
            && row_body(&changed["results"][1]).contains("Keep the updated owner decision")
    );
    fixture.close()
}
