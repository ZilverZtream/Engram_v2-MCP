#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29, auditor P0 #6 / row 0 (owner decision 09:32):
//! the 143-tool surface must be TIERED — the vital-capability tools are
//! advertised by default, everything else stays callable but is listed only
//! through `list_advanced_tools` (or `advertise_all_tools = true`).

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
fn the_default_surface_is_the_core_tier_and_the_full_surface_is_opt_in() {
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
fn advertise_all_tools_defaults_to_the_tiered_surface() {
    let cfg = Config::default();
    assert!(
        !cfg.advertise_all_tools,
        "the tiered surface is the default (auditor P0 #6); the full list is opt-in"
    );
}
