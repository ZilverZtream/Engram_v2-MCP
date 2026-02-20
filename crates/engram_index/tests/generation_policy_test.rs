use engram_core::{RelPath, namespaces};
use engram_index::{HybridSearchEngine, IndexDoc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_generation_semantics_enforcement() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp_path = std::env::temp_dir().join(format!("engram_test_{}", now));
    std::fs::create_dir_all(&tmp_path).unwrap();
    let tantivy_dir = tmp_path.join("tantivy");
    let lancedb_dir = tmp_path.join("lancedb");

    let cfg = engram_core::Config::default();
    let engine = HybridSearchEngine::new(
        tantivy_dir,
        lancedb_dir,
        &cfg,
    )
    .await
    .unwrap();

    let project_id = "test_project";
    let cancel = CancellationToken::new();

    // 1. Snapshot namespace (memory) - should keep generation as-is
    let doc_mem = IndexDoc {
        generation: 42,
        chunk_id: 1,
        path: RelPath::new("file.rs"),
        language: "rust".into(),
        content: "content".into(),
        namespace: namespaces::NAMESPACE_MEMORY.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 2,
        doc_id: "doc1".into(),
        content_hash: "hash1".into(),
    };

    // 2. GlobalMutable namespace (memory_bank) - should force generation to 0
    let doc_mb = IndexDoc {
        generation: 42,
        chunk_id: 2,
        path: RelPath::new("section"),
        language: "markdown".into(),
        content: "bank content".into(),
        namespace: namespaces::NAMESPACE_MEMORY_BANK.into(),
        author: None,
        timestamp: None,
        start_line: 0,
        end_line: 0,
        doc_id: "doc2".into(),
        content_hash: "hash2".into(),
    };

    engine
        .index_docs(project_id, &[doc_mem, doc_mb], &cancel)
        .await
        .unwrap();

    // Verify memory doc (Snapshot)
    let res_mem = engine
        .get_doc_by_doc_id(project_id, namespaces::NAMESPACE_MEMORY, 42, "doc1")
        .unwrap();
    assert!(
        res_mem.is_some(),
        "Memory doc should be found with generation 42"
    );

    // Verify memory_bank doc (GlobalMutable)
    // get_doc_by_doc_id correctly handles GlobalMutable by internally using generation 0.
    // So calling it with 42 SHOULD still find it because it maps to 0.
    let res_mb_any_gen = engine
        .get_doc_by_doc_id(project_id, namespaces::NAMESPACE_MEMORY_BANK, 42, "doc2")
        .unwrap();
    assert!(
        res_mb_any_gen.is_some(),
        "Memory bank doc SHOULD be found even when called with gen 42 due to internal mapping to 0"
    );

    // To PROVE it's stored as 0, we can manually build a PK with 42 and check get_doc_by_pk.
    // That should fail because the actual stored PK has generation 0.
    let pk_42 = format!(
        "{}:{}:42:doc2",
        project_id,
        namespaces::NAMESPACE_MEMORY_BANK
    );
    let res_mb_raw_42 = engine.get_doc_by_pk(&pk_42).unwrap();
    assert!(
        res_mb_raw_42.is_none(),
        "Memory bank doc should NOT be found if we manually bypass policy and look for gen 42 in PK"
    );

    // Manual PK with 0 should work.
    let pk_0 = format!(
        "{}:{}:0:doc2",
        project_id,
        namespaces::NAMESPACE_MEMORY_BANK
    );
    let res_mb_raw_0 = engine.get_doc_by_pk(&pk_0).unwrap();
    assert!(
        res_mb_raw_0.is_some(),
        "Memory bank doc SHOULD be found with manual PK for generation 0"
    );
}
