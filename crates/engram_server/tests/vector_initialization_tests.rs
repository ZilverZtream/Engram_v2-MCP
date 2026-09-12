use engram_core::{Config, ProjectRecord, RelPath};
use engram_index::{HybridSearchEngine, IndexDoc};
use engram_server::{AppState, Engram};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn initialize_vectors_scope_preserves_corpora_and_generation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    std::fs::create_dir(&root).unwrap();
    let pid = "vector-init-fixture";
    let data = temp.path().join("data");
    let project = data.join("projects").join(pid);
    let seed = HybridSearchEngine::new(
        project.join("tantivy"),
        project.join("lancedb"),
        &Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for ns in ["memory", "history", "memory_bank"] {
        let doc = IndexDoc {
            generation: if ns == "memory" { 7 } else { 0 },
            chunk_id: 123,
            path: RelPath::new("fixture.txt"),
            language: "text".into(),
            content: format!("Preserve the full {ns} evidence."),
            namespace: ns.into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 1,
            doc_id: format!("{ns}-fixture"),
            content_hash: "unchanged-hash".into(),
        };
        seed.index_docs(pid, &[doc], &CancellationToken::new())
            .await
            .unwrap();
    }
    drop(seed);
    let (state, _) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root.clone()],
        embedding_backend: "local".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: pid.into(),
            project_name: "Generic vector initialization".into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(pid, "active_generation", "7")
        .unwrap();
    state
        .registry
        .set_meta(pid, "pr_ingest_watermark", "preserved-watermark")
        .unwrap();
    let engram = Engram::new(state.clone());
    let request = |wipe| {
        serde_json::from_value(json!({
            "project_id":pid,"scope":"initialize_vectors","wipe_and_reindex":wipe
        }))
        .unwrap()
    };
    assert!(engram.handle_repair_project(request(true)).await.is_err());
    let result = engram.handle_repair_project(request(false)).await.unwrap();
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Initialized 3 vectors")
    );
    assert_eq!(
        state
            .registry
            .get_meta(pid, "active_generation")
            .unwrap()
            .as_deref(),
        Some("7")
    );
    assert_eq!(
        state
            .registry
            .get_meta(pid, "pr_ingest_watermark")
            .unwrap()
            .as_deref(),
        Some("preserved-watermark")
    );
    for ns in ["memory", "history", "memory_bank"] {
        let result = engram
            .handle_get_chunk(
                serde_json::from_value(json!({
                    "project_id":pid,"namespace":ns,"doc_id":format!("{ns}-fixture")
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains(&format!("Preserve the full {ns} evidence."))
        );
    }
    assert!(
        engram
            .handle_repair_project(request(false))
            .await
            .unwrap_err()
            .message
            .contains("already exists")
    );
    assert_eq!(
        state
            .active_indexing_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        state
            .get_project_cached(pid)
            .unwrap()
            .search
            .count_vectors(pid)
            .await
            .unwrap(),
        3
    );
}
