//! Startup priming must not churn through more projects than the runtime cache holds.
use engram_core::{ContentHash, DocIdStr, ProjectRecord, config::Config};
use engram_index::IndexDoc;
use engram_server::{
    actors::warmup::warm_all_projects, services::project_service::ensure_project_runtime,
    state::AppState, tools::Engram,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_keeps_recent_projects_warm_and_older_projects_load_on_demand() {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        allowed_roots: vec![temp.path().to_path_buf()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    };
    let content = "public class UniqueWarmupNeedle {}";
    let hash = ContentHash::compute(content.as_bytes());
    let doc = IndexDoc {
        generation: 1,
        chunk_id: engram_index::chunk_id_from_content_hash(&hash),
        doc_id: DocIdStr::compute("Orders.cs", 1, 1, &hash).0,
        content_hash: hash.0,
        path: "Orders.cs".into(),
        language: "csharp".into(),
        content: content.into(),
        namespace: "memory".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
    };
    {
        let (state, _) = AppState::new(config.clone()).unwrap();
        for number in 0..7 {
            let id = format!("warm-{number}");
            let root = temp.path().join(&id);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("Orders.cs"), content).unwrap();
            state
                .registry
                .put_project(&ProjectRecord {
                    project_id: id.clone(),
                    project_name: id.clone(),
                    directory: root.to_string_lossy().into_owned(),
                    project_type: "general".into(),
                    created_at_ms: 0,
                    updated_at_ms: number,
                    reindex_required_since_ms: None,
                })
                .unwrap();
            state
                .registry
                .set_meta(&id, "active_generation", "1")
                .unwrap();
            let runtime = ensure_project_runtime(&state, &id).await.unwrap();
            runtime
                .search
                .index_docs(&id, std::slice::from_ref(&doc), &CancellationToken::new())
                .await
                .unwrap();
        }
    }
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
}
