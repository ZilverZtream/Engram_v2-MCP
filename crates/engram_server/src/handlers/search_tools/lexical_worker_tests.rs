//! An actual queued lexical worker keeps source data after its RPC is cancelled.
use std::{collections::HashMap, sync::{atomic::{AtomicBool, Ordering}, mpsc, Arc, LazyLock, Mutex, Weak}, time::Duration};
use anyhow::{anyhow, ensure, Context, Result};
use crate::{state::AppState, tools::Engram};

const WAIT: Duration = Duration::from_secs(30);
static PROBES: LazyLock<Mutex<HashMap<String, Weak<Probe>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
struct Probe {
    claimed: AtomicBool,
    entered: mpsc::SyncSender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
    complete: mpsc::SyncSender<()>,
}
pub(super) struct Observation(Arc<Probe>);
pub(super) fn observe(project: &str) -> Option<Observation> {
    let probe = PROBES.lock().unwrap_or_else(|e| e.into_inner()).get(project)?.upgrade()?;
    probe.claimed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).ok()?;
    Some(Observation(probe))
}
impl Observation {
    pub(super) fn pause(&self) {
        let _ = self.0.entered.try_send(());
        let _ = self.0.resume.lock().unwrap_or_else(|e| e.into_inner()).recv_timeout(WAIT);
    }
}
impl Drop for Observation { fn drop(&mut self) { let _ = self.0.complete.try_send(()); } }

struct Registration {
    project: String,
    _probe: Arc<Probe>,
    resume: mpsc::SyncSender<()>,
}
impl Registration {
    fn new(project: &str) -> (Self, mpsc::Receiver<()>, mpsc::Receiver<()>) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let (complete_tx, complete_rx) = mpsc::sync_channel(1);
        let probe = Arc::new(Probe { claimed: AtomicBool::new(false), entered: entered_tx,
            resume: Mutex::new(resume_rx), complete: complete_tx });
        PROBES.lock().unwrap_or_else(|e| e.into_inner()).insert(project.into(), Arc::downgrade(&probe));
        (Self { project: project.into(), _probe: probe, resume: resume_tx }, entered_rx, complete_rx)
    }
    fn release(&self) { let _ = self.resume.try_send(()); }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.release();
        PROBES.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.project);
    }
}

fn document(generation: u64, id: &str, marker: &str) -> engram_index::IndexDoc {
    let text = format!("pub fn {marker}() -> usize {{ 7 }}");
    engram_index::IndexDoc { generation, chunk_id: generation,
        path: engram_core::RelPath::new("changed.rs"), language: "rust".into(),
        content_hash: engram_core::ContentHash::compute(text.as_bytes()).0,
        content: text, namespace: "memory".into(), author: None, timestamp: None,
        start_line: 1, end_line: 1, doc_id: id.into() }
}

#[test]
fn fts_search_keeps_selected_source_until_response() -> Result<()> { run_case(false) }
#[test]
fn cancelled_fts_search_keeps_source_until_worker_finishes() -> Result<()> { run_case(true) }

fn run_case(cancel_rpc: bool) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(&root)?;
        drop(git2::Repository::init(&root)?);
        let old = document(1, "old", "old_marker");
        std::fs::write(root.join("changed.rs"), &old.content)?;
        let cfg = engram_core::Config { data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()], embedding_backend: "fts_only".into(),
            llm_backend: "none".into(), ..Default::default() };
        std::fs::create_dir_all(&cfg.data_dir)?;
        let (state, _) = AppState::new(cfg)?;
        let project = format!("search-worker-{}", uuid::Uuid::new_v4());
        state.registry.put_project(&engram_core::ProjectRecord { project_id: project.clone(),
            project_name: "Search source lifetime".into(), project_type: "rust".into(),
            directory: root.to_string_lossy().into(), created_at_ms: 1, updated_at_ms: 1,
            reindex_required_since_ms: None })?;
        state.registry.set_meta(&project, "active_generation", "1")?;
        let engram = Arc::new(Engram::new(state.clone()));
        let ps = engram.ensure_project_runtime(&project).await.map_err(|e| anyhow!(e.to_string()))?;
        let cancel = tokio_util::sync::CancellationToken::new();
        ps.search.index_docs(&project, &[old], &cancel).await?;
        let (registration, entered_rx, complete_rx) = Registration::new(&project);
        let server = engram.clone();
        let pid = project.clone();
        let mut task = Some(tokio::spawn(async move {
            server.handle_search_memory(serde_json::from_value(serde_json::json!({
                "project_id":pid,"query":"old_marker","semantic":false,"include_content":true,
                "max_content_chars_per_result":4000,"max_results":3,"include_user_memory":false
            })).unwrap()).await
        }));
        let mut entered = false;
        let phase: Result<()> = async {
            entered_rx.recv_timeout(WAIT).context("FTS worker did not enter")?;
            entered = true;
            ps.search.prepare_source_snapshot(&project, 1, 2,
                &[engram_core::RelPath::new("changed.rs")], &[], &cancel).await?;
            let new = document(2, "new", "new_marker");
            std::fs::write(root.join("changed.rs"), &new.content)?;
            ps.search.index_docs(&project, &[new], &cancel).await?;
            ps.search.seal_source_snapshot(&project, 2)?;
            state.registry.set_meta(&project, "active_generation", "2")?;
            ps.search.purge_old_generations(&project, 2).await?;
            ensure!(ps.search.count_docs(&project)? == 2, "Worker lost old source");
            if cancel_rpc {
                let rpc = task.take().context("Missing search task")?;
                rpc.abort();
                let joined = rpc.await;
                ensure!(matches!(joined, Err(ref error) if error.is_cancelled()), "Search did not cancel");
                ps.search.purge_old_generations(&project, 2).await?;
                ensure!(ps.search.count_docs(&project)? == 2, "RPC cancellation released the worker's source");
            }
            Ok(())
        }.await;
        // Release/join before propagating any failure. The observation's
        // completion acknowledgement follows the worker's source lease drop.
        registration.release();
        let completed = if entered {
            complete_rx.recv_timeout(WAIT).context("FTS worker did not release its source lease")
        } else { Ok(()) };
        let reply = if let Some(mut rpc) = task.take() {
            if phase.is_err() || completed.is_err() { rpc.abort(); }
            Some(match tokio::time::timeout(WAIT, &mut rpc).await {
                Ok(joined) => joined.map_err(|e| anyhow!(e.to_string()))
                    .and_then(|reply| reply.map_err(|e| anyhow!(e.to_string()))),
                Err(_) => {
                    rpc.abort(); let _ = rpc.await;
                    Err(anyhow!("Search did not finish after worker release"))
                }
            })
        } else { None };
        let checks: Result<()> = async {
            phase?;
            completed?;
            if !cancel_rpc {
                let response = reply.context("Missing search response")??;
                let text = &response.content[0].as_text().context("Search response was not text")?.text;
                ensure!(text.contains("pub fn old_marker()"), "Old source missing");
                ensure!(!text.contains("new_marker"), "Search mixed generations");
            }
            ps.search.purge_old_generations(&project, 2).await?;
            ensure!(ps.search.count_docs(&project)? == 1, "Released old source was not collectible");
            Ok(())
        }.await;
        drop(registration); drop(ps); drop(engram); drop(state);
        checks
    });
    drop(runtime);
    match (result, temporary.close()) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Fixture cleanup also failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
