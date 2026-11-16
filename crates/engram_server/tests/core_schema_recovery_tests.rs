//! Returned clarification actions stay usable in the selected tool preset.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const SQL: &str = "CREATE TABLE [sales].[Orders] ([Id] int NOT NULL, [Note] nvarchar(40) NULL);\nCREATE TABLE [archive].[Orders] ([Archived] bit NOT NULL);\n";
fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_and_full_sql_clarifications_recover_the_selected_source_declaration() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let root = owned.path().join("source");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("Schema.sql"), SQL)?;
    for full in [false, true] {
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join(if full { "full" } else { "core" }),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: full,
            ..Default::default()
        })?;
        // Close each runtime/database owner before the next mode and fixture cleanup.
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned preset clarification","project_type":"general","wait":true,
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
        let ambiguous = server
            .handle_ask_codebase(serde_json::from_value(json!({
                "project_id":project,"question":"What are the columns of the Orders table?",
                "output_format":"json","max_tokens":1000000,
            }))?)
            .await?;
        let ambiguous: Value = serde_json::from_str(body(&ambiguous))?;
        ensure!(ambiguous["status"] == "ambiguous");
        let candidate = ambiguous["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["provider"] == "schema")
            .map(|item| serde_json::from_str::<Value>(item["content"].as_str().unwrap()))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .find(|item| item["candidate"]["name"] == "[sales].[Orders]")
            .unwrap();
        ensure!(candidate["status"] == "ambiguous");
        ensure!(candidate.get("table").is_none());
        let action = &candidate["recovery"];
        let selected = if full {
            ensure!(action["tool"] == "query_schema");
            ensure!(
                action["arguments"]
                    == json!({"project_id":project,
                "query_type":{"kind":"table","name":"[sales].[Orders]"}})
            );
            let result = server
                .handle_query_schema(serde_json::from_value(action["arguments"].clone())?)
                .await?;
            let result: Value = serde_json::from_str(body(&result))?;
            result["results"][0]["table"].clone()
        } else {
            ensure!(action["tool"] == "ask_codebase");
            ensure!(
                action["arguments"]["question"]
                    .as_str()
                    .unwrap()
                    .contains("\"[sales].[Orders]\"")
            );
            ensure!(action["arguments"]["output_format"] == "json");
            let result = server
                .handle_ask_codebase(serde_json::from_value(action["arguments"].clone())?)
                .await?;
            let result: Value = serde_json::from_str(body(&result))?;
            let schema = result["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["provider"] == "schema")
                .unwrap();
            let facts: Value = serde_json::from_str(schema["content"].as_str().unwrap())?;
            facts["table"].clone()
        };
        ensure!(selected["name"] == "[sales].[Orders]");
        ensure!(
            selected["columns"][0]["name"] == "Id" && selected["columns"][0]["nullable"] == false
        );
        ensure!(
            selected["columns"][1]["name"] == "Note" && selected["columns"][1]["nullable"] == true
        );
        ensure!(selected["citation"]["path"] == "Schema.sql");
        ensure!(
            selected["citation"]["source_hash"]
                == blake3::hash(SQL.as_bytes()).to_hex().to_string()
        );
        drop(server);
    }
    owned.close()?;
    Ok(())
}
