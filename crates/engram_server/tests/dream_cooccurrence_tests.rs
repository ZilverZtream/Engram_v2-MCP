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
    let parts: Vec<_> = pk.splitn(4, ':').collect();
    SearchHitLite {
        source_project_id: (parts.len() == 4).then(|| parts[0].to_string()),
        namespace: (parts.len() == 4).then(|| parts[1].to_string()),
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

#[tokio::test]
async fn knowledge_search_must_not_create_source_file_inventory() {
    let (_tmp, state) = state();
    let hits = vec![
        hit(
            "dream-cooccurrence-test:business_logic:0:note-a",
            "notes/summary.md",
        ),
        hit(
            "dream-cooccurrence-test:memory_bank:0:note-b",
            "memory_bank:review-guide",
        ),
    ];
    record_cooccurrence(&state, PID, &hits).await.unwrap();
    assert!(
        state
            .graph
            .list_file_node_metadata_with_generation(PID)
            .unwrap()
            .is_empty(),
        "Knowledge search results must not become indexed source files"
    );
    let format =
        engram_server::services::project_service::source_index_format_coverage_for_generation(
            &state, PID, 7,
        )
        .await
        .unwrap();
    assert_eq!(
        format,
        (0, vec![]),
        "Learning must not require a source reindex"
    );
    assert_eq!(
        state
            .graph
            .neighbors(
                PID,
                engram_graph::EdgeKind::CoOccurrence,
                &format!("pk:{}", hits[0].pk),
                10
            )
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn search_preserves_all_canonical_source_file_fields() {
    for generation in [6, 8] {
        let (_tmp, state) = state();
        let node = engram_graph::Node {
            node_id: "file:src/ledger.vb".into(),
            node_type: "file".into(),
            name: "ledger.vb".into(),
            namespace: "custom_source".into(),
            language: "vb".into(),
            file_path: RelPath::new("src/ledger.vb"),
            start_line: 1,
            end_line: 20,
            generation,
            metadata: Some(
                serde_json::json!({"source_index_version":3,"file_hash":"preserved-fingerprint","mtime_ms":123,"size":42}),
            ),
        };
        state.graph.upsert_nodes(PID, &[node.clone()]).unwrap();
        let before_format =
            engram_server::services::project_service::source_index_format_coverage_for_generation(
                &state, PID, 7,
            )
            .await
            .unwrap();
        record_cooccurrence(
            &state,
            PID,
            &[hit(
                &format!("dream-cooccurrence-test:custom_source:{generation}:source"),
                "src/ledger.vb",
            )],
        )
        .await
        .unwrap();
        let after = state.graph.get_node(PID, &node.node_id).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(node).unwrap(),
            "Recording a hit cannot rewrite canonical namespace, generation, range or fingerprints"
        );
        assert_eq!(
            state
                .graph
                .neighbors(
                    PID,
                    engram_graph::EdgeKind::Dependency,
                    "file:src/ledger.vb",
                    10
                )
                .unwrap(),
            vec![(
                format!("pk:dream-cooccurrence-test:custom_source:{generation}:source"),
                1
            )]
        );
        let after_format =
            engram_server::services::project_service::source_index_format_coverage_for_generation(
                &state, PID, 7,
            )
            .await
            .unwrap();
        assert_eq!(
            before_format, after_format,
            "Learning must not hide pending publication"
        );
        assert_eq!(
            !after_format.1.is_empty(),
            generation > 7,
            "This checks metadata publication state, not source bytes or semantics"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn knowledge_learning_keeps_real_source_health_and_freshness_complete() {
    use engram_server::tools::Engram;
    use rmcp::handler::server::tool::Parameters;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("source");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ledger.rs"), "pub fn total() -> i32 { 1 }\n").unwrap();
    let (state, _rx) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let api = Engram::new(state.clone());
    api.index_project(Parameters(engram_server::IndexProjectRequest {
        directory: root.to_string_lossy().into_owned(),
        project_name: "learning-health".into(),
        project_type: engram_server::models::ProjectType::General,
        wait: true,
        dedupe_by_directory: false,
    }))
    .await
    .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let text = |result: rmcp::model::CallToolResult| {
        result
            .content
            .into_iter()
            .filter_map(|c| match c.raw {
                rmcp::model::RawContent::Text(t) => Some(t.text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let before = text(
        api.get_index_freshness(Parameters(engram_server::GetIndexFreshnessRequest {
            project_id: pid.clone(),
            check_disk: true,
        }))
        .await
        .unwrap(),
    );
    assert!(before.contains("generation_complete: true"), "{before}");
    record_cooccurrence(
        &state,
        &pid,
        &[
            hit(
                &format!("{pid}:business_logic:0:knowledge-a"),
                "notes/decisions.md",
            ),
            hit(
                &format!("{pid}:memory_bank:0:knowledge-b"),
                "memory_bank:guide",
            ),
        ],
    )
    .await
    .unwrap();
    let after = text(
        api.get_index_freshness(Parameters(engram_server::GetIndexFreshnessRequest {
            project_id: pid.clone(),
            check_disk: true,
        }))
        .await
        .unwrap(),
    );
    assert!(after.contains("generation_complete: true"), "{after}");
    assert!(after.contains("disk_check: no_changes_detected"), "{after}");
    assert!(after.contains("source_index_format: current"), "{after}");
    let health = text(
        api.project_health(Parameters(engram_server::ProjectIdRequest {
            project_id: pid,
        }))
        .await
        .unwrap(),
    );
    assert!(health.starts_with("Health: OK"), "{health}");
}

#[tokio::test]
async fn mixed_user_project_and_unknown_hits_keep_provenance_and_never_invent_source_ownership() {
    let (_tmp, state) = state();
    let hits = vec![
        hit(
            "dream-cooccurrence-test:business_logic:0:own",
            "same/path.md",
        ),
        hit("__user__:memory_bank:0:user", "same/path.md"),
        hit("legacy-without-origin", "same/path.md"),
    ];
    record_cooccurrence(&state, PID, &hits).await.unwrap();
    assert!(state.graph.list_file_node_metadata(PID).unwrap().is_empty());
    assert!(
        state
            .graph
            .list_file_node_metadata("__user__")
            .unwrap()
            .is_empty()
    );
    for (hit, expected) in hits.iter().zip([Some(PID), Some("__user__"), None]) {
        let node = state
            .graph
            .get_node(PID, &format!("search-document:{}", hit.pk))
            .unwrap()
            .unwrap();
        assert_eq!(node.node_type, "search_document");
        let metadata = node.metadata.unwrap();
        assert_eq!(metadata["source_project_id"].as_str(), expected);
        assert_eq!(
            metadata["provenance_status"],
            if expected.is_some() {
                "supplied_search_identity"
            } else {
                "unknown"
            }
        );
        assert_eq!(
            state
                .graph
                .neighbors(
                    PID,
                    engram_graph::EdgeKind::CoOccurrence,
                    &format!("pk:{}", hit.pk),
                    10
                )
                .unwrap()
                .len(),
            2
        );
    }
}

#[tokio::test]
async fn mismatched_provenance_cannot_reuse_canonical_file() {
    let (_tmp, state) = state();
    let mut h = hit("__user__:memory:0:external", "ledger.vb");
    h.source_project_id = Some(PID.into());
    h.namespace = Some("memory".into());
    record_cooccurrence(&state, PID, &[h.clone()])
        .await
        .unwrap();
    let node = state
        .graph
        .get_node(PID, &format!("search-document:{}", h.pk))
        .unwrap()
        .unwrap();
    assert_eq!(node.metadata.unwrap()["provenance_status"], "unknown");
    assert!(state.graph.list_file_node_metadata(PID).unwrap().is_empty());
}

#[tokio::test]
async fn user_hit_cannot_link_matching_project_source_path() {
    let (_tmp, state) = state();
    let node = engram_graph::Node {
        node_id: "file:shared.md".into(),
        node_type: "file".into(),
        name: "shared.md".into(),
        namespace: "memory".into(),
        language: "markdown".into(),
        file_path: RelPath::new("shared.md"),
        start_line: 1,
        end_line: 5,
        generation: 6,
        metadata: Some(serde_json::json!({"file_hash":"source-only", "source_index_version":3})),
    };
    state.graph.upsert_nodes(PID, &[node.clone()]).unwrap();
    let hits = [
        hit("__user__:memory:0:user", "shared.md"),
        hit("dream-cooccurrence-test:memory:6:own", "shared.md"),
    ];
    record_cooccurrence(&state, PID, &hits).await.unwrap();
    let linked = state
        .graph
        .neighbors(PID, engram_graph::EdgeKind::Dependency, &node.node_id, 10)
        .unwrap();
    assert_eq!(
        linked,
        vec![("pk:dream-cooccurrence-test:memory:6:own".into(), 1)]
    );
    assert!(
        state
            .graph
            .get_node(PID, "search-document:__user__:memory:0:user")
            .unwrap()
            .is_some()
    );
    assert_eq!(
        serde_json::to_value(state.graph.get_node(PID, &node.node_id).unwrap().unwrap()).unwrap(),
        serde_json::to_value(node).unwrap()
    );
}
