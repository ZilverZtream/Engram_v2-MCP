//! A refused dossier source must lead to an executable, qualified source read.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram, tool_surface};
use serde_json::{Value, json};

const CHILD: &str = "widgets/Threshold.ascx";
const CONTROL: &str =
    "<%@ Control Language='VB' %>\n<asp:TextBox ID='Threshold' runat='server' />\n";

struct Fixture {
    // On success and unwind, close database/runtime owners before the directory.
    server: Engram,
    owned: tempfile::TempDir,
    project: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        std::fs::create_dir_all(root.join("widgets"))?;
        std::fs::write(root.join("web.config"), "<configuration />\n")?;
        std::fs::write(
            root.join("Settings.sql"),
            "CREATE TABLE dbo.Settings (Threshold int NOT NULL);\nCREATE TABLE dbo.SettingsAudit (AuditId int NOT NULL);\n",
        )?;
        std::fs::write(
            root.join("Settings.aspx"),
            "<%@ Page Language='VB' %>\n<%@ Register Src='~/widgets/Threshold.ascx' TagName='Threshold' TagPrefix='u' %>\n<u:Threshold runat='server' />\n",
        )?;
        std::fs::write(
            root.join("AGENTS.md"),
            "Preserve ownership checks and read-only exceptions.\n",
        )?;
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
        let indexed=server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned dossier recovery","project_type":"general","wait":true
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
        // Ordinary editing creates a registered component after the selected index.
        std::fs::write(root.join(CHILD), CONTROL)?;
        Ok(Self {
            server,
            owned,
            project,
        })
    }
    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unindexed_registered_control_retains_its_gap_and_returns_executable_current_source_recovery()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let generation = engram_server::services::project_service::get_active_generation(
        &fixture.server.state,
        &fixture.project,
    )
    .await?;
    let reply=fixture.server.handle_one_shot_feature_plan(serde_json::from_value(json!({
        "project_id":fixture.project,"feature_description":"Adjust Threshold settings editor",
        "model_after":"Settings.aspx","target_stack":"webforms_vbnet","detail_level":"Detailed","dossier_only":true,
        "schema_tables":["dbo.Settings", "dbo.Missing", "dbo.SettingsAudit"],"history_semantic_timeout_ms":0,
        "max_tokens":600
    }))?).await?;
    let text = &reply.content[0].as_text().unwrap().text;
    ensure!(text.len() <= 2400);
    let preview: Value = serde_json::from_str(text)?;
    let delivery = &preview["response_delivery"];
    ensure!(delivery["status"] == "DEFERRED_FEATURE_PLAN" && preview["provider_calls"] == 0);
    let mut action = delivery["next_call"].clone();
    let mut original = String::new();
    std::fs::write(
        fixture.owned.path().join("source/AGENTS.md"),
        "Different working-tree guidance after capture.\n",
    )?;
    while !action.is_null() {
        ensure!(action["tool"] == "one_shot_feature_plan");
        let reply = fixture
            .server
            .handle_one_shot_feature_plan(serde_json::from_value(action["arguments"].clone())?)
            .await?;
        let text = &reply.content[0].as_text().unwrap().text;
        ensure!(text.len() <= 2400);
        let page: Value = serde_json::from_str(text)?;
        original.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(Some(original.len() as u64) == delivery["original_bytes"].as_u64());
    ensure!(delivery["response_id"] == blake3::hash(original.as_bytes()).to_hex().as_str());
    ensure!(!original.contains("Different working-tree guidance after capture."));
    let dossier: Value = serde_json::from_str(&original)?;
    ensure!(dossier["provenance"]["provider_calls"] == 0);
    ensure!(dossier["source_coverage"]["explicit_model_after_file"] == "Settings.aspx");
    ensure!(dossier["source_evidence"][0]["file_path"] == "Settings.aspx");
    let schema = &dossier["planning_context"]["schema"][0];
    ensure!(
        schema["name"] == "dbo.Settings" && schema["result"]["active_generation"] == generation
    );
    let table = &schema["result"]["results"][0]["table"];
    ensure!(
        table["columns"][0]["name"] == "Threshold" && table["citation"]["path"] == "Settings.sql"
    );
    let schema = dossier["planning_context"]["schema"].as_array().unwrap();
    ensure!(schema.len() == 3);
    ensure!(schema[1]["name"] == "dbo.Missing" && schema[1]["result"]["results"] == json!([]));
    ensure!(schema[2]["result"]["active_generation"] == generation);
    ensure!(schema[2]["result"]["results"][0]["table"]["columns"][0]["name"] == "AuditId");
    ensure!(
        dossier["source_coverage"]["recovery"]["arguments"]["history_semantic_timeout_ms"] == 0
    );
    ensure!(
        !dossier["source_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["file_path"] == CHILD)
    );
    let gap = dossier["source_coverage"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["file_path"] == CHILD)
        .unwrap();
    let action = &gap["recovery"];
    ensure!(action["tool"] == "get_full_method_body");
    ensure!(
        action["arguments"]["project_id"] == fixture.project
            && action["arguments"]["file_path"] == CHILD
    );
    ensure!(
        gap["recovery_scope"]
            .as_str()
            .unwrap()
            .contains("does not admit source")
    );
    for full in [true, false] {
        ensure!(
            tool_surface::advertised(Engram::tool_registry().list_all(), full)
                .iter()
                .any(|tool| tool.name == action["tool"].as_str().unwrap())
        );
    }
    let source = fixture
        .server
        .handle_get_full_method_body(serde_json::from_value(action["arguments"].clone())?)
        .await?;
    let source: Value = serde_json::from_str(&source.content[0].as_text().unwrap().text)?;
    ensure!(source["file_path"] == CHILD);
    ensure!(
        source["source_code"]
            .as_str()
            .unwrap()
            .contains("<asp:TextBox ID='Threshold' runat='server' />")
    );
    ensure!(source["source_file_hash"] == blake3::hash(CONTROL.as_bytes()).to_hex().to_string());
    ensure!(
        engram_server::services::project_service::get_active_generation(
            &fixture.server.state,
            &fixture.project
        )
        .await?
            == generation
    );
    fixture.close()
}
