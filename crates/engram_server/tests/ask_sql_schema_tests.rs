//! Ordinary schema questions need all columns, not an apparently complete excerpt.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_question_preserves_complete_columns_and_withholds_changed_declarations() -> Result<()>
{
    let owned = tempfile::tempdir()?;
    let root = owned.path().join("source");
    std::fs::create_dir(&root)?;
    let columns = (1..=48)
        .map(|i| {
            format!(
                "[Attribute{i:02}] nvarchar(100) {}",
                if i % 2 == 0 { "NOT NULL" } else { "NULL" }
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let source = format!("CREATE TABLE [sales].[Orders] (\n{columns}\n);\n");
    std::fs::write(root.join("Orders.sql"), &source)?;
    std::fs::write(
        root.join("Settings.vb"),
        "Public Module Settings\n Public Function GetOrderLimit() As Integer\n  Return 7\n End Function\nEnd Module\n",
    )?;
    std::fs::write(
        root.join("Model.dbml"),
        r#"<Database Name="Owned" xmlns="http://schemas.microsoft.com/linqtosql/dbml/2007"><Table Name="dbo.LegacyAccounts" Member="LegacyAccounts"><Type Name="LegacyAccount"><Column Name="Id" Member="Id" Type="System.Int32" /></Type></Table></Database>"#,
    )?;
    // Same basename must not replace an explicitly qualified question target.
    std::fs::write(
        root.join("Archive.sql"),
        "CREATE TABLE [archive].[Orders] ([ArchivedOnly] bit NOT NULL);\n",
    )?;
    std::fs::write(
        root.join("AGENTS.md"),
        "Schema declarations do not establish deployed state.\n",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        owned,
    };
    let indexed = fixture.server.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"Owned SQL question","project_type":"general","wait":true,
    }))?).await?;
    let project = indexed.content[0]
        .as_text()
        .unwrap()
        .text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_owned();
    let request = json!({"project_id":project,"question":"What are the columns and nullability of the sales.Orders table?", "output_format":"json", "max_tokens":1000000});
    let response = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(request.clone())?)
        .await?;
    let report: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text)?;
    let evidence = report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "schema")
        .expect("complete schema evidence");
    let declaration: Value = serde_json::from_str(evidence["content"].as_str().unwrap())?;
    let table = &declaration["table"];
    let actual = table["columns"].as_array().unwrap();
    ensure!(actual.len() == 48);
    for (i, column) in actual.iter().enumerate() {
        ensure!(column["name"] == format!("Attribute{:02}", i + 1));
        ensure!(column["nullable"] == (i % 2 == 0));
        ensure!(column["data_type"] == "NVARCHAR(100)");
    }
    ensure!(table["citation"]["path"] == "Orders.sql");
    ensure!(
        table["citation"]["source_hash"] == blake3::hash(source.as_bytes()).to_hex().to_string()
    );
    ensure!(table["citation"]["start_line"] == 1 && table["citation"]["end_line"] == 50);
    ensure!(
        !evidence["content"]
            .as_str()
            .unwrap()
            .contains("ArchivedOnly")
    );
    ensure!(declaration["source_coverage"]["database_execution"] == false);
    ensure!(
        report["project_policy_sources"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["content"]
                .as_str()
                .unwrap_or("")
                .contains("do not establish deployed state"))
    );

    let described = fixture.server.handle_ask_codebase(serde_json::from_value(json!({
        "project_id":project,"question":"Describe sales.Orders","output_format":"json","max_tokens":1000000
    }))?).await?;
    let described: Value = serde_json::from_str(&described.content[0].as_text().unwrap().text)?;
    let described_table: Value = serde_json::from_str(
        described["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["provider"] == "schema")
            .expect("resolved SQL table selects schema evidence")["content"]
            .as_str()
            .unwrap(),
    )?;
    ensure!(described_table["table"] == *table);
    for question in ["Describe GetOrderLimit", "Describe LegacyAccounts"] {
        let control = fixture
            .server
            .handle_ask_codebase(serde_json::from_value(json!({
                "project_id":project,"question":question,"output_format":"json","max_tokens":1000000
            }))?)
            .await?;
        let control: Value = serde_json::from_str(&control.content[0].as_text().unwrap().text)?;
        ensure!(
            !control["providers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|provider| provider["provider"] == "schema")
        );
    }
    let ambiguous = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(json!({
            "project_id":project,"question":"What are the columns of the Orders table?",
            "output_format":"json","max_tokens":1000000
        }))?)
        .await?;
    let ambiguous: Value = serde_json::from_str(&ambiguous.content[0].as_text().unwrap().text)?;
    ensure!(ambiguous["status"] == "ambiguous");
    let candidates = ambiguous["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["provider"] == "schema")
        .collect::<Vec<_>>();
    ensure!(candidates.len() == 2);
    for candidate in candidates {
        let body: Value = serde_json::from_str(candidate["content"].as_str().unwrap())?;
        ensure!(body["status"] == "ambiguous" && body["candidate"]["citation"]["path"].is_string());
        ensure!(body.get("table").is_none());
        ensure!(body["recovery"]["arguments"]["query_type"]["name"].is_string());
    }
    std::fs::write(
        root.join("Orders.sql"),
        source.replace("Attribute48", "ChangedWithoutIndex"),
    )?;
    let changed = fixture
        .server
        .handle_ask_codebase(serde_json::from_value(request)?)
        .await?;
    let changed: Value = serde_json::from_str(&changed.content[0].as_text().unwrap().text)?;
    ensure!(changed["status"] != "answered");
    ensure!(
        !changed["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "schema")
    );
    ensure!(
        changed["unknowns"]
            .to_string()
            .contains("SQL source differs from indexed fingerprint")
    );
    let Fixture { server, owned } = fixture;
    drop(server);
    owned.close()?;
    Ok(())
}
