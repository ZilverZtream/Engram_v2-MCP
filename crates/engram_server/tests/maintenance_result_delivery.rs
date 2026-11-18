//! Completed maintenance outcomes must remain recoverable without replay.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram, read_report_delivery as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content};
use serde_json::{Value, json};
use std::path::Path;

const PROJECT: &str = "owned-maintenance-delivery";
const TOOLS: &[&str] = &["repair_project", "index_git_history", "ingest_zip_history", "ingest_quality_gates"];

fn owned() -> Result<tempfile::TempDir> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    Ok(tempfile::Builder::new().prefix("mrd-").tempdir_in(parent)?)
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

fn prepare(tool: &str, arguments: Value) -> Result<(delivery::Report, CallToolRequestParam)> {
    let mut request = serde_json::from_value(json!({"name":tool,"arguments":arguments}))?;
    let report = delivery::prepare(&mut request, &Engram::tool_registry().list_all())?
        .ok_or_else(|| anyhow::anyhow!("Missing maintenance delivery for {tool}"))?;
    Ok((report, request))
}

fn recover(data: &Path, first: &CallToolResult) -> Result<String> {
    let manifest: Value = serde_json::from_str(text(first))?;
    let mut next = manifest["response_delivery"]["next_call"].clone();
    let mut exact = String::new();
    loop {
        ensure!(next["arguments"].get("adoption").is_none());
        ensure!(next["arguments"].get("scope").is_none());
        let (report, _) = prepare(next["tool"].as_str().unwrap(), next["arguments"].clone())?;
        let page = report.recover(data)?.unwrap();
        ensure!(text(&page).len() <= 8192);
        let page: Value = serde_json::from_str(text(&page))?;
        ensure!(page["start_byte"] == exact.len());
        exact.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true { break; }
        next = page["next_call"].clone();
    }
    ensure!(blake3::hash(exact.as_bytes()).to_hex().as_str() == manifest["response_delivery"]["response_id"].as_str().unwrap());
    ensure!(manifest["response_delivery"]["original_bytes"] == exact.len());
    Ok(exact)
}

fn expire_snapshot(directory: &Path, name: &str) -> Result<usize> {
    let mut found = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            found += expire_snapshot(&entry.path(), name)?;
        } else if entry.file_name().to_str() == Some(name) {
            let file = std::fs::OpenOptions::new().write(true).open(entry.path())?;
            file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600))?;
            found += 1;
        }
    }
    Ok(found)
}

#[tokio::test]
async fn complete_ownership_preview_survives_source_changes_without_adoption_or_reimport() -> Result<()> {
    let owned = owned()?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let rules: Vec<_> = (0..94).map(|i| json!({
        "rule":format!("Rule {i:03}: {} Exception: approved unsent drafts remain editable.",
            "Keep current tenant evidence separate from policy approval. ".repeat(9)),
        "path_scope":"Site/Reports/**"
    })).collect();
    let policy = serde_json::to_string(&rules)?;
    let source = root.join("policy.json");
    std::fs::write(&source, &policy)?;
    // Application/database handles precede owned directory teardown on unwind.
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"), allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default()
    })?;
    state.registry.put_project(&ProjectRecord {
        project_id: PROJECT.into(), project_name: "Owned maintenance delivery".into(),
        project_type: "general".into(), directory: root.to_string_lossy().into_owned(),
        created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    })?;
    let server = Engram::new(state);
    let (report, request) = prepare("ingest_quality_gates", json!({
        "project_id":PROJECT,"source_path":"policy.json","source_type":"rules","max_tokens":2048,
        "adoption":{"mode":"preview","expected_source_blake3":blake3::hash(policy.as_bytes()).to_hex().to_string(),
            "bindings":[],"decision_ref":"owned-read-only-discovery"}
    }))?;
    let original = server.handle_ingest_quality_gates(serde_json::from_value(json!(request.arguments.unwrap()))?).await?;
    ensure!(text(&original).len() > 40960);
    let first = report.finish(&server.state.cfg.data_dir, original.clone())?;
    ensure!(text(&first).len() <= 8192);
    std::fs::write(&source, "[{\"rule\":\"Changed since capture\"}]")?;
    let exact = recover(&server.state.cfg.data_dir, &first)?;
    let restored: Value = serde_json::from_str(&exact)?;
    ensure!(restored == serde_json::to_value(original)?);
    let preview: Value = serde_json::from_str(restored["content"][0]["text"].as_str().unwrap())?;
    ensure!(preview["source_entries"].as_array().unwrap().len() == 94);
    ensure!(preview["adoption_performed"] == false && preview["authored_files_written"] == 0);
    ensure!(preview["source_entries"].as_array().unwrap().iter().all(|row| row["text"].as_str().unwrap().contains("approved unsent drafts")));
    ensure!(server.state.registry.list_repo_rules(PROJECT)?.is_empty());
    drop(server);
    owned.close()?;
    Ok(())
}

#[test]
fn returned_operation_errors_and_successes_keep_their_outcomes_when_retention_fails() -> Result<()> {
    let owned = owned()?;
    let blocked = owned.path().join("blocked-data");
    std::fs::write(&blocked, "Owned file blocks report-cache directory creation")?;
    let catalog = tool_surface::advertised(Engram::tool_registry().list_all(), true);
    for tool in TOOLS {
        let schema = &catalog.iter().find(|entry| entry.name == *tool).unwrap().input_schema;
        for field in ["max_tokens", "response_id", "response_offset", "output_json"] {
            ensure!(schema["properties"].get(field).is_some());
        }
        for failed in [false, true] {
            let parts = vec![Content::text(format!("Returned {tool} outcome: preserve scope and approved exception.\n").repeat(1400)),
                Content::text("Original second result part; queued acknowledgement is not completion.")];
            let original = if failed { CallToolResult::error(parts) } else { CallToolResult::success(parts) };
            let args = json!({"project_id":PROJECT,"max_tokens":2048});
            let (report, _) = prepare(tool, args.clone())?;
            let first = report.finish(owned.path(), original.clone())?;
            ensure!(first.is_error == original.is_error && text(&first).len() <= 8192);
            let exact: Value = serde_json::from_str(&recover(owned.path(), &first)?)?;
            ensure!(exact == serde_json::to_value(&original)?);
            let manifest: Value = serde_json::from_str(text(&first))?;
            let id = manifest["response_delivery"]["response_id"].as_str().unwrap();
            ensure!(expire_snapshot(owned.path(), &format!("{id}.txt"))? == 1);
            let (expired, _) = prepare(tool, manifest["response_delivery"]["next_call"]["arguments"].clone())?;
            let refusal = expired.recover(owned.path()).unwrap_err();
            ensure!(refusal.message.contains("Do not repeat"));
            ensure!(!refusal.message.contains("rerun the original request"));
            let (report, _) = prepare(tool, args)?;
            let preserved = report.finish(&blocked, original.clone())?;
            ensure!(preserved.is_error == original.is_error);
            ensure!(serde_json::to_value(&preserved.content[..original.content.len()])? == serde_json::to_value(&original.content)?);
            let warning: Value = serde_json::from_str(&preserved.content.last().unwrap().as_text().unwrap().text)?;
            ensure!(warning["tool"] == *tool);
            ensure!(warning["response_delivery"]["status"] == "ORIGINAL_OUTCOME_PRESERVED_DELIVERY_FAILED");
            ensure!(warning["response_delivery"]["budget_honored"] == false);
        }
    }
    owned.close()?;
    Ok(())
}
