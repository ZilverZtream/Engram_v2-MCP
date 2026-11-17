//! Agent update outcomes must survive bounded delivery without repeating work.
use anyhow::{Result, ensure};
use engram_server::{Engram, read_report_delivery as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content};
use serde_json::{Value, json};

fn prepare(args: Value) -> Result<(delivery::Report, CallToolRequestParam)> {
    let mut request = serde_json::from_value(json!({"name":"update_project","arguments":args}))?;
    let report = delivery::prepare(&mut request, &Engram::tool_registry().list_all())?
        .expect("update result delivery must be available");
    Ok((report, request))
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[test]
fn update_surface_and_small_job_acknowledgement_keep_original_contract() -> Result<()> {
    for catalog in [
        tool_surface::advertised(Engram::tool_registry().list_all(), true),
        tool_surface::advertised(Engram::tool_registry().list_all(), false),
    ] {
        let tool = catalog
            .iter()
            .find(|tool| tool.name == "update_project")
            .unwrap();
        for control in [
            "max_tokens",
            "response_id",
            "response_offset",
            "output_json",
        ] {
            ensure!(tool.input_schema["properties"].get(control).is_some());
        }
        ensure!(
            tool.description
                .as_deref()
                .unwrap()
                .contains("never repeats the update")
        );
    }
    let owned = tempfile::tempdir()?;
    let args = json!({"project_id":"owned-update","wait":false,"max_commits":1});
    let (plain, request) = prepare(args.clone())?;
    ensure!(request.arguments.unwrap() == args.as_object().unwrap().clone());
    let original = CallToolResult::success(vec![Content::text(
        "Update job started.\njob_id: owned-job-1",
    )]);
    let delivered = plain.finish(owned.path(), original.clone())?;
    ensure!(serde_json::to_value(delivered)? == serde_json::to_value(original)?);
    ensure!(std::fs::read_dir(owned.path())?.count() == 0);
    Ok(())
}

#[test]
fn bounded_update_recovers_complete_acknowledgement_and_separate_observation() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let original = CallToolResult::success(vec![
        Content::text("Source update returned; unchanged-source refresh does not require a new generation."),
        Content::text(json!({"instruction_reference_observation":{
            "status":"observed","authored_files_written":0,"atomic_index_and_policy_snapshot":false,
            "before":"Require owner review.\n".repeat(1600),
            "after":"Require owner review; preserve the accepted exception.\n".repeat(1600)
        }}).to_string()),
    ]);
    let (report, request) = prepare(json!({"project_id":"owned-update","wait":true,
        "max_tokens":2048,"output_json":true}))?;
    ensure!(
        request.arguments.unwrap()
            == json!({"project_id":"owned-update","wait":true})
                .as_object()
                .unwrap()
                .clone()
    );
    let result = report.finish(owned.path(), original.clone())?;
    ensure!(text(&result).len() <= 8192);
    let manifest: Value = serde_json::from_str(text(&result))?;
    ensure!(manifest["operation"]["handler_returned"] == true);
    ensure!(
        manifest["operation"]["instruction"]
            .as_str()
            .unwrap()
            .contains("Do not repeat")
    );
    let id = manifest["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut recovered = String::new();
    while !action.is_null() {
        ensure!(action["tool"] == "update_project");
        let (page, request) = prepare(action["arguments"].clone())?;
        // Recovery has no fresh operation arguments and happens before routing.
        ensure!(request.arguments.unwrap().len() == 1);
        let page = page.recover(owned.path())?.unwrap();
        ensure!(text(&page).len() <= 8192);
        let page: Value = serde_json::from_str(text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(blake3::hash(recovered.as_bytes()).to_hex().as_str() == id);
    let envelope: Value = serde_json::from_str(&recovered)?;
    ensure!(envelope["content"].as_array().unwrap().len() == 1);
    let envelope: Value = serde_json::from_str(envelope["content"][0]["text"].as_str().unwrap())?;
    ensure!(envelope["result"] == serde_json::to_value(original)?);
    ensure!(envelope["response_delivery"]["complete"] == true);
    Ok(())
}

#[test]
fn invalid_budget_precedes_update_and_retention_failure_preserves_returned_outcome() -> Result<()> {
    let error =
        prepare(json!({"project_id":"owned-update","wait":true,"max_tokens":1})).unwrap_err();
    ensure!(error.to_string().contains("Operation was not run"));
    let owned = tempfile::tempdir()?;
    let unavailable_storage = owned.path().join("not-a-data-directory");
    std::fs::write(
        &unavailable_storage,
        "Owned regular file; cannot retain a snapshot here.",
    )?;
    let original = CallToolResult::success(vec![
        Content::text(
            "Update returned with a source warning; inspect this outcome before further work.",
        ),
        Content::text("Complete separate observation. ".repeat(2000)),
    ]);
    let (report, _) = prepare(json!({"project_id":"owned-update","max_tokens":2048}))?;
    let returned = report.finish(&unavailable_storage, original.clone())?;
    ensure!(returned.is_error != Some(true));
    let value = serde_json::to_value(&returned)?;
    let original_value = serde_json::to_value(original)?;
    ensure!(value["content"][0] == original_value["content"][0]);
    ensure!(value["content"][1] == original_value["content"][1]);
    let warning: Value =
        serde_json::from_str(returned.content[2].as_text().unwrap().text.as_str())?;
    ensure!(warning["response_delivery"]["budget_honored"] == false);
    ensure!(warning["response_delivery"]["recovery_available"] == false);
    ensure!(
        warning["response_delivery"]["instruction"]
            .as_str()
            .unwrap()
            .contains("Do not repeat")
    );
    Ok(())
}
