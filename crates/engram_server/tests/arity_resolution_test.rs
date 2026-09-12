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

#[tokio::test]
async fn signature_shaped_roslyn_target_resolves_cross_file() {
    // Regression: the VB Roslyn sidecar emitted call targets WITH the
    // parameter type list ("_x.Cls.Save(Integer, String)") while method
    // definitions are bare ("_x.Cls.Save") — every cross-file call stayed a
    // :: placeholder. The batch resolver must strip the signature before
    // deriving lookup keys.
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
            project_name: "SigStripTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    let mut stats = engram_index::IngestStats::default();
    let callee_path = engram_core::RelPath::new("io/cls.vb");
    let caller_path = engram_core::RelPath::new("pages/edit.vb");
    stats.symbols.push((
        std::sync::Arc::new(callee_path.clone()),
        sym("_x.Cls.Save", 10, 2),
    ));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        sym("DoEdit", 5, 0),
    ));

    stats.edges.push((
        std::sync::Arc::new(caller_path.clone()),
        engram_index::ExtractedEdge {
            source_name: "DoEdit".to_string(),
            source_kind: "function".to_string(),
            source_start_line: 6,
            source_language: "vb".to_string(),
            target_name: "_x.Cls.Save(Integer, String)".to_string(),
            target_kind: Some("function".to_string()),
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: None,
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
        !call.target_id.starts_with("::"),
        "signature-shaped target must not stay a placeholder, got {}",
        call.target_id
    );
    assert!(
        call.target_id.contains("cls.vb"),
        "must bind to the bare-named definition in cls.vb, got {}",
        call.target_id
    );
}

#[tokio::test]
async fn same_language_candidate_beats_cross_language_tie() {
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
            project_name: "LangTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // Same short name in a JS file and a VB file; the VB caller's bare-name
    // call must bind to the VB candidate (TODO-19), not tie-break randomly.
    let mut stats = engram_index::IngestStats::default();
    let js_path = engram_core::RelPath::new("scripts/util.js");
    let vb_path = engram_core::RelPath::new("code/util.vb");
    let caller_path = engram_core::RelPath::new("pages/page.vb");
    fn plain(name: &str, line: u32) -> engram_index::ExtractedSymbol {
        engram_index::ExtractedSymbol {
            name: name.to_string(),
            kind: "function".to_string(),
            start_line: line,
            end_line: line + 3,
            metadata: None,
        }
    }
    stats
        .symbols
        .push((std::sync::Arc::new(js_path.clone()), plain("Render", 4)));
    stats
        .symbols
        .push((std::sync::Arc::new(vb_path.clone()), plain("Render", 9)));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        plain("Page_Load", 2),
    ));
    stats.edges.push((
        std::sync::Arc::new(caller_path.clone()),
        engram_index::ExtractedEdge {
            source_name: "Page_Load".to_string(),
            source_kind: "function".to_string(),
            source_start_line: 3,
            source_language: "vb".to_string(),
            target_name: "Render".to_string(),
            target_kind: Some("function".to_string()),
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: None,
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
        .find(|e| e.source_id.contains("Page_Load"))
        .expect("call edge");
    assert!(
        call.target_id.contains("util.vb"),
        "VB caller must bind to the VB candidate, got {}",
        call.target_id
    );
    let meta = call.metadata.as_ref().expect("metadata");
    assert_eq!(
        meta.get("resolution").and_then(|v| v.as_str()),
        Some("batch_same_lang")
    );
    assert!(
        meta.get("cross_language").is_none(),
        "same-language binding must not be flagged"
    );
}

#[tokio::test]
async fn constructor_overloads_keep_all_call_sites_after_ingestion() {
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
    let save1 = engram_core::RelPath::new("model/widget.vb");
    let save2 = engram_core::RelPath::new("model/widget.vb");
    let caller_path = engram_core::RelPath::new("pages/edit.vb");
    stats.symbols.push((
        std::sync::Arc::new(save1.clone()),
        sym("Example.Widget.New", 10, 0),
    ));
    stats.symbols.push((
        std::sync::Arc::new(save2.clone()),
        sym("Example.Widget.New", 30, 1),
    ));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        sym("DoEdit", 5, 0),
    ));

    let mut call_meta = HashMap::new();
    call_meta.insert("args".to_string(), "1".to_string());
    stats.edges.push((
        std::sync::Arc::new(caller_path.clone()),
        engram_index::ExtractedEdge {
            source_name: "DoEdit".to_string(),
            source_kind: "function".to_string(),
            source_start_line: 6,
            source_language: "vb".to_string(),
            target_name: "Example.Widget.New".to_string(),
            target_kind: Some("function".to_string()),
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: Some(call_meta),
        },
    ));

    stats.edges[0]
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("call_site_line".into(), "6".into());
    let mut repeated = stats.edges[0].clone();
    repeated
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("call_site_line".into(), "8".into());
    stats.edges.push(repeated);
    let mut default_ctor = stats.edges[0].clone();
    default_ctor
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("args".into(), "0".into());
    default_ctor
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("call_site_line".into(), "10".into());
    stats.edges.push(default_ctor);

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
        .find(|e| e.source_id.contains("DoEdit") && e.target_id.ends_with(":30"))
        .expect("call edge exists");

    assert!(
        call.target_id.contains("model/widget.vb"),
        "must bind to the one-argument constructor, got {}",
        call.target_id
    );
    let meta = call.metadata.as_ref().expect("metadata");
    assert_eq!(
        meta.get("resolution").and_then(|v| v.as_str()),
        Some("batch_arity_match"),
        "resolution method must record the arity match"
    );
    assert_eq!(meta["call_site_lines"], serde_json::json!([6, 8]));
    let default_call = edges
        .iter()
        .find(|e| e.target_id.ends_with(":10") && e.source_id.contains("DoEdit"))
        .unwrap();
    assert_eq!(
        default_call.metadata.as_ref().unwrap()["call_site_lines"],
        serde_json::json!([10])
    );
}

