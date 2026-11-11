//! S01: ordinary public file updates must inherit unrelated physical rows.
//! This checks logical stored-row identity and document-provider work, not SSD
//! writes or whether the storage engine rewrites a containing physical fragment.

use anyhow::{Context, Result, ensure};
use engram_core::{Config, build_pk};
use engram_index::{HybridQuery, HybridSearchEngine};
use engram_server::{AppState, Engram};
use rmcp::{handler::server::tool::Parameters, model::CallToolResult};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

const ORIGINAL: &str = "pub fn sone_original() -> u32 { 17 }\n";
const REPLACEMENT: &str = "pub fn sone_replacement_changed() -> u32 { 203 }\n";
const UNTOUCHED: &str = "pub fn sone_untouched() -> u32 { 31 }\n";
const MODEL: &str = "sone-plain-fixture"; // No automatic document/query prefix.
const INITIAL: u8 = 0;
const QUERY_BEFORE: u8 = 1;
const UPDATE: u8 = 2;
const QUERY_AFTER: u8 = 3;

#[derive(Clone, Debug, serde::Serialize)]
struct ObservedInput {
    phase: u8,
    input: String,
}

struct RecordingOllama {
    url: String,
    phase: Arc<AtomicU8>,
    inputs: Arc<Mutex<Vec<ObservedInput>>>,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

impl RecordingOllama {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let phase = Arc::new(AtomicU8::new(INITIAL));
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let stop = CancellationToken::new();
        let (record_phase, record_inputs, shutdown) = (phase.clone(), inputs.clone(), stop.clone());
        let task = tokio::spawn(async move {
            let mut requests = 0;
            loop {
                let (mut stream, _) = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    accepted = listener.accept() => accepted?,
                };
                requests += 1;
                ensure!(
                    requests <= 32,
                    "Unexpected embedding request multiplication"
                );
                tokio::time::timeout(
                    Duration::from_secs(5),
                    Self::respond(&mut stream, &record_phase, &record_inputs),
                )
                .await??;
            }
            Ok(())
        });
        Ok(Self {
            url,
            phase,
            inputs,
            stop,
            task: Some(task),
        })
    }

    async fn respond(
        stream: &mut TcpStream,
        phase: &AtomicU8,
        inputs: &Mutex<Vec<ObservedInput>>,
    ) -> Result<()> {
        let mut raw = Vec::new();
        let mut buffer = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).await?;
            ensure!(read > 0, "Incomplete mock HTTP header");
            raw.extend_from_slice(&buffer[..read]);
            if let Some(end) = raw.windows(4).position(|x| x == b"\r\n\r\n") {
                ensure!(end <= 8192);
                break end + 4;
            }
            ensure!(raw.len() <= 8192, "Mock HTTP header exceeded bound");
        };
        let header = std::str::from_utf8(&raw[..header_end])?;
        ensure!(header.starts_with("POST /api/embed HTTP/1.1\r\n"));
        let lengths = header
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(lengths.len() == 1 && lengths[0] <= 64 * 1024);
        let body_end = header_end + lengths[0];
        while raw.len() < body_end {
            let read = stream.read(&mut buffer).await?;
            ensure!(read > 0, "Incomplete mock HTTP body");
            raw.extend_from_slice(&buffer[..read]);
            ensure!(raw.len() <= body_end, "Unexpected pipelined HTTP request");
        }
        let request: serde_json::Value = serde_json::from_slice(&raw[header_end..body_end])?;
        ensure!(request["model"] == MODEL);
        let texts = request["input"]
            .as_array()
            .context("Expected actual Ollama input array")?;
        ensure!(!texts.is_empty() && texts.len() <= 16);
        {
            let mut observed = inputs
                .lock()
                .map_err(|_| anyhow::anyhow!("Recording mutex poisoned"))?;
            ensure!(observed.len() + texts.len() <= 64);
            for text in texts {
                observed.push(ObservedInput {
                    phase: phase.load(Ordering::SeqCst),
                    input: text.as_str().context("Expected string input")?.to_owned(),
                });
            }
        }
        // Correct dimension and nonzero vectors; this fixture tests identity,
        // not model relevance. All selected source rows fit below query top_k.
        let body = json!({"embeddings":vec![vec![1.0f32, 0.0, 0.0, 0.0]; texts.len()]}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await?;
        Ok(())
    }

    fn recorded(&self, phase: u8) -> Result<Vec<String>> {
        Ok(self
            .inputs
            .lock()
            .map_err(|_| anyhow::anyhow!("Recording mutex poisoned"))?
            .iter()
            .filter(|row| row.phase == phase)
            .map(|row| row.input.clone())
            .collect())
    }

    async fn finish(&mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}

impl Drop for RecordingOllama {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        // The enclosing test drops its explicit runtime before the TempDir.
        // Thus unwind also drains task destruction before filesystem teardown.
    }
}

