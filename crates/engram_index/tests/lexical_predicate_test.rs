use engram_core::{Config, RelPath};
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn qualifying_documents_survive_more_than_one_page_of_rejected_hits() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let engine = HybridSearchEngine::new(tmp.path().join("fts"), tmp.path().join("vectors"), &cfg)
        .await
        .unwrap();
    let mut docs = Vec::new();
    for n in 0..100 {
        let content = if n == 99 {
            format!(
                "matching story | kinds: database {}",
                "unrelated ".repeat(100)
            )
        } else {
            "matching story | kinds: backend".into()
        };
        docs.push(IndexDoc {
            generation: 0,
            chunk_id: n,
            path: RelPath::new(&format!("pr:{n}")),
            language: "text".into(),
            content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
            content,
            namespace: "history".into(),
            author: None,
            timestamp: Some(1),
            start_line: 1,
            end_line: 1,
            doc_id: format!("doc{n}"),
        });
    }
    engine
        .index_docs("project", &docs, &CancellationToken::new())
        .await
        .unwrap();
    let q = HybridQuery {
        project_id: "project".into(),
        namespace: "history".into(),
        generation: 1,
        text: "matching story".into(),
        top_k: 1,
        fts_mode: "strict".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };
    assert_ne!(engine.lexical_search(&q).unwrap()[0].doc_id, "doc99");
    let mut visited = 0;
    let hits = engine
        .lexical_search_matching(&q, &mut |_, content| {
            visited += 1;
            content.contains("kinds: database")
        })
        .unwrap();
    assert!(visited > 32, "fixture must exercise paging: {visited}");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id, "doc99");
    assert!(
        engine
            .lexical_search_matching(&q, &mut |_, _| false)
            .unwrap()
            .is_empty()
    );
    let mut other_project = q.clone();
    other_project.project_id = "unrelated".into();
    assert!(
        engine
            .lexical_search_matching(&other_project, &mut |_, _| true)
            .unwrap()
            .is_empty()
    );
}
