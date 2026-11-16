//! Wanted opt-in delivery: machine-readable reports retain original evidence.
use anyhow::{Result, ensure};
use engram_server::{Engram, read_report_delivery as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content};
use serde_json::{Value, json};

fn prepare(tool: &str, args: Value) -> Result<(delivery::Report, CallToolRequestParam)> {
    let mut request = serde_json::from_value(json!({"name":tool,"arguments":args}))?;
    let report = delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap();
    Ok((report, request))
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[test]
fn json_report_surface_preserves_native_json_owners_and_default_replies() -> Result<()> {
    let registry = Engram::tool_registry().list_all();
    let raw = registry
        .iter()
        .find(|tool| tool.name == "list_memory_bank")
        .unwrap();
    ensure!(raw.input_schema["properties"].get("output_json").is_none());
    for catalog in [
        tool_surface::advertised(registry.clone(), true),
        tool_surface::advertised(registry.clone(), false),
    ] {
        let tool = catalog
            .iter()
            .find(|tool| tool.name == "list_memory_bank")
            .unwrap();
        ensure!(tool.input_schema["properties"]["output_json"]["type"] == "boolean");
    }
    let advanced = tool_surface::advanced(registry.clone());
    let state = advanced
        .iter()
        .find(|tool| tool.name == "trace_state_usage")
        .unwrap();
    ensure!(state.input_schema["properties"]["output_json"]["type"] == "boolean");
    let native = registry
        .iter()
        .find(|tool| tool.name == "autonomous_decision_gate")
        .unwrap();
    let advertised = delivery::advertise(native.clone());
    ensure!(
        advertised.input_schema["properties"]["output_json"]
            == native.input_schema["properties"]["output_json"]
    );
    let (_, request) = prepare(
        "autonomous_decision_gate",
        json!({"project_id":"owned-json",
        "proposed_change":"Preserve approval","output_json":true}),
    )?;
    ensure!(request.arguments.unwrap()["output_json"] == true);
    let owned = tempfile::tempdir()?;
    let original =
        CallToolResult::success(vec![Content::text("Source unverified; preserve approval.")]);
    let (plain, _) = prepare("list_memory_bank", json!({"project_id":"owned-json"}))?;
    ensure!(
        serde_json::to_value(plain.finish(owned.path(), original.clone())?)?
            == serde_json::to_value(original)?
    );
    ensure!(std::fs::read_dir(owned.path())?.count() == 0);
    Ok(())
}

#[test]
fn escaped_json_budget_and_recovery_retain_all_original_evidence_parts() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let original = CallToolResult::success(vec![
        Content::text("\"\\\n".repeat(450)),
        Content::text("Å: source hash abc123. Exception: require approval; coverage incomplete."),
    ]);
    ensure!(
        original
            .content
            .iter()
            .map(|row| row.as_text().unwrap().text.len())
            .sum::<usize>()
            < 2048
    );
    let (fitting, request) = prepare(
        "list_memory_bank",
        json!({"project_id":"owned-json",
        "output_json":true,"max_tokens":8192}),
    )?;
    ensure!(request.arguments.unwrap().get("output_json").is_none());
    let fitting = fitting.finish(owned.path(), original.clone())?;
    let envelope: Value = serde_json::from_str(text(&fitting))?;
    ensure!(
        envelope["schema_version"] == 1
            && envelope["tool"] == "list_memory_bank"
            && envelope["project_id"] == "owned-json"
    );
    ensure!(envelope["result"] == serde_json::to_value(&original)?);
    ensure!(
        envelope["response_delivery"]["coverage"]
            .as_str()
            .unwrap()
            .contains("Existing handler limits")
    );
    let exact = serde_json::to_string(&fitting)?;
    let (small, _) = prepare(
        "list_memory_bank",
        json!({"project_id":"owned-json",
        "output_json":true,"max_tokens":512}),
    )?;
    let deferred = small.finish(owned.path(), original)?;
    ensure!(text(&deferred).len() <= 2048);
    let manifest: Value = serde_json::from_str(text(&deferred))?;
    let id = manifest["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    ensure!(id == blake3::hash(exact.as_bytes()).to_hex().as_str());
    let mut recovered = String::new();
    loop {
        let (recovery, _) = prepare(
            "list_memory_bank",
            json!({"project_id":"owned-json",
            "response_id":id,"response_offset":recovered.len(),"max_tokens":512}),
        )?;
        let page = recovery.recover(owned.path())?.unwrap();
        ensure!(text(&page).len() <= 2048);
        let page: Value = serde_json::from_str(text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
    }
    ensure!(
        recovered == exact,
        "Recovery must preserve the escaped envelope and both original evidence parts exactly"
    );
    Ok(())
}
