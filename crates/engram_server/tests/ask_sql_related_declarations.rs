//! Ordinary SQL answers retain admitted related evidence and callable recovery.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use futures::FutureExt;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::{panic::AssertUnwindSafe, path::Path};

const TABLE_PATH: &str = "db/Tables/Orders.sql";
const RELATED_PATH: &str = "db/Changes/Orders.sql";
const TABLE: &str = "CREATE TABLE [sales].[Orders] ([Id] int NOT NULL PRIMARY KEY, [AccountId] int NULL);\nGO\nCREATE TABLE [sales].[Accounts] ([Id] int NOT NULL PRIMARY KEY);\nGO\n";
const RELATED: &str = "CREATE NONCLUSTERED INDEX [IX_Orders_Account] ON [sales].[Orders] ([AccountId]);\nGO\nCREATE VIEW [sales].[OrderView] AS SELECT [Id], [AccountId] FROM [sales].[Orders];\nGO\nCREATE PROCEDURE [sales].[GetOrders] AS SELECT [Id] FROM [sales].[Orders];\nGO\nCREATE TRIGGER [sales].[TR_Orders_Audit] ON [sales].[Orders] AFTER INSERT AS BEGIN SELECT [Id] FROM inserted; END;\nGO\nALTER TABLE [sales].[Orders] ADD CONSTRAINT [FK_Orders_Accounts] FOREIGN KEY ([AccountId]) REFERENCES [sales].[Accounts] ([Id]);\nGO\nALTER TABLE [sales].[Orders] ADD [ReviewNote] nvarchar(60) NULL;\nGO\n";
const GUIDE: &str =
    "Source declarations do not establish deployed state. Exception: preserve manual approval.\n";
const CHANGE_GUIDE: &str = "---\napplyTo: db/Changes/**\n---\nReview migration conditions; preserve the rollback exception.\n";
const QUESTION: &str = "Describe the sales.Orders table and its source-declared indexes, triggers and related objects.";
const KEYS: [&str; 6] = [
    "indexes",
    "triggers",
    "views",
    "stored_procedures",
    "alter_table_foreign_keys",
    "alter_table_columns",
];
const NAMES: [&str; 6] = [
    "IX_Orders_Account",
    "TR_Orders_Audit",
    "OrderView",
    "GetOrders",
    "FK_Orders_Accounts",
    "ReviewNote",
];

fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

