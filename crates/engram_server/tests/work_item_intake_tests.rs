//! Intake failures must stop before a title-only change-set is retrieved.
use engram_core::config::Config;
use engram_server::{state::AppState, tools::Engram};
use serde_json::json;

#[tokio::test]
async fn referenced_work_item_without_coordinates_blocks_before_retrieval() {
    let temp = tempfile::tempdir().unwrap();
    let config = Config {
        allowed_roots: vec![temp.path().to_path_buf()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _receiver) = AppState::new(config).unwrap();
    let server = Engram::new(state);
    // No registered coordinates or remote exist, so this stays offline even
    // when the test host has ADO_PAT. A retrieval attempt would instead fail
    // with an unknown-project error, which is not the asserted intake result.
    for story in ["DMO-847 Fix assignment", "Bug #847 Fix assignment"] {
        for item_text in [None, Some(""), Some(" \n ")] {
            let mut request = json!({"project_id":"intake-fixture", "story":story});
            if let Some(text) = item_text {
                request["work_item_text"] = json!(text);
            }
            let error = server
                .handle_get_change_set(serde_json::from_value(request).unwrap())
                .await
                .expect_err("missing item evidence must block intake");
            assert!(error.message.contains("INCOMPLETE_INTAKE"), "{error}");
            assert!(error.message.contains("No change-set dossier was generated"));
        }
    }
}


#[tokio::test]
async fn rich_text_metadata_preserves_history_retrieval() {
    use engram_core::{ContentHash, DocIdStr, ProjectRecord};
    use engram_index::IndexDoc;
    use tokio_util::sync::CancellationToken;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots: vec![root.clone()], data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(), ..Default::default()
    }).unwrap();
    let pid = "rich-intake-history";
    state.registry.put_project(&ProjectRecord {
        project_id: pid.into(), project_name: pid.into(),
        directory: root.to_string_lossy().into_owned(), project_type: "general".into(),
        created_at_ms: 0, updated_at_ms: 0, reindex_required_since_ms: None,
    }).unwrap();
    state.registry.set_meta(pid, "active_generation", "1").unwrap();
    engram_server::services::project_service::ensure_project_runtime(&state, pid).await.unwrap();
    let mut docs = Vec::new();
    for (file, content) in [("Orders.cs", "Search orders"), ("Transfer.cs", "attachments opaque image width screenshot") ] {
        std::fs::write(root.join(file), "class Fixture {}\n").unwrap();
        let path = format!("diff:{}:{file}", "a".repeat(40));
        let hash = ContentHash::compute(content.as_bytes());
        docs.push(IndexDoc {
            generation: 1, chunk_id: engram_index::chunk_id_from_content_hash(&hash),
            doc_id: DocIdStr::compute(&path, 0, 0, &hash).0, content_hash: hash.0,
            path: path.into(), language: "csharp".into(), content: content.into(),
            namespace: "history".into(), author: None, timestamp: Some(1767225600),
            start_line: 0, end_line: 0,
        });
    }
    state.get_project_cached(pid).unwrap().search.index_docs(pid, &docs, &CancellationToken::new()).await.unwrap();
    let server = Engram::new(state);
    let plain = "Search orders";
    let rich = r#"<div>Search orders<img src="https://example.test/attachments/opaque" width="920" title="screenshot"></div>"#;
    // Positive counterexample: raw HTML attributes can suppress the relevant
    // history hit through query syntax. The dossier must normalize first.
    let raw_history = server.handle_search_history(serde_json::from_value(json!({
        "project_id": pid, "query": rich, "fts_mode": "loose", "limit": 12,
    })).unwrap()).await.unwrap();
    assert!(!raw_history.content[0].as_text().unwrap().text.contains("Orders.cs"), "Raw-query control must reproduce lost recall");
    let mut outputs = Vec::new();
    let capture = format!("Work item 847, original revision 6\nTitle: {plain}\n\nDescription (complete original HTML):\n{rich}\n\nAcceptance Criteria: [empty in original revision]\nOriginal referenced image: original.png");
    for (text, supplied_capture) in [(plain, None), (rich, None), ("AB#847", Some(capture.as_str()))] {
        let result = server.handle_get_change_set(serde_json::from_value(json!({
            "project_id": pid, "story": text, "output_json": true,
            "work_item_text": supplied_capture,
        })).unwrap()).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        if let Some(capture) = supplied_capture {
            assert!(value["story"].as_str().unwrap().contains(capture), "Complete supplied evidence must survive retrieval normalization");
        } else {
            assert_eq!(value["story"], text, "Original evidence must survive retrieval normalization");
        }
        let history: Vec<_> = value["files"].as_array().unwrap().iter()
            .filter(|f| f["signals"].as_array().unwrap().iter().any(|s| s == "history"))
            .map(|f| f["path"].as_str().unwrap().to_string()).collect();
        assert!(history.iter().any(|p| p == "Orders.cs"), "{value}");
        assert!(!history.iter().any(|p| p == "Transfer.cs"), "Image metadata added an unrelated history candidate: {value}");
        outputs.push(history);
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0], outputs[2], "An ID plus a complete capture must retain the same history recall");
}
