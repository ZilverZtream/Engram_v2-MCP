//! `date_before` is an exclusive cutoff on every leg. The vector leg filtered
//! `timestamp < before` while the lexical leg filtered `[after TO before]`, so
//! passing a story commit's own timestamp as the cutoff still returned that
//! commit through BM25 — the replay leak measured on 40 stories (95 hits on
//! the answer commit). `date_after` stays inclusive, so adjacent windows
//! partition time without overlap or gap.

use engram_core::{Config, RelPath};
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn date_before_excludes_the_cutoff_instant_on_every_lexical_path() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let engine = HybridSearchEngine::new(tmp.path().join("fts"), tmp.path().join("vectors"), &cfg)
        .await
        .unwrap();
    let docs = [
        (99, "commit:before"),
        (100, "commit:at"),
        (101, "commit:after"),
    ]
    .iter()
    .enumerate()
    .map(|(n, (ts, path))| {
        let content = "missing photo filter".to_string();
        IndexDoc {
            generation: 0,
            chunk_id: n as u64,
            path: RelPath::new(*path),
            language: "text".into(),
            content_hash: blake3::hash(path.as_bytes()).to_hex().to_string(),
            content,
            namespace: "history".into(),
            author: None,
            timestamp: Some(*ts),
            start_line: 0,
            end_line: 0,
            doc_id: format!("doc{n}"),
        }
    })
    .collect::<Vec<_>>();
    engine
        .index_docs("project", &docs, &CancellationToken::new())
        .await
        .unwrap();
    let query = HybridQuery {
        project_id: "project".into(),
        namespace: "history".into(),
        generation: 1,
        text: "missing photo filter".into(),
        top_k: 10,
        fts_mode: "loose".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: Some(100),
        date_before: Some(101),
        use_mmr: false,
    };
    let only = |paths: Vec<String>| {
        assert_eq!(paths, vec!["commit:at".to_string()]);
    };
    only(
        engine
            .lexical_search(&query)
            .unwrap()
            .into_iter()
            .map(|h| h.path.as_str().to_string())
            .collect(),
    );
    only(
        engine
            .lexical_search_with_content(&query)
            .unwrap()
            .into_iter()
            .map(|(h, _, _)| h.path.as_str().to_string())
            .collect(),
    );
    only(
        engine
            .lexical_search_matching(&query, &mut |_, _| true)
            .unwrap()
            .into_iter()
            .map(|h| h.path.as_str().to_string())
            .collect(),
    );
    only(
        engine
            .search(&query, None, &CancellationToken::new())
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.path.as_str().to_string())
            .collect(),
    );
}
