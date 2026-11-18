//! SQL functions are named source objects; ordinary program functions stay separate.
use anyhow::{Context, Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram, services::schema_query::Catalog};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const SQL: &str = "CREATE FUNCTION [billing].[CalculateFee] (@amount decimal(10,2))\nRETURNS decimal(10,2)\nAS\nBEGIN\n DECLARE @rate decimal(10,2);\n SELECT @rate = Rate FROM dbo.FeeRates WHERE Id = 1;\n RETURN @amount * @rate;\nEND\nGO\n";
const GUIDE: &str = "SQL declarations are source evidence. Preserve approved fee exceptions; do not infer deployed rates.\n";

fn text(result: &CallToolResult) -> Result<&str> {
    Ok(&result.content[0].as_text().context("reply text")?.text)
}

#[test]
fn scalar_and_table_valued_functions_keep_named_bodies_and_do_not_become_tables() -> Result<()> {
    let mut catalog = Catalog::default();
    catalog.add_source("Fee.sql", SQL)?;
    let tvf = "CREATE OR ALTER FUNCTION dbo.OpenOrders() RETURNS TABLE AS RETURN (SELECT Id FROM dbo.Orders WHERE Done=0);\nGO\n";
    catalog.add_source("Open.sql", tvf)?;
    let request =
        |kind: Value| serde_json::from_value(json!({"project_id":"owned","query_type":kind}));
    let scalar = catalog.query(&request(
        json!({"kind":"function","name":"billing.CalculateFee"}),
    )?)?;
    ensure!(
        scalar["total_matches"] == 1 && scalar["results"][0]["name"] == "[billing].[CalculateFee]"
    );
    let row = &scalar["results"][0];
    ensure!(
        row["declaration"]
            .as_str()
            .context("declaration")?
            .contains("RETURN @amount * @rate")
    );
    ensure!(row["references"] == json!(["[dbo].[FeeRates]"]));
    ensure!(row["citation"]["source_hash"] == blake3::hash(SQL.as_bytes()).to_hex().as_str());
    ensure!(row["citation"]["start_line"] == 1 && row["citation"]["end_line"] == 8);
    let table_function = catalog.query(&request(
        json!({"kind":"function","name":"dbo.OpenOrders"}),
    )?)?;
    ensure!(
        table_function["results"][0]["declaration"]
            .as_str()
            .unwrap()
            .contains("RETURNS TABLE")
    );
    ensure!(table_function["results"][0]["references"] == json!(["[dbo].[Orders]"]));
    ensure!(
        catalog.query(&request(json!({"kind":"all_tables"}))?)?["total_matches"] == 0,
        "RETURNS TABLE must not invent a deployed table declaration"
    );
    Ok(())
}

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}

async fn named_function_case(core: bool) -> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("sf-").tempdir_in(parent)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    for (name, source) in [
        ("Fee.sql", SQL),
        (
            "Archive.sql",
            "CREATE FUNCTION archive.CalculateFee(@amount int) RETURNS int AS BEGIN RETURN @amount; END\nGO\n",
        ),
        (
            "Rates.sql",
            "CREATE TABLE dbo.FeeRates (Id int, Rate decimal(10,2));\n",
        ),
        (
            "client.js",
            "export function calculateClientFee() { return 7; }\n",
        ),
        ("AGENTS.md", GUIDE),
    ] {
        std::fs::write(root.join(name), source)?;
    }
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: !core,
        watch_debounce_secs: 3600,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        owned,
    };
    let indexed = fixture.server.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"Owned SQL function questions","project_type":"general","wait":true
    }))?).await?;
    let project = text(&indexed)?
        .split("project_id: ")
        .nth(1)
        .context("project id")?
        .lines()
        .next()
        .context("id line")?
        .trim()
        .to_owned();
    let ask = |question: &str| {
        serde_json::from_value(json!({"project_id":project,"question":question,
                                                            "output_format":"json","max_tokens":1000000}))
    };
    let answer = fixture
        .server
        .handle_ask_codebase(ask("Describe SQL function billing.CalculateFee")?)
        .await?;
    let answer: Value = serde_json::from_str(text(&answer)?)?;
    ensure!(answer["status"] != "unsupported");
    let evidence: Vec<_> = answer["evidence"]
        .as_array()
        .context("evidence")?
        .iter()
        .filter(|item| item["provider"] == "schema")
        .collect();
    ensure!(evidence.len() == 1, "{answer}");
    let facts: Value = serde_json::from_str(evidence[0]["content"].as_str().context("facts")?)?;
    ensure!(facts["function"]["kind"] == "function");
    ensure!(facts["function"]["name"] == "[billing].[CalculateFee]");
    ensure!(
        facts["function"]["declaration"]
            .as_str()
            .unwrap()
            .contains("RETURN @amount * @rate")
    );
    ensure!(facts["function"]["references"] == json!(["[dbo].[FeeRates]"]));
    ensure!(
        facts["function"]["citation"]["source_hash"]
            == blake3::hash(SQL.as_bytes()).to_hex().as_str()
    );
    ensure!(
        answer["project_policy_sources"]
            .to_string()
            .contains("Preserve approved fee exceptions")
    );
    let ambiguous = fixture
        .server
        .handle_ask_codebase(ask("Describe SQL function CalculateFee")?)
        .await?;
    let ambiguous: Value = serde_json::from_str(text(&ambiguous)?)?;
    ensure!(ambiguous["status"] == "ambiguous", "{ambiguous}");
    let candidates: Vec<_> = ambiguous["evidence"]
        .as_array()
        .context("candidates")?
        .iter()
        .filter(|item| item["provider"] == "schema")
        .collect();
    ensure!(candidates.len() == 2);
    for candidate in candidates {
        let candidate: Value = serde_json::from_str(candidate["content"].as_str().unwrap())?;
        let action = &candidate["recovery"];
        if core {
            ensure!(action["tool"] == "ask_codebase");
            let resolved = fixture
                .server
                .handle_ask_codebase(serde_json::from_value(action["arguments"].clone())?)
                .await?;
            let resolved: Value = serde_json::from_str(text(&resolved)?)?;
            ensure!(resolved["status"] != "ambiguous");
        } else {
            ensure!(
                action["tool"] == "query_schema"
                    && action["arguments"]["query_type"]["kind"] == "function"
            );
            let resolved = fixture
                .server
                .handle_query_schema(serde_json::from_value(action["arguments"].clone())?)
                .await?;
            let resolved: Value = serde_json::from_str(text(&resolved)?)?;
            ensure!(resolved["total_matches"] == 1);
        }
    }
    let ordinary = fixture
        .server
        .handle_ask_codebase(ask("Describe function calculateClientFee")?)
        .await?;
    let ordinary: Value = serde_json::from_str(text(&ordinary)?)?;
    ensure!(
        !ordinary["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "schema")
    );
    std::fs::write(
        root.join("Fee.sql"),
        SQL.replace("@amount * @rate", "@amount"),
    )?;
    let stale = fixture
        .server
        .handle_ask_codebase(ask("Describe SQL function billing.CalculateFee")?)
        .await?;
    let stale: Value = serde_json::from_str(text(&stale)?)?;
    ensure!(
        !stale["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "schema")
    );
    let Fixture { server, owned } = fixture;
    drop(server);
    owned.close()?;
    Ok(())
}

#[tokio::test]
async fn full_function_questions_preserve_complete_source_and_callable_qualified_choices()
-> Result<()> {
    named_function_case(false).await
}

#[tokio::test]
async fn core_function_questions_preserve_complete_source_and_callable_qualified_choices()
-> Result<()> {
    named_function_case(true).await
}
