#![cfg(feature = "vector")]
#![allow(clippy::unwrap_used)]

use engram_core::{Config, ProjectRecord, RelPath, registry::ReindexReason};
use engram_index::{HybridSearchEngine, IndexDoc};
use engram_server::{AppState, Engram, state::AppEvent, services::project_service};
use serde_json::json;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::Duration;
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::TcpListener, sync::{Notify, Semaphore}};
use tokio_util::sync::CancellationToken;

const PID: &str = "scoped-repair";

/// Local provider fixture. No external services, model downloads or project
/// copies; callers can pause the response at an actual HTTP request boundary.
struct Provider {
    url: String,
    started: Arc<Notify>,
    release: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
    stop: CancellationToken,
    worker: tokio::task::JoinHandle<()>,
}

impl Provider {
    async fn start(valid: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let (signal, gate, count, shutdown) = (started.clone(), release.clone(), calls.clone(), stop.clone());
        let worker = tokio::spawn(async move {
            loop {
                let (mut socket, _) = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    accepted = listener.accept() => accepted.unwrap(),
                };
                let mut request = Vec::new();
                let body = loop {
                    let mut buffer = [0u8; 4096];
                    let n = socket.read(&mut buffer).await.unwrap();
                    if n == 0 { return; }
                    request.extend_from_slice(&buffer[..n]);
                    assert!(request.len() <= 64 * 1024, "fixture request must remain bounded");
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]);
                        let length = header.lines().find_map(|line| line.split_once(':')
                            .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())).unwrap();
                        if request.len() >= end + 4 + length {
                            break serde_json::from_slice::<serde_json::Value>(&request[end + 4..end + 4 + length]).unwrap();
                        }
                    }
                };
                count.fetch_add(1, Ordering::SeqCst);
                signal.notify_one();
                let permit = gate.acquire().await.unwrap();
                permit.forget();
                let texts = body["input"].as_array().map_or(1, Vec::len);
                let vector = vec![0.25_f32; if valid { 1536 } else { 1 }];
                let data = (0..texts).map(|index| json!({"index":index,"embedding":vector})).collect::<Vec<_>>();
                let body = json!({"data":data}).to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                // A failed requesting client is itself part of the cancellation
                // scenario; close its socket and keep fixture teardown usable.
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Self { url, started, release, calls, stop, worker }
    }

    async fn close(self) {
        self.stop.cancel();
        self.release.add_permits(64);
        self.worker.await.unwrap();
    }
}

async fn fixture(configure: impl FnOnce(&mut Config), count: usize) -> (tempfile::TempDir, AppState) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source");
    std::fs::create_dir(&root).unwrap();
    let data = temp.path().join("data");
    let project = data.join("projects").join(PID);
    let seed = HybridSearchEngine::new(project.join("tantivy"), project.join("lancedb"),
        &Config { embedding_backend: "fts_only".into(), ..Default::default() }).await.unwrap();
    let docs = (0..count).map(|n| IndexDoc {
        generation: 7, chunk_id: n as u64, path: RelPath::new("a.rs"), language: "rust".into(),
        content: format!("Stored evidence {n}"), namespace: "memory".into(), author: None,
        timestamp: None, start_line: 1, end_line: 1, doc_id: format!("doc-{n}"), content_hash: format!("hash-{n}"),
    }).collect::<Vec<_>>();
    seed.index_docs(PID, &docs, &CancellationToken::new()).await.unwrap();
    drop(seed);
    let mut config = Config { data_dir: data, allowed_roots: vec![root.clone()],
        embedding_backend: "local".into(), ..Default::default() };
    configure(&mut config);
    let (state, _) = AppState::new(config).unwrap();
    state.registry.put_project(&ProjectRecord { project_id: PID.into(), project_name: "Repair fixture".into(),
        project_type: "general".into(), directory: root.to_string_lossy().into(), created_at_ms: 1,
        updated_at_ms: 1, reindex_required_since_ms: None }).unwrap();
    state.registry.set_meta(PID, "active_generation", "7").unwrap();
    (temp, state)
}

fn remote_config(config: &mut Config, url: &str) {
    config.embedding_backend = "openai".into();
    config.openai_api_base = Some(url.into());
    config.openai_api_key = Some("local-fixture-key".into());
    config.embedding_request_timeout_secs = 10;
}