#[tokio::test]
async fn ambiguous_same_file_overloads_remain_unresolved_after_resolver() {
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
    let save1 = engram_core::RelPath::new("model.vb");
    let save2 = engram_core::RelPath::new("model.vb");
    let caller_path = engram_core::RelPath::new("model.vb");
    stats
        .symbols
        .push((std::sync::Arc::new(save1.clone()), sym("Save", 10, 1)));
    stats
        .symbols
        .push((std::sync::Arc::new(save2.clone()), sym("Save", 30, 1)));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        sym("DoEdit", 5, 0),
    ));

    let mut call_meta = HashMap::new();
    call_meta.insert("args".to_string(), "1".to_string());
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
        call.target_id.starts_with("::"),
        "ambiguous binding: {}",
        call.target_id
    );
    state.graph.resolve_symbol_edges(&project_id).unwrap();
    let after = state
        .graph
        .list_edges(&project_id, Some(engram_graph::EdgeKind::Calls))
        .unwrap();
    assert!(
        after
            .iter()
            .filter(|e| e.source_id.contains("DoEdit"))
            .all(|e| e.target_id.starts_with("::")),
        "{after:?}"
    );
}

#[tokio::test]
async fn optional_constructor_arguments_bind_within_required_and_declared_bounds() {
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
    let save1 = engram_core::RelPath::new("model/widget.vb");
    let save2 = engram_core::RelPath::new("model/widget.vb");
    let caller_path = engram_core::RelPath::new("pages/edit.vb");
    stats.symbols.push((
        std::sync::Arc::new(save1.clone()),
        sym("Example.Widget.New", 10, 0),
    ));
    stats.symbols.push((
        std::sync::Arc::new(save2.clone()),
        sym("Example.Widget.New", 30, 4),
    ));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        sym("DoEdit", 5, 0),
    ));

    stats.symbols[1]
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("arity_min".into(), "2".into());
    let mut call_meta = HashMap::new();
    call_meta.insert("args".to_string(), "3".to_string());
    stats.edges.push((
        std::sync::Arc::new(caller_path.clone()),
        engram_index::ExtractedEdge {
            source_name: "DoEdit".to_string(),
            source_kind: "function".to_string(),
            source_start_line: 6,
            source_language: "vb".to_string(),
            target_name: "Example.Widget.New".to_string(),
            target_kind: Some("function".to_string()),
            target_start_line: None,
            kind: "calls".to_string(),
            metadata: Some(call_meta),
        },
    ));

    stats.edges[0]
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("call_site_line".into(), "6".into());
    let mut repeated = stats.edges[0].clone();
    repeated
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("call_site_line".into(), "8".into());
    stats.edges.push(repeated);
    let mut default_ctor = stats.edges[0].clone();
    default_ctor
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("args".into(), "0".into());
    default_ctor
        .1
        .metadata
        .as_mut()
        .unwrap()
        .insert("call_site_line".into(), "10".into());
    stats.edges.push(default_ctor);

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
        .find(|e| e.source_id.contains("DoEdit") && e.target_id.ends_with(":30"))
        .expect("call edge exists");

    assert!(
        call.target_id.contains("model/widget.vb"),
        "must bind to the optional-argument constructor, got {}",
        call.target_id
    );
    let meta = call.metadata.as_ref().expect("metadata");
    assert_eq!(
        meta.get("resolution").and_then(|v| v.as_str()),
        Some("batch_arity_match"),
        "resolution method must record the arity match"
    );
    assert_eq!(meta["call_site_lines"], serde_json::json!([6, 8]));
    let default_call = edges
        .iter()
        .find(|e| e.target_id.ends_with(":10") && e.source_id.contains("DoEdit"))
        .unwrap();
    assert_eq!(
        default_call.metadata.as_ref().unwrap()["call_site_lines"],
        serde_json::json!([10])
    );
}

