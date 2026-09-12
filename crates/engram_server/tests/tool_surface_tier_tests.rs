#![allow(clippy::unwrap_used)]
//! Full discovery by default, with an explicit limited core opt-in.

use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tool_surface::{CORE_TOOLS, advertised};
use engram_server::tools::Engram;
use std::collections::BTreeSet;

fn engram() -> (tempfile::TempDir, Engram) {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("proj")).unwrap();
    let cfg = Config {
        allowed_roots: vec![tmp.path().join("proj")],
        data_dir: tmp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    (tmp, Engram::new(state))
}

#[test]
fn every_core_tool_exists_and_covers_the_ten_capabilities() {
    let (_tmp, engram) = engram();
    let all: BTreeSet<String> = engram
        .tool_router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let missing: Vec<&str> = CORE_TOOLS
        .iter()
        .copied()
        .filter(|t| !all.contains(*t))
        .collect();
    assert!(
        missing.is_empty(),
        "core tools that do not exist: {missing:?}"
    );
    for must in [
        "get_change_set",
        "get_method_edit_context",
        "get_page_context",
        "pre_commit_review",
        "pre_push_audit",
        "get_concept_footprint",
        "find_symbol_references",
        "find_implementation_pattern",
        "analyze_file_coding_style",
        "ask_codebase",
        "trace_ui_event",
        "trace_data_flow",
        "find_connection_path",
        "map_guards_and_settings",
        "immune_check",
        "detect_incomplete_changes",
        "find_similar_changes",
        "impact_analysis",
        "compute_blast_radius",
        "check_edit_safety",
        "list_advanced_tools",
    ] {
        assert!(CORE_TOOLS.contains(&must), "{must} must be a core tool");
    }
    assert!(
        CORE_TOOLS.len() <= 32,
        "the core tier stays small: {}",
        CORE_TOOLS.len()
    );
}

#[test]
fn full_and_core_surfaces_cover_their_registered_tools() {
    let (_tmp, engram) = engram();
    let all = engram.tool_router.list_all();
    assert!(
        all.len() > 100,
        "the router still holds the whole surface ({})",
        all.len()
    );

    let core = advertised(all.clone(), false);
    let core_names: BTreeSet<String> = core.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(
        core_names,
        CORE_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
        "advertise_all_tools=false advertises exactly the core tier"
    );

    let full = advertised(all.clone(), true);
    assert_eq!(
        full.len(),
        all.len(),
        "advertise_all_tools=true advertises everything"
    );
}

#[test]
fn advertise_all_tools_defaults_to_full_for_rust_and_yaml() {
    let cfg = Config::default();
    assert!(cfg.advertise_all_tools);
    let mut value = serde_yaml::to_value(&cfg).unwrap();
    value
        .as_mapping_mut()
        .unwrap()
        .remove(serde_yaml::Value::String("advertise_all_tools".into()));
    let yaml: Config = serde_yaml::from_value(value.clone()).unwrap();
    assert!(yaml.advertise_all_tools);
    value["advertise_all_tools"] = serde_yaml::Value::Bool(false);
    let core: Config = serde_yaml::from_value(value).unwrap();
    assert!(!core.advertise_all_tools);
}

#[tokio::test]
async fn protocol_discovery_includes_ociusx_agent_dependencies_with_schemas() {
    use rmcp::ServiceExt;
    let (_tmp, engram) = engram();
    let expected: BTreeSet<String> = engram
        .tool_router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
    let server = tokio::spawn(async move { engram.serve(server_io).await.unwrap() });
    let client = ().serve(client_io).await.unwrap();
    let server = server.await.unwrap();
    let listed = client.list_all_tools().await.unwrap();
    let names: BTreeSet<String> = listed.iter().map(|t| t.name.to_string()).collect();
    assert_eq!(
        names, expected,
        "tools/list must not silently remove .NET tools"
    );
    for name in [
        "get_full_method_body",
        "get_chunk",
        "get_table_schema",
        "describe_setting",
        "find_merged_work",
        "query_business_logic",
        "read_memory_bank",
        "validate_generated_code",
        "validate_sql_fragment",
        "derive_test_matrix",
        "find_tests_for_method",
        "map_validation_controls",
        "trace_state_usage",
        "analyze_business_logic",
        "list_memory_bank",
        "list_repo_rules",
        "get_setting",
        "list_settings",
        "find_references",
        "analyze_temporal_couplings",
        "list_projects",
    ] {
        let tool = listed
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(
            tool.input_schema.get("type"),
            Some(&serde_json::json!("object")),
            "{name}"
        );
        let schema = serde_json::to_string(&tool.input_schema).unwrap();
        assert!(
            !schema.contains("\"$ref\""),
            "unresolved reference in {name}"
        );
    }
    client.cancel().await.unwrap();
    server.cancel().await.unwrap();
}