fn text(result: CallToolResult) -> Result<String> {
    ensure!(
        result.is_error != Some(true),
        "Public tool returned an error: {result:?}"
    );
    Ok(result
        .content
        .iter()
        .filter_map(|item| item.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn query(project: &str, generation: u64, term: &str) -> HybridQuery {
    HybridQuery {
        project_id: project.into(),
        namespace: "memory".into(),
        generation,
        text: term.into(),
        top_k: 20,
        fts_mode: "loose".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    }
}

fn unchanged_row(
    index: &HybridSearchEngine,
    project: &str,
    selected: u64,
    original_generation: u64,
) -> Result<(String, String)> {
    let hits = index.lexical_search(&query(project, selected, "sone_untouched"))?;
    ensure!(hits.len() == 1 && hits[0].path.as_str() == "untouched.rs");
    let hit = &hits[0];
    ensure!(
        hit.pk == build_pk(project, "memory", original_generation, &hit.doc_id),
        "Unchanged content must retain its physical generation and PK"
    );
    let stored = index
        .get_doc_by_pk(&hit.pk)?
        .context("Missing unchanged physical row")?;
    ensure!(
        stored.0.as_str() == "untouched.rs"
            && stored.2.as_bytes() == UNTOUCHED.as_bytes()
            && stored.3 == 1
            && stored.4 == 1,
        "Complete untouched source bytes/range changed"
    );
    // This inventory is physical/unfiltered, not the selected logical view.
    let all = index.list_docs_in_namespace(project, "memory")?;
    let siblings: Vec<_> = all
        .iter()
        .filter(|row| row.path == "untouched.rs")
        .collect();
    ensure!(
        siblings.len() == 1 && siblings[0].doc_id == hit.doc_id,
        "A replacement/duplicate unchanged row must not hide outside the selected view"
    );
    Ok((hit.pk.clone(), hit.doc_id.clone()))
}

async fn public_edit(temporary: &Path, recorder: &RecordingOllama) -> Result<()> {
    let root = temporary.join("repo");
    std::fs::create_dir(&root)?;
    // Stop discovery at the owned root instead of finding outer Engram history.
    drop(git2::Repository::init(&root)?);
    std::fs::write(root.join("changed.rs"), ORIGINAL)?;
    std::fs::write(root.join("untouched.rs"), UNTOUCHED)?;
    let cfg = Config {
        data_dir: temporary.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: if cfg!(feature = "vector") {
            "ollama"
        } else {
            "fts_only"
        }
        .into(),
        embedding_model: Some(MODEL.into()),
        ollama_url: Some(recorder.url.clone()),
        ollama_embed_dim: Some(4),
        embedding_request_timeout_secs: 3,
        llm_backend: "none".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _events) = AppState::new(cfg)?;
    let engram = Engram::new(state.clone());
    let outcome: Result<()> = async {
        let indexed = text(engram.index_project(Parameters(serde_json::from_value(json!({
            "directory":root,"project_name":"S01 public file edit","project_type":"rust","wait":true
        }))?)).await?)?;
        let project = indexed.split("project_id: ").nth(1).and_then(|rest| rest.lines().next())
            .context("Public initial index omitted project identity")?.trim().to_owned();
        let original_generation = state.registry.active_source_generation(&project)?;
        let ps = state.get_project_cached(&project).context("Missing indexed runtime")?;
        let index = &ps.search;
        // Keep the initial population stable for the physical-row delta: a
        // concurrent collection must not remove the old edited row mid-check.
        let _retained_original = index.pin_source_generation(&project, original_generation)?;
        ensure!(index.list_docs_in_namespace(&project, "memory")?.len() == 2);
        let original_sibling = unchanged_row(index, &project, original_generation, original_generation)?;
        let initial_inputs = recorder.recorded(INITIAL)?;

        #[cfg(feature = "vector")]
        let vector_count_before = {
            // Public initial indexing also stores an Indexing Report in the
            // memory bank. Record that work separately from source embeddings.
            ensure!(initial_inputs.iter().all(|input|
                input == ORIGINAL || input == UNTOUCHED || input.starts_with("# Indexing Report\n")),
                "Unexpected initial provider input: {initial_inputs:?}");
            for source in [ORIGINAL, UNTOUCHED] {
                ensure!(initial_inputs.iter().filter(|input| input.as_str() == source).count() == 1,
                    "Initial public indexing must actually embed each source once: {initial_inputs:?}");
            }
            recorder.phase.store(QUERY_BEFORE, Ordering::SeqCst);
            let hits = index.vector_search(&query(&project, original_generation, "query before public edit"), &CancellationToken::new()).await?;
            ensure!(hits.len() == 2 && hits.iter().any(|hit| hit.pk == original_sibling.0));
            ensure!(recorder.recorded(QUERY_BEFORE)? == ["query before public edit"],
                "Deliberate query work is measured separately from document inputs");
            ensure!(index.vector_generation_inventory(&project, Some("memory"), original_generation).await?
                == (2, BTreeSet::from(["changed.rs".into(), "untouched.rs".into()])));
            index.count_vectors(&project).await?
        };

        recorder.phase.store(UPDATE, Ordering::SeqCst);
        std::fs::write(root.join("changed.rs"), REPLACEMENT)?;
        ensure!(std::fs::read(root.join("untouched.rs"))? == UNTOUCHED.as_bytes());
        let updated = text(engram.update_project(Parameters(serde_json::from_value(json!({
            "project_id":project,"wait":true,"max_commits":0,"index_antipatterns":false
        }))?)).await?)?;
        ensure!(updated.contains("files=1"), "Only one real file changed: {updated}");
        let current = state.registry.active_source_generation(&project)?;
        ensure!(current == original_generation + 1);
        ensure!(unchanged_row(index, &project, current, original_generation)? == original_sibling);
        ensure!(index.get_doc_by_pk(&build_pk(&project, "memory", current, &original_sibling.1))?.is_none(),
            "No unchanged row may be copied into the new physical generation");
        let physical = index.list_docs_in_namespace(&project, "memory")?;
        ensure!(physical.len() == 3, "Exactly one new source chunk should be physically added: {physical:?}");
        let current_changed = index.lexical_search(&query(&project, current, "sone_replacement_changed"))?;
        ensure!(current_changed.len() == 1 && current_changed[0].path.as_str() == "changed.rs");
        let changed_pk = &current_changed[0].pk;
        ensure!(*changed_pk == build_pk(&project, "memory", current, &current_changed[0].doc_id));
        ensure!(index.get_doc_by_pk(changed_pk)?.context("Missing new source body")?.2 == REPLACEMENT);
        ensure!(index.lexical_search(&query(&project, current, "sone_original"))?.is_empty());
        let update_inputs = recorder.recorded(UPDATE)?;

        #[cfg(feature = "vector")]
        {
            ensure!(update_inputs == [REPLACEMENT],
                "Public update must embed only the changed document: {update_inputs:?}");
            ensure!(index.count_vectors(&project).await? == vector_count_before + 1,
                "No replacement copies of unrelated physical vectors may be added");
            ensure!(index.vector_generation_inventory(&project, Some("memory"), current).await?
                == (2, BTreeSet::from(["changed.rs".into(), "untouched.rs".into()])));
            recorder.phase.store(QUERY_AFTER, Ordering::SeqCst);
            let hits = index.vector_search(&query(&project, current, "query after public edit"), &CancellationToken::new()).await?;
            ensure!(hits.len() == 2 && hits.iter().map(|hit| hit.pk.clone()).collect::<BTreeSet<_>>()
                == BTreeSet::from([original_sibling.0.clone(), changed_pk.clone()]),
                "Native selected vector PKs must retain the old sibling and only the new changed row");
            ensure!(recorder.recorded(QUERY_AFTER)? == ["query after public edit"]);
        }
        #[cfg(not(feature = "vector"))]
        {
            ensure!(initial_inputs.is_empty() && update_inputs.is_empty());
            ensure!(index.document_embedding_identity().is_none());
            ensure!(index.count_vectors(&project).await? == 0);
            ensure!(index.vector_generation_inventory(&project, Some("memory"), current).await? == (0, BTreeSet::new()));
            ensure!(recorder.inputs.lock().map_err(|_| anyhow::anyhow!("Recording mutex poisoned"))?.is_empty(),
                "No-vector public indexing/update must make no loopback embedding requests");
        }
        eprintln!("S01_PUBLIC_UPDATE {}", json!({"native_vectors":cfg!(feature="vector"),
            "original_generation":original_generation,"published_generation":current,
            "unchanged_physical_pk":original_sibling.0,"changed_physical_pk":changed_pk,
            "source_physical_rows_before":2,"source_physical_rows_after":physical.len(),
            "initial_document_phase_inputs":initial_inputs,"update_document_phase_inputs":update_inputs,
            "query_before_inputs":recorder.recorded(QUERY_BEFORE)?,"query_after_inputs":recorder.recorded(QUERY_AFTER)?}));
        Ok(())
    }.await;
    // Join any work owned by public handlers before dropping database owners,
    // even when an assertion returned an error. Runtime teardown covers unwind.
    loop {
        let jobs: Vec<_> = state
            .active_jobs
            .write()
            .await
            .drain()
            .map(|(_, task)| task)
            .collect();
        if jobs.is_empty() {
            break;
        }
        for task in jobs {
            task.await?;
        }
    }
    drop(engram);
    drop(state);
    outcome
}

#[test]
fn public_one_file_update_preserves_unrelated_physical_rows_and_embedding_work() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    // Declared after TempDir: runtime shutdown also precedes fixture destruction
    // if setup or an assertion unwinds. No detached provider task outlives it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            let mut recorder = RecordingOllama::start().await?;
            let result = public_edit(temporary.path(), &recorder).await;
            let shutdown = recorder.finish().await;
            result.and(shutdown)
        })
    }));
    drop(runtime);
    temporary.close()?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
