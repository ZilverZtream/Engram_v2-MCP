#![cfg(feature = "vector")]
//! Optional semantic outages must not discard matching source or knowledge.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

struct Provider {
    url: String,
    inputs: Arc<Mutex<Vec<String>>>,
    fail: Arc<AtomicUsize>,
    stop: CancellationToken,
    worker: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl Provider {
    async fn new() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicUsize::new(0));
        let failure_mode = fail.clone();
        let stop = CancellationToken::new();
        let (captured, shutdown) = (inputs.clone(), stop.clone());
        let worker = tokio::spawn(async move {
            loop {
                let mut socket = tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    accepted = listener.accept() => accepted?.0,
                };
                let mut request = Vec::new();
                let body = loop {
                    let mut buffer = [0; 4096];
                    let n = tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        read = socket.read(&mut buffer) => read?,
                    };
                    ensure!(n > 0, "incomplete HTTP request");
                    request.extend_from_slice(&buffer[..n]);
                    ensure!(request.len() < 128 * 1024);
                    if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length: usize = header
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .ok_or_else(|| anyhow::anyhow!("missing content length"))?
                            .trim()
                            .parse()?;
                        if request.len() >= end + 4 + length {
                            break serde_json::from_slice::<Value>(
                                &request[end + 4..end + 4 + length],
                            )?;
                        }
                    }
                };
                let texts: Vec<String> = match &body["input"] {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a.iter().map(|s| s.as_str().unwrap().to_owned()).collect(),
                    _ => anyhow::bail!("unexpected embedding input"),
                };
                captured.lock().unwrap().extend(texts.iter().cloned());
                let mode = failure_mode.load(Ordering::SeqCst);
                if mode == 2 {
                    tokio::select! {
                        _ = shutdown.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {},
                    }
                }
                if mode == 1 {
                    socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                    continue;
                }
                let body =
                    json!({"embeddings":vec![vec![1.0,0.0,0.0,0.0];texts.len()]}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await?;
            }
        });
        Ok(Self {
            url,
            inputs,
            fail,
            stop,
            worker: Some(worker),
        })
    }

    async fn close(&mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            worker.await??;
        }
        Ok(())
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

