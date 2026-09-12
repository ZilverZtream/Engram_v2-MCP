#![allow(clippy::unwrap_used)]
//! External audit round 2, item 8 (TS→API route resolution, live r43): two
//! extractors can describe the SAME edge — the Roslyn sidecar's
//! `api.action → DeleteChangeRequest` Calls edge and the dispatch-arm scan's
//! twin carrying `dispatch_key = "athDeleteByID"`. They collapse to one graph
//! key (kind, source, target); with last-writer-wins the key is lost and the
//! name route `api.ajax('athDeleteByID')` stays unbound. Ingest must UNION
//! the metadata of same-key edges — every emitter's facts survive.
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::tempdir;

fn sym(name: &str, line: u32) -> engram_index::ExtractedSymbol {
    engram_index::ExtractedSymbol {
        name: name.to_string(),
        kind: "function".to_string(),
        start_line: line,
        end_line: line + 5,
        metadata: None,
    }
}

fn broker_call(meta: &[(&str, &str)]) -> engram_index::ExtractedEdge {
    let mut m = HashMap::new();
    for (k, v) in meta {
        m.insert((*k).to_string(), (*v).to_string());
    }
    engram_index::ExtractedEdge {
        source_name: "api.action".to_string(),
        source_kind: "function".to_string(),
        source_start_line: 62,
        source_language: "vb".to_string(),
        target_name: "DeleteChangeRequest".to_string(),
        target_kind: None,
        target_start_line: None,
        kind: "calls".to_string(),
        metadata: if m.is_empty() { None } else { Some(m) },
    }
}

#[tokio::test]
async fn same_key_edges_from_two_extractors_keep_both_metadata() {
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
            project_name: "EdgeUnion".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let broker = Arc::new(engram_core::RelPath::new("api-json/api-broker.vb"));
    let implementation = Arc::new(engram_core::RelPath::new("ata/api-json/api-atahuvud.vb"));
    let mut stats = engram_index::IngestStats::default();
    stats.symbols.push((broker.clone(), sym("api.action", 62)));
    stats
        .symbols
        .push((implementation.clone(), sym("api.DeleteChangeRequest", 197)));
    // the sidecar's edge (knows the call's arity, not the arm)
    stats
        .edges
        .push((broker.clone(), broker_call(&[("args", "1")])));
    // the dispatch-arm scan's twin (knows the arm, not the arity)
    stats.edges.push((
        broker.clone(),
        broker_call(&[("dispatch_key", "athDeleteByID")]),
    ));
    engram
        .process_ingest_stats_for_test(&project_id, 1, &stats)
        .await
        .unwrap();

    let edges = state
        .graph
        .list_edges_by_kind(&project_id, engram_graph::EdgeKind::Calls, 100)
        .unwrap();
    let e = edges
        .iter()
        .find(|e| e.source_id.contains("api.action"))
        .unwrap_or_else(|| panic!("the broker's calls edge is missing: {edges:?}"));
    let m = e
        .metadata
        .as_ref()
        .and_then(|m| m.as_object())
        .expect("edge metadata");
    assert_eq!(
        m.get("dispatch_key").and_then(|v| v.as_str()),
        Some("athDeleteByID"),
        "the arm's key must survive the sidecar twin: {m:?}"
    );
    assert_eq!(
        m.get("args").and_then(|v| v.as_str()),
        Some("1"),
        "the sidecar's fact must survive the arm twin: {m:?}"
    );
    assert_eq!(
        edges
            .iter()
            .filter(|e| e.source_id.contains("api.action"))
            .count(),
        1,
        "one edge, not two"
    );
}
