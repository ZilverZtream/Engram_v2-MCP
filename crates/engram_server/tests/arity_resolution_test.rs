//! TODO-13: arity-aware call resolution through the real ingest path.
//!
//! Two same-name functions with different arities in different files; a call
//! edge carrying `args` metadata must bind to the arity-matching overload
//! and stamp `resolution: batch_arity_match`.

#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::collections::HashMap;
use tempfile::tempdir;

fn sym(name: &str, line: u32, arity: u32) -> engram_index::ExtractedSymbol {
    let mut m = HashMap::new();
    m.insert("arity".to_string(), arity.to_string());
    engram_index::ExtractedSymbol {
        name: name.to_string(),
        kind: "function".to_string(),
        start_line: line,
        end_line: line + 5,
        metadata: Some(m),
    }
}

#[tokio::test]
async fn call_with_args_binds_to_matching_overload() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.vb"), "' a").unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "ArityTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let projects = state.registry.list_projects().unwrap();
    let project_id = projects[0].project_id.clone();

    let mut stats = engram_index::IngestStats::default();
    let save1 = engram_core::RelPath::new("io/save1.vb");
    let save2 = engram_core::RelPath::new("io/save2.vb");
    let caller_path = engram_core::RelPath::new("pages/edit.vb");
    stats
        .symbols
        .push((std::sync::Arc::new(save1.clone()), sym("Save", 10, 1)));
    stats
        .symbols
        .push((std::sync::Arc::new(save2.clone()), sym("Save", 10, 3)));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        sym("DoEdit", 5, 0),
    ));

    let mut call_meta = HashMap::new();
    call_meta.insert("args".to_string(), "3".to_string());
    stats.edges.push((
        std::sync::Arc::new(caller_path.clone()),
        engram_index::ExtractedEdge {
            source_name: "DoEdit".to_string(),
            source_kind: "function".to_string(),
            source_start_line: 6,
            source_language: "vb".to_string(),
            target_name: "Save".to_string(),
            target_kind: Some("function".to_string()),
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: Some(call_meta),
        },
    ));

    engram
        .process_ingest_stats_for_test(&project_id, 1, &stats)
        .await
        .unwrap();

    let edges = state
        .graph
        .list_edges(&project_id, Some(engram_graph::EdgeKind::Calls))
        .unwrap();
    let call = edges
        .iter()
        .find(|e| e.source_id.contains("DoEdit"))
        .expect("call edge exists");

    assert!(
        call.target_id.contains("save2.vb"),
        "must bind to the 3-arg overload in save2.vb, got {}",
        call.target_id
    );
    let meta = call.metadata.as_ref().expect("metadata");
    assert_eq!(
        meta.get("resolution").and_then(|v| v.as_str()),
        Some("batch_arity_match"),
        "resolution method must record the arity match"
    );
}
