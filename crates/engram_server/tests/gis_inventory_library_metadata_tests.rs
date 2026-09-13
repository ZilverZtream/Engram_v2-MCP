#![allow(clippy::unwrap_used)]

use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

#[tokio::test]
async fn gis_inventory_reads_current_and_legacy_spatial_library_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _events) = AppState::new(cfg).unwrap();
    let project_id = "gis-library-metadata";
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
        .registry
        .set_meta(project_id, "active_generation", "1")
        .unwrap();
    let edges = vec![
        Edge {
            source_id: "file:map.ts".into(),
            target_id: "gis:google_maps:kmllayer".into(),
            namespace: "memory".into(),
            language: "typescript".into(),
            edge_kind: EdgeKind::SpatialCall,
            weight: 1,
            generation: 1,
            metadata: Some(json!({
                "library": "google_maps",
                "map_class": "KmlLayer",
                "modern_equivalent": "Manual migration analysis required",
                "count": "2"
            })),
            updated_at_ms: 0,
        },
        Edge {
            source_id: "file:legacy.js".into(),
            target_id: "gis:google_maps:map".into(),
            namespace: "memory".into(),
            language: "javascript".into(),
            edge_kind: EdgeKind::SpatialCall,
            weight: 1,
            generation: 1,
            metadata: Some(json!({
                "gis_library": "google_maps",
                "map_class": "Map",
                "modern_equivalent": "GoogleMap",
                "count": "1"
            })),
            updated_at_ms: 0,
        },
    ];
    state.graph.upsert_edges(project_id, &edges).unwrap();

    let result = Engram::new(state)
        .handle_get_gis_inventory(
            serde_json::from_value(json!({"project_id": project_id})).unwrap(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("Map API usage (3 call sites)"), "{text}");
    assert!(
        text.contains("google_maps.KmlLayer: 2 call site(s)"),
        "{text}"
    );
    assert!(text.contains("google_maps.Map: 1 call site(s)"), "{text}");
}
