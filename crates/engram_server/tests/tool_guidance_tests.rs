use engram_server::{
    Engram,
    tool_guidance::{self, Policy},
};
use rmcp::model::{CallToolResult, Content};
use serde_json::{Value, json};

fn run(tool: &str, arguments: Value, payload: Value, full: bool) -> (String, Value) {
    let mut result = CallToolResult::success(vec![Content::text(payload.to_string())]);
    let status = tool_guidance::annotate(
        tool,
        &tool_guidance::context(arguments.as_object()),
        &mut result,
        &Engram::tool_registry().list_all(),
        full,
    );
    (
        status.into(),
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap(),
    )
}

#[test]
fn tool_guidance_covers_registered_surface_without_touching_core_or_closed_tools() {
    let registry = Engram::tool_registry().list_all();
    assert!(registry.len() > 100);
    for tool in &registry {
        let policy = tool_guidance::policy(&tool.name);
        assert!(
            !matches!(policy, Policy::Unknown),
            "Unaccounted registered tool: {}",
            tool.name
        );
        if let Policy::Suggest(choices) = policy {
            for (name, _) in choices {
                assert!(
                    registry.iter().any(|tool| tool.name == *name),
                    "Unknown next tool: {name}"
                );
            }
        }
    }
    for name in engram_server::tool_surface::CORE_TOOLS
        .iter()
        .copied()
        .chain([
            "compute_blast_radius",
            "get_codebase_overview",
            "one_shot_feature_plan",
        ])
    {
        let original = CallToolResult::success(vec![Content::text("exact original bytes\n")]);
        let mut result = original.clone();
        assert_eq!(
            tool_guidance::annotate(name, &Default::default(), &mut result, &registry, true),
            "preserved"
        );
        assert_eq!(result, original);
    }
}

