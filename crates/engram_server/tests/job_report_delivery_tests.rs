//! Job observation remains bounded without confusing captured and live state.
use anyhow::{Result, ensure};
use engram_core::{Config, JobRecord};
use engram_server::{AppState, Engram, read_report_delivery as delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn prepare(tool: &str, arguments: Value) -> Result<delivery::Report> {
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":tool,"arguments":arguments}))?;
    Ok(delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[tokio::test]
async fn captured_job_catalog_and_status_recover_after_rows_and_owners_are_gone() -> Result<()> {
    // This outer owner outlives all state/database owners, including unwinding.
    let owned = tempfile::tempdir()?;
    let data = owned.path().join("data");
    let captured = {
        let (state, _) = AppState::new(Config {
            data_dir: data.clone(),
            allowed_roots: vec![owned.path().to_path_buf()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        for i in 0..64 {
            state.registry.put_job(&JobRecord {
                job_id: format!("owned-background-job-{i:02}"),
                kind: "index_project".into(),
                project_id: Some(format!("owned-project-{}", i % 2)),
                status: "running".into(),
                message: "Observed source phase; completion is not whole-job completion.\n"
                    .repeat(180),
                progress_pct: 37,
                estimated_time_remaining_ms: Some(1000),
                created_at_ms: 1,
                updated_at_ms: 2,
            })?;
        }
        let server = Engram::new(state.clone());
        let all = server
            .handle_list_jobs(serde_json::from_value(json!({}))?)
            .await?;
        ensure!(text(&all).len() > 2048);
        let filtered = server
            .handle_list_jobs(serde_json::from_value(
                json!({"project_id":"owned-project-0"}),
            )?)
            .await?;
        ensure!(text(&filtered).lines().count() == 32);
        let status = server
            .handle_get_job_status(serde_json::from_value(
                json!({"job_id":"owned-background-job-00"}),
            )?)
            .await?;
        ensure!(text(&status).contains("status: running") && text(&status).contains("progress:"));
        let mut captured = Vec::new();
        for (tool, original) in [("list_jobs", all), ("get_job_status", status)] {
            let exact = serde_json::to_string(&original)?;
            let bounded = prepare(tool, json!({"max_tokens":512}))?.finish(&data, original)?;
            ensure!(text(&bounded).len() <= 2048);
            let manifest: Value = serde_json::from_str(text(&bounded))?;
            let action = manifest["response_delivery"]["next_call"].clone();
            ensure!(manifest.get("project_id").is_none());
            ensure!(action["arguments"].get("project_id").is_none());
            ensure!(action["arguments"].get("job_id").is_none());
            captured.push((exact, manifest, action));
        }
        for i in 0..64 {
            state
                .registry
                .delete_job(&format!("owned-background-job-{i:02}"))?;
        }
        let empty = server
            .handle_list_jobs(serde_json::from_value(json!({}))?)
            .await?;
        ensure!(text(&empty).is_empty());
        ensure!(
            server
                .handle_get_job_status(serde_json::from_value(
                    json!({"job_id":"owned-background-job-00"})
                )?)
                .await
                .is_err()
        );
        captured
    };
    for (exact, manifest, mut action) in captured {
        let mut recovered = String::new();
        while !action.is_null() {
            ensure!(action["arguments"].get("job_id").is_none());
            ensure!(action["arguments"].get("project_id").is_none());
            let report = prepare(
                action["tool"].as_str().unwrap(),
                action["arguments"].clone(),
            )?;
            let page = report.recover(&data)?.unwrap();
            ensure!(text(&page).len() <= 2048);
            let page: Value = serde_json::from_str(text(&page))?;
            recovered.push_str(page["content"].as_str().unwrap());
            action = page["next_call"].clone();
        }
        ensure!(
            recovered == exact,
            "Captured jobs must not be replaced by a fresh poll"
        );
        ensure!(
            manifest["response_delivery"]["response_id"]
                == blake3::hash(recovered.as_bytes()).to_hex().to_string()
        );
    }
    let tools = Engram::tool_registry().list_all();
    let cancel = tools.iter().find(|tool| tool.name == "cancel_job").unwrap();
    ensure!(
        delivery::eligible(cancel).is_none(),
        "Cancellation must remain a separate mutation"
    );
    Ok(())
}
