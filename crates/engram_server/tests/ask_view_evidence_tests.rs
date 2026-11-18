//! Named SQL views must retain complete source facts through the natural-language entry.
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

async fn view_case(mode: &str) -> Result<()> {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("p")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let sql = format!(
        "CREATE VIEW [sales].[OrderSummary] AS\nSELECT\n{} 'ORIGINAL_VIEW_TAIL' AS CompletionMarker;\nGO\n",
        (0..1200)
            .map(|i| format!(" {i} AS Observation{i:04},\n"))
            .collect::<String>()
    );
    let guide = "View declarations are source evidence; preserve approval exceptions and do not infer deployment.\n";
    for (path, content) in [
        ("Audit.sql", sql.as_str()),
        (
            "Archive.sql",
            "CREATE VIEW [archive].[OrderSummary] AS SELECT 2 AS ArchivedOnly;\nGO\n",
        ),
        (
            "Settings.vb",
            "Public Module Settings\n Public Function ComputeOrderLimit() As Integer\n  Return 7\n End Function\nEnd Module\n",
        ),
        ("AGENTS.md", guide),
    ] {
        std::fs::write(root.join(path), content)?;
    }
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: mode != "core",
        watch_debounce_secs: 3600,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        _owned: owned,
    };
    let index=fixture.server.handle_index_project(serde_json::from_value(json!({"directory":root,
        "project_name":"Owned SQL view question","project_type":"dotnet_webforms_vb","wait":true}))?).await?;
    let index_text = text(&index);
    let project = index_text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let direct = fixture
        .server
        .handle_query_schema(serde_json::from_value(json!({"project_id":project,
        "query_type":{"kind":"view","name":"sales.OrderSummary"},"max_tokens":65536}))?)
        .await?;
    let direct: Value = serde_json::from_str(text(&direct))?;
    let args = json!({"project_id":project,"question":"Describe SQL view sales.OrderSummary","output_format":"json","max_tokens":1000000});
    let full = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(args.clone())?)
        .await?;
    let full: Value = serde_json::from_str(text(&full))?;
    ensure!(
        full["status"] != "unsupported",
        "A complete matching source declaration must support the named view premise"
    );
    let evidence = full["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "schema")
        .unwrap();
    let facts: Value = serde_json::from_str(evidence["content"].as_str().unwrap())?;
    ensure!(facts["view"] == direct["results"][0]);
    ensure!(
        facts["view"]["declaration"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_VIEW_TAIL")
    );
    ensure!(
        facts["view"]["citation"]["source_hash"]
            == blake3::hash(sql.as_bytes()).to_hex().to_string()
    );
    ensure!(
        full["project_policy_sources"]
            .to_string()
            .contains("preserve approval exceptions")
    );
    let ambiguous = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(json!({"project_id":project,
        "question":"Describe SQL view OrderSummary","output_format":"json","max_tokens":1000000}))?)
        .await?;
    let ambiguous: Value = serde_json::from_str(text(&ambiguous))?;
    ensure!(
        ambiguous["status"] == "ambiguous"
            && ambiguous["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["provider"] == "schema")
                .count()
                == 2
    );
    for evidence in ambiguous["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["provider"] == "schema")
    {
        let body: Value = serde_json::from_str(evidence["content"].as_str().unwrap())?;
        if mode == "core" {
            ensure!(body["recovery"]["tool"] == "ask_codebase");
            ensure!(
                body["recovery"]["arguments"]["question"]
                    .as_str()
                    .unwrap()
                    .contains("SQL view")
            );
        } else {
            ensure!(body["recovery"]["tool"] == "query_schema");
            ensure!(body["recovery"]["arguments"]["query_type"]["kind"] == "view");
        }
    }
    let control = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(json!({"project_id":project,
        "question":"Describe ComputeOrderLimit","output_format":"json","max_tokens":1000000}))?)
        .await?;
    let control: Value = serde_json::from_str(text(&control))?;
    ensure!(
        !control["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "schema")
    );
    let mut bounded = args.clone();
    bounded["max_tokens"] = json!(2048);
    let bounded = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(bounded)?)
        .await?;
    ensure!(text(&bounded).len() <= 8192);
    let bounded: Value = serde_json::from_str(text(&bounded))?;
    let mut action = bounded["response_delivery"]["next_call"].clone();
    std::fs::write(
        root.join("Audit.sql"),
        sql.replace("ORIGINAL_VIEW_TAIL", "CHANGED_AFTER_CAPTURE"),
    )?;
    std::fs::write(root.join("AGENTS.md"), "Changed guidance after capture.\n")?;
    let stale = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(args)?)
        .await?;
    let stale: Value = serde_json::from_str(text(&stale))?;
    ensure!(
        !stale["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "schema")
    );
    let mut recovered = String::new();
    while !action.is_null() {
        let page = fixture
            .server
            .handle_ask_codebase(serde_json::from_value(action["arguments"].clone())?)
            .await?;
        ensure!(text(&page).len() <= 8192);
        let page: Value = serde_json::from_str(text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(
        serde_json::from_str::<Value>(&recovered)? == full,
        "Saved recovery must preserve the complete original view and guidance after edits"
    );
    ensure!(
        bounded["response_delivery"]["response_id"]
            == blake3::hash(recovered.as_bytes()).to_hex().to_string()
    );
    let Fixture { server, _owned } = fixture;
    drop(server);
    _owned.close()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_view_questions_preserve_full_evidence_and_original_recovery() -> Result<()> {
    view_case("full").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_view_ambiguity_has_callable_recovery_without_schema_tool() -> Result<()> {
    view_case("core").await
}
