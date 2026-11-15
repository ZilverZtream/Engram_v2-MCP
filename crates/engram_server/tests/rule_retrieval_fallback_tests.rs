//! Useful local rule evidence must survive an unavailable embedding provider.
use engram_core::{Config, ContentHash, RelPath};
use engram_index::{HybridQuery, HybridSearchEngine, IndexDoc};
use engram_server::services::rule_retrieval;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

struct Provider(tokio::task::JoinHandle<()>);
impl Drop for Provider {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn provider() -> (Provider, String, Arc<AtomicBool>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let unavailable = Arc::new(AtomicBool::new(false));
    let fail = unavailable.clone();
    let worker = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let len: usize = headers
                        .lines()
                        .find_map(|s| s.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let (status, body) = if fail.load(Ordering::SeqCst) {
                (503, "{}".to_owned())
            } else {
                let end = request.windows(4).position(|s| s == b"\r\n\r\n").unwrap();
                let body: serde_json::Value = serde_json::from_slice(&request[end + 4..]).unwrap();
                let count = body["input"].as_array().map_or(1, Vec::len);
                (
                    200,
                    serde_json::json!({"embeddings": vec![vec![1.0, 0.0]; count]}).to_string(),
                )
            };
            socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (Provider(worker), url, unavailable)
}

fn query(namespace: &str) -> HybridQuery {
    HybridQuery {
        project_id: "owned".into(),
        namespace: namespace.into(),
        generation: 0,
        text: "invoiceexclusion".into(),
        top_k: 1,
        fts_mode: "loose".into(),
        include_path_prefixes: Some(vec!["selected/".into()]),
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: true,
    }
}

fn doc(id: &str, path: &str, namespace: &str) -> IndexDoc {
    let content = format!("invoiceexclusion {id}: preserve reviewed exceptions");
    IndexDoc {
        generation: 0,
        chunk_id: 1,
        doc_id: id.into(),
        path: RelPath::new(path),
        language: "markdown".into(),
        content_hash: ContentHash::compute(content.as_bytes()).0,
        content,
        namespace: namespace.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
    }
}

#[tokio::test]
async fn outage_preserves_project_namespace_filters_and_limit_then_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let (provider, url, unavailable) = provider().await;
    let cfg = Config {
        embedding_backend: "ollama".into(),
        embedding_model: Some("fixture".into()),
        ollama_url: Some(url),
        ollama_embed_dim: Some(2),
        ..Default::default()
    };
    let engine = Arc::new(
        HybridSearchEngine::new(temp.path().join("fts"), temp.path().join("vectors"), &cfg)
            .await
            .unwrap(),
    );
    let cancel = CancellationToken::new();
    for docs in [
        vec![doc("business", "selected/Invoice.md", "business_logic")],
        vec![doc("quality", "selected/Policy.md", "quality_gate"),
             doc("wrongpath", "excluded/Policy.md", "quality_gate")],
        vec![doc("wrongnamespace", "selected/Other.md", "history")],
    ] {
        engine.index_docs("owned", &docs, &cancel).await.unwrap();
    }
    engine
        .index_docs(
            "other",
            &[doc("wrongproject", "selected/Policy.md", "quality_gate")],
            &cancel,
        )
        .await
        .unwrap();
    unavailable.store(true, Ordering::SeqCst);
    for (namespace, id) in [("business_logic", "business"), ("quality_gate", "quality")] {
        let result = rule_retrieval::search(&engine, &query(namespace), &cancel)
            .await
            .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].doc_id, id);
        assert!(result.note.contains("lexical fallback"));
        assert!(result.note.contains("semantic matches may be missing"));
    }
    unavailable.store(false, Ordering::SeqCst);
    let recovered = rule_retrieval::search(&engine, &query("business_logic"), &cancel)
        .await
        .unwrap();
    assert_eq!(recovered.hits[0].doc_id, "business");
    assert!(recovered.note.is_empty());
    drop(engine);
    provider.0.abort();
    // Await cancellation so the listening socket closes before fixture teardown.
    while !provider.0.is_finished() {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn cancellation_is_not_reported_as_successful_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Config {
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let engine = Arc::new(
        HybridSearchEngine::new(temp.path().join("fts"), temp.path().join("vectors"), &cfg)
            .await
            .unwrap(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = rule_retrieval::search(&engine, &query("quality_gate"), &cancel).await;
    assert!(result.err().unwrap().to_string().contains("cancelled"));
}
