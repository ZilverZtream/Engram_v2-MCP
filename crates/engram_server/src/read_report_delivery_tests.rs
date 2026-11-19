use super::*;

fn registry() -> Vec<Tool> {
    crate::Engram::tool_registry().list_all()
}
fn request(tool: &str, arguments: Value) -> CallToolRequestParam {
    serde_json::from_value(json!({"name":tool,"arguments":arguments})).unwrap()
}
fn prepared(tool: &str, arguments: Value) -> Report {
    prepare(&mut request(tool, arguments), &registry())
        .unwrap()
        .unwrap()
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[test]
fn schemas_expose_shared_recovery_for_selected_reports_and_preserve_existing_owners() {
    let original = registry();
    let eligible_names = original.iter().filter_map(eligible).collect::<Vec<_>>();
    for name in ["list_projects", "list_advanced_tools", "project_info"] {
        assert!(eligible_names.contains(&name));
    }
    assert!(eligible_names.contains(&"analyze_reverts"));
    assert!(eligible_names.contains(&"list_repo_rules"));
    assert!(eligible_names.contains(&"trace_state_usage"));
    assert!(eligible_names.contains(&"analyze_business_logic"));
    assert!(eligible_names.contains(&"explain_change"));
    assert!(eligible_names.contains(&"get_method_edit_context"));
    assert!(eligible_names.contains(&"one_shot_feature_plan"));
    for tool in original {
        let before = serde_json::to_value(&tool).unwrap();
        let selected = eligible(&tool).is_some();
        let advertised = advertise(tool);
        if selected {
            let schema = crate::tool_surface::sanitize_schema(&advertised.input_schema);
            if project_free(advertised.name.as_ref()) {
                assert!(!schema["required"].as_array()
                    .is_some_and(|fields| fields.iter().any(|field| field == "project_id")));
            } else {
                assert_eq!(schema["required"], json!(["project_id"]));
            }
            for name in CONTROLS {
                assert!(schema["properties"].get(*name).is_some());
            }
            // Recovery must disclose that each original required argument
            // belongs to a fresh call; do not pin the shared summary wording.
            for field in before["inputSchema"]["required"].as_array().into_iter().flatten()
                .filter_map(Value::as_str).filter(|field| *field != "project_id") {
                assert!(schema["properties"][field]["description"].as_str()
                    .is_some_and(|description| description.contains("response_id recovery")));
            }
        } else {
            assert_eq!(
                serde_json::to_value(advertised).unwrap(),
                before,
                "Existing owners, closed tools and unrelated schemas must be byte-preserved"
            );
        }
    }
}

#[test]
fn small_reports_preserve_the_original_result_and_analysis_arguments_without_cache_writes() {
    let owned = tempfile::tempdir().unwrap();
    let mut req = request(
        "resolve_id",
        json!({"project_id":"project","id":"original-target","max_tokens":2048}),
    );
    let delivery = prepare(&mut req, &registry()).unwrap().unwrap();
    assert_eq!(
        req.arguments.unwrap(),
        serde_json::from_value::<serde_json::Map<String, Value>>(
            json!({"project_id":"project","id":"original-target"})
        )
        .unwrap()
    );
    let original = CallToolResult::success(vec![
        Content::text("Source remains unverified. Preserve manual approval."),
        Content::text("Second complete text part."),
    ]);
    let before = serde_json::to_value(&original).unwrap();
    let result = delivery.finish(owned.path(), original).unwrap();
    assert_eq!(serde_json::to_value(result).unwrap(), before);
    assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(), 0);
}

