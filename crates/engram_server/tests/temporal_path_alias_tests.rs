#![allow(clippy::unwrap_used)]

use engram_core::config::Config;
use engram_graph::EdgeKind;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

#[tokio::test]
async fn current_path_recovers_and_deduplicates_pre_move_temporal_edges() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(project_dir.join("Site/src")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        project_dir.join("Site/src/layer.ts"),
        "export class Layer {}\n",
    )
    .unwrap();
    std::fs::write(project_dir.join("Site/src/map.ts"), "export class Map {}\n").unwrap();

    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _events) = AppState::new(cfg).unwrap();
    let project_id = "temporal-path-alias";
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: project_id.into(),
            project_name: project_id.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .graph
        .increment_undirected_edge(
            project_id,
            "history",
            "typescript",
            EdgeKind::TemporalCoupling,
            "file:src/layer.ts",
            "file:src/map.ts",
            7,
            1,
        )
        .unwrap();
    // A duplicate identity from the tree move must not appear as a second
    // neighbor or inflate the weight when aliases are reconciled.
    state
        .graph
        .increment_undirected_edge(
            project_id,
            "history",
            "typescript",
            EdgeKind::TemporalCoupling,
            "file:src/layer.ts",
            "file:Site/src/map.ts",
            5,
            1,
        )
        .unwrap();

    let engram = Engram::new(state);
    let result = engram
        .handle_analyze_temporal_couplings(
            serde_json::from_value(json!({
                "project_id": project_id,
                "file_path": "Site/src/layer.ts",
                "min_frequency": 1,
                "limit": 10
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("Path aliases reconciled"), "{text}");
    assert!(
        text.contains("file:Site/src/layer.ts <-> file:Site/src/map.ts (weight=7)"),
        "{text}"
    );
    assert_eq!(text.matches("file:Site/src/map.ts").count(), 1, "{text}");

    let reverse = engram
        .handle_analyze_temporal_couplings(
            serde_json::from_value(json!({
                "project_id": project_id,
                "file_path": "src/layer.ts",
                "min_frequency": 1,
                "limit": 10
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let reverse_text = &reverse.content[0].as_text().unwrap().text;
    assert!(
        reverse_text.contains("file:Site/src/layer.ts <-> file:Site/src/map.ts (weight=7)"),
        "{reverse_text}"
    );
}