fn schema(answer: &Value) -> Result<Value> {
    let item = answer["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "schema")
        .ok_or_else(|| anyhow::anyhow!("Expected admitted source schema evidence"))?;
    Ok(serde_json::from_str(item["content"].as_str().unwrap())?)
}

async fn ask(server: &Engram, project: &str, question: &str, tokens: usize) -> Result<Value> {
    let result = server
        .handle_ask_codebase(serde_json::from_value(json!({
            "project_id":project,"question":question,"output_format":"json","max_tokens":tokens,
        }))?)
        .await?;
    Ok(serde_json::from_str(body(&result))?)
}

fn write(root: &Path, path: &str, source: &str) -> Result<()> {
    let target = root.join(path);
    std::fs::create_dir_all(target.parent().unwrap())?;
    std::fs::write(target, source)?;
    Ok(())
}

async fn run(mode: &str) -> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("sr-").tempdir_in(parent)?;
    let root = owned.path().join("root");
    for (path, source) in [
        (TABLE_PATH, TABLE),
        (RELATED_PATH, RELATED),
        ("AGENTS.md", GUIDE),
        (".github/instructions/changes.instructions.md", CHANGE_GUIDE),
        (
            ".github/instructions/other.instructions.md",
            "---\napplyTo: unrelated/**\n---\nUNRELATED_RULE_MUST_NOT_BE_SELECTED\n",
        ),
    ] {
        write(&root, path, source)?;
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
    let server = Engram::new(state);
    let result = AssertUnwindSafe(async {
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned SQL related evidence","project_type":"general","wait":true,
        }))?).await?;
        let project = body(&indexed).split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let answer = ask(&server, project, QUESTION, 1000000).await?;
        let facts = schema(&answer)?;
        ensure!(NAMES.iter().all(|name| facts.to_string().contains(name)));
        ensure!(facts["table"]["columns"].as_array().unwrap().len() == 2);
        ensure!(facts["source_coverage"]["database_execution"] == false);
        if mode == "full" {
            let response = server.handle_query_schema(serde_json::from_value(json!({
                "project_id":project,"query_type":{"kind":"table","name":"sales.Orders"},"max_tokens":65536,
            }))?).await?;
            let specialist: Value = serde_json::from_str(body(&response))?;
            for key in KEYS { ensure!(facts[key] == specialist["results"][0][key]); }
            for key in KEYS {
                let row = &facts[key][0];
                let citation = if row.get("object").is_some() { &row["object"]["citation"] } else { &row["citation"] };
                ensure!(citation["path"] == RELATED_PATH);
                ensure!(citation["source_hash"] == blake3::hash(RELATED.as_bytes()).to_hex().to_string());
                ensure!(citation["start_line"].as_u64().unwrap() > 0);
            }
            ensure!(answer["project_policy_sources"].to_string().contains("rollback exception"));
            ensure!(!answer["project_policy_sources"].to_string().contains("UNRELATED_RULE"));
            ensure!(facts["alter_table_columns"][0]["application_status"] == "not_established");
            let markdown = server.handle_ask_codebase(serde_json::from_value(json!({
                "project_id":project,"question":QUESTION,"output_format":"markdown","max_tokens":1000000,
            }))?).await?;
            ensure!(NAMES.iter().all(|name| body(&markdown).contains(name)));
            ensure!(body(&markdown).contains("declaration_included") && body(&markdown).contains("rollback exception"));
        } else if mode == "scope" {
            let scoped = ask(&server, project, &format!("{QUESTION} under db/Tables"), 1000000).await?;
            let scoped_facts = schema(&scoped)?;
            ensure!(scoped_facts["indexes"].as_array().unwrap().len() == 1);
            ensure!(scoped_facts["indexes"][0]["kind"] == "column_key_constraint");
            for key in &KEYS[1..] { ensure!(scoped_facts[*key].as_array().unwrap().is_empty()); }
            ensure!(!scoped["project_policy_sources"].to_string().contains("rollback exception"));
            write(&root, RELATED_PATH, "-- changed after selected source\n")?;
            let stale = ask(&server, project, QUESTION, 1000000).await?;
            let current = schema(&stale)?;
            ensure!(current["indexes"] == scoped_facts["indexes"]);
            for key in &KEYS[1..] { ensure!(current[*key].as_array().unwrap().is_empty()); }
            ensure!(stale["status"] != "answered");
            ensure!(stale["unknowns"].to_string().contains("SQL source differs from indexed fingerprint"));
        } else {
            for key in ["triggers", "views", "stored_procedures"] {
                let object = &facts[key][0]["object"];
                ensure!(object["declaration_included"] == false);
                let action = &object["recovery"];
                ensure!(action["tool"] == "get_full_method_body");
                let response = server.handle_get_full_method_body(serde_json::from_value(action["arguments"].clone())?).await?;
                let source: Value = serde_json::from_str(body(&response))?;
                ensure!(source["source_code"] == RELATED.trim_end_matches('\n'));
                ensure!(source["source_file_hash"] == blake3::hash(RELATED.as_bytes()).to_hex().to_string());
                ensure!(source["file_excerpt"]["reaches_eof"] == true);
            }
            let bounded = ask(&server, project, QUESTION, 2048).await?;
            ensure!(bounded["response_delivery"]["status"] == "DEFERRED_ASK_REPORT");
            write(&root, RELATED_PATH, "-- changed after report capture\n")?;
            write(&root, "AGENTS.md", "New guidance after capture.\n")?;
            let mut action = bounded["response_delivery"]["next_call"].clone();
            let mut restored = String::new();
            let mut pages = 0;
            while !action.is_null() {
                pages += 1;
                ensure!(pages <= 64, "Owned report recovery did not terminate");
                let response = server.handle_ask_codebase(serde_json::from_value(action["arguments"].clone())?).await?;
                ensure!(body(&response).len() <= 8192);
                let page: Value = serde_json::from_str(body(&response))?;
                ensure!(page["start_byte"] == restored.len());
                restored.push_str(page["content"].as_str().unwrap());
                action = page["next_call"].clone();
            }
            let original: Value = serde_json::from_str(&restored)?;
            ensure!(schema(&original)? == facts);
            ensure!(original["project_policy_sources"].to_string().contains("manual approval"));
            ensure!(bounded["response_delivery"]["response_id"] == blake3::hash(restored.as_bytes()).to_hex().to_string());
        }
        Ok::<(), anyhow::Error>(())
    }).catch_unwind().await;
    drop(server);
    owned.close()?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_table_answer_retains_complete_related_metadata_citations_guidance_and_markdown()
-> Result<()> {
    run("full").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_related_declarations_respect_scope_and_withhold_changed_source() -> Result<()> {
    run("scope").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_recovers_related_bodies_and_exact_saved_report_after_source_changes() -> Result<()> {
    run("core").await
}
