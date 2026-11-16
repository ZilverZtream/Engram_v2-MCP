//! Public-API contract for bounded existing WebForms reports; dedicated Cargo target.
use engram_server::{Engram, read_report_delivery as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content, Tool};
use serde_json::{Value, json};

const REPORTS: &[&str] = &[
    "analyze_business_logic",
    "traverse_graph",
    "list_memory_bank",
    "pre_push_audit",
    "project_health",
    "get_ui_conformance",
    "get_gis_inventory",
    "list_settings",
    "get_setting",
    "analyze_temporal_couplings",
    "analyze_error_stack",
    "analyze_file_coding_style",
    "get_extraction_confidence",
    "map_validation_controls",
    "map_auth_config",
    "map_page_lifecycle",
    "analyze_viewstate_deps",
    "map_ajax_regions",
    "trace_data_flow",
    "get_migration_dossier",
    "check_migration_coverage",
    "get_migration_progress",
    "analyze_full_project_migration",
    "analyze_sync_hazards",
    "get_jquery_inventory",
    "get_session_workflows",
    "get_vb_translation_traps",
    "find_tests_for_method",
    "find_dead_methods",
    "trace_ui_event",
    "trace_ui_action",
    "get_ui_blueprint",
];
fn registry() -> Vec<Tool> {
    Engram::tool_registry().list_all()
}
fn request(tool: &str, arguments: Value) -> CallToolRequestParam {
    serde_json::from_value(json!({"name":tool,"arguments":arguments})).unwrap()
}
fn prepared(tool: &str, arguments: Value) -> delivery::Report {
    delivery::prepare(&mut request(tool, arguments), &registry())
        .unwrap()
        .unwrap()
}
fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[test]
fn webforms_reports_advertise_recovery_preserving_original_formats_and_existing_owners() {
    let tools = registry();
    assert_eq!(tools.iter().filter_map(delivery::eligible).count(), 47);
    for name in REPORTS {
        let original = tools
            .iter()
            .find(|tool| tool.name == *name)
            .unwrap()
            .clone();
        assert_eq!(delivery::eligible(&original), Some(*name));
        let format = original
            .input_schema
            .get("properties")
            .and_then(|properties| properties.get("output_json"))
            .cloned();
        let advertised = delivery::advertise(original);
        assert_eq!(advertised.input_schema["required"], json!(["project_id"]));
        assert_eq!(
            advertised
                .input_schema
                .get("properties")
                .and_then(|properties| properties.get("output_json"))
                .cloned(),
            format
        );
        for control in ["max_tokens", "response_id", "response_offset"] {
            assert!(advertised.input_schema["properties"].get(control).is_some());
        }
        let full = tool_surface::advertised(tools.clone(), true);
        let full = full.iter().find(|tool| tool.name == *name).unwrap();
        assert!(full.input_schema["properties"].get("max_tokens").is_some());
    }
    for name in [
        "query_schema",
        "trace_user_request",
        "get_method_info",
        "get_change_set",
        "compute_blast_radius",
        "get_codebase_overview",
        "impact_analysis",
        "describe_table",
        "update_memory_bank",
        "analyze_reverts",
        "update_migration_status",
        "get_rust_diagnostics",
        "get_c_diagnostics",
        "get_cpp_diagnostics",
    ] {
        let original = tools.iter().find(|tool| tool.name == name).unwrap().clone();
        let before = serde_json::to_value(&original).unwrap();
        assert!(delivery::eligible(&original).is_none(), "{name}");
        assert_eq!(
            serde_json::to_value(delivery::advertise(original)).unwrap(),
            before,
            "{name}"
        );
    }
}

#[test]
fn webforms_report_recovery_keeps_all_original_evidence_parts_and_tool_scope() {
    let owned = tempfile::tempdir().unwrap();
    for name in REPORTS {
        let original = CallToolResult::success(vec![
            Content::text(
                json!({"tool":name,"warnings":["Source coverage is incomplete; no runtime proof"],
                "evidence":"Accepted source condition ? and exact scope\n".repeat(900)})
                .to_string(),
            ),
            Content::text("Keep the independent second text part and its provenance."),
        ]);
        let expected = serde_json::to_string(&original).unwrap();
        let report = prepared(name, json!({"project_id":"owned","max_tokens":2048}));
        let first = report.finish(owned.path(), original).unwrap();
        assert!(body(&first).len() <= 8192);
        let manifest: Value = serde_json::from_str(body(&first)).unwrap();
        assert_eq!(manifest["report_content"], "DEFERRED_NOT_SUPPLIED");
        let mut next = manifest["response_delivery"]["next_call"].clone();
        let mut content = String::new();
        loop {
            assert_eq!(next["tool"], *name);
            let report = prepared(name, next["arguments"].clone());
            let result = report.recover(owned.path()).unwrap().unwrap();
            assert!(body(&result).len() <= 8192);
            let page: Value = serde_json::from_str(body(&result)).unwrap();
            assert_eq!(page["start_byte"], content.len());
            content.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            next = page["next_call"].clone();
        }
        assert_eq!(content, expected, "{name}");
        assert_eq!(
            blake3::hash(content.as_bytes()).to_hex().as_str(),
            manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
        );
        let id = manifest["response_delivery"]["response_id"]
            .as_str()
            .unwrap();
        let other = prepared(
            name,
            json!({"project_id":"foreign","response_id":id,"max_tokens":2048}),
        );
        assert!(other.recover(owned.path()).is_err());
        let other_tool = if *name == REPORTS[0] {
            REPORTS[1]
        } else {
            REPORTS[0]
        };
        let other = prepared(
            other_tool,
            json!({"project_id":"owned","response_id":id,"max_tokens":2048}),
        );
        assert!(other.recover(owned.path()).is_err());
    }
}

#[test]
fn webforms_small_reports_keep_their_format_and_original_analysis_arguments() {
    let owned = tempfile::tempdir().unwrap();
    for name in [
        "map_page_lifecycle",
        "map_validation_controls",
        "get_migration_dossier",
    ] {
        let mut req = request(
            name,
            json!({"project_id":"owned","file_path":"Access.aspx", "output_json":true, "max_tokens":2048}),
        );
        let report = delivery::prepare(&mut req, &registry()).unwrap().unwrap();
        assert_eq!(
            req.arguments.unwrap(),
            serde_json::from_value::<serde_json::Map<String, Value>>(
                json!({"project_id":"owned","file_path":"Access.aspx","output_json":true})
            )
            .unwrap()
        );
        let original = CallToolResult::success(vec![Content::text(
            "{\"coverage\":\"partial\",\"validators\":[]}",
        )]);
        let before = serde_json::to_value(&original).unwrap();
        assert_eq!(
            serde_json::to_value(report.finish(owned.path(), original).unwrap()).unwrap(),
            before
        );
    }
    assert_eq!(std::fs::read_dir(owned.path()).unwrap().count(), 0);
}

#[test]
fn webforms_invalid_budget_refuses_before_source_analysis() {
    let mut req = request(
        "map_validation_controls",
        json!({"project_id":"missing-project","file_path":"missing.aspx","max_tokens":1}),
    );
    let error = delivery::prepare(&mut req, &registry()).unwrap_err();
    assert!(error.message.contains("Analysis was not run"));
}