#[tokio::test]
async fn disconnected_repair_retains_project_lock_until_native_completion_and_preserves_new_failures() {
    let provider = Provider::start(true).await;
    let (_temp, state) = fixture(|config| remote_config(config, &provider.url), 1).await;
    state.registry.require_reindex(PID, ReindexReason::Source, 30).unwrap();
    state.registry.require_reindex(PID, ReindexReason::Vector, 10).unwrap();
    let worker_state = state.clone();
    let request = tokio::spawn(async move {
        Engram::new(worker_state).handle_repair_project(serde_json::from_value(json!({
            "project_id":PID,"scope":"vector_only"
        })).unwrap()).await
    });
    tokio::time::timeout(Duration::from_secs(15), provider.started.notified()).await.unwrap();
    request.abort();
    let _ = request.await;
    let slot_held = state.active_indexing_count.load(Ordering::SeqCst) == 1;
    let project_lock_held = state.try_acquire_project_update_lock(PID).await.is_none();
    state.registry.require_reindex(PID, ReindexReason::Vector, 10).unwrap();
    provider.release.add_permits(8);
    // Reacquiring the service lock proves both native completion and durable
    // settlement ran, even though the original tool future has disappeared.
    let lock = tokio::time::timeout(Duration::from_secs(20), state.acquire_project_update_lock(PID)).await.unwrap();
    drop(lock);
    let slot_released = state.active_indexing_count.load(Ordering::SeqCst) == 0;
    let newer_pending = state.registry.reindex_requirement_token(PID, ReindexReason::Vector).unwrap().is_some();
    let runtime = state.get_project_cached(PID).unwrap();
    let rows = runtime.search.count_vectors(PID).await.unwrap();
    let head = state.registry.get_meta(PID, "active_generation").unwrap();
    let second = project_service::repair_project_scoped(&state, PID, "vector_only").await;
    let final_reasons = state.registry.reindex_reasons(PID).unwrap();
    let calls = provider.calls.load(Ordering::SeqCst);
    provider.close().await;
    assert!(slot_held && project_lock_held && slot_released);
    assert!(newer_pending, "a successful older repair cannot erase the newer failure");
    assert_eq!(rows, 1);
    assert_eq!(head.as_deref(), Some("7"));
    assert!(second.unwrap().contains("Recovered 1 vectors"));
    assert_eq!(final_reasons, vec![ReindexReason::Source]);
    assert_eq!(calls, 1, "the second repair can reuse the verified embedding cache");
}