#[tokio::test]
async fn external_constructor_does_not_bind_to_unrelated_local_new() {
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
    let save1 = engram_core::RelPath::new("model.vb");
    let save2 = engram_core::RelPath::new("model.vb");
    let caller_path = engram_core::RelPath::new("model.vb");
    stats.symbols.push((
        std::sync::Arc::new(save1.clone()),
        sym("Example.Local.New", 10, 0),
    ));
    stats
        .symbols
        .push((std::sync::Arc::new(save2.clone()), sym("Unrelated", 30, 3)));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        sym("DoEdit", 5, 0),
    ));

    let mut call_meta = HashMap::new();
    call_meta.insert("args".to_string(), "0".to_string());
    stats.edges.push((
        std::sync::Arc::new(caller_path.clone()),
        engram_index::ExtractedEdge {
            source_name: "DoEdit".to_string(),
            source_kind: "function".to_string(),
            source_start_line: 6,
            source_language: "vb".to_string(),
            target_name: "External.Widget.New".to_string(),
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

    assert_eq!(call.target_id, "::External.Widget.New");
    state.graph.resolve_symbol_edges(&project_id).unwrap();
    let after = state
        .graph
        .list_edges(&project_id, Some(engram_graph::EdgeKind::Calls))
        .unwrap();
    assert!(
        after
            .iter()
            .filter(|e| e.source_id.contains("DoEdit"))
            .all(|e| e.target_id == "::External.Widget.New"),
        "{after:?}"
    );
}

#[tokio::test]
async fn external_qualified_receiver_does_not_bind_to_unrelated_local_method() {
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
    let save1 = engram_core::RelPath::new("model.vb");
    let save2 = engram_core::RelPath::new("model.vb");
    let caller_path = engram_core::RelPath::new("model.vb");
    stats.symbols.push((
        std::sync::Arc::new(save1.clone()),
        sym("Example.Local.Exists", 10, 0),
    ));
    stats
        .symbols
        .push((std::sync::Arc::new(save2.clone()), sym("Unrelated", 30, 3)));
    stats.symbols.push((
        std::sync::Arc::new(caller_path.clone()),
        sym("DoEdit", 5, 0),
    ));

    let mut call_meta = HashMap::new();
    call_meta.insert("args".to_string(), "0".to_string());
    stats.edges.push((
        std::sync::Arc::new(caller_path.clone()),
        engram_index::ExtractedEdge {
            source_name: "DoEdit".to_string(),
            source_kind: "function".to_string(),
            source_start_line: 6,
            source_language: "vb".to_string(),
            target_name: "IO.Directory.Exists".to_string(),
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

    assert_eq!(call.target_id, "::IO.Directory.Exists");
    state.graph.resolve_symbol_edges(&project_id).unwrap();
    let after = state
        .graph
        .list_edges(&project_id, Some(engram_graph::EdgeKind::Calls))
        .unwrap();
    assert!(
        after
            .iter()
            .filter(|e| e.source_id.contains("DoEdit"))
            .all(|e| e.target_id == "::IO.Directory.Exists"),
        "{after:?}"
    );
}
