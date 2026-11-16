//! Saved analysis delivery must not repeat provider calls or persisted documents.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram, read_report_delivery as delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

struct Provider {
    url: String,
    calls: Arc<AtomicUsize>,
    short: Arc<AtomicBool>,
    stop: CancellationToken,
    worker: Option<tokio::task::JoinHandle<Result<()>>>,
}
impl Provider {
    async fn new() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let calls = Arc::new(AtomicUsize::new(0));
        let short = Arc::new(AtomicBool::new(false));
        let stop = CancellationToken::new();
        let (counter, small, cancel) = (calls.clone(), short.clone(), stop.clone());
        let worker = tokio::spawn(async move {
            loop {
                let (mut socket, _) = tokio::select! { _=cancel.cancelled()=>return Ok(()), value=listener.accept()=>value? };
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = tokio::select! { _=cancel.cancelled()=>return Ok(()), value=socket.read(&mut buffer)=>value? };
                    ensure!(count > 0, "Incomplete owned provider request");
                    request.extend_from_slice(&buffer[..count]);
                    ensure!(request.len() < 1024 * 1024);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length: usize = header
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse()?;
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let answer = if small.load(Ordering::SeqCst) {
                    json!({"purpose":"Small owned summary","business_rules":[],"steps":[]})
                } else {
                    json!({"purpose":"Preserve the supplied review qualification and source ownership. ".repeat(55),
                        "business_rules":(0..16).map(|i|format!("IF the owned condition holds THEN retain its authored exception. {i}")).collect::<Vec<_>>(),
                        "steps":["Read the owned input.","Retain its qualifications."],
                        "side_effects_detail":"Source-only fixture output; execution and semantic correctness unverified."})
                };
                let body =
                    json!({"choices":[{"message":{"content":answer.to_string()}}]}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await?;
            }
        });
        Ok(Self {
            url,
            calls,
            short,
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
    server: Engram,
    provider: Provider,
    owned: tempfile::TempDir,
    root: std::path::PathBuf,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        std::fs::create_dir(&root)?;
        let source=format!("Public Class Rules\n{}End Class\n",(0..12).map(|i|format!("Public Function Rule_{i:02}(value As Integer) As Integer\nReturn value + {i}\nEnd Function\n")).collect::<String>());
        std::fs::write(root.join("Rules.vb"), source)?;
        let provider = Provider::new().await?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "openai".into(),
            llm_model: Some("owned-fixture".into()),
            llm_openai_api_key: Some("owned-fixture-key".into()),
            llm_openai_api_base: Some(provider.url.clone()),
            advertise_all_tools: true,
            watch_debounce_secs: 3600,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed=server.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned business report","project_type":"general","wait":true}))?).await?;
        let project = text(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            server,
            provider,
            owned,
            root,
            project,
        })
    }
    async fn close(self) -> Result<()> {
        let Self {
            server,
            mut provider,
            owned,
            ..
        } = self;
        drop(server);
        provider.close().await?;
        owned.close()?;
        Ok(())
    }
    fn docs(&self) -> Result<Vec<(String, String)>> {
        let search = &self
            .server
            .state
            .get_project_cached(&self.project)
            .unwrap()
            .search;
        let mut result = Vec::new();
        for doc in search.list_docs_in_namespace(&self.project, "business_logic")? {
            let content = search
                .get_doc_by_doc_id(&self.project, "business_logic", 0, &doc.doc_id)?
                .unwrap()
                .2;
            result.push((doc.doc_id, content));
        }
        result.sort();
        Ok(result)
    }
}
fn text(value: &CallToolResult) -> &str {
    &value.content[0].as_text().unwrap().text
}
fn prepared(project: &str, mut args: Value) -> Result<delivery::Report> {
    args["project_id"] = json!(project);
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":"analyze_business_logic","arguments":args}))?;
    Ok(delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_business_analyses_recover_exactly_without_regeneration_or_republication()
-> Result<()> {
    let mut f = Fixture::new().await?;
    let mut captures = Vec::new();
    for output_json in [true, false] {
        let original=f.server.handle_analyze_business_logic(serde_json::from_value(json!({"project_id":f.project,"file_path":"Rules.vb","output_json":output_json,"max_concurrent":1}))?).await?;
        ensure!(original.is_error != Some(true));
        ensure!(text(&original).len() > 40960);
        if output_json {
            let report: Value = serde_json::from_str(text(&original))?;
            let methods = report["methods"].as_array().unwrap();
            ensure!(methods.len() == 12);
            for i in 0..12 {
                ensure!(
                    methods
                        .iter()
                        .any(|m| m["method_name"] == format!("Rule_{i:02}")
                            && m["file_path"] == "Rules.vb"
                            && m["extraction_provenance"]["origin"] == "llm")
                );
            }
        }
        let default = prepared(&f.project, json!({"file_path":"Rules.vb"}))?
            .finish(&f.server.state.cfg.data_dir, original.clone())?;
        ensure!(text(&default).len() <= 40960);
        ensure!(
            serde_json::from_str::<Value>(text(&default))?["report_content"]
                == "DEFERRED_NOT_SUPPLIED"
        );
        let deferred = prepared(
            &f.project,
            json!({"file_path":"Rules.vb","max_tokens":1024}),
        )?
        .finish(&f.server.state.cfg.data_dir, original.clone())?;
        ensure!(text(&deferred).len() <= 4096);
        captures.push((
            serde_json::to_string(&original)?,
            serde_json::from_str::<Value>(text(&deferred))?,
        ));
    }
    // A later small reanalysis is published normally; recovery must not overwrite it.
    f.provider.short.store(true, Ordering::SeqCst);
    let small=f.server.handle_analyze_business_logic(serde_json::from_value(json!({"project_id":f.project,"file_path":"Rules.vb","method_name":"Rule_00","output_json":true}))?).await?;
    ensure!(small.is_error != Some(true) && text(&small).len() < 40960);
    let fitting = prepared(&f.project, json!({"file_path":"Rules.vb"}))?
        .finish(&f.server.state.cfg.data_dir, small.clone())?;
    ensure!(serde_json::to_string(&fitting)? == serde_json::to_string(&small)?);
    let docs = f.docs()?;
    ensure!(docs.len() == 12);
    ensure!(
        docs.iter()
            .any(|(_, body)| body.contains("Small owned summary"))
    );
    let calls = f.provider.calls.load(Ordering::SeqCst);
    f.provider.close().await?;
    std::fs::remove_file(f.root.join("Rules.vb"))?;
    for (exact, manifest) in captures {
        let mut action = manifest["response_delivery"]["next_call"].clone();
        let mut restored = String::new();
        for _ in 0..1000 {
            ensure!(action["arguments"].get("file_path").is_none());
            ensure!(action["arguments"].get("method_name").is_none());
            let page = prepared(&f.project, action["arguments"].clone())?
                .recover(&f.server.state.cfg.data_dir)?
                .unwrap();
            ensure!(text(&page).len() <= 4096);
            let page: Value = serde_json::from_str(text(&page))?;
            ensure!(page["start_byte"] == restored.len());
            restored.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            action = page["next_call"].clone();
        }
        ensure!(restored == exact);
        ensure!(
            blake3::hash(restored.as_bytes()).to_hex().as_str()
                == manifest["response_delivery"]["response_id"]
                    .as_str()
                    .unwrap()
        );
    }
    ensure!(f.provider.calls.load(Ordering::SeqCst) == calls);
    ensure!(f.docs()? == docs);
    f.close().await
}
