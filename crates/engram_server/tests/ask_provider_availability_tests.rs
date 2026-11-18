#![cfg(feature = "vector")]
//! Natural-language evidence stays usable when optional semantic retrieval fails.
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

const GUIDE: &str = "Keep audit annotations. Exception: read-only previews do not submit.\n";
struct Fixture {
    app: Engram,
    pid: String,
    provider: Provider,
    owned: tempfile::TempDir,
}
impl Fixture {
    async fn new(semantic: bool) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("ask-").tempdir_in(parent)?;
        let provider = Provider::new().await?;
        let root = owned.path().join("project");
        for dir in ["site", "other", "schema"] {
            std::fs::create_dir_all(root.join(dir))?;
        }
        std::fs::write(root.join("AGENTS.md"), GUIDE)?;
        std::fs::write(
            root.join("site/order.vb"),
            "Public Class OrderActions\n Public Function CanSubmitOrder() As Boolean\n  Return True\n End Function\nEnd Class\n",
        )?;
        std::fs::write(
            root.join("other/order.vb"),
            "Public Class OtherActions\n Public Function CanSubmitOrder() As Boolean\n  Return False\n End Function\nEnd Class\n",
        )?;
        std::fs::write(
            root.join("site/order.ts"),
            "export function AuthorizationProbe() { return true; }\n",
        )?;
        std::fs::write(
            root.join("schema/order.sql"),
            "CREATE TABLE dbo.OrderAudit (AuthorizationProbe int NOT NULL);\n",
        )?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: if semantic { "ollama" } else { "fts_only" }.into(),
            embedding_model: Some("owned-ask-provider".into()),
            ollama_url: Some(provider.url.clone()),
            ollama_embed_dim: Some(4),
            embedding_request_timeout_secs: 2,
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state);
        let indexed = app.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned typed evidence availability","project_type":"dotnet_webforms_vb","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().unwrap().text;
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
            owned,
        })
    }
    async fn ask(&self, question: &str, deadline_ms: u64, format: &str) -> Result<String> {
        let response = self
            .app
            .handle_ask_codebase(serde_json::from_value(json!({
                "project_id":self.pid,"question":question,"output_format":format,
                "depth":"standard","deadline_ms":deadline_ms,"max_tokens":65536,
                "route_with_model":false,"synthesize":false
            }))?)
            .await?;
        ensure!(response.is_error != Some(true));
        Ok(response.content[0].as_text().unwrap().text.clone())
    }
    async fn close(self) -> Result<()> {
        let Self {
            app,
            mut provider,
            owned,
            ..
        } = self;
        drop(app);
        provider.close().await?;
        owned.close()?;
        Ok(())
    }
}
fn arm<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["provider"] == name)
        .unwrap()
}
fn has_fallback(value: &Value) -> bool {
    value.to_string().contains("lexical fallback")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_embeddings_preserve_scoped_vb_body_and_authored_guidance() -> Result<()> {
    let fixture = Fixture::new(true).await?;
    let result = async {
        let healthy: Value = serde_json::from_str(
            &fixture
                .ask(
                    "Find CanSubmitOrder in site/order.vb healthyflight",
                    8000,
                    "json",
                )
                .await?,
        )?;
        ensure!(arm(&healthy, "code")["status"] == "hit");
        ensure!(!has_fallback(arm(&healthy, "code")));
        fixture.provider.fail.store(1, Ordering::SeqCst);
        let report: Value = serde_json::from_str(
            &fixture
                .ask(
                    "Find CanSubmitOrder in site/order.vb offlineflight",
                    8000,
                    "json",
                )
                .await?,
        )?;
        ensure!(
            arm(&report, "code")["status"] == "hit" && has_fallback(arm(&report, "code")),
            "{report}"
        );
        let source = report["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == "source_code" && row["path"] == "site/order.vb")
            .unwrap();
        ensure!(source["content"].as_str().unwrap().contains("Return True"));
        ensure!(!source["content"].as_str().unwrap().contains("Return False"));
        ensure!(has_fallback(&source["warnings"]));
        ensure!(source["generation"] == report["snapshot"]["project_generation"]);
        ensure!(
            report["project_policy_sources"]
                .to_string()
                .contains("read-only previews do not submit")
        );
        let markdown = fixture
            .ask(
                "Find CanSubmitOrder in site/order.vb markdownflight",
                8000,
                "markdown",
            )
            .await?;
        ensure!(markdown.contains("Return True") && markdown.contains("lexical fallback"));
        Ok(())
    }
    .await;
    fixture.close().await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_semantics_preserve_the_sql_modality_and_question_path_scope() -> Result<()> {
    let fixture = Fixture::new(true).await?;
    let result = async {
        fixture.provider.fail.store(2, Ordering::SeqCst);
        let start = std::time::Instant::now();
        let report: Value = serde_json::from_str(
            &fixture
                .ask(
                    "Find SQL AuthorizationProbe definitions in schema/order.sql slowflight",
                    1000,
                    "json",
                )
                .await?,
        )?;
        ensure!(start.elapsed() < std::time::Duration::from_secs(3));
        let modality = arm(&report, "modality:sql");
        ensure!(
            modality["status"] == "hit"
                && modality["count"].as_u64().unwrap() > 0
                && has_fallback(modality),
            "{report}"
        );
        ensure!(report["evidence"].as_array().unwrap().iter().any(|row| {
            row["path"] == "schema/order.sql"
                && row["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("CREATE TABLE dbo.OrderAudit")
        }));
        ensure!(
            !report["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["kind"] == "source_code" && row["path"] == "site/order.ts")
        );
        Ok(())
    }
    .await;
    fixture.close().await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_lexical_fallback_does_not_hide_unavailable_semantic_coverage() -> Result<()> {
    let fixture = Fixture::new(true).await?;
    let result = async {
        fixture.provider.fail.store(2, Ordering::SeqCst);
        let report: Value =
            serde_json::from_str(&fixture.ask("NoMatchingEvidenceJson", 1000, "json").await?)?;
        let code = arm(&report, "code");
        ensure!(
            code["status"] == "timed_out" && code["count"] == 0 && has_fallback(code),
            "{report}"
        );
        let markdown = fixture
            .ask("NoMatchingEvidenceMarkdown", 1000, "markdown")
            .await?;
        ensure!(
            markdown.contains("lexical fallback")
                && markdown.contains("Semantic match coverage is unavailable")
        );
        Ok(())
    }
    .await;
    fixture.close().await?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_source_answers_keep_their_existing_evidence_without_provider_work() -> Result<()> {
    let fixture = Fixture::new(false).await?;
    let result = async {
        let report: Value = serde_json::from_str(
            &fixture
                .ask("Find AuthorizationProbe in site/order.ts", 1000, "json")
                .await?,
        )?;
        ensure!(
            arm(&report, "code")["status"] == "hit" && !has_fallback(arm(&report, "code")),
            "{report}"
        );
        ensure!(
            report["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["path"] == "site/order.ts")
        );
        ensure!(fixture.provider.inputs.lock().unwrap().is_empty());
        Ok(())
    }
    .await;
    fixture.close().await?;
    result
}
