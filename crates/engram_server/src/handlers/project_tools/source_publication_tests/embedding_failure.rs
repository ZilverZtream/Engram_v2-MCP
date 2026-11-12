use super::*;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

struct UnavailableProvider {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl UnavailableProvider {
    fn new() -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                        let mut request = [0u8; 8192];
                        let _ = stream.read(&mut request);
                        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            url,
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for UnavailableProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn body(result: CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| item.as_text())
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn commit_source(root: &Path, message: &str) -> anyhow::Result<()> {
    let repo = git2::Repository::open(root).or_else(|_| git2::Repository::init(root))?;
    let mut index = repo.index()?;
    index.add_path(Path::new("source.rs"))?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    let author = git2::Signature::now("Owned failure fixture", "fixture@example.invalid")?;
    repo.commit(Some("HEAD"), &author, &author, message, &tree, &parents)?;
    Ok(())
}

async fn reported(
    app: &Engram,
    pid: &str,
    wait: bool,
    result: CallToolResult,
    kind: &str,
) -> anyhow::Result<String> {
    if wait {
        return Ok(body(result));
    }
    finish_owned_jobs(app).await;
    let started = body(result);
    let job_id = started
        .lines()
        .find_map(|line| line.strip_prefix("job_id: "))
        .ok_or_else(|| anyhow::anyhow!("Missing job identity: {started}"))?;
    let job = app
        .state
        .registry
        .get_job(job_id)?
        .ok_or_else(|| anyhow::anyhow!("Missing {kind} job"))?;
    assert_eq!(job.kind, kind);
    assert_eq!(job.project_id.as_deref(), Some(pid));
    assert_eq!(
        job.status,
        if cfg!(feature = "vector") {
            "degraded"
        } else {
            "done"
        },
        "{}",
        job.message
    );
    Ok(job.message.clone())
}

fn check_warning(report: &str) {
    if cfg!(feature = "vector") {
        assert!(
            report.contains("1 chunks unembedded")
                && report.contains("repair_project(scope=\"vector_only\")"),
            "{report}"
        );
    } else {
        assert!(
            !report.contains("unembedded"),
            "Vector-disabled indexing must not report provider failure: {report}"
        );
    }
}

async fn check_source(
    app: &Engram,
    pid: &str,
    declaration: &str,
    authored: &str,
) -> anyhow::Result<()> {
    let generation = app.state.registry.active_source_generation(pid)?;
    let runtime = app
        .ensure_project_runtime(pid)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let view = runtime.search.pin_source_generation(pid, generation)?;
    let mut q = query(generation, declaration);
    q.project_id = pid.into();
    let hits = runtime.search.lexical_search(&q)?;
    assert_eq!(hits.len(), 1);
    let recovered = runtime
        .search
        .recover_selected_source_hit(pid, &view, &hits[0])?
        .unwrap();
    assert_eq!(recovered.2, authored);
    assert_eq!(
        app.state
            .graph
            .query_nodes(pid, None, Some(declaration), None, 10)?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn embedding_failure_source_handlers_publish_and_report_sync_and_background() -> anyhow::Result<()>
{
    for wait in [true, false] {
        let temp = tempfile::tempdir()?;
        // Runtime precedes all test owners and drains tasks before TempDir on
        // unwinding, including initial-index centrality warming.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let provider = UnavailableProvider::new()?;
        let result = runtime.block_on(async {
            let root = temp.path().join("repo"); std::fs::create_dir(&root)?;
            let original = "pub fn original_marker() -> u32 { 17 }\n";
            std::fs::write(root.join("source.rs"), original)?;
            commit_source(&root, "failure original source")?;
            let data = temp.path().join("data"); std::fs::create_dir(&data)?;
            let (state, _) = AppState::new(Config { data_dir: data, allowed_roots: vec![root.clone()],
                embedding_backend: "ollama".into(), ollama_url: Some(provider.url.clone()),
                ollama_embed_dim: Some(768), embedding_request_timeout_secs: 2,
                llm_backend: "none".into(), ..Default::default() })?;
            let app = Engram::new(state);
            let result = app.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned failure publication","project_type":"general","wait":wait}))?).await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let pid = app.state.registry.list_projects()?.into_iter().next().unwrap().project_id;
            check_warning(&reported(&app, &pid, wait, result, "index_project").await?);
            check_source(&app, &pid, "original_marker", original).await?;
            let changed = "pub fn changed_marker() -> u32 { 29 }\n";
            std::fs::write(root.join("source.rs"), changed)?;
            commit_source(&root, "failure changed source")?;
            let result = app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":!wait,"max_commits":0,"index_antipatterns":false}))?).await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            check_warning(&reported(&app, &pid, !wait, result, "update_project").await?);
            check_source(&app, &pid, "changed_marker", changed).await?;
            // update_project may already have ingested the tip. Explicitly
            // rewalk both authored commits so this operation's failed count
            // can be compared with the complete retained history corpus.
            let history_result = app.handle_index_git_history(serde_json::from_value(json!({"project_id":pid,"wait":wait,"max_commits":2,"force":true,"index_antipatterns":false}))?).await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let history_report = reported(&app, &pid, wait, history_result, "index_git_history").await?;
            let ps = app.ensure_project_runtime(&pid).await.map_err(|error| anyhow::anyhow!("{error}"))?;
            let history_docs = ps.search.list_docs_in_namespace(&pid, "history")?;
            assert!(!history_docs.is_empty());
            if cfg!(feature = "vector") {
                assert!(history_report.contains(&format!("unembedded_docs: {}", history_docs.len())) && history_report.contains("vector_only"), "{history_report}");
            }
            drop(ps);
            // A new metadata-only commit must report degraded history while
            // preserving the unchanged source head, including background jobs.
            commit_source(&root, "failure history-only refresh")?;
            let generation = app.state.registry.active_source_generation(&pid)?;
            let refresh_result = app.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":wait,"max_commits":1,"index_antipatterns":false}))?).await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let refresh_report = reported(&app, &pid, wait, refresh_result, "update_project").await?;
            assert_eq!(app.state.registry.active_source_generation(&pid)?, generation);
            if cfg!(feature = "vector") {
                assert!(refresh_report.contains("unembedded_docs: 1") && refresh_report.contains("degraded"), "{refresh_report}");
            }
            let result = app.handle_repair_project(serde_json::from_value(json!({"project_id":pid,"scope":"full","max_commits":0}))?).await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            check_warning(&body(result));
            check_source(&app, &pid, "changed_marker", changed).await?;
            finish_owned_jobs(&app).await;
            drop(app);
            Ok::<_, anyhow::Error>(())
        });
        drop(provider);
        drop(runtime);
        temp.close()?;
        result?;
    }
    Ok(())
}
