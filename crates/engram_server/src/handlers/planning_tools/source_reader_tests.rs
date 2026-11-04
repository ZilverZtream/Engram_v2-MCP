//! A planning response and its uncancellable source workers keep the selection
//! they started with while another update publishes and collection runs.
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::Duration;

pub(super) struct Observation {
    entered: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
    complete: mpsc::Sender<()>,
}
impl Observation {
    pub(super) fn pause(&self) {
        self.entered.send(()).expect("planning observer present");
        self.resume.recv_timeout(Duration::from_secs(30)).expect("planning worker released");
    }
}
impl Drop for Observation {
    fn drop(&mut self) { let _ = self.complete.send(()); }
}
type Observations = std::collections::HashMap<(String, &'static str), Observation>;
static OBSERVATIONS: LazyLock<Mutex<Observations>> = LazyLock::new(|| Mutex::new(Observations::new()));

pub(super) fn observe(project: &str, phase: &'static str) -> Option<Observation> {
    OBSERVATIONS.lock().unwrap().remove(&(project.to_owned(), phase))
}

fn document(generation: u64, id: &str, marker: &str) -> engram_index::IndexDoc {
    let text = format!("pub fn {marker}() -> usize {{ 7 }}");
    engram_index::IndexDoc { generation, chunk_id: generation,
        path: engram_core::RelPath::new("changed.rs"), language: "rust".into(),
        content_hash: engram_core::ContentHash::compute(text.as_bytes()).0,
        content: text, namespace: "memory".into(), author: None, timestamp: None,
        start_line: 1, end_line: 1, doc_id: id.into() }
}

fn exercise(phase: &'static str, abort: bool) -> anyhow::Result<()> {
    // Runtime is dropped before the owned directory on success or unwinding.
    let temp = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let outcome = runtime.block_on(async {
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root)?;
        drop(git2::Repository::init(&root)?);
        let old = document(1, "old", "old_marker");
        std::fs::write(root.join("changed.rs"), &old.content)?;
        let cfg = engram_core::Config { data_dir: temp.path().join("data"),
            allowed_roots: vec![root.clone()], embedding_backend: "fts_only".into(),
            llm_backend: "none".into(), ..Default::default() };
        std::fs::create_dir_all(&cfg.data_dir)?;
        let (state, _) = crate::state::AppState::new(cfg)?;
        let project = format!("planning-reader-{}", uuid::Uuid::new_v4());
        state.registry.put_project(&engram_core::ProjectRecord {
            project_id: project.clone(), project_name: "Planning source lifetime".into(),
            project_type: "rust".into(), directory: root.to_string_lossy().into(),
            created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None })?;
        state.registry.set_meta(&project, "active_generation", "1")?;
        let engram = Arc::new(crate::tools::Engram::new(state.clone()));
        let ps = engram.ensure_project_runtime(&project).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let cancel = tokio_util::sync::CancellationToken::new();
        ps.search.index_docs(&project, &[old], &cancel).await?;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (complete_tx, complete_rx) = mpsc::channel();
        OBSERVATIONS.lock().unwrap().insert((project.clone(), phase), Observation {
            entered: entered_tx, resume: resume_rx, complete: complete_tx });
        let server = engram.clone();
        let pid = project.clone();
        let mut rpc = Some(tokio::spawn(async move {
            if phase == "pattern_lexical" {
                server.handle_find_implementation_pattern(serde_json::from_value(serde_json::json!({
                    "project_id":pid,"pattern_query":"old_marker","output_json":true
                })).unwrap()).await
            } else {
                server.handle_get_concept_footprint(serde_json::from_value(serde_json::json!({
                    "project_id":pid,"concept":"old_marker"
                })).unwrap()).await
            }
        }));
        // Always release and join the worker before propagating failures.
        let observed = async {
            entered_rx.recv_timeout(Duration::from_secs(10))?;
            ps.search.prepare_source_snapshot(&project, 1, 2,
                &[engram_core::RelPath::new("changed.rs")], &[], &cancel).await?;
            let new = document(2, "new", "new_marker");
            std::fs::write(root.join("changed.rs"), &new.content)?;
            ps.search.index_docs(&project, &[new], &cancel).await?;
            ps.search.seal_source_snapshot(&project, 2)?;
            state.registry.set_meta(&project, "active_generation", "2")?;
            ps.search.purge_old_generations(&project, 2).await?;
            anyhow::ensure!(ps.search.count_docs(&project)? == 2,
                "Collection removed the request's source before its read");
            if abort {
                let task = rpc.take().unwrap();
                task.abort();
                anyhow::ensure!(task.await.is_err(), "RPC must be cancelled");
                ps.search.purge_old_generations(&project, 2).await?;
                anyhow::ensure!(ps.search.count_docs(&project)? == 2,
                    "RPC cancellation released a running source worker's selection");
            }
            anyhow::Ok(())
        }.await;
        let _ = resume_tx.send(());
        let completed = complete_rx.recv_timeout(Duration::from_secs(30));
        let response = if let Some(task) = rpc.take() {
            if observed.is_err() { task.abort(); let _ = task.await; None }
            else { Some(task.await) }
        } else { None };
        OBSERVATIONS.lock().unwrap().remove(&(project.clone(), phase));
        let collected = ps.search.purge_old_generations(&project, 2).await;
        let final_count = ps.search.count_docs(&project);
        drop(ps); drop(engram); drop(state);
        observed?;
        completed?; // Observation drops after the worker's owned lease.
        if let Some(response) = response {
            let response = response?.map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let text = &response.content[0].as_text().unwrap().text;
            anyhow::ensure!(text.contains("changed.rs"), "Pinned source disappeared: {text}");
            anyhow::ensure!(!text.contains("new_marker"), "Response mixed published source selections: {text}");
            if phase == "pattern_lexical" {
                anyhow::ensure!(text.contains("pub fn old_marker()"), "Old exemplar was lost: {text}");
            } else {
                anyhow::ensure!(text.contains("- lexical: complete (1 files from 1 hits")
                    && text.contains("- literal: complete (1 files from 1 matches"),
                    "Both source passes must read the retained original selection: {text}");
            }
        }
        collected?;
        anyhow::ensure!(final_count? == 1, "Completed planning worker retained obsolete source");
        anyhow::Ok(())
    });
    drop(runtime);
    let closed = temp.close();
    outcome?;
    closed?;
    Ok(())
}

#[test]
fn concept_lexical_read_survives_publication_and_collection() -> anyhow::Result<()> { exercise("concept_lexical", false) }
#[test]
fn concept_literal_read_survives_publication_and_collection() -> anyhow::Result<()> { exercise("concept_literal", false) }
#[test]
fn pattern_read_survives_publication_and_collection() -> anyhow::Result<()> { exercise("pattern_lexical", false) }
#[test]
fn cancelled_concept_lexical_worker_keeps_its_source() -> anyhow::Result<()> { exercise("concept_lexical", true) }
#[test]
fn cancelled_concept_literal_worker_keeps_its_source() -> anyhow::Result<()> { exercise("concept_literal", true) }
#[test]
fn cancelled_pattern_worker_keeps_its_source() -> anyhow::Result<()> { exercise("pattern_lexical", true) }
