//! SQL report size must not erase selected declarations, qualifications or source identity.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::path::PathBuf;

struct Fixture {
    // On unwind, database/runtime owners close before their owned directory.
    server: Engram,
    owned: tempfile::TempDir,
    root: PathBuf,
    project: String,
    source: String,
}
impl Fixture {
    async fn new(large: bool) -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        std::fs::create_dir(&root)?;
        let source = if large {
            let mut sql = "CREATE PROCEDURE dbo.BuildReport AS\n".to_string();
            for i in 0..500 {
                sql.push_str(&format!(
                    "SELECT '{}{i}';\n",
                    "accepted qualification ".repeat(8)
                ));
            }
            sql.push_str("GO\n");
            sql
        } else {
            "CREATE TABLE dbo.Orders ([Id] int NOT NULL PRIMARY KEY, [Note] nvarchar(80) NULL);\nCREATE TABLE dbo.Archive ([Id] int NOT NULL);\n".into()
        };
        std::fs::write(root.join("schema.sql"), &source)?;
        std::fs::write(
            root.join("AGENTS.md"),
            "Schema declarations do not establish deployed state.\nException: preserve the separately accepted read-only path.\n",
        )?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: true,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned SQL report","project_type":"general","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().unwrap().text;
        let project = text
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
            root,
            project,
            source,
        })
    }
    async fn query(&self, query_type: Value, max_tokens: Option<usize>) -> Result<CallToolResult> {
        Ok(self
            .server
            .handle_query_schema(serde_json::from_value(json!({
                "project_id":self.project,"query_type":query_type,"max_tokens":max_tokens
            }))?)
            .await?)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_large_schema_report_recovers_after_source_deletion_without_query_arguments()
-> Result<()> {
    let fixture = Fixture::new(true).await?;
    let query = json!({"kind":"stored_proc","sp_name":"dbo.BuildReport"});
    let complete = fixture.query(query.clone(), Some(65536)).await?;
    ensure!(body(&complete).len() > 65536 && body(&complete).len() < 262144);
    let expected = serde_json::to_string(&complete)?;
    let original: Value = serde_json::from_str(body(&complete))?;
    ensure!(
        original.get("next_tools").is_some() && body(&complete).contains("find_missing_indexes")
    );
    ensure!(
        original["results"][0]["citation"]["source_hash"]
            == blake3::hash(fixture.source.as_bytes()).to_hex().to_string()
    );
    ensure!(body(&complete).contains("accepted read-only path") && body(&complete).contains("499"));
    let mut manifests = Vec::new();
    for budget in [None, Some(2048)] {
        let deferred = fixture.query(query.clone(), budget).await?;
        let manifest: Value = serde_json::from_str(body(&deferred))?;
        let limit = budget.unwrap_or(16384) * 4;
        ensure!(body(&deferred).len() <= limit);
        ensure!(manifest["report_content"] == "DEFERRED_NOT_SUPPLIED");
        ensure!(manifest["response_delivery"]["original_format"] == "mcp_call_tool_result_json");
        ensure!(manifest["response_delivery"]["response_budget_bytes"] == limit);
        ensure!(manifest["response_delivery"]["original_bytes"] == expected.len());
        ensure!(
            manifest["response_delivery"]["next_call"]["arguments"]
                .get("query_type")
                .is_none()
        );
        manifests.push(manifest);
    }
    std::fs::remove_file(fixture.root.join("schema.sql"))?;
    std::fs::write(
        fixture.root.join("AGENTS.md"),
        "CURRENT_GUIDANCE_MUST_NOT_REPLACE_CAPTURE\n",
    )?;
    for manifest in manifests {
        let mut action = manifest["response_delivery"]["next_call"].clone();
        let mut restored = String::new();
        for _ in 0..100 {
            let result = fixture
                .server
                .handle_query_schema(serde_json::from_value(action["arguments"].clone())?)
                .await?;
            let page: Value = serde_json::from_str(body(&result))?;
            ensure!(page["start_byte"] == restored.len());
            restored.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            action = page["next_call"].clone();
        }
        ensure!(restored == expected);
        ensure!(
            blake3::hash(restored.as_bytes()).to_hex().as_str()
                == manifest["response_delivery"]["response_id"]
                    .as_str()
                    .unwrap()
        );
        let mcp: CallToolResult = serde_json::from_str(&restored)?;
        let restored_report: Value = serde_json::from_str(body(&mcp))?;
        ensure!(restored_report == original);
        ensure!(restored_report["coverage"]["database_execution"] == false);
    }
    fixture.close()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_schema_reply_is_unchanged_across_budgets_and_paging_stays_a_fresh_query()
-> Result<()> {
    let fixture = Fixture::new(false).await?;
    let query = json!({"kind":"table","name":"dbo.Orders"});
    let small = fixture.query(query.clone(), Some(2048)).await?;
    let ample = fixture.query(query, Some(65536)).await?;
    ensure!(body(&small) == body(&ample));
    let value: Value = serde_json::from_str(body(&small))?;
    ensure!(value.get("response_delivery").is_none());
    ensure!(
        value["results"][0]["table"]["columns"]
            .as_array()
            .unwrap()
            .len()
            == 2
    );
    let page = fixture.server.handle_query_schema(serde_json::from_value(json!({
        "project_id":fixture.project,"query_type":{"kind":"column","table":"dbo.Orders","column":"Id"},"limit":1
    }))?).await?;
    let value: Value = serde_json::from_str(body(&page))?;
    ensure!(value["results"][0]["column"]["name"] == "Id");
    let first = fixture
        .server
        .handle_query_schema(serde_json::from_value(json!({
            "project_id":fixture.project,"query_type":{"kind":"all_tables"},"limit":1
        }))?)
        .await?;
    let first: Value = serde_json::from_str(body(&first))?;
    let action = &first["next_page"]["arguments"];
    ensure!(action["offset"] == 1 && action["query_type"]["kind"] == "all_tables");
    ensure!(action.get("response_id").is_none() && action.get("response_offset").is_none());
    let next = fixture
        .server
        .handle_query_schema(serde_json::from_value(action.clone())?)
        .await?;
    let next: Value = serde_json::from_str(body(&next))?;
    ensure!(next["results"][0]["name"] != first["results"][0]["name"]);
    ensure!(next["next_offset"].is_null());
    fixture.close()?;
    Ok(())
}

#[tokio::test]
async fn fresh_query_remains_required_while_delivery_controls_and_metadata_are_explicit()
-> Result<()> {
    let fixture = Fixture::new(false).await?;
    for (args, expected) in [
        (
            json!({"project_id":"unregistered"}),
            "query_type is required",
        ),
        (
            json!({"project_id":"unregistered","response_offset":1}),
            "response_offset requires response_id",
        ),
        (
            json!({"project_id":"unregistered","max_tokens":1,"query_type":{"kind":"all_tables"}}),
            "Analysis was not run",
        ),
        (
            json!({"project_id":"unregistered","max_tokens":65537}),
            "max_tokens must be 1..=65536",
        ),
        (
            json!({"project_id":"unregistered","response_id":"bad"}),
            "64-character lowercase BLAKE3",
        ),
    ] {
        let error = fixture
            .server
            .handle_query_schema(serde_json::from_value(args)?)
            .await
            .unwrap_err();
        ensure!(error.message.contains(expected), "{}", error.message);
    }
    let tools = fixture.server.tool_router.list_all();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "query_schema")
        .unwrap();
    let properties = tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap();
    ensure!(properties.contains_key("response_id") && properties.contains_key("response_offset"));
    let required = tool
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .unwrap();
    ensure!(required.iter().any(|field| field == "project_id"));
    ensure!(!required.iter().any(|field| field == "query_type"));
    ensure!(
        tool.description
            .as_ref()
            .unwrap()
            .contains("response_delivery.next_call")
    );
    fixture.close()?;
    Ok(())
}