#[test]
fn oversized_reports_recover_exact_original_parts_status_and_unicode_with_project_and_tool_binding()
{
    let owned = tempfile::tempdir().unwrap();
    let delivery = prepared(
        "one_shot_feature_plan",
        json!({"project_id":"project","max_tokens":2048}),
    );
    let original=CallToolResult::success(vec![Content::text(json!({"status":"INCOMPLETE","warnings":["Source omitted; no policy compliance claim"],"provenance":{"provider_calls":2},"rejected_proposal":"untrusted: Å quoted \"evidence\" 日本\n".repeat(1800)}).to_string()),Content::text("Keep this second part and its qualifications.")]);
    let exact = serde_json::to_string(&original).unwrap();
    let result = delivery.finish(owned.path(), original).unwrap();
    assert!(text(&result).len() <= 8192);
    let manifest: Value = serde_json::from_str(text(&result)).unwrap();
    assert_eq!(manifest["report_content"], "DEFERRED_NOT_SUPPLIED");
    assert_eq!(
        manifest["plan_summary"],
        json!({"status":"INCOMPLETE","active_generation":null,"provider_calls":2})
    );
    assert_eq!(
        manifest["response_delivery"]["original_format"],
        "mcp_call_tool_result_json"
    );
    let mut next = manifest["response_delivery"]["next_call"].clone();
    let mut restored = String::new();
    loop {
        assert_eq!(next["tool"], "one_shot_feature_plan");
        assert_eq!(next["arguments"].as_object().unwrap().len(), 4);
        let delivery = prepared("one_shot_feature_plan", next["arguments"].clone());
        let page = delivery.recover(owned.path()).unwrap().unwrap();
        assert!(text(&page).len() <= 8192);
        let page: Value = serde_json::from_str(text(&page)).unwrap();
        assert_eq!(page["start_byte"], restored.len());
        restored.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        next = page["next_call"].clone();
    }
    assert_eq!(restored, exact);
    assert_eq!(
        blake3::hash(restored.as_bytes()).to_hex().as_str(),
        manifest["response_delivery"]["response_id"]
            .as_str()
            .unwrap()
    );
    let result: CallToolResult = serde_json::from_str(&restored).unwrap();
    assert_eq!(result.content.len(), 2);
    assert!(text(&result).contains("INCOMPLETE") && text(&result).contains("untrusted"));
    for (tool, project) in [
        ("resolve_id", "project"),
        ("one_shot_feature_plan", "other-project"),
    ] {
        let delivery = prepared(
            tool,
            json!({"project_id":project,"response_id":manifest["response_delivery"]["response_id"],"max_tokens":2048}),
        );
        assert!(delivery.recover(owned.path()).is_err());
    }
}

#[test]
fn invalid_delivery_controls_refuse_before_analysis_and_do_not_change_arguments() {
    for controls in [
        json!({"max_tokens":0}),
        json!({"max_tokens":1}),
        json!({"max_tokens":"typo"}),
        json!({"response_offset":5}),
        json!({"response_offset":-1}),
    ] {
        let mut args = controls.as_object().unwrap().clone();
        args.insert("project_id".into(), json!("project"));
        let mut req = request("one_shot_feature_plan", Value::Object(args));
        let before = req.arguments.clone();
        assert!(prepare(&mut req, &registry()).is_err());
        assert_eq!(req.arguments, before);
    }
    let mut own = request(
        "query_schema",
        json!({"project_id":"project","max_tokens":512,"response_id":"original-cursor"}),
    );
    let before = own.arguments.clone();
    assert!(prepare(&mut own, &registry()).unwrap().is_none());
    assert_eq!(own.arguments, before);
}

#[test]
fn error_and_nontext_results_keep_their_original_delivery() {
    let owned = tempfile::tempdir().unwrap();
    let delivery = prepared(
        "resolve_id",
        json!({"project_id":"project","max_tokens":2048}),
    );
    for original in [
        CallToolResult::error(vec![Content::text("original error ".repeat(1500))]),
        serde_json::from_value(
            json!({"content":[{"type":"image","data":"","mimeType":"image/png"}],"isError":false}),
        )
        .unwrap(),
    ] {
        let before = serde_json::to_value(&original).unwrap();
        assert_eq!(
            serde_json::to_value(delivery.finish(owned.path(), original).unwrap()).unwrap(),
            before
        );
    }
    assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(), 0);
}