#[test]
fn tool_guidance_follows_the_actual_repeated_method_without_inventing_identifiers() {
    let payload = json!({"status":"static_candidates", "findings":[{
        "repeated_call":{"file":"Site/Orders.aspx.vb","method":"BindOrders","start_line":42},
        "database_call":{"site":{"file":"Site/Store.vb","method":"ReadOrder"}}
    }], "coverage":{}});
    let (status, result) = run(
        "detect_n_plus_one",
        json!({"project_id":"project", "file_path":"Site/Orders.aspx.vb"}),
        payload.clone(),
        true,
    );
    assert_eq!(status, "added");
    assert_eq!(result["findings"], payload["findings"]);
    let hint = result["next_tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["tool"] == "get_method_edit_context")
        .unwrap();
    assert_eq!(
        hint["arguments"],
        json!({"project_id":"project","file_path":"Site/Orders.aspx.vb","method_name":"BindOrders"})
    );
    // A real public request model must accept the supplied follow-up arguments.
    serde_json::from_value::<engram_server::GetMethodEditContextRequest>(hint["arguments"].clone())
        .unwrap();
    let no_method =
        json!({"findings":[{"repeated_call":{"file":"Site/Orders.aspx.vb","start_line":42}}]});
    let (_, missing) = run(
        "detect_n_plus_one",
        json!({"project_id":"project"}),
        no_method,
        true,
    );
    assert_eq!(missing["next_tools"][0]["tool"], "get_full_method_body");
    assert_eq!(
        missing["next_tools"][0]["arguments"],
        json!({"project_id":"project","file_path":"Site/Orders.aspx.vb","line_start":42})
    );
    serde_json::from_value::<engram_server::GetFullMethodBodyRequest>(
        missing["next_tools"][0]["arguments"].clone(),
    )
    .unwrap();
}

#[test]
fn tool_guidance_schema_source_followup_preserves_exact_source_coordinates_and_profile() {
    let payload = json!({"status":"found","code_reference_leads":{"results":[{
        "file":"Site/Orders.vb","start_line":21,"end_line":29,"source_hash":"fingerprint"
    }]}});
    let (_, result) = run(
        "query_schema",
        json!({"project_id":"project"}),
        payload.clone(),
        true,
    );
    assert_eq!(
        result["code_reference_leads"],
        payload["code_reference_leads"]
    );
    let hint = &result["next_tools"][0];
    assert_eq!(hint["tool"], "get_full_method_body");
    assert_eq!(
        hint["arguments"],
        json!({"project_id":"project","file_path":"Site/Orders.vb","line_start":21,"line_end":29})
    );
    serde_json::from_value::<engram_server::GetFullMethodBodyRequest>(hint["arguments"].clone())
        .unwrap();
    assert!(
        result["next_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["tool"] == "find_missing_indexes")
    );
    let (_, core) = run(
        "query_schema",
        json!({"project_id":"project"}),
        payload,
        false,
    );
    assert!(core["next_tools"].as_array().unwrap().iter().all(|h|
        engram_server::tool_surface::CORE_TOOLS.contains(&h["tool"].as_str().unwrap())));
    let (_, no_leads) = run(
        "query_schema",
        json!({"project_id":"project"}),
        json!({"status":"found"}),
        true,
    );
    assert!(
        no_leads["next_tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["tool"] != "get_full_method_body")
    );
}

#[test]
fn tool_guidance_empty_unavailable_and_source_gaps_do_not_suggest_editing_findings() {
    for (payload, expected) in [
        (
            json!({"status":"static_candidates","findings":[]}),
            "grep_project",
        ),
        (
            json!({"status":"NOT_APPLICABLE","findings":[]}),
            "project_info",
        ),
        (
            json!({"status":"static_candidates","findings":[],"selected_vb_files":0}),
            "project_info",
        ),
        (
            json!({"status":"static_candidates","findings":[],"coverage":{"unavailable_files":["Changed.vb"]}}),
            "get_index_freshness",
        ),
    ] {
        let (_, result) = run(
            "detect_n_plus_one",
            json!({"project_id":"project"}),
            payload.clone(),
            true,
        );
        assert_eq!(result["next_tools"][0]["tool"], expected);
        assert_eq!(result["status"], payload["status"]);
        assert!(
            result["next_tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h["tool"] != "check_edit_safety")
        );
    }
}

#[test]
fn tool_guidance_preserves_schema_ambiguity_and_does_not_misread_no_error_status() {
    let (_, result) = run(
        "query_schema",
        json!({"project_id":"p"}),
        json!({"status":"ambiguous","results":[{"name":"a.Items"},{"name":"b.Items"}]}),
        true,
    );
    assert_eq!(result["next_tools"][0]["tool"], "query_schema");
    assert!(result["next_tools"][0].get("arguments").is_none());
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    let (_, result) = run(
        "analyze_error_stack",
        json!({"project_id":"p"}),
        json!({"status":"no_errors_found"}),
        true,
    );
    assert_ne!(result["next_tools_status"], "analysis_unavailable");
}

#[test]
fn tool_guidance_discloses_budget_reduction_when_only_the_status_fits() {
    let payload = json!({"findings":[],"evidence":"kept"});
    let expected = json!({"findings":[],"evidence":"kept","next_tools":[],
        "next_tools_status":"suggestions_reduced_to_preserve_response_budget"});
    let tokens = expected.to_string().len().div_ceil(4);
    let (_, result) = run(
        "detect_n_plus_one",
        json!({"project_id":"p","max_tokens":tokens}),
        payload,
        true,
    );
    assert_eq!(result, expected);
    assert!(result.to_string().len() <= tokens * 4);
}

#[test]
fn tool_guidance_never_sacrifices_a_complete_result_or_recovery_to_fit_hints() {
    let registry = Engram::tool_registry().list_all();
    let payload = json!({"findings":[],"next_page":{"tool":"detect_n_plus_one","arguments":{"project_id":"p","file_offset":8}},"evidence":"x".repeat(500)}).to_string();
    let original = CallToolResult::success(vec![Content::text(payload.clone())]);
    let mut result = original.clone();
    let input = json!({"project_id":"p","max_tokens":payload.len().div_ceil(4)});
    let status = tool_guidance::annotate(
        "detect_n_plus_one",
        &tool_guidance::context(input.as_object()),
        &mut result,
        &registry,
        true,
    );
    assert_eq!(status, "budget_preserved");
    assert_eq!(result, original);
    let (_, roomy) = run(
        "detect_n_plus_one",
        json!({"project_id":"p","max_tokens":1024}),
        serde_json::from_str(&payload).unwrap(),
        true,
    );
    assert!(roomy.to_string().len() <= 4096);
    assert_eq!(roomy["evidence"], "x".repeat(500));
    assert_eq!(roomy["next_page"]["arguments"]["file_offset"], 8);
}

#[test]
fn tool_guidance_respects_implicit_budgets_errors_and_existing_guidance() {
    let registry = Engram::tool_registry().list_all();
    let payload = format!("{{\"value\":\"{}\"}}", "x".repeat(65536 - 12));
    assert_eq!(payload.len(), 65536);
    let original = CallToolResult::success(vec![Content::text(payload)]);
    let mut result = original.clone();
    assert_eq!(
        tool_guidance::annotate(
            "query_schema",
            &Default::default(),
            &mut result,
            &registry,
            true
        ),
        "budget_preserved"
    );
    assert_eq!(result, original);
    for original in [
        CallToolResult::error(vec![Content::text(
            "Use the existing precise recovery request",
        )]),
        CallToolResult::success(vec![Content::text(
            "{\"next_tools\":[\"get_chunk\"],\"data\":7}",
        )]),
        CallToolResult::success(vec![Content::text("[1,2,3]")]),
    ] {
        let mut result = original.clone();
        tool_guidance::annotate(
            "query_schema",
            &Default::default(),
            &mut result,
            &registry,
            true,
        );
        assert_eq!(result, original);
    }
}

#[test]
fn tool_guidance_does_not_copy_input_blobs_and_preserves_markdown() {
    let input = json!({"project_id":"p","sql":"private SQL input","api_key":"private key","file_path":"Example.vb"});
    let context = tool_guidance::context(input.as_object());
    assert!(!context.contains_key("sql") && !context.contains_key("api_key"));
    let registry = Engram::tool_registry().list_all();
    let original = "# Source analysis\n\nA concrete finding.\n";
    let mut result = CallToolResult::success(vec![Content::text(original)]);
    assert_eq!(
        tool_guidance::annotate(
            "analyze_error_stack",
            &context,
            &mut result,
            &registry,
            true
        ),
        "added"
    );
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.starts_with(original) && text.contains("next_tools:"));
    assert!(!text.contains("private"));
}
