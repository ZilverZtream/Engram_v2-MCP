#![cfg(feature = "vector")]
//! A scoped history lookup must not embed changes the caller excluded.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
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
                        let length: usize = header.lines().find_map(|l| l.strip_prefix("content-length:"))
                            .ok_or_else(|| anyhow::anyhow!("missing content length"))?.trim().parse()?;
                        if request.len() >= end + 4 + length {
                            break serde_json::from_slice::<Value>(&request[end+4..end+4+length])?;
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
                if mode == 1 || (mode == 2 && texts.iter().any(|s| s.starts_with("search_query: "))) {
                    socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                    continue;
                }
                let body = json!({"embeddings":vec![vec![1.0,0.0];texts.len()]}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await?;
            }
        });
        Ok(Self {url, inputs, fail, stop, worker:Some(worker)})
    }

    async fn close(&mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() { worker.await??; }
        Ok(())
    }
}

async fn provider_failure_preserves_usable_history(mode: usize) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let mut provider = Provider::new().await?;
    provider.fail.store(mode, Ordering::SeqCst);
    let result = async {
        let root = temporary.path().join("repo");
        let repo = git2::Repository::init(&root)?;
        let path = "Site/App_Code/PhotoRules.vb";
        let base = commit(&repo, &[path], "Initial photo rules", "Fixture Author", 100)?;
        let selected = commit(&repo, &[path], "Require minimum uploaded photos", "Fixture Author", 200)?;
        commit(&repo, &["Site/other.vb"], "Require minimum photos elsewhere", "Fixture Author", 200)?;
        commit(&repo, &[path], "Require minimum photos by other author", "Other", 200)?;
        commit(&repo, &[path, "Site/Q/generated.js"], "Require minimum photos excluded", "Fixture Author", 200)?;
        let cutoff = commit(&repo, &[path], "Require minimum photos later date", "Fixture Author", 201)?;
        commit(&repo, &[path], "Require minimum photos after revision", "Fixture Author", 200)?;
        let (state, _) = AppState::new(Config {
            data_dir:temporary.path().join("data"), allowed_roots:vec![root.clone()],
            embedding_backend:"ollama".into(), embedding_model:Some("nomic-fixture".into()),
            ollama_url:Some(provider.url.clone()), ollama_embed_dim:Some(2), llm_backend:"none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id:"outage-history".into(), project_name:"WebForms history fixture".into(),
            project_type:"dotnet_webforms_vb".into(), directory:root.to_string_lossy().into(),
            created_at_ms:1, updated_at_ms:1, reindex_required_since_ms:None,
        })?;
        state.registry.set_meta("outage-history","active_generation","1")?;
        let engram = Engram::new(state);
        let request = json!({"project_id":"outage-history","query":"minimum uploaded photos",
            "mode":"precedent","file_filter":path,"exclude_paths":["Site/Q/"],
            "author_filter":"Fixture Author","date_after":200,"date_before":201,"as_of_rev":cutoff,
            "rerank":false,"limit":10,"output_json":true});
        let degraded = search(&engram, request.clone()).await?;
        ensure!(degraded["results"].as_array().unwrap().len()==1 && degraded["results"][0]["commit"]==selected,"{degraded}");
        ensure!(degraded.to_string().contains("lexical fallback (semantic retrieval unavailable)"),"{degraded}");
        let inputs = provider.inputs.lock().unwrap().clone();
        ensure!(inputs.iter().any(|s|s.starts_with("search_document: ")));
        if mode == 2 { ensure!(inputs.iter().any(|s|s.starts_with("search_query: "))); }
        let mut historical = request.clone(); historical["as_of_rev"]=json!(base);
        ensure!(search(&engram,historical).await?["results"].as_array().unwrap().is_empty());
        let mut text_request = request.clone(); text_request["output_json"]=json!(false);
        let text = engram.handle_search_history(serde_json::from_value(text_request)?).await?;
        let text = &text.content[0].as_text().unwrap().text;
        ensure!(text.contains(&selected) && text.contains("lexical fallback (semantic retrieval unavailable)"),"{text}");
        provider.fail.store(0, Ordering::SeqCst);
        let recovered = search(&engram, request).await?;
        ensure!(recovered["results"][0]["commit"]==selected,"{recovered}");
        ensure!(!recovered.to_string().contains("lexical fallback"),"{recovered}");
        Ok(())
    }.await;
    let closed = provider.close().await;
    temporary.close()?;
    closed?;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_history_survives_document_embedding_outage() -> Result<()> {
    provider_failure_preserves_usable_history(1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_history_survives_query_embedding_outage() -> Result<()> {
    provider_failure_preserves_usable_history(2).await
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() { worker.abort(); }
    }
}

fn commit(repo: &git2::Repository, files: &[&str], title: &str, author: &str, when: i64) -> Result<String> {
    let mut index = repo.index()?;
    for path in files {
        let target = repo.workdir().unwrap().join(path);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(&target, format!("' {title}\n"))?;
        index.add_path(std::path::Path::new(path))?;
    }
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let parent: Vec<_> = repo.head().ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
    let parents: Vec<_> = parent.iter().collect();
    let signature = git2::Signature::new(author,"fixture@example.invalid",&git2::Time::new(when,0))?;
    Ok(repo.commit(Some("HEAD"),&signature,&signature,title,&tree,&parents)?.to_string())
}

async fn search(engram: &Engram, request: Value) -> Result<Value> {
    let result = engram.handle_search_history(serde_json::from_value(request)?).await?;
    Ok(serde_json::from_str(&result.content[0].as_text().unwrap().text)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_history_embeds_only_eligible_changes_and_reuses_filters_on_warm_cache() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let mut provider = Provider::new().await?;
    let result = async {
        let root = temporary.path().join("repo");
        let repo = git2::Repository::init(&root)?;
        let path = "Site/App_Code/FileLayerZoomPolicy.vb";
        let base = commit(&repo,&[path],"Early map policy","Fixture Author",100)?;
        let selected = commit(&repo,&[path],"Selected viewport override","Fixture Author",200)?;
        commit(&repo,&["Site/other.vb"],"Different file","Fixture Author",200)?;
        let other = commit(&repo,&[path],"Different author viewport","Other",200)?;
        commit(&repo,&[path,"Site/Q/generated.js"],"Excluded companion","Fixture Author",200)?;
        let cutoff = commit(&repo,&[path],"Exclusive end boundary","Fixture Author",201)?;
        commit(&repo,&[path],"Future change with earlier timestamp","Fixture Author",200)?;
        let (state, _) = AppState::new(Config {
            data_dir:temporary.path().join("data"),allowed_roots:vec![root.clone()],
            embedding_backend:"ollama".into(),embedding_model:Some("nomic-fixture".into()),
            ollama_url:Some(provider.url.clone()),ollama_embed_dim:Some(2),llm_backend:"none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id:"filtered-history".into(),project_name:"WebForms history fixture".into(),
            project_type:"dotnet_webforms_vb".into(),directory:root.to_string_lossy().into(),
            created_at_ms:1,updated_at_ms:1,reindex_required_since_ms:None,
        })?;
        state.registry.set_meta("filtered-history","active_generation","1")?;
        let engram = Engram::new(state);
        let request = json!({"project_id":"filtered-history","query":"How did we implement viewport overrides for uploaded file layers?",
            "mode":"precedent","file_filter":"Site\\App_Code\\FileLayerZoomPolicy.vb","exclude_paths":["Site\\Q\\"],
            "author_filter":"Fixture Author","date_after":200,"date_before":201,"as_of_rev":cutoff,
            "rerank_timeout_ms":0,"limit":10,"output_json":true});
        let first = search(&engram,request.clone()).await?;
        ensure!(first["results"].as_array().unwrap().len()==1 && first["results"][0]["commit"]==selected,"{first}");
        let documents:Vec<_> = provider.inputs.lock().unwrap().iter().filter(|s|s.starts_with("search_document: ")).cloned().collect();
        ensure!(documents.len()==1 && documents[0].contains("Selected viewport override"),"Excluded changes reached embedding provider: {documents:?}");

        let mut changed = request.clone();changed["author_filter"]=json!("Other");
        let second = search(&engram,changed).await?;
        ensure!(second["results"].as_array().unwrap().len()==1 && second["results"][0]["commit"]==other,"{second}");
        let documents:Vec<_> = provider.inputs.lock().unwrap().iter().filter(|s|s.starts_with("search_document: ")).cloned().collect();
        ensure!(documents.len()==2 && documents[1].contains("Different author viewport"),"{documents:?}");
        let warm = search(&engram,request.clone()).await?;
        ensure!(warm["results"]==first["results"],"Warm cache changed eligible results");
        let before_empty = provider.inputs.lock().unwrap().len();
        let mut empty = request.clone();empty["file_filter"]=json!("absent/");empty["query"]=json!("Uncached empty selection question");
        ensure!(search(&engram,empty).await?["results"].as_array().unwrap().is_empty());
        let mut historical = request;historical["as_of_rev"]=json!(base);historical["query"]=json!("Another uncached historical question");
        ensure!(search(&engram,historical).await?["results"].as_array().unwrap().is_empty());
        ensure!(provider.inputs.lock().unwrap().len()==before_empty,"Empty selection contacted embedding provider");
        Ok(())
    }.await;
    let closed = provider.close().await;
    temporary.close()?;
    closed?;
    result
}
