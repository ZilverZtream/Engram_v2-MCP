//! Keep every selected diff fact and rendered explanation under caller budgets.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram, read_report_delivery as delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    root: std::path::PathBuf,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        std::fs::create_dir_all(root.join("Site/Orders/validation"))?;
        let mut patch = String::new();
        for i in 0..80 {
            let name = format!("Site/Orders/validation/order_{i:03}.ts");
            let before = format!("export const threshold_{i} = 1;\n");
            let after = before.replace("= 1", "= 2");
            std::fs::write(root.join(&name), &before)?;
            patch.push_str(&format!("diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -1 +1 @@\n-{before}+{after}"));
        }
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
            "directory":root,"project_name":"Owned diff explanation","project_type":"general","wait":true
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
        std::fs::write(root.join("change.patch"), patch)?;
        Ok(Self {
            server,
            owned,
            root,
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
fn text(reply: &CallToolResult) -> &str {
    &reply.content[0].as_text().unwrap().text
}
fn prepared(project: &str, controls: Value) -> Result<delivery::Report> {
    let mut arguments = controls;
    arguments["project_id"] = json!(project);
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":"explain_change","arguments":arguments}))?;
    Ok(delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diff_explanations_keep_fitting_defaults_and_exact_json_markdown_recovery_after_patch_deletion()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let mut captures = Vec::new();
    for format in ["json", "markdown"] {
        let original = fixture
            .server
            .handle_explain_change(serde_json::from_value(json!({
                "project_id":fixture.project,"diff":"change.patch","output_format":format
            }))?)
            .await?;
        ensure!(text(&original).len() > 4096 && text(&original).len() < 40960);
        if format == "json" {
            let report: Value = serde_json::from_str(text(&original))?;
            let files = report["narrative"]["affected_files"].as_array().unwrap();
            ensure!(files.len() == 80);
            for i in 0..80 {
                ensure!(files.iter().any(|row|row["path"]==format!("Site/Orders/validation/order_{i:03}.ts")));
            }
            ensure!(
                report.get("commit_message").is_some()
                    && report.get("pr_description").is_some()
                    && report.get("changelog_entry").is_some()
            );
        } else {
            ensure!(
                text(&original).contains("## Commit message")
                    && text(&original).contains("## PR description")
            );
        }
        let exact = serde_json::to_string(&original)?;
        let fitting = prepared(
            &fixture.project,
            json!({"diff":"change.patch","output_format":format}),
        )?
        .finish(&fixture.server.state.cfg.data_dir, original.clone())?;
        ensure!(serde_json::to_string(&fitting)? == exact);
        let deferred = prepared(
            &fixture.project,
            json!({"diff":"change.patch","output_format":format,"max_tokens":1024}),
        )?
        .finish(&fixture.server.state.cfg.data_dir, original)?;
        ensure!(text(&deferred).len() <= 4096);
        let manifest: Value = serde_json::from_str(text(&deferred))?;
        ensure!(manifest["report_content"] == "DEFERRED_NOT_SUPPLIED");
        ensure!(manifest["response_delivery"]["original_format"] == "mcp_call_tool_result_json");
        captures.push((exact, manifest));
    }
    std::fs::remove_file(fixture.root.join("change.patch"))?;
    for (exact, manifest) in captures {
        let mut action = manifest["response_delivery"]["next_call"].clone();
        let mut restored = String::new();
        for _ in 0..100 {
            ensure!(action["arguments"].get("diff").is_none());
            let page = prepared(&fixture.project, action["arguments"].clone())?
                .recover(&fixture.server.state.cfg.data_dir)?
                .unwrap();
            ensure!(text(&page).len() <= 4096);
            let page: Value = serde_json::from_str(text(&page))?;
            ensure!(page["start_byte"] == restored.len());
            restored.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            action = page["next_call"].clone();
        }
        ensure!(restored == exact);
        ensure!(
            blake3::hash(restored.as_bytes()).to_hex().as_str()
                == manifest["response_delivery"]["response_id"]
                    .as_str()
                    .unwrap()
        );
    }
    fixture.close()
}
