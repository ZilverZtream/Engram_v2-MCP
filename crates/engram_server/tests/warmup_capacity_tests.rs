//! Startup priming must not churn through more projects than the runtime cache holds.
use engram_core::{ProjectRecord, config::Config};
use engram_server::{actors::warmup::warm_all_projects, state::AppState, tools::Engram};
use serde_json::json;

#[test]
fn startup_keeps_recent_projects_warm_and_older_projects_load_on_demand() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let config = Config {
        allowed_roots: vec![temp.path().to_path_buf()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    };
    let content = "public class UniqueWarmupNeedle {}";
    runtime.block_on(async {
        {
            let (state, _) = AppState::new(config.clone()).unwrap();
            let server = Engram::new(state.clone());
            for number in 0..7 {
                let id = format!("warm-{number}");
                let root = temp.path().join(&id);
                std::fs::create_dir_all(&root).unwrap();
                std::fs::write(root.join("Orders.cs"), content).unwrap();
                state
                    .registry
                    .register_source_project(&ProjectRecord {
                        project_id: id.clone(),
                        project_name: id.clone(),
                        directory: root.to_string_lossy().into_owned(),
                        project_type: "general".into(),
                        created_at_ms: 0,
                        updated_at_ms: number,
                        reindex_required_since_ms: None,
                    })
                    .unwrap();
                let result = server
                    .handle_repair_project(
                        serde_json::from_value(json!({
                            "project_id": id, "scope": "full", "wipe_and_reindex": false,
                            "max_commits": 0, "index_antipatterns": false,
                        }))
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(!result.is_error.unwrap_or(false));
                assert!(
                    matches!(state.registry.source_snapshot_requirement(&id).unwrap(),
                (head, engram_core::registry::SourceSnapshotRequirement::Managed) if head > 0)
                );
                // Fix recency after the public producer's own update timestamp.
                let mut record = state.registry.get_project(&id).unwrap().unwrap();
                record.updated_at_ms = number;
                state.registry.put_project(&record).unwrap();
            }
            assert!(state.registry.list_jobs(None).unwrap().is_empty());
        }
    });
    drop(runtime);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (fresh, _) = AppState::new(config).unwrap();
        let warmed = warm_all_projects(&fresh).await;
        assert_eq!(
            warmed, 5,
            "Startup must not prime beyond the five-project runtime capacity"
        );
        assert!(
            fresh.get_project_cached("warm-6").is_some(),
            "Warming older projects must not evict the newest project"
        );
        for id in ["warm-0", "warm-1"] {
            assert!(
                !fresh.projects.contains_key(id),
                "An unselected runtime was opened"
            );
            assert!(
                !fresh.node_snapshot_cache.contains_key(id),
                "An unselected node snapshot was allocated"
            );
            assert!(
                !fresh.setting_prior_cache.contains_key(id),
                "An unselected settings prior was allocated"
            );
            assert!(
                !fresh.co_change_cache.contains_key(id),
                "An unselected history snapshot was allocated"
            );
            assert!(
                !fresh.ui_catalog_cache.contains_key(id),
                "An unselected UI catalog was allocated"
            );
        }
        let server = Engram::new(fresh.clone());
        for id in ["warm-6", "warm-0"] {
            let result = server
                .handle_search_memory(
                    serde_json::from_value(json!({
                        "project_id": id, "query": "UniqueWarmupNeedle", "semantic": false,
                        "include_content": true, "max_results": 1,
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let text = result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("Orders.cs") && text.contains(content),
                "Warm and demand-loaded projects must both retrieve the stored source: {text}"
            );
        }
        assert!(
            fresh.get_project_cached("warm-0").is_some(),
            "Unselected projects remain available on demand"
        );
        assert!(fresh.registry.list_jobs(None).unwrap().is_empty());
    });
    drop(runtime);
    temp.close().unwrap();
}
