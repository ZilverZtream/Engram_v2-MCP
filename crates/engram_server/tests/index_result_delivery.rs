//! AI bootstrap must retain operation identities and recover without reindexing.
use anyhow::{Result, ensure};
use engram_server::{Engram, read_report_delivery as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content};
use serde_json::{Value, json};

const PROJECT: &str = "6b8073d5-dc42-4c82-a9e3-ddf699029949";
const JOB: &str = "8cda1ed8-3939-4aa4-a035-e33c9c8b249b";

fn prepare(args: Value) -> Result<(delivery::Report, CallToolRequestParam)> {
    let mut request = serde_json::from_value(json!({"name":"index_project","arguments":args}))?;
    let report = delivery::prepare(&mut request, &Engram::tool_registry().list_all())?
        .expect("index delivery must be available before registration");
    Ok((report, request))
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn indexed() -> String {
    format!("\u{2705} Indexed project_id: {PROJECT}\n\n# Indexing Report\n")
}

#[test]
fn core_bootstrap_keeps_plain_acknowledgements_and_exposes_typed_json_identities() -> Result<()> {
    for full in [true, false] {
        let tools = tool_surface::advertised(Engram::tool_registry().list_all(), full);
        let tool = tools
            .iter()
            .find(|tool| tool.name == "index_project")
            .unwrap();
        for key in [
            "max_tokens",
            "response_id",
            "response_offset",
            "output_json",
        ] {
            ensure!(tool.input_schema["properties"].get(key).is_some());
        }
        ensure!(tool.input_schema["required"] == json!([]));
        ensure!(
            tool.description
                .as_deref()
                .unwrap()
                .contains("never repeats indexing")
        );
    }
    let owned = tempfile::tempdir()?;
    for (header, kind, job) in [
        (indexed(), "indexed", None),
        (
            format!(
                "Existing project registration.\nproject_id: {PROJECT}\nproject_name: Owned\ndirectory: owned\nCheck freshness."
            ),
            "existing_registration",
            None,
        ),
        (
            format!("\u{1f7e1} Index job started.\njob_id: {JOB}\nproject_id: {PROJECT}"),
            "index_job_started",
            Some(JOB),
        ),
    ] {
        let original = CallToolResult::success(vec![Content::text(header)]);
        let args = json!({"directory":"owned","project_name":"Owned","wait":false});
        let (plain, request) = prepare(args.clone())?;
        ensure!(request.arguments.unwrap() == args.as_object().unwrap().clone());
        ensure!(
            serde_json::to_value(plain.finish(owned.path(), original.clone())?)?
                == serde_json::to_value(&original)?
        );
        let (json, _) =
            prepare(json!({"directory":"owned","project_name":"Owned","output_json":true}))?;
        let result = json.finish(owned.path(), original.clone())?;
        let envelope: Value = serde_json::from_str(text(&result))?;
        ensure!(envelope["report_scope"] == "server_operation");
        ensure!(envelope.get("project_id").is_none());
        ensure!(envelope["result"] == serde_json::to_value(original)?);
        ensure!(envelope["operation"]["project_id"] == PROJECT);
        ensure!(envelope["operation"]["acknowledgement"] == kind);
        ensure!(envelope["operation"]["job_id"].as_str() == job);
        ensure!(envelope["operation"].get("job_complete").is_none());
    }
    let (report, _) =
        prepare(json!({"directory":"owned","project_name":"Owned","output_json":true}))?;
    let original = CallToolResult::success(vec![Content::text(format!(
        "Path was rejected.\nproject_id: {PROJECT}"
    ))]);
    let result = report.finish(owned.path(), original)?;
    let value: Value = serde_json::from_str(text(&result))?;
    ensure!(value["operation"].get("project_id").is_none());
    ensure!(std::fs::read_dir(owned.path())?.count() == 0);
    Ok(())
}

#[test]
fn bounded_index_report_keeps_ids_and_recovers_every_warning_and_guidance_part() -> Result<()> {
    let owned = tempfile::tempdir()?;
    // The real formatter's warning output; no claim that normal projects exceed40KiB.
    let mut stats = engram_index::IngestStats::default();
    stats.files = 80;
    stats.unembedded_chunks = 80;
    stats.failed_embedding_batches = 1;
    stats.warnings = (0..80).map(|n| format!("Site/scripts/feature{n}.ts: source retained; embedding unavailable; preserve the lexical-only qualification and inspect provider status.")).collect();
    let original = CallToolResult::success(vec![
        Content::text(format!(
            "{}{}",
            indexed(),
            engram_server::services::project_service::generate_indexing_report(&stats)
        )),
        Content::text(
            "Authored instruction: owner review is required before changing a payment workflow.",
        ),
    ]);
    let (report, _) = prepare(
        json!({"directory":"owned","project_name":"Owned","max_tokens":1024,"output_json":true}),
    )?;
    let result = report.finish(owned.path(), original.clone())?;
    ensure!(text(&result).len() <= 4096);
    let manifest: Value = serde_json::from_str(text(&result))?;
    ensure!(manifest["operation"]["project_id"] == PROJECT);
    ensure!(manifest["operation"]["acknowledgement"] == "indexed");
    let id = manifest["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut body = String::new();
    while !action.is_null() {
        ensure!(action["tool"] == "index_project");
        for absent in [
            "project_id",
            "directory",
            "project_name",
            "wait",
            "dedupe_by_directory",
        ] {
            ensure!(action["arguments"].get(absent).is_none());
        }
        let (page, request) = prepare(action["arguments"].clone())?;
        ensure!(request.arguments.unwrap().is_empty());
        let page = page.recover(owned.path())?.unwrap();
        ensure!(text(&page).len() <= 4096);
        let page: Value = serde_json::from_str(text(&page))?;
        ensure!(page["start_byte"].as_u64() == Some(body.len() as u64));
        body.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(blake3::hash(body.as_bytes()).to_hex().as_str() == id);
    let recovered: CallToolResult = serde_json::from_str(&body)?;
    let envelope: Value = serde_json::from_str(text(&recovered))?;
    ensure!(envelope["result"] == serde_json::to_value(original)?);
    ensure!(envelope["operation"]["project_id"] == PROJECT);
    Ok(())
}

#[test]
fn refused_budget_and_unavailable_delivery_never_invite_duplicate_indexing() -> Result<()> {
    let error =
        prepare(json!({"directory":"owned","project_name":"Owned","max_tokens":1})).unwrap_err();
    ensure!(error.to_string().contains("Operation was not run"));
    let owned = tempfile::tempdir()?;
    let file = owned.path().join("not-a-directory");
    std::fs::write(&file, "Owned fixture")?;
    let original = CallToolResult::success(vec![
        Content::text(indexed()),
        Content::text("Original warning. ".repeat(1000)),
    ]);
    let (report, _) =
        prepare(json!({"directory":"owned","project_name":"Owned","max_tokens":1024}))?;
    let result = report.finish(&file, original.clone())?;
    ensure!(result.is_error != Some(true));
    let result_value = serde_json::to_value(&result)?;
    let original_value = serde_json::to_value(original)?;
    ensure!(result_value["content"][0] == original_value["content"][0]);
    ensure!(result_value["content"][1] == original_value["content"][1]);
    let warning: Value = serde_json::from_str(&result.content[2].as_text().unwrap().text)?;
    ensure!(warning["operation"]["project_id"] == PROJECT);
    ensure!(warning["response_delivery"]["budget_honored"] == false);
    ensure!(warning["response_delivery"]["recovery_available"] == false);
    let (recovery, _) = prepare(json!({"response_id":"0".repeat(64),"max_tokens":1024}))?;
    let error = recovery.recover(owned.path()).unwrap_err();
    ensure!(error.message.contains("Do not repeat index_project"));
    ensure!(!error.message.contains("rerun the original"));
    Ok(())
}
