//! Public delivery contracts for immutable selected query windows.
use engram_server::{Engram, retained_results as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content, Tool};
use serde_json::{Value, json};

// Keep the existing shared-report behavior covered by the same focused target.
#[path = "webforms_report_delivery_tests.rs"]
mod existing_reports;

const TOOLS: &[&str] = &["search_memory", "search_history", "grep_project"];
fn registry() -> Vec<Tool> {
    Engram::tool_registry().list_all()
}
fn request(tool: &str, args: Value) -> CallToolRequestParam {
    serde_json::from_value(json!({"name":tool,"arguments":args})).unwrap()
}
fn prepared(tool: &str, args: Value) -> engram_server::read_report_delivery::Report {
    delivery::prepare(&mut request(tool, args), &registry())
        .unwrap()
        .unwrap()
}
fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[test]
fn ordinary_search_requests_preserve_existing_budgets_filters_and_offsets() {
    for tool in TOOLS {
        let args = json!({"project_id":"owned","query":"accepted","offset":7,
            "max_tokens":2048,"output_json":true,"include_path_prefixes":["client/"]});
        let mut call = request(tool, args.clone());
        let before = serde_json::to_value(&call).unwrap();
        assert!(delivery::prepare(&mut call, &registry()).unwrap().is_none());
        assert_eq!(serde_json::to_value(call).unwrap(), before);
        let mut explicit = args.clone();
        explicit["retain_response"] = json!(false);
        explicit["response_offset"] = json!(0);
        let mut call = request(tool, explicit);
        assert!(delivery::prepare(&mut call, &registry()).unwrap().is_none());
        assert_eq!(serde_json::to_value(call.arguments.unwrap()).unwrap(), args);
    }
}

#[test]
fn retained_windows_recover_every_selected_identity_and_original_qualification() {
    let owned = tempfile::tempdir().unwrap();
    for tool in TOOLS {
        let rows: Vec<_> = (0..16).map(|id|json!({"id":id,"rank":id+1,
            "content":format!("accepted source {id}: {}", "quote=\" backslash=\\\n\u{00e5}\u{1f980} ".repeat(500))})).collect();
        let original = CallToolResult::success(vec![
            Content::text(json!({"results":rows,"warnings":["Only the selected window; no exhaustive corpus claim"],
                "guidance":"Preserve the authored exception; capture-time guidance is historical."}).to_string()),
            Content::text("Original independent evidence qualification."),
        ]);
        let expected = serde_json::to_string(&original).unwrap();
        let mut call = request(
            tool,
            json!({"project_id":"owned","query":"accepted",
            "output_json":true,"max_tokens":2048,"retain_response":true,"offset":3,"limit":16}),
        );
        let report = delivery::prepare(&mut call, &registry()).unwrap().unwrap();
        let remaining = call.arguments.unwrap();
        assert!(!remaining.contains_key("max_tokens"));
        assert!(!remaining.contains_key("retain_response"));
        assert_eq!(remaining["query"], "accepted");
        assert_eq!(remaining["offset"], 3);
        assert_eq!(remaining["limit"], 16);
        let first = report.finish(owned.path(), original).unwrap();
        assert!(body(&first).len() <= 8192);
        let manifest: Value = serde_json::from_str(body(&first)).unwrap();
        assert!(
            manifest["response_delivery"]["capture_scope"]
                .as_str()
                .unwrap()
                .contains("not underlying source objects")
        );
        let mut next = manifest["response_delivery"]["next_call"].clone();
        let mut all = String::new();
        loop {
            let fresh = prepared(tool, next["arguments"].clone());
            let recovered = fresh.recover(owned.path()).unwrap().unwrap();
            assert!(body(&recovered).len() <= 8192);
            let page: Value = serde_json::from_str(body(&recovered)).unwrap();
            assert_eq!(page["start_byte"], all.len());
            all.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            next = page["next_call"].clone();
        }
        assert_eq!(all, expected);
        assert_eq!(
            blake3::hash(all.as_bytes()).to_hex().as_str(),
            manifest["response_delivery"]["response_id"]
        );
        let original: CallToolResult = serde_json::from_str(&all).unwrap();
        let data: Value = serde_json::from_str(body(&original)).unwrap();
        assert_eq!(data["results"].as_array().unwrap().len(), 16);
        for (id, row) in data["results"].as_array().unwrap().iter().enumerate() {
            assert_eq!(row["id"], id);
            assert_eq!(row["rank"], id + 1);
        }
    }
}

#[test]
fn even_small_windows_retain_an_identity_and_reject_other_project_or_tool() {
    let owned = tempfile::tempdir().unwrap();
    let original = CallToolResult::success(vec![Content::text(
        "{\"results\":[],\"warnings\":[\"No absence proof\"]}",
    )]);
    let report = prepared(
        "search_memory",
        json!({"project_id":"owned","query":"accepted","retain_response":true,"output_json":true}),
    );
    let first = report.finish(owned.path(), original.clone()).unwrap();
    let manifest: Value = serde_json::from_str(body(&first)).unwrap();
    let id = manifest["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    assert_eq!(
        id,
        blake3::hash(serde_json::to_string(&original).unwrap().as_bytes())
            .to_hex()
            .as_str()
    );
    for (tool, project) in [
        ("search_history", "owned"),
        ("grep_project", "owned"),
        ("search_memory", "foreign"),
    ] {
        assert!(
            prepared(tool, json!({"project_id":project,"response_id":id}))
                .recover(owned.path())
                .is_err()
        );
    }
}

#[test]
fn retained_controls_fail_before_capture_and_original_errors_are_preserved() {
    for tool in TOOLS {
        for args in [
            json!({"project_id":"owned","retain_response":true}),
            json!({"project_id":"owned","retain_response":true,"output_json":true,"max_tokens":1}),
            json!({"project_id":"owned","response_offset":1}),
            json!({"project_id":"owned","retain_response":"yes"}),
        ] {
            assert!(delivery::prepare(&mut request(tool, args), &registry()).is_err());
        }
    }
    let owned = tempfile::tempdir().unwrap();
    let report = prepared(
        "search_memory",
        json!({"project_id":"owned","retain_response":true,"output_json":true}),
    );
    let original = CallToolResult::error(vec![Content::text(
        "Original source unavailable; no results supplied.",
    )]);
    assert_eq!(
        serde_json::to_value(report.finish(owned.path(), original.clone()).unwrap()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(), 0);
}

#[test]
fn retention_schema_keeps_original_properties_and_core_full_contracts_equal() {
    let tools = registry();
    let full = tool_surface::advertised(tools.clone(), true);
    let core = tool_surface::advertised(tools.clone(), false);
    for name in TOOLS {
        let original = tools.iter().find(|tool| tool.name == *name).unwrap();
        let advertised = full.iter().find(|tool| tool.name == *name).unwrap();
        assert_eq!(advertised.input_schema["required"], json!(["project_id"]));
        for (key, value) in original.input_schema["properties"].as_object().unwrap() {
            assert_eq!(
                &advertised.input_schema["properties"][key], value,
                "{name} {key}"
            );
        }
        for control in ["retain_response", "response_id", "response_offset"] {
            assert!(advertised.input_schema["properties"].get(control).is_some());
        }
        if let Some(tool) = core.iter().find(|tool| tool.name == *name) {
            assert_eq!(tool.input_schema, advertised.input_schema);
        }
    }
}