#[tokio::test]
async fn failed_admitted_initialization_keeps_durable_warning_and_source_head() {
    let provider = Provider::start(false).await;
    let (_temp, state) = fixture(|config| remote_config(config, &provider.url), 1).await;
    provider.release.add_permits(8);
    let result = project_service::repair_project_scoped(&state, PID, "initialize_vectors").await;
    provider.close().await;
    assert!(result.is_err(), "wrong provider dimensions cannot initialize a usable table");
    assert_eq!(state.registry.reindex_reasons(PID).unwrap(), vec![ReindexReason::Vector]);
    assert_eq!(state.registry.get_meta(PID, "active_generation").unwrap().as_deref(), Some("7"));
    assert_eq!(state.get_project_cached(PID).unwrap().search.count_docs(PID).unwrap(), 1);
    assert_eq!(state.active_indexing_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn automatic_repair_refuses_in_progress_ingestion_without_leaving_false_warning() {
    let (_temp, state) = fixture(|_| {}, 30).await;
    let runtime = project_service::ensure_project_runtime(&state, PID).await.unwrap();
    let ingestion = runtime.search.begin_mutation().unwrap();
    let attempted = project_service::repair_vectors_if_mismatch(&state, PID).await;
    let reasons = state.registry.reindex_reasons(PID).unwrap();
    drop(ingestion);
    assert!(attempted.is_err());
    assert!(reasons.is_empty(), "normal lexical-before-vector ingestion is not a durable defect");
    project_service::repair_project_scoped(&state, PID, "vector_only").await.unwrap();
    let lock = state.acquire_project_update_lock(PID).await;
    let waiter_state = state.clone();
    let waiter = tokio::spawn(async move { project_service::repair_vectors_if_mismatch(&waiter_state, PID).await });
    // Resolve/recheck while the automatic request is serialized behind us.
    drop(lock);
    assert_eq!(waiter.await.unwrap().unwrap(), project_service::VectorRepairOutcome::NoCurrentCountMismatch);
    assert!(state.registry.reindex_reasons(PID).unwrap().is_empty());
}

#[tokio::test]
async fn delayed_duplicate_notification_cannot_resurrect_a_settled_requirement() {
    let temp = tempfile::tempdir().unwrap();
    let (state, receiver) = AppState::new(Config { data_dir: temp.path().join("data"),
        allowed_roots: vec![temp.path().to_path_buf()],
        dream_idle_after_secs: 3600, ..Default::default() }).unwrap();
    state.registry.put_project(&ProjectRecord { project_id: PID.into(), project_name: "Notifications".into(),
        project_type: "general".into(), directory: String::new(), created_at_ms: 1, updated_at_ms: 1,
        reindex_required_since_ms: None }).unwrap();
    let token = state.registry.require_reindex(PID, ReindexReason::Vector, 10).unwrap().unwrap();
    state.registry.settle_reindex_requirement(&token).unwrap();
    state.events_tx.send(AppEvent::FullReindexRequired { project_id: PID.into() }).unwrap();
    state.events_tx.send(AppEvent::FullReindexRequired { project_id: PID.into() }).unwrap();
    let stop = CancellationToken::new();
    let worker = tokio::spawn(engram_server::actors::dreamer::run_dreamer(state.clone(), receiver, stop.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.events_tx.len() != 0 { tokio::task::yield_now().await; }
    }).await.unwrap();
    stop.cancel();
    worker.await.unwrap();
    assert!(state.registry.reindex_reasons(PID).unwrap().is_empty());
    assert!(state.registry.get_project(PID).unwrap().unwrap().reindex_required_since_ms.is_none());
    assert!(state.get_project_cached(PID).is_none(), "a notification must not create new indexing/dream work");
}

#[tokio::test]
async fn disabled_embeddings_do_not_create_a_repair_requirement_or_advance_source() {
    let (_temp, state) = fixture(|config| config.embedding_backend = "fts_only".into(), 30).await;
    let automatic = project_service::repair_vectors_if_mismatch(&state, PID).await.unwrap();
    assert_eq!(automatic, project_service::VectorRepairOutcome::Disabled);
    assert!(project_service::repair_project_scoped(&state, PID, "vector_only").await.is_err());
    assert!(state.registry.reindex_reasons(PID).unwrap().is_empty());
    assert_eq!(state.registry.get_meta(PID, "active_generation").unwrap().as_deref(), Some("7"));
}

#[tokio::test]
async fn equal_store_counts_do_not_hide_a_pending_recovery_warning() {
    let (_temp, state) = fixture(|_| {}, 1).await;
    project_service::repair_project_scoped(&state, PID, "vector_only").await.unwrap();
    state.registry.require_reindex(PID, ReindexReason::Vector, 10).unwrap();
    let report = engram_server::services::integrity_service::check_project_integrity_with_policy(&state, PID, false).await.unwrap();
    assert!(report.mismatches.is_empty(), "fixture has equal lexical/vector counts");
    assert!(!report.overall_healthy, "counts do not establish the disposition of a known recovery requirement");
    assert!(state.registry.reindex_requirement_token(PID, ReindexReason::Vector).unwrap().is_some());
}

#[tokio::test]
async fn disabled_embedding_recovery_cannot_claim_orphan_rows_were_repaired_or_mark_integrity_healthy() {
    let (temp, state) = fixture(|config| config.embedding_backend = "fts_only".into(), 1).await;
    let connection = engram_index::vector::connect(&state.cfg.data_dir.join("projects").join(PID).join("lancedb")).await.unwrap();
    let (table, _) = engram_index::vector::open_or_create_table(&connection, "project_scoped_repair", 384).await.unwrap();
    let ids = (0..121).map(|n| format!("orphan-{n}")).collect::<Vec<_>>();
    let pks = ids.iter().map(|id| engram_core::build_pk(PID, "memory", 7, id)).collect::<Vec<_>>();
    let batch = engram_index::vector::create_record_batch(PID, "memory", 7, &pks, &ids,
        &vec!["hash".into(); 121], &(0..121_u64).collect::<Vec<_>>(), &vec!["a.rs".into(); 121],
        &vec!["rust".into(); 121], &vec![None; 121], &vec![None; 121], &vec![vec![0.25; 384]; 121], 384).unwrap();
    engram_index::vector::upsert_vectors(&table, vec![batch]).await.unwrap();
    drop(table);
    drop(connection);
    use engram_server::services::integrity_service;
    integrity_service::clear_repair_attempts_for_resolved(PID, &[]);
    let report = integrity_service::check_project_integrity_with_policy(&state, PID, true).await.unwrap();
    assert_eq!(report.tantivy_doc_count, 1);
    assert_eq!(report.vector_doc_count, 121);
    assert!(!report.overall_healthy);
    assert_eq!(report.repairs_attempted.len(), 1);
    let repair = &report.repairs_attempted[0];
    assert!(!repair.success);
    assert_eq!(repair.items_repaired, 0);
    assert_eq!(repair.action, "vector_recovery_disabled");
    assert!(state.registry.reindex_reasons(PID).unwrap().is_empty());
    assert_eq!(state.registry.get_meta(PID, "active_generation").unwrap().as_deref(), Some("7"));
    integrity_service::clear_repair_attempts_for_resolved(PID, &[]);
    drop(state);
    temp.close().unwrap();
}
