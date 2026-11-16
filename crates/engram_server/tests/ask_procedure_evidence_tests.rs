//! A procedure question must retain the existing reader's complete source facts.
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
async fn procedure_question_preserves_complete_reader_evidence_and_original_recovery() -> Result<()>
{
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
        "CREATE PROCEDURE [sales].[ReadOrderAudit] @OrderId int AS\nBEGIN\n{} SELECT 'ORIGINAL_PROCEDURE_TAIL' AS CompletionMarker;\nEND\nGO\n",
        (0..1200)
            .map(|i| format!(" SELECT @OrderId AS Observation{i:04};\n"))
            .collect::<String>()
    );
    let guide = "Procedure declarations are source evidence; preserve approval exceptions and do not infer deployment.\n";
    for (path, content) in [
        ("Audit.sql", sql.as_str()),
        (
            "Archive.sql",
            "CREATE PROCEDURE [archive].[ReadOrderAudit] AS SELECT 2;\nGO\n",
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
        advertise_all_tools: true,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        _owned: owned,
    };
    let index=fixture.server.handle_index_project(serde_json::from_value(json!({"directory":root,
        "project_name":"Owned procedure question","project_type":"dotnet_webforms_vb","wait":true}))?).await?;
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
        "query_type":{"kind":"stored_proc","sp_name":"sales.ReadOrderAudit"},"max_tokens":65536}))?)
        .await?;
    let direct: Value = serde_json::from_str(text(&direct))?;
    let args = json!({"project_id":project,"question":"Describe sales.ReadOrderAudit","output_format":"json","max_tokens":1000000});
    let full = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(args.clone())?)
        .await?;
    let full: Value = serde_json::from_str(text(&full))?;
    ensure!(
        full["status"] != "unsupported",
        "A complete matching source declaration must support the named procedure premise"
    );
    let evidence = full["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "schema")
        .unwrap();
    let facts: Value = serde_json::from_str(evidence["content"].as_str().unwrap())?;
    ensure!(facts["stored_procedure"] == direct["results"][0]);
    ensure!(
        facts["stored_procedure"]["declaration"]
            .as_str()
            .unwrap()
            .contains("ORIGINAL_PROCEDURE_TAIL")
    );
    ensure!(
        facts["stored_procedure"]["citation"]["source_hash"]
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
        "question":"Describe ReadOrderAudit","output_format":"json","max_tokens":1000000}))?)
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
    bounded["max_tokens"] = json!(1024);
    let bounded = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(bounded)?)
        .await?;
    ensure!(text(&bounded).len() <= 4096);
    let bounded: Value = serde_json::from_str(text(&bounded))?;
    let mut action = bounded["response_delivery"]["next_call"].clone();
    std::fs::write(
        root.join("Audit.sql"),
        sql.replace("ORIGINAL_PROCEDURE_TAIL", "CHANGED_AFTER_CAPTURE"),
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
        ensure!(text(&page).len() <= 4096);
        let page: Value = serde_json::from_str(text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(
        serde_json::from_str::<Value>(&recovered)? == full,
        "Saved recovery must preserve the complete original procedure and guidance after edits"
    );
    ensure!(
        bounded["response_delivery"]["response_id"]
            == blake3::hash(recovered.as_bytes()).to_hex().to_string()
    );
    Ok(())
}
