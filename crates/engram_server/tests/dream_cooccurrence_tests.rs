#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 — Dream row (owner decision 10:33: fix first,
//! then ablate). Live OciusX evidence: a 10-hit `search_memory` produced the
//! 10 `chunk` nodes and +20 `dependency` edges the recorder writes, but ZERO
//! `co_occurrence` edges — the dreamer's only input never lands, so every
//! dream cycle finds no clusters and "succeeds" doing nothing.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_server::actors::dreamer::record_cooccurrence;
use engram_server::state::{AppState, SearchHitLite};

const PID: &str = "dream-cooccurrence-test";

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "7")
        .unwrap();
    (tmp, state)
}

fn hit(pk: &str, path: &str) -> SearchHitLite {
    SearchHitLite {
        pk: pk.into(),
        doc_id: format!("doc-{pk}"),
        path: RelPath::new(path),
        chunk_id: Some(1),
    }
}

#[tokio::test]
async fn a_search_session_records_co_occurrence_edges_between_its_hits() {
    let (_tmp, state) = state();
    let hits = vec![
        hit("p1", "Site/App_Code/a.vb"),
        hit("p2", "Site/App_Code/b.vb"),
        hit("p3", "Site/App_Code/c.vb"),
    ];
    record_cooccurrence(&state, PID, &hits).await.unwrap();

    let counts = state.graph.count_edges_by_kind(PID).unwrap();
    assert_eq!(
        counts.get("dependency").copied().unwrap_or(0),
        6,
        "3 file<->chunk pairs, both directions: {counts:?}"
    );
    assert_eq!(
        counts.get("co_occurrence").copied().unwrap_or(0),
        6,
        "3 hits = 3 chunk pairs, both directions: {counts:?}"
    );
    // The edges are readable the way the dreamer's clustering reads them.
    let n = state
        .graph
        .neighbors(PID, engram_graph::EdgeKind::CoOccurrence, "pk:p1", 10)
        .unwrap();
    assert_eq!(n.len(), 2, "p1 co-occurs with p2 and p3: {n:?}");
}

#[tokio::test]
async fn repeated_sessions_accumulate_weight_instead_of_duplicating() {
    let (_tmp, state) = state();
    let hits = vec![hit("p1", "a.vb"), hit("p2", "b.vb")];
    record_cooccurrence(&state, PID, &hits).await.unwrap();
    record_cooccurrence(&state, PID, &hits).await.unwrap();
    let counts = state.graph.count_edges_by_kind(PID).unwrap();
    assert_eq!(
        counts.get("co_occurrence").copied().unwrap_or(0),
        2,
        "{counts:?}"
    );
    let n = state
        .graph
        .neighbors(PID, engram_graph::EdgeKind::CoOccurrence, "pk:p1", 10)
        .unwrap();
    assert_eq!(
        n,
        vec![("pk:p2".to_string(), 2)],
        "weight 2 after two sessions"
    );
}