struct Fixture {
    app: Engram,
    pid: String,
    provider: Provider,
    temporary: tempfile::TempDir,
}
impl Fixture {
    async fn new(budget_ms: u64) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temporary = tempfile::Builder::new()
            .prefix("hybrid-")
            .tempdir_in(parent)?;
        let provider = Provider::new().await?;
        let root = temporary.path().join("project");
        std::fs::create_dir_all(root.join("Site"))?;
        let source = "export function AuthorizationProbe() {\n // Create order after authorization.\n return true;\n}\n";
        std::fs::write(root.join("Site/Order.ts"), source)?;
        std::fs::write(root.join("Outside.ts"), source)?;
        std::fs::write(
            root.join("AGENTS.md"),
            "Preserve authorization. Exception: read-only previews do not submit.\n",
        )?;
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "ollama".into(),
            embedding_model: Some("owned-query-fixture".into()),
            ollama_url: Some(provider.url.clone()),
            ollama_embed_dim: Some(4),
            llm_backend: "none".into(),
            vector_search_timeout_ms: budget_ms,
            embedding_request_timeout_secs: 2,
            ..Default::default()
        })?;
        let app = Engram::new(state);
        let result = app.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned hybrid query","project_type":"general","wait":true
        }))?).await?;
        let text = &result.content[0].as_text().unwrap().text;
        let pid = text
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            app,
            pid,
            provider,
            temporary,
        })
    }
    async fn query(&self, query: &str, extra: Value) -> Result<rmcp::model::CallToolResult> {
        let mut request = json!({"project_id":self.pid,"query":query,"fts_mode":"loose",
            "use_mmr":false,"output_json":true,"include_user_memory":false});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Ok(self
            .app
            .handle_search_memory(serde_json::from_value(request)?)
            .await?)
    }
    async fn close(self) -> Result<()> {
        let Self {
            app,
            mut provider,
            temporary,
            ..
        } = self;
        drop(app);
        provider.close().await?;
        temporary.close()?;
        Ok(())
    }
}
fn parsed(result: &rmcp::model::CallToolResult) -> Result<Value> {
    Ok(serde_json::from_str(
        &result.content[0].as_text().unwrap().text,
    )?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_failure_keeps_filtered_source_and_guidance_without_claiming_semantic_ranking()
-> Result<()> {
    let fixture = Fixture::new(5000).await?;
    let result = async {
        let filter = json!({"include_path_prefixes":["Site/"]});
        let fast = parsed(
            &fixture
                .query("AuthorizationProbe fast", filter.clone())
                .await?,
        )?;
        ensure!(fast["semantic_quality"] == "semantic");
        ensure!(fast["retrieval"]["namespaces"][0]["status"] == "hybrid_completed");
        fixture.provider.fail.store(1, Ordering::SeqCst);
        let fallback = parsed(
            &fixture
                .query("AuthorizationProbe provideroffline", filter.clone())
                .await?,
        )?;
        ensure!(
            !fallback["results"].as_array().unwrap().is_empty(),
            "{fallback}"
        );
        ensure!(
            fallback["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["path"] == "Site/Order.ts")
        );
        ensure!(fallback["semantic_quality"] == "not_used");
        ensure!(fallback["retrieval"]["configured_semantic_quality"] == "semantic");
        ensure!(
            fallback["retrieval"]["namespaces"][0]["status"]
                == "lexical_fallback_provider_unavailable",
            "{fallback}"
        );
        ensure!(
            fallback["warnings"]
                .to_string()
                .contains("lexical fallback")
        );
        ensure!(
            fallback["guidance"]
                .to_string()
                .contains("read-only previews do not submit")
        );
        let text = fixture
            .query(
                "AuthorizationProbe markdownoffline",
                json!({"include_path_prefixes":["Site/"],"output_json":false}),
            )
            .await?;
        let text = &text.content[0].as_text().unwrap().text;
        ensure!(text.contains("Site/Order.ts") && text.contains("lexical fallback"));
        let pure = fixture.app.handle_vector_search(serde_json::from_value(json!({
            "project_id":fixture.pid,"query":"AuthorizationProbe vectoroffline","output_json":true
        }))?).await;
        ensure!(
            pure.is_err(),
            "Explicit vector_search must not silently become lexical"
        );
        Ok(())
    }
    .await;
    fixture.close().await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_provider_uses_one_budget_for_code_and_knowledge_without_namespace_retry_fanout()
-> Result<()> {
    let fixture = Fixture::new(100).await?;
    let result = async {
        fixture.app.handle_update_memory_bank(serde_json::from_value(json!({
            "project_id":fixture.pid,"section_id":"authorization","section":"memory_bank:authorization",
            "content":"AuthorizationProbe knowledge preserves authorization for order creation."
        }))?).await?;
        let user_id = engram_core::namespaces::USER_PROJECT_ID;
        let mut user = fixture.app.state.registry.get_project(&fixture.pid)?.unwrap();
        user.project_id = user_id.into();
        user.project_name = "Owned standing memory".into();
        fixture.app.state.registry.put_project(&user)?;
        fixture.app.handle_update_memory_bank(serde_json::from_value(json!({
            "project_id":user_id,"section_id":"standing","section":"memory_bank:standing",
            "content":"AuthorizationProbe standing user guidance preserves authorization."
        }))?).await?;
        fixture.provider.fail.store(2,Ordering::SeqCst);
        let before = fixture.provider.inputs.lock().unwrap().len();
        let began = std::time::Instant::now();
        let fallback = parsed(&fixture.query("AuthorizationProbe slowquery",json!({"search_scope":"all","include_user_memory":true})).await?)?;
        ensure!(began.elapsed()<std::time::Duration::from_secs(2));
        let rows = fallback["results"].as_array().unwrap();
        ensure!(rows.iter().any(|row|row["path"]=="Site/Order.ts"),"{fallback}");
        ensure!(rows.iter().any(|row|row["path"]=="memory_bank:authorization"),"{fallback}");
        ensure!(rows.iter().any(|row| row["path"] == "memory_bank:standing"
            && row["namespace"] == "memory_bank" && row["source"] == "user:memory_bank"
            && row["project_id"] == user_id),"{fallback}");
        ensure!(fixture.provider.inputs.lock().unwrap().len()==before+1,"Repeated provider work across namespace fanout");
        ensure!(fallback["retrieval"]["namespaces"].as_array().unwrap().len()>1);
        ensure!(fallback["retrieval"]["namespaces"].as_array().unwrap().iter()
            .all(|row|row["status"]=="lexical_fallback_budget_exhausted"));
        ensure!(fallback["semantic_quality"]=="not_used");
        Ok(())
    }.await;
    fixture.close().await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_fallback_retains_coverage_warnings_in_json_and_markdown() -> Result<()> {
    let fixture = Fixture::new(5000).await?;
    let result = async {
        fixture.provider.fail.store(1, Ordering::SeqCst);
        let empty = parsed(&fixture.query("NoMatchingEvidenceJson", json!({})).await?)?;
        ensure!(empty["results"].as_array().unwrap().is_empty());
        ensure!(empty["semantic_quality"] == "not_used");
        ensure!(empty["warnings"].to_string().contains("lexical fallback"));
        let text = fixture
            .query("NoMatchingEvidenceMarkdown", json!({"output_json":false}))
            .await?;
        let text = &text.content[0].as_text().unwrap().text;
        ensure!(text.contains("no_hits") && text.contains("lexical fallback"));
        Ok(())
    }
    .await;
    fixture.close().await?;
    result
}
