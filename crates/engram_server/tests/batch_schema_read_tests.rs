//! Multiple SQL source reads keep ordinary evidence and independent failures.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

struct Fixture {
    server: Engram,
    _owned: tempfile::TempDir,
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_batch_preserves_complete_reads_errors_policy_and_live_recovery() -> Result<()> {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("q")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let columns = (0..48)
        .map(|i| format!("[Column{i:02}] nvarchar(100) NULL"))
        .collect::<Vec<_>>()
        .join(",\n");
    let sql = format!(
        "CREATE TABLE [sales].[Orders] (\n{columns}\n);\nGO\nCREATE TABLE [dbo].[Customers] (\n{columns}\n);\n"
    );
    std::fs::write(root.join("Schema.sql"), &sql)?;
    std::fs::write(
        root.join("AGENTS.md"),
        "Preserve explicit schema qualification and accepted approval exceptions.\n",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: true,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        _owned: owned,
    };
    let indexed = fixture
        .server
        .handle_index_project(serde_json::from_value(json!({"directory":root,
        "project_name":"Owned SQL batch","project_type":"dotnet_webforms_vb","wait":true}))?)
        .await?;
    let indexed = text(&indexed);
    let project = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let first = json!({"project_id":project,"query_type":{"kind":"table","name":"sales.Orders"},"max_tokens":65536});
    let second = json!({"project_id":project,"query_type":{"kind":"table","name":"dbo.Customers"},"max_tokens":65536});
    let expected_first = fixture
        .server
        .handle_query_schema(serde_json::from_value(first.clone())?)
        .await?;
    let expected_second = fixture
        .server
        .handle_query_schema(serde_json::from_value(second.clone())?)
        .await?;
    let requests = json!([
        {"request_id":"orders","request":{"tool":"query_schema","arguments":first}},
        {"request_id":"missing","request":{"tool":"query_schema","arguments":{"project_id":"missing-owned-schema","query_type":{"kind":"table","name":"sales.Orders"}}}},
        {"request_id":"customers","request":{"tool":"query_schema","arguments":second}}
    ]);
    let full = fixture
        .server
        .handle_batch_read(serde_json::from_value(
            json!({"requests":requests,"max_tokens":65536}),
        )?)
        .await?;
    let full: Value = serde_json::from_str(text(&full))?;
    ensure!(full["complete"] == true && full["results"][1]["status"] == "request_error");
    ensure!(
        full["results"][0]["response"] == serde_json::to_value(&expected_first)?
            && full["results"][2]["response"] == serde_json::to_value(&expected_second)?
    );
    ensure!(
        full["results"][0]["response"]
            .to_string()
            .contains("accepted approval exceptions")
    );
    let bounded = fixture
        .server
        .handle_batch_read(serde_json::from_value(
            json!({"requests":requests,"max_tokens":1024}),
        )?)
        .await?;
    ensure!(text(&bounded).len() <= 4096);
    let bounded: Value = serde_json::from_str(text(&bounded))?;
    let omitted = bounded["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["status"] == "omitted_shared_response_budget")
        .collect::<Vec<_>>();
    ensure!(!omitted.is_empty());
    for row in &omitted {
        ensure!(row["response"].is_null() && row["recovery"]["tool"] == "query_schema");
        let recovered = fixture
            .server
            .handle_query_schema(serde_json::from_value(
                row["recovery"]["arguments"].clone(),
            )?)
            .await?;
        let facts: Value = serde_json::from_str(text(&recovered))?;
        ensure!(
            facts["results"][0]["table"]["columns"]
                .as_array()
                .unwrap()
                .len()
                == 48
        );
        ensure!(
            facts["results"][0]["table"]["citation"]["source_hash"]
                == blake3::hash(sql.as_bytes()).to_hex().to_string()
        );
    }
    std::fs::write(
        root.join("Schema.sql"),
        sql.replace("Column47", "ChangedAfterCapture"),
    )?;
    let action = &omitted[0]["recovery"];
    let changed = fixture
        .server
        .handle_query_schema(serde_json::from_value(action["arguments"].clone())?)
        .await?;
    let changed: Value = serde_json::from_str(text(&changed))?;
    ensure!(
        changed["results"].as_array().unwrap().is_empty()
            && !changed["coverage"]["gaps"].as_array().unwrap().is_empty(),
        "Ordinary omitted-result recovery must observe current source withholding rather than claim an atomic captured batch"
    );
    Ok(())
}
