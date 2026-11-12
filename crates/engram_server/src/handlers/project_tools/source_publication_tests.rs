//! Desired publication behavior across legacy migration, cancellation,
//! corrupted metadata, admission and cold re-open. No live project is touched.
use super::*;
use crate::state::AppState;
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_index::{HybridQuery, IndexDoc};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const PROJECT: &str = "publication-contract";
#[path = "source_publication_tests/queued_gc.rs"]
mod queued_gc;
#[path = "source_publication_tests/revision_completeness.rs"]
mod revision_completeness;
#[path = "source_publication_tests/full_recovery.rs"]
mod full_recovery;
#[path = "source_publication_tests/process_exclusion.rs"]
mod process_exclusion;
#[path = "source_publication_tests/repair_history_options.rs"]
mod repair_history_options;
#[path = "source_publication_tests/project_deletion.rs"]
mod project_deletion;
#[path = "source_publication_tests/diagnostic_selection.rs"]
mod diagnostic_selection;
#[path = "source_publication_tests/job_cancellation.rs"]
mod job_cancellation;
#[path = "source_publication_tests/embedding_failure.rs"]
mod embedding_failure;

#[derive(Clone)]
struct PublicationPause {
    project: String,
    after_commit: bool,
    fail_cleanup: bool,
    entered: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}
static BACKGROUND_PUBLICATION_PAUSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<usize, PublicationPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

struct ResumePublication {
    registry_id: usize,
    resume: Arc<tokio::sync::Notify>,
}
impl Drop for ResumePublication {
    fn drop(&mut self) {
        BACKGROUND_PUBLICATION_PAUSES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.registry_id);
        self.resume.notify_one();
    }
}

pub(super) async fn before_background_publication(
    registry: &Arc<engram_core::Registry>,
    project: &str,
) {
    let pause = BACKGROUND_PUBLICATION_PAUSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(Arc::as_ptr(registry) as usize))
        .filter(|pause| pause.project == project && !pause.after_commit)
        .cloned();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.resume.notified().await;
    }
}

pub(super) async fn after_background_publication(
    registry: &Arc<engram_core::Registry>,
    project: &str,
) -> bool {
    let pause = BACKGROUND_PUBLICATION_PAUSES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(Arc::as_ptr(registry) as usize))
        .filter(|pause| pause.project == project && pause.after_commit)
        .cloned();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.resume.notified().await;
        return pause.fail_cleanup;
    }
    false
}

#[derive(Clone)]
struct ForegroundPublicationPause {
    project: String,
    after_commit: bool,
    cancel: bool,
    fail_step: Option<&'static str>,
    entered: Arc<tokio::sync::Notify>,
    resume: Arc<tokio::sync::Notify>,
}
static FOREGROUND_PUBLICATION_PAUSES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<usize, ForegroundPublicationPause>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

struct ResumeForegroundPublication {
    registry_id: usize,
    resume: Arc<tokio::sync::Notify>,
}
impl Drop for ResumeForegroundPublication {
    fn drop(&mut self) {
        FOREGROUND_PUBLICATION_PAUSES.lock().unwrap_or_else(|error| error.into_inner())
            .remove(&self.registry_id);
        self.resume.notify_one();
    }
}

pub(super) async fn foreground_publication(
    registry: &Arc<engram_core::Registry>, project: &str, after_commit: bool,
    cancel: &CancellationToken,
) -> Option<&'static str> {
    let pause = FOREGROUND_PUBLICATION_PAUSES.lock().unwrap_or_else(|error| error.into_inner())
        .get(&(Arc::as_ptr(registry) as usize))
        .filter(|pause| pause.project == project && pause.after_commit == after_commit).cloned();
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.resume.notified().await;
        if pause.cancel { cancel.cancel(); }
        return pause.fail_step;
    }
    None
}

fn fixture() -> (tempfile::TempDir, Engram) {
    fixture_with_backend("fts_only")
}

fn fixture_with_backend(backend: &str) -> (tempfile::TempDir, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let engram = fixture_at(temp.path(), backend);
    (temp, engram)
}

fn fixture_at(temp: &Path, backend: &str) -> Engram {
    fixture_at_with_initial_head(temp, backend, true)
}

fn fixture_with_absent_head() -> (tempfile::TempDir, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let engram = fixture_at_with_initial_head(temp.path(), "fts_only", false);
    (temp, engram)
}

fn fixture_at_with_initial_head(temp: &Path, backend: &str, published: bool) -> Engram {
    let root = temp.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    drop(git2::Repository::init(&root).unwrap());
    std::fs::write(root.join("source.rs"), "pub fn new_marker() -> u32 { 2 }\n").unwrap();
    let cfg = Config {
        data_dir: temp.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: backend.into(),
        max_project_files: Some(8),
        max_project_bytes: Some(64 * 1024),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _) = AppState::new(cfg).unwrap();
    // Ordinary controls have an explicit old published numeric head. An
    // absent-head control must obtain legacy attestation from its real producer;
    // empty storage alone cannot grant that authority.
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PROJECT.into(),
            project_name: "Source publication".into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    if published { state.registry.set_meta(PROJECT, "active_generation", "1").unwrap(); }
    Engram::new(state)
}

async fn close(temp: tempfile::TempDir, engram: Engram) {
    finish_owned_jobs(&engram).await;
    drop(engram);
    temp.close().unwrap();
}

async fn finish_owned_jobs(engram: &Engram) {
    loop {
        let jobs = engram
            .state
            .active_jobs
            .write()
            .await
            .drain()
            .map(|(_, task)| task)
            .collect::<Vec<_>>();
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            job.await.unwrap();
        }
    }
}

async fn attempt_scan_dependent_action(engram: &Engram, action: &str) -> bool {
    match action {
        "update_sync" | "update_background" => {
            let response = engram
                .handle_update_project(
                    serde_json::from_value(json!({
                        "project_id":PROJECT, "wait": action == "update_sync",
                        "max_commits":1, "index_antipatterns":false
                    }))
                    .unwrap(),
                )
                .await;
            if action == "update_sync" {
                return response.is_err();
            }
            finish_owned_jobs(engram).await;
            let jobs = engram.state.registry.list_jobs(Some(PROJECT)).unwrap();
            response.is_ok()
                && jobs
                    .iter()
                    .any(|job| job.kind == "update_project" && job.status == "failed")
        }
        "full" | "graph_only" => engram
            .handle_repair_project(
                serde_json::from_value(json!({
                    "project_id":PROJECT, "scope":action, "wipe_and_reindex":action == "full",
                    "max_commits":1, "index_antipatterns":false
                }))
                .unwrap(),
            )
            .await
            .is_err(),
        _ => panic!("unknown fixture action"),
    }
}

async fn failed_scan_state_violations(engram: &Engram, action: &str) -> Vec<String> {
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let generation = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    let before_rows = ps
        .search
        .lexical_search(&query(generation, "new_marker"))
        .unwrap()
        .into_iter()
        .map(|hit| hit.pk)
        .collect::<Vec<_>>();
    let before_graph = serde_json::to_value(
        engram
            .state
            .graph
            .list_file_node_metadata_with_generation(PROJECT)
            .unwrap(),
    )
    .unwrap();
    engram
        .state
        .registry
        .set_meta(PROJECT, "last_git_oid", "retained-history")
        .unwrap();
    let rejected = attempt_scan_dependent_action(engram, action).await;
    let head = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    let after_rows = ps
        .search
        .lexical_search(&query(generation, "new_marker"))
        .unwrap()
        .into_iter()
        .map(|hit| hit.pk)
        .collect::<Vec<_>>();
    let after_graph = serde_json::to_value(
        engram
            .state
            .graph
            .list_file_node_metadata_with_generation(PROJECT)
            .unwrap(),
    )
    .unwrap();
    let history = engram
        .state
        .registry
        .get_meta(PROJECT, "last_git_oid")
        .unwrap();
    let runtime_preserved = engram
        .state
        .projects
        .get(PROJECT)
        .is_some_and(|cached| std::sync::Arc::ptr_eq(&cached.search, &ps.search));
    drop(ps);
    [
        (rejected, "must reject a failed source scan"),
        (
            !before_rows.is_empty(),
            "fixture must contain searchable source",
        ),
        (head == generation, "must preserve the published head"),
        (
            after_rows == before_rows,
            "must preserve searchable physical rows",
        ),
        (
            after_graph == before_graph,
            "must preserve graph fingerprints",
        ),
        (
            history.as_deref() == Some("retained-history"),
            "must preserve history watermark",
        ),
        (
            runtime_preserved,
            "must reject before runtime cache eviction",
        ),
    ]
    .into_iter()
    .filter_map(|(ok, message)| (!ok).then(|| format!("{action}: {message}")))
    .collect()
}

#[tokio::test]
async fn unavailable_source_root_preserves_data_for_updates_and_destructive_repairs() {
    for regular_file in [false, true] {
        for action in ["update_sync", "update_background", "full", "graph_only"] {
            let (temp, engram) = fixture();
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            })).unwrap()).await.unwrap();
            finish_owned_jobs(&engram).await;
            let root = temp.path().join("repo");
            let backup = temp.path().join("unavailable-repo");
            // Both absolute targets are inside this exact owned fixture.
            assert_eq!(
                root.canonicalize().unwrap().parent(),
                Some(temp.path().canonicalize().unwrap().as_path())
            );
            assert_eq!(backup.parent(), Some(temp.path()));
            assert!(!backup.exists());
            std::fs::rename(&root, &backup).unwrap();
            if regular_file {
                std::fs::write(&root, "not a source directory").unwrap();
            }
            let violations = failed_scan_state_violations(&engram, action).await;
            close(temp, engram).await;
            assert!(violations.is_empty(), "{violations:?}");
        }
    }
}

#[tokio::test]
async fn over_budget_source_scan_preserves_data_before_updates_and_destructive_repairs() {
    for count_overflow in [true, false] {
        for action in ["update_sync", "update_background", "full", "graph_only"] {
            let (temp, engram) = fixture();
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            })).unwrap()).await.unwrap();
            finish_owned_jobs(&engram).await;
            let root = temp.path().join("repo");
            if count_overflow {
                for index in 0..8 {
                    std::fs::write(root.join(format!("extra{index}.rs")), "fn extra() {}\n")
                        .unwrap();
                }
            } else {
                std::fs::write(root.join("extra.rs"), vec![b' '; 64 * 1024]).unwrap();
            }
            let violations = failed_scan_state_violations(&engram, action).await;
            close(temp, engram).await;
            assert!(violations.is_empty(), "{violations:?}");
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn unreadable_ignore_rules_preserve_data_before_updates_and_destructive_repairs() {
    use std::os::windows::fs::OpenOptionsExt;
    for action in ["update_sync", "update_background", "full", "graph_only"] {
        let (temp, engram) = fixture();
        engram
            .handle_repair_project(
                serde_json::from_value(json!({
                    "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        finish_owned_jobs(&engram).await;
        let rules = temp.path().join("repo/.ignore");
        std::fs::write(&rules, "ignored.rs\n").unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&rules)
            .unwrap();
        let violations = failed_scan_state_violations(&engram, action).await;
        drop(lock);
        close(temp, engram).await;
        assert!(violations.is_empty(), "{violations:?}");
    }
}

#[tokio::test]
async fn successfully_scanned_empty_source_root_still_publishes_genuine_deletions() {
    let (temp, engram) = fixture();
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    finish_owned_jobs(&engram).await;
    let before = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    std::fs::remove_file(temp.path().join("repo/source.rs")).unwrap();
    let response = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let after = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let rows = ps.search.lexical_search(&query(after, "new_marker"));
    let paths = engram
        .state
        .graph
        .list_file_node_metadata_with_generation(PROJECT)
        .unwrap();
    drop(ps);
    close(temp, engram).await;
    response.unwrap();
    assert_eq!(after, before + 1);
    assert!(rows.unwrap().is_empty());
    assert!(paths.is_empty());
}

fn old_doc() -> IndexDoc {
    IndexDoc {
        generation: 1,
        chunk_id: 1,
        path: RelPath::new("source.rs"),
        language: "rust".into(),
        content: "old_marker".into(),
        namespace: "memory".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: "old-document".into(),
        content_hash: ContentHash::compute(b"old_marker").0,
    }
}

fn query(generation: u64, text: &str) -> HybridQuery {
    HybridQuery {
        project_id: PROJECT.into(),
        namespace: "memory".into(),
        generation,
        text: text.into(),
        top_k: 8,
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

#[tokio::test]
async fn cancelled_first_legacy_migration_is_retryable_and_preserves_an_existing_reader() {
    let (temp, engram) = fixture_with_absent_head();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    ps.search
        .index_docs(PROJECT, &[old_doc()], &CancellationToken::new())
        .await
        .unwrap();
    // This fixture just produced the complete legacy payload; its explicit
    // attestation is not inferred from absent catalog files.
    let legacy = engram.state.registry.capture_source_head(PROJECT).unwrap();
    assert!(engram.state.registry.attest_legacy_source_head(&legacy).unwrap());
    let old_reader = ps.search.pin_source_generation(PROJECT, 1).unwrap();
    let base = engram
        .capture_source_head(PROJECT, &ps.info.directory, &ps.info.project_type)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let extraction = engram_index::SourceExtractionExpectation::for_paths(100, ["source.rs"]).unwrap();
    let interrupted = engram
        .index_full_files_with_parse_guard(
            &ps.search,
            PROJECT,
            "memory",
            2,
            base.generation(),
            Path::new(&ps.info.directory),
            vec![PathBuf::from(&ps.info.directory).join("source.rs")],
            100,
            &extraction,
            &cancel,
            move |_, _| trigger.cancel(),
        )
        .await;
    let was_cancelled = cancel.is_cancelled() && interrupted.is_err();
    let staged_directory_exists = ps.search.has_managed_source_snapshots(PROJECT).unwrap();
    let old_head_unchanged = engram
        .state
        .registry
        .get_meta(PROJECT, "active_generation")
        .unwrap()
        .is_none();
    let preserved_before_retry = ps
        .search
        .lexical_search(&query(1, "old_marker"))
        .unwrap()
        .len();
    engram.state.projects.remove(PROJECT);
    drop(ps);
    // Cold read must distinguish this attested interrupted migration from
    // loss of an authoritative published head, without opening a runtime.
    let cold_generation = project_service::get_active_generation(&engram.state, PROJECT).await;
    let stayed_cold = engram.state.projects.is_empty();
    let repaired = engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let head = engram
        .state
        .registry
        .get_meta(PROJECT, "active_generation")
        .unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let new_rows = ps
        .search
        .lexical_search(&query(2, "new_marker"))
        .map(|hits| hits.len());
    let gc_while_pinned = ps.search.purge_old_generations(PROJECT, 2).await;
    let preserved_after_retry = ps
        .search
        .lexical_search(&query(1, "old_marker"))
        .map(|hits| hits.len());
    drop(old_reader);
    let final_gc = ps.search.purge_old_generations(PROJECT, 2).await;
    let retired = ps.search.pin_source_generation(PROJECT, 1).is_err();
    drop(ps);
    close(temp, engram).await;
    assert!(was_cancelled && staged_directory_exists && old_head_unchanged);
    assert_eq!(preserved_before_retry, 1);
    assert_eq!(cold_generation.unwrap(), 1);
    assert!(stayed_cold);
    repaired.unwrap();
    assert_eq!(head.as_deref(), Some("2"));
    assert!(new_rows.unwrap() > 0);
    gc_while_pinned.unwrap();
    assert_eq!(preserved_after_retry.unwrap(), 1);
    final_gc.unwrap();
    assert!(retired);
}

#[tokio::test]
async fn corrupt_head_rejects_full_or_graph_repair_before_wiping_graph_and_history() {
    for (scope, wipe) in [("full", true), ("graph_only", false)] {
        let (temp, engram) = fixture();
        engram
            .state
            .registry
            .set_meta(PROJECT, "active_generation", "broken-head")
            .unwrap();
        engram
            .state
            .registry
            .set_meta(PROJECT, "last_git_oid", "preserved-history")
            .unwrap();
        let marker = engram_graph::Node {
            node_id: "human-marker".into(),
            node_type: "function".into(),
            name: "Human marker".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: RelPath::new("source.rs"),
            start_line: 1,
            end_line: 1,
            generation: 1,
            metadata: None,
        };
        engram.state.graph.upsert_nodes(PROJECT, &[marker]).unwrap();
        let health = engram
            .handle_project_health(serde_json::from_value(json!({"project_id":PROJECT})).unwrap())
            .await;
        let repair = engram.handle_repair_project(serde_json::from_value(json!({
            "project_id":PROJECT,"scope":scope,"wipe_and_reindex":wipe,"max_commits":1,"index_antipatterns":false
        })).unwrap()).await;
        let graph_preserved = engram
            .state
            .graph
            .get_node(PROJECT, "human-marker")
            .unwrap()
            .is_some();
        let head = engram
            .state
            .registry
            .get_meta(PROJECT, "active_generation")
            .unwrap();
        let history = engram
            .state
            .registry
            .get_meta(PROJECT, "last_git_oid")
            .unwrap();
        close(temp, engram).await;
        assert!(health.is_err(), "health must not fabricate generation one");
        assert!(repair.is_err());
        assert!(graph_preserved);
        assert_eq!(head.as_deref(), Some("broken-head"));
        assert_eq!(history.as_deref(), Some("preserved-history"));
    }
}

#[tokio::test]
async fn busy_storage_rejects_destructive_repair_before_graph_or_watermark_changes() {
    let (temp, engram) = fixture();
    engram
        .state
        .registry
        .set_meta(PROJECT, "active_generation", "1")
        .unwrap();
    engram
        .state
        .registry
        .set_meta(PROJECT, "last_git_oid", "preserved-history")
        .unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let marker = engram_graph::Node {
        node_id: "human-marker".into(),
        node_type: "function".into(),
        name: "Human marker".into(),
        namespace: "memory".into(),
        language: "rust".into(),
        file_path: RelPath::new("source.rs"),
        start_line: 1,
        end_line: 1,
        generation: 1,
        metadata: None,
    };
    engram.state.graph.upsert_nodes(PROJECT, &[marker]).unwrap();
    let operation = ps.search.begin_operation().unwrap();
    let repair = engram.handle_repair_project(serde_json::from_value(json!({
        "project_id":PROJECT,"scope":"full","wipe_and_reindex":true,"max_commits":1,"index_antipatterns":false
    })).unwrap()).await;
    let graph_preserved = engram
        .state
        .graph
        .get_node(PROJECT, "human-marker")
        .unwrap()
        .is_some();
    let history = engram
        .state
        .registry
        .get_meta(PROJECT, "last_git_oid")
        .unwrap();
    drop(operation);
    drop(ps);
    close(temp, engram).await;
    assert!(repair.is_err());
    assert!(graph_preserved);
    assert_eq!(history.as_deref(), Some("preserved-history"));
}

#[tokio::test]
async fn cold_snapshot_store_with_lost_registry_metadata_is_not_assumed_to_be_legacy() {
    let (temp, engram) = fixture();
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let record = engram.state.registry.get_project(PROJECT).unwrap().unwrap();
    engram.state.projects.remove(PROJECT);
    // Simulate recovery of only the legacy project record while the managed
    // stores remain. No authority/absence proof survives this metadata loss.
    engram
        .state
        .registry
        .delete_all_for_project(PROJECT)
        .unwrap();
    engram.state.registry.put_project(&record).unwrap();
    let result = project_service::get_active_generation(&engram.state, PROJECT).await;
    let stayed_cold = engram.state.projects.is_empty();
    let head = engram
        .state
        .registry
        .get_meta(PROJECT, "active_generation")
        .unwrap();
    close(temp, engram).await;
    assert!(result.is_err());
    assert!(stayed_cold);
    assert!(head.is_none());
}

#[tokio::test]
async fn source_directory_change_refreshes_cached_identity_and_retry_uses_the_same_engine() {
    let (temp, engram) = fixture();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let original_engine = ps.search.clone();
    let moved = PathBuf::from(&ps.info.directory).join("new-location");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::write(
        moved.join("source.rs"),
        "pub fn moved_marker() -> u32 { 9 }\n",
    )
    .unwrap();
    let mut record = engram.state.registry.get_project(PROJECT).unwrap().unwrap();
    record.directory = moved.to_string_lossy().into();
    engram.state.registry.put_project(&record).unwrap();
    let stale_update = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let refreshed = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let identity_current = refreshed.info.directory == record.directory;
    let engine_reused = std::sync::Arc::ptr_eq(&original_engine, &refreshed.search);
    let retry = engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let content = refreshed.search.lexical_search(&query(2, "moved_marker"));
    drop(refreshed);
    drop(original_engine);
    drop(ps);
    close(temp, engram).await;
    assert!(stale_update.is_err());
    assert!(identity_current && engine_reused);
    retry.unwrap();
    assert!(!content.unwrap().is_empty());
}

#[tokio::test]
async fn rejected_publication_retries_unpublished_fingerprints_instead_of_reporting_a_noop() {
    let (temp, engram) = fixture();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let initial = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let mut published = old_doc(); published.generation = 2;
    ps.search.prepare_full_source_snapshot(PROJECT, 1, 2, &[RelPath::new("source.rs")], &CancellationToken::new()).await.unwrap();
    ps.search.index_docs(PROJECT, &[published], &CancellationToken::new()).await.unwrap();
    ps.search.seal_source_snapshot(PROJECT, 2).unwrap();
    assert!(engram.state.registry.publish_source_head(&initial, 2).unwrap());
    let base = engram
        .capture_source_head(PROJECT, &ps.info.directory, &ps.info.project_type)
        .await
        .unwrap();
    let extraction = engram_index::SourceExtractionExpectation::for_paths(100, ["source.rs"]).unwrap();
    let stats = engram
        .index_full_files_with_parse_guard(
            &ps.search,
            PROJECT,
            "memory",
            3,
            base.generation(),
            Path::new(&ps.info.directory),
            vec![PathBuf::from(&ps.info.directory).join("source.rs")],
            100,
            &extraction,
            &CancellationToken::new(),
            |_, _| {},
        )
        .await
        .unwrap();
    engram
        .process_ingest_stats(PROJECT, 3, &stats)
        .await
        .unwrap();
    // A real metadata ABA returns to the same published base. Exact source
    // authority must still reject this stale writer and replay staged stats.
    engram
        .state
        .registry
        .set_meta(PROJECT, "active_generation", "3")
        .unwrap();
    engram.state.registry.set_meta(PROJECT, "active_generation", "2").unwrap();
    let publication_admission = ps.search.begin_project_mutation(PROJECT).unwrap();
    let lost = engram
        .publish_source_generation(&publication_admission, PROJECT, base, 3, None, extraction)
        .await;
    drop(publication_admission);
    let before = ps.search.lexical_search(&query(2, "old_marker"));
    // The disk bytes now match the newly staged graph fingerprints. Their
    // generation is nevertheless unpublished, so ordinary refresh must retry.
    let retry = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let head = engram
        .state
        .registry
        .get_meta(PROJECT, "active_generation")
        .unwrap();
    let content = ps.search.lexical_search(&query(3, "new_marker"));
    drop(ps);
    close(temp, engram).await;
    assert!(lost.is_err());
    assert_eq!(before.unwrap().len(), 1);
    let response = retry.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(!text.contains("no_changes_detected"), "{text}");
    assert!(text.contains("files=1"), "{text}");
    assert_eq!(head.as_deref(), Some("3"));
    assert!(!content.unwrap().is_empty());
}

#[tokio::test]
async fn ordinary_directory_retry_does_not_trust_old_root_stats_for_different_bytes() {
    let (temp, engram) = fixture();
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let original = PathBuf::from(&ps.info.directory).join("source.rs");
    let modified = std::fs::metadata(&original).unwrap().modified().unwrap();
    let moved = PathBuf::from(&ps.info.directory).join("replacement");
    std::fs::create_dir_all(&moved).unwrap();
    let replacement = moved.join("source.rs");
    let original_text = std::fs::read_to_string(&original).unwrap();
    let different = original_text.replace("new_marker", "alt_marker");
    assert_eq!(different.len(), original_text.len());
    std::fs::write(&replacement, different).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let mut record = engram.state.registry.get_project(PROJECT).unwrap().unwrap();
    record.directory = moved.to_string_lossy().into();
    engram.state.registry.put_project(&record).unwrap();
    let request =
        json!({"project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false});
    let stale = engram
        .handle_update_project(serde_json::from_value(request.clone()).unwrap())
        .await;
    let retry = engram
        .handle_update_project(serde_json::from_value(request).unwrap())
        .await;
    let head = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    let current = ps.search.lexical_search(&query(head, "alt_marker"));
    let obsolete = ps.search.lexical_search(&query(head, "new_marker"));
    drop(ps);
    close(temp, engram).await;
    assert!(stale.is_err());
    let response = retry.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(
        !text.contains("no_changes_detected"),
        "Replacement root contains different bytes: {text}"
    );
    assert!(!current.unwrap().is_empty());
    assert!(obsolete.unwrap().is_empty());
}

#[tokio::test]
async fn identical_replacement_root_is_verified_once_without_rewriting_unchanged_source() {
    let (temp, engram) = fixture();
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let original = PathBuf::from(&ps.info.directory).join("source.rs");
    let modified = std::fs::metadata(&original).unwrap().modified().unwrap();
    let moved = PathBuf::from(&ps.info.directory).join("identical");
    std::fs::create_dir_all(&moved).unwrap();
    let replacement = moved.join("source.rs");
    std::fs::copy(&original, &replacement).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let original_rows = ps
        .search
        .lexical_search(&query(2, "new_marker"))
        .unwrap()
        .into_iter()
        .map(|hit| hit.pk)
        .collect::<Vec<_>>();
    let mut record = engram.state.registry.get_project(PROJECT).unwrap().unwrap();
    record.directory = moved.to_string_lossy().into();
    engram.state.registry.put_project(&record).unwrap();
    let request =
        json!({"project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false});
    let stale = engram
        .handle_update_project(serde_json::from_value(request.clone()).unwrap())
        .await;
    let first = engram
        .handle_update_project(serde_json::from_value(request.clone()).unwrap())
        .await;
    let verified_once = !engram
        .state
        .registry
        .source_identity_needs_verification(PROJECT)
        .unwrap();
    let stable_token = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let second = engram
        .handle_update_project(serde_json::from_value(request).unwrap())
        .await;
    let current_token = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let stable_head =
        stable_token.generation() == current_token.generation() && current_token.generation() == 2;
    let after = ps
        .search
        .lexical_search(&query(2, "new_marker"))
        .unwrap()
        .into_iter()
        .map(|hit| hit.pk)
        .collect::<Vec<_>>();
    drop(ps);
    close(temp, engram).await;
    assert!(stale.is_err());
    for result in [first, second] {
        let response = result.unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        assert!(text.contains("no_changes_detected"), "{text}");
    }
    assert!(verified_once && stable_head);
    assert_eq!(
        after, original_rows,
        "identity-only refresh must keep original physical chunks"
    );
}

#[tokio::test]
async fn rapid_same_size_edits_publish_new_bytes_and_preserve_existing_readers() {
    let (temp, engram) = fixture();
    let root = temp.path().join("repo");
    let stamp = |nanos| std::time::UNIX_EPOCH + std::time::Duration::new(1_780_000_000, nanos);
    let cases = [
        (
            "source.rs",
            "first_tick",
            "later_tick",
            100_000_000,
            900_000_000,
        ),
        (
            "rapid.rs",
            "first_fast",
            "later_fast",
            100_000_100,
            100_000_900,
        ),
    ];
    let set_time = |path: &Path, nanos| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp(nanos)))
            .unwrap();
    };
    for (name, old, _, initial, _) in cases {
        let path = root.join(name);
        std::fs::write(&path, format!("pub fn {old}() -> u32 {{ 1 }}\n")).unwrap();
        set_time(&path, initial);
    }
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let previous = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    let held = ps.search.pin_source_generation(PROJECT, previous).unwrap();
    for (name, _, new, _, changed) in cases {
        let path = root.join(name);
        std::fs::write(&path, format!("pub fn {new}() -> u32 {{ 2 }}\n")).unwrap();
        set_time(&path, changed);
    }
    let update = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let current = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    let hits = cases.map(|(_, old, new, _, _)| {
        (
            ps.search
                .lexical_search(&query(current, new))
                .unwrap()
                .len(),
            ps.search
                .lexical_search(&query(current, old))
                .unwrap()
                .len(),
            ps.search
                .lexical_search(&query(previous, old))
                .unwrap()
                .len(),
        )
    });
    drop(held);
    drop(ps);
    close(temp, engram).await;
    let response = update.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(
        !text.contains("no_changes_detected"),
        "rapid edits must be indexed: {text}"
    );
    assert_eq!(current, previous + 1);
    for (new, obsolete, retained) in hits {
        assert!(
            new > 0 && obsolete == 0 && retained > 0,
            "current search must show the edit while an existing reader keeps its old view"
        );
    }
}

#[cfg(windows)]
#[tokio::test]
async fn timestamp_only_refresh_is_verified_once_and_survives_restart_without_content_reads() {
    use std::os::windows::fs::OpenOptionsExt;
    let (temp, engram) = fixture();
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let path = temp.path().join("repo/source.rs");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new().set_modified(modified + std::time::Duration::from_secs(2)),
        )
        .unwrap();
    let before = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let first = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let after_first = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let config = (*engram.state.cfg).clone();
    finish_owned_jobs(&engram).await;
    drop(engram);
    // Deny content reads while permitting metadata inspection. A cache that
    // verified identical bytes once should survive a cold process and need no
    // second hash, parse or rewrite for the same untouched file.
    let locked = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(2 | 4)
        .open(&path)
        .unwrap();
    let (state, _) = AppState::new(config).unwrap();
    let engram = Engram::new(state);
    let cached = serde_json::to_value(
        engram
            .state
            .graph
            .list_file_node_metadata_with_generation(PROJECT)
            .unwrap(),
    )
    .unwrap();
    let freshness = engram
        .handle_get_index_freshness(
            serde_json::from_value(json!({
                "project_id":PROJECT,"check_disk":true
            }))
            .unwrap(),
        )
        .await;
    let after_readonly = serde_json::to_value(
        engram
            .state
            .graph
            .list_file_node_metadata_with_generation(PROJECT)
            .unwrap(),
    )
    .unwrap();
    let second = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let after_second = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let held = ps
        .search
        .pin_source_generation(PROJECT, before.generation())
        .unwrap();
    std::fs::write(
        temp.path().join("repo/other.rs"),
        "pub fn another_file() -> u32 { 4 }\n",
    )
    .unwrap();
    let unrelated = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let after_unrelated = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let third = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let after_third = engram.state.registry.capture_source_head(PROJECT).unwrap();
    drop(locked);
    let retained = ps
        .search
        .lexical_search(&query(before.generation(), "new_marker"))
        .map(|hits| hits.len());
    drop(held);
    drop(ps);
    close(temp, engram).await;
    for response in [first, second, third] {
        let response = response.unwrap();
        assert!(
            response.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("no_changes_detected"),
            "timestamp-only refresh must preserve source without reading its content twice"
        );
    }
    let freshness = freshness.unwrap();
    assert!(
        freshness.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("disk_check: no_changes_detected")
    );
    assert_eq!(
        cached, after_readonly,
        "health/freshness checks must not warm persistent caches"
    );
    assert_eq!(format!("{before:?}"), format!("{after_first:?}"));
    assert_eq!(format!("{before:?}"), format!("{after_second:?}"));
    unrelated.unwrap();
    assert_eq!(after_unrelated.generation(), before.generation() + 1);
    assert_eq!(format!("{after_unrelated:?}"), format!("{after_third:?}"));
    assert!(retained.unwrap() > 0);
}

#[cfg(windows)]
#[tokio::test]
async fn legacy_coarse_stats_warm_without_reindexing_and_health_stays_readonly() {
    use std::os::windows::fs::OpenOptionsExt;
    let (temp, engram) = fixture();
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let id = engram_core::ids::NodeId::file("source.rs").0;
    let mut original = engram.state.graph.get_node(PROJECT, &id).unwrap().unwrap();
    let metadata = original.metadata.as_mut().unwrap().as_object_mut().unwrap();
    metadata.remove("mtime_ns");
    metadata.remove("verified_stat_v1");
    let coarse = original.metadata.clone();
    engram
        .state
        .graph
        .upsert_nodes(PROJECT, &[original])
        .unwrap();
    let before = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let check = engram
        .handle_get_index_freshness(
            serde_json::from_value(json!({
                "project_id":PROJECT,"check_disk":true
            }))
            .unwrap(),
        )
        .await;
    let readonly = engram
        .state
        .graph
        .get_node(PROJECT, &id)
        .unwrap()
        .unwrap()
        .metadata;
    let after_check = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let warmed = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let path = temp.path().join("repo/source.rs");
    let locked = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(2 | 4)
        .open(&path)
        .unwrap();
    let repeated = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let after = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let retained_metadata = engram
        .state
        .graph
        .get_node(PROJECT, &id)
        .unwrap()
        .unwrap()
        .metadata
        .unwrap();
    drop(locked);
    close(temp, engram).await;
    check.unwrap();
    assert_eq!(
        readonly, coarse,
        "read-only freshness must not warm legacy metadata"
    );
    assert_eq!(format!("{before:?}"), format!("{after_check:?}"));
    for response in [warmed, repeated] {
        assert!(
            response.unwrap().content[0]
                .as_text()
                .unwrap()
                .text
                .contains("no_changes_detected")
        );
    }
    assert_eq!(format!("{before:?}"), format!("{after:?}"));
    assert_eq!(
        retained_metadata["mtime"],
        coarse.as_ref().unwrap()["mtime"]
    );
    assert_eq!(
        retained_metadata["file_hash"],
        coarse.as_ref().unwrap()["file_hash"]
    );
}

#[cfg(windows)]
#[tokio::test]
async fn unreadable_changed_source_refuses_publication_and_retry_keeps_both_reader_views() {
    use std::os::windows::fs::OpenOptionsExt;
    let (temp, engram) = fixture();
    engram
        .handle_repair_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let before = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let before_graph = serde_json::to_value(
        engram
            .state
            .graph
            .list_file_node_metadata_with_generation(PROJECT)
            .unwrap(),
    )
    .unwrap();
    let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
    let held = ps
        .search
        .pin_source_generation(PROJECT, before.generation())
        .unwrap();
    let path = temp.path().join("repo/source.rs");
    std::fs::write(
        &path,
        "pub fn alt_marker() -> u32 { 3 } // changed source bytes\n",
    )
    .unwrap();
    let locked = std::fs::OpenOptions::new()
        .write(true)
        .share_mode(2 | 4)
        .open(&path)
        .unwrap();
    let failed = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let after_failure = engram.state.registry.capture_source_head(PROJECT).unwrap();
    let failed_graph = serde_json::to_value(
        engram
            .state
            .graph
            .list_file_node_metadata_with_generation(PROJECT)
            .unwrap(),
    )
    .unwrap();
    let old_after_failure = ps
        .search
        .lexical_search(&query(before.generation(), "new_marker"));
    drop(locked);
    let retry = engram
        .handle_update_project(
            serde_json::from_value(json!({
                "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
            }))
            .unwrap(),
        )
        .await;
    let current = engram
        .state
        .registry
        .active_source_generation(PROJECT)
        .unwrap();
    let old = ps
        .search
        .lexical_search(&query(before.generation(), "new_marker"));
    let new = ps.search.lexical_search(&query(current, "alt_marker"));
    drop(held);
    drop(ps);
    close(temp, engram).await;
    assert!(
        failed.is_err(),
        "read failure must not publish a snapshot missing this source file"
    );
    assert_eq!(format!("{before:?}"), format!("{after_failure:?}"));
    assert_eq!(before_graph, failed_graph);
    assert!(!old_after_failure.unwrap().is_empty());
    retry.unwrap();
    assert_eq!(current, before.generation() + 1);
    assert!(!old.unwrap().is_empty() && !new.unwrap().is_empty());
}

#[tokio::test]
async fn foreground_and_background_registration_publish_their_own_complete_initial_generation() {
    for wait in [true, false] {
        let (temp, engram) = fixture();
        let root = engram
            .state
            .registry
            .get_project(PROJECT)
            .unwrap()
            .unwrap()
            .directory;
        engram
            .state
            .registry
            .delete_all_for_project(PROJECT)
            .unwrap();
        let response = engram
            .handle_index_project(
                serde_json::from_value(json!({
                    "directory":root,"project_name":"Initial publication","project_type":"general",
                    "wait":wait,"dedupe_by_directory":false
                }))
                .unwrap(),
            )
            .await;
        // A completed initial job may enqueue history work. Join each owned
        // wave before closing the fixture; the empty Git root prevents any
        // parent-checkout history discovery.
        loop {
            let jobs = engram
                .state
                .active_jobs
                .write()
                .await
                .drain()
                .map(|(_, task)| task)
                .collect::<Vec<_>>();
            if jobs.is_empty() {
                break;
            }
            for job in jobs {
                job.await.unwrap();
            }
        }
        let records = engram.state.registry.list_projects().unwrap();
        let project = records[0].project_id.clone();
        let head = engram.state.registry.active_source_generation(&project);
        let baseline = engram
            .state
            .registry
            .get_meta(&project, "last_full_index_generation")
            .unwrap();
        let identity_verified = !engram
            .state
            .registry
            .source_identity_needs_verification(&project)
            .unwrap();
        let ps = engram.ensure_project_runtime(&project).await.unwrap();
        let mut request = query(1, "new_marker");
        request.project_id = project;
        let content = ps.search.lexical_search(&request);
        drop(ps);
        close(temp, engram).await;
        response.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(head.unwrap(), 1);
        assert_eq!(baseline.as_deref(), Some("1"));
        assert!(identity_verified);
        assert!(!content.unwrap().is_empty());
    }
}

#[tokio::test]
async fn cancelled_or_rejected_background_publication_retains_previous_graph_declarations() {
    for reject in [false, true] {
        let (temp, engram) = fixture();
        engram
            .handle_repair_project(
                serde_json::from_value(json!({
                    "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        finish_owned_jobs(&engram).await;
        let head = engram.state.registry.capture_source_head(PROJECT).unwrap();
        let legacy = engram_graph::Node {
            node_id: "removed-declaration-from-published-head".into(),
            node_type: "function".into(),
            name: "old_declaration".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: RelPath::new("source.rs"),
            start_line: 20,
            end_line: 21,
            generation: head.generation(),
            metadata: None,
        };
        let mut caller = legacy.clone();
        caller.node_id = "unchanged-caller".into();
        caller.file_path = RelPath::new("untouched.rs");
        let edge = engram_graph::Edge {
            source_id: caller.node_id.clone(),
            target_id: legacy.node_id.clone(),
            namespace: "memory".into(),
            language: "rust".into(),
            edge_kind: engram_graph::EdgeKind::Calls,
            weight: 7,
            generation: head.generation(),
            metadata: None,
            updated_at_ms: 42,
        };
        engram
            .state
            .graph
            .upsert_nodes_and_edges(PROJECT, &[legacy.clone(), caller], &[edge.clone()])
            .unwrap();
        std::fs::write(
            temp.path().join("repo/source.rs"),
            "pub fn replacement_marker() -> u32 { 3 }\n",
        )
        .unwrap();
        let pause = PublicationPause {
            project: PROJECT.into(),
            after_commit: false,
            fail_cleanup: false,
            entered: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        let registry_id = Arc::as_ptr(&engram.state.registry) as usize;
        BACKGROUND_PUBLICATION_PAUSES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(registry_id, pause.clone());
        let release = ResumePublication {
            registry_id,
            resume: pause.resume.clone(),
        };
        let submitted = engram
            .handle_update_project(
                serde_json::from_value(json!({
                    "project_id":PROJECT,"wait":false,"max_commits":1,"index_antipatterns":false
                }))
                .unwrap(),
            )
            .await;
        let reached = submitted.is_ok()
            && tokio::time::timeout(std::time::Duration::from_secs(30), pause.entered.notified())
                .await
                .is_ok();
        let mut interrupted = false;
        if reached {
            if reject {
                // A real source-identity correction invalidates the staged CAS
                // without authorizing deletion of the previously published graph.
                let replacement = temp.path().join("repo/replacement");
                std::fs::create_dir_all(&replacement).unwrap();
                let mut project = engram.state.registry.get_project(PROJECT).unwrap().unwrap();
                project.directory = replacement.to_string_lossy().into_owned();
                engram.state.registry.put_project(&project).unwrap();
                interrupted = true;
            } else {
                let jobs = engram.state.registry.list_jobs(Some(PROJECT)).unwrap();
                if let Some(job) = jobs
                    .iter()
                    .find(|job| job.kind == "update_project" && job.status == "running")
                {
                    if let Some(token) = engram
                        .state
                        .cancellation_tokens
                        .read()
                        .await
                        .get(&job.job_id)
                    {
                        token.cancel();
                        interrupted = true;
                    }
                }
            }
        }
        drop(release); // Also releases the waiter on every normal failure path.
        finish_owned_jobs(&engram).await;
        let retained = engram
            .state
            .graph
            .get_node(PROJECT, &legacy.node_id)
            .unwrap();
        let retained_edges = engram
            .state
            .graph
            .list_edges_by_kind(PROJECT, engram_graph::EdgeKind::Calls, 100)
            .unwrap();
        let generation = engram
            .state
            .registry
            .active_source_generation(PROJECT)
            .unwrap();
        let jobs = engram.state.registry.list_jobs(Some(PROJECT)).unwrap();
        let refused = jobs.iter().any(|job| {
            job.kind == "update_project" && matches!(job.status.as_str(), "cancelled" | "failed")
        });
        // A later successful publication is allowed to retire this old symbol.
        let mut retry_pruned = true;
        if reached && interrupted && !reject {
            let retry = engram
                .handle_update_project(
                    serde_json::from_value(json!({
                        "project_id":PROJECT,"wait":false,"max_commits":1,"index_antipatterns":false
                    }))
                    .unwrap(),
                )
                .await;
            finish_owned_jobs(&engram).await;
            retry_pruned = retry.is_ok()
                && engram
                    .state
                    .registry
                    .active_source_generation(PROJECT)
                    .unwrap()
                    > head.generation()
                && engram
                    .state
                    .graph
                    .get_node(PROJECT, &legacy.node_id)
                    .unwrap()
                    .is_none();
        }
        close(temp, engram).await;
        assert!(
            reached && interrupted && refused,
            "The fixture must interrupt a fully staged background update"
        );
        assert_eq!(
            generation,
            head.generation(),
            "An interrupted candidate must not advance the head"
        );
        assert_eq!(
            serde_json::to_value(retained).unwrap(),
            serde_json::to_value(Some(legacy)).unwrap(),
            "Published declarations must remain until a candidate actually wins publication"
        );
        assert!(
            retained_edges
                .iter()
                .any(|stored| serde_json::to_value(stored).unwrap()
                    == serde_json::to_value(&edge).unwrap()),
            "An unpublished candidate must retain incoming edges from unchanged declarations too"
        );
        assert!(
            retry_pruned,
            "A successful background retry must retire removed declarations after publication"
        );
    }
}

#[tokio::test]
async fn background_postcommit_cancellation_or_cleanup_failure_reports_visible_committed_generation()
 {
    for fail_cleanup in [false, true] {
        let (temp, engram) = fixture();
        engram
            .handle_repair_project(
                serde_json::from_value(json!({
                    "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        finish_owned_jobs(&engram).await;
        let previous = engram
            .state
            .registry
            .active_source_generation(PROJECT)
            .unwrap();
        std::fs::write(
            temp.path().join("repo/source.rs"),
            "pub fn committed_marker() -> u32 { 3 }\n",
        )
        .unwrap();
        let pause = PublicationPause {
            project: PROJECT.into(),
            after_commit: true,
            fail_cleanup,
            entered: Arc::new(tokio::sync::Notify::new()),
            resume: Arc::new(tokio::sync::Notify::new()),
        };
        let registry_id = Arc::as_ptr(&engram.state.registry) as usize;
        BACKGROUND_PUBLICATION_PAUSES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(registry_id, pause.clone());
        let release = ResumePublication {
            registry_id,
            resume: pause.resume.clone(),
        };
        let submitted = engram
            .handle_update_project(
                serde_json::from_value(json!({
                    "project_id":PROJECT,"wait":false,"max_commits":1,"index_antipatterns":false
                }))
                .unwrap(),
            )
            .await;
        let reached = submitted.is_ok()
            && tokio::time::timeout(std::time::Duration::from_secs(30), pause.entered.notified())
                .await
                .is_ok();
        let visible_before_release = engram
            .state
            .registry
            .active_source_generation(PROJECT)
            .unwrap();
        let mut cancelled = fail_cleanup;
        if reached && !fail_cleanup {
            let jobs = engram.state.registry.list_jobs(Some(PROJECT)).unwrap();
            if let Some(job) = jobs
                .iter()
                .find(|job| job.kind == "update_project" && job.status == "running")
            {
                if let Some(token) = engram
                    .state
                    .cancellation_tokens
                    .read()
                    .await
                    .get(&job.job_id)
                {
                    token.cancel();
                    cancelled = true;
                }
            }
        }
        drop(release);
        finish_owned_jobs(&engram).await;
        let generation = engram
            .state
            .registry
            .active_source_generation(PROJECT)
            .unwrap();
        let jobs = engram.state.registry.list_jobs(Some(PROJECT)).unwrap();
        let report = jobs
            .iter()
            .find(|job| job.kind == "update_project")
            .cloned();
        let ps = engram.ensure_project_runtime(PROJECT).await.unwrap();
        let hits = ps
            .search
            .lexical_search(&query(generation, "committed_marker"));
        drop(ps);
        close(temp, engram).await;
        assert!(
            reached && cancelled,
            "The fixture must interrupt after actual head publication"
        );
        assert_eq!(visible_before_release, previous + 1);
        assert_eq!(
            generation,
            previous + 1,
            "Postcommit interruption cannot report an unpublished candidate"
        );
        assert!(
            !hits.unwrap().is_empty(),
            "Committed search content remains visible"
        );
        let report = report.unwrap();
        assert_eq!(report.status, "degraded");
        assert!(report.message.contains("published"), "{}", report.message);
        if fail_cleanup {
            assert!(
                report.message.contains("resolver refresh skipped"),
                "{}",
                report.message
            );
        } else {
            assert!(
                report.message.contains("cancellation"),
                "{}",
                report.message
            );
        }
    }
}

async fn complete_foreground_source(
    search: &engram_index::HybridSearchEngine,
    generation: u64,
    marker: &str,
    expected: &str,
) -> anyhow::Result<String> {
    use anyhow::ensure;
    let held = search.pin_source_generation(PROJECT, generation)?;
    let selected = query(generation, marker);
    let lexical = search.lexical_search(&selected)?;
    ensure!(
        lexical.len() == 1 && search.count_docs_in_generation(PROJECT, "memory", generation)? == 1,
        "Published source must contain exactly the complete known fixture file"
    );
    let hit = &lexical[0];
    let (path, _, body, first, last) = search
        .recover_selected_source_hit(PROJECT, &held, hit)?
        .ok_or_else(|| anyhow::anyhow!("Published source body is missing"))?;
    ensure!(
        path.as_str() == "source.rs" && body == expected && first == 1 && last == 1,
        "Published source bytes or line range differ from independent fixture input"
    );
    ensure!(
        hit.pk == engram_core::build_pk(PROJECT, "memory", generation, &hit.doc_id),
        "This changed source must belong to the expected physical revision"
    );
    #[cfg(feature = "vector")]
    {
        let vectors = search
            .vector_search(&selected, &CancellationToken::new())
            .await?;
        ensure!(
            vectors.len() == 1 && vectors[0].pk == hit.pk,
            "Native vector selection must match the complete published lexical revision"
        );
        ensure!(
            search
                .vector_generation_inventory(PROJECT, Some("memory"), generation)
                .await?
                == (1, std::collections::BTreeSet::from(["source.rs".into()])),
            "Native source coverage must contain exactly the published fixture file"
        );
    }
    #[cfg(not(feature = "vector"))]
    ensure!(
        search.document_embedding_identity().is_none() && search.count_vectors(PROJECT).await? == 0,
        "The actual no-vector build must remain lexical-only"
    );
    Ok(hit.pk.clone())
}

// One owned runtime drains actual async/blocking work before fixture removal,
// including early errors. Hooks pause a real public wait=true source update;
// they do not retain storage/source admission on behalf of the test.
fn foreground_publication_case(after_commit: bool, step: &'static str) -> anyhow::Result<()> {
    use anyhow::ensure;
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(async {
            let engram = fixture_at(owned.path(), if cfg!(feature="vector") { "local" } else { "fts_only" });
            let original_source = std::fs::read_to_string(owned.path().join("repo/source.rs"))?;
            let replacement_source = "pub fn foreground_published_marker() -> u32 { 33 }\n";
            engram.handle_repair_project(serde_json::from_value(json!({
                "project_id":PROJECT,"scope":"full","max_commits":1,"index_antipatterns":false
            }))?).await?;
            finish_owned_jobs(&engram).await;
            let head = engram.state.registry.capture_source_head(PROJECT)?;
            let previous = head.generation();
            let original_project = engram.state.registry.get_project(PROJECT)?.unwrap();
            // This is an ordinary unscoped engine handle, not a source or
            // mutation pin. A competing identity correction must not force
            // this observer to reload the runtime against the new directory.
            let ps = engram.ensure_project_runtime(PROJECT).await?;
            let original_pk = complete_foreground_source(&ps.search, previous, "new_marker", &original_source).await?;
            let legacy_id = "foreground-removed-published-declaration";
            engram.state.graph.upsert_nodes(PROJECT, &[engram_graph::Node {
                node_id:legacy_id.into(),node_type:"function".into(),name:"old_declaration".into(),
                namespace:"memory".into(),language:"rust".into(),file_path:RelPath::new("source.rs"),
                start_line:20,end_line:21,generation:previous,metadata:None,
            }])?;
            if step == "purge settlement" {
                engram.state.registry.set_meta(PROJECT,"purge_pending",&previous.to_string())?;
            }
            std::fs::write(owned.path().join("repo/source.rs"), replacement_source)?;
            let pause = ForegroundPublicationPause {
                project:PROJECT.into(),after_commit,cancel:step=="cancellation",
                fail_step:(!matches!(step,"cancellation"|"rejected CAS")).then_some(step),
                entered:Arc::new(tokio::sync::Notify::new()),resume:Arc::new(tokio::sync::Notify::new()),
            };
            let registry_id = Arc::as_ptr(&engram.state.registry) as usize;
            FOREGROUND_PUBLICATION_PAUSES.lock().unwrap_or_else(|error|error.into_inner())
                .insert(registry_id,pause.clone());
            let release = ResumeForegroundPublication {registry_id,resume:pause.resume.clone()};
            let caller = Engram::new(engram.state.clone());
            let mut request = tokio::spawn(async move {
                caller.handle_update_project(serde_json::from_value(json!({
                    "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
                })).unwrap()).await
            });
            let reached = tokio::time::timeout(std::time::Duration::from_secs(30),pause.entered.notified()).await.is_ok();
            let visible = engram.state.registry.active_source_generation(PROJECT);
            let intervention = (|| -> anyhow::Result<()> {
                if reached && step=="rejected CAS" {
                    let replacement=owned.path().join("repo/replacement");
                    std::fs::create_dir(&replacement)?;
                    let mut changed=original_project.clone();
                    changed.directory=replacement.to_string_lossy().into_owned();
                    engram.state.registry.put_project(&changed)?;
                }
                Ok(())
            })();
            drop(release);
            let response = match tokio::time::timeout(std::time::Duration::from_secs(30),&mut request).await {
                Ok(joined)=>joined.map_err(|error|anyhow::anyhow!("Owned foreground request failed: {error}")),
                Err(_)=>{request.abort();let _=request.await;Err(anyhow::anyhow!("Owned foreground request timed out"))}
            };
            finish_owned_jobs(&engram).await;
            intervention?;
            ensure!(reached,"Public foreground request did not reach its actual publication boundary");
            let response=response?;
            let generation=engram.state.registry.active_source_generation(PROJECT)?;
            if after_commit {
                ensure!(visible?==previous+1 && generation==previous+1,
                    "Postcommit interruption must preserve exactly the visible published generation");
                let response=response?;
                ensure!(response.is_error!=Some(true));
                let report=&response.content[0].as_text().unwrap().text;
                ensure!(report.contains("status: degraded") && report.contains(&format!("active_generation: {generation}"))
                    && report.contains(&format!("Source generation {generation}")) && report.contains("published"),
                    "Postcommit failure did not identify the committed degraded result: {report}");
                complete_foreground_source(&ps.search, generation, "foreground_published_marker", replacement_source).await?;
                let old_exists=engram.state.graph.get_node(PROJECT,legacy_id)?.is_some();
                match step {
                    "cancellation"=>ensure!(report.contains("published before cancellation"),"{report}"),
                    "source purge"=>{
                        ensure!(report.contains("source purge/settlement is incomplete"),"{report}");
                        ensure!(engram.state.registry.get_meta(PROJECT,"purge_pending")?.as_deref()==Some(generation.to_string().as_str()));
                    }
                    "purge settlement"=>{
                        ensure!(report.contains("clearing the pending flag failed"),"{report}");
                        ensure!(engram.state.registry.get_meta(PROJECT,"purge_pending")?.as_deref()==Some(previous.to_string().as_str()));
                    }
                    "scoped graph cleanup"=>ensure!(old_exists && report.contains("resolver refresh skipped"),"{report}"),
                    "App_Code derived-edge refresh"=>ensure!(!old_exists && report.contains("App_Code derived-edge refresh failure")
                        && report.contains("symbol resolver refresh skipped"),"{report}"),
                    _=>unreachable!(),
                }
            } else {
                ensure!(visible?==previous && generation==previous && response.is_err(),
                    "An unpublished candidate must remain a strict public failure");
                ensure!(!ps.search.lexical_search(&query(previous,"new_marker"))?.is_empty()
                    && ps.search.lexical_search(&query(previous,"foreground_published_marker"))?.is_empty()
                    && engram.state.graph.get_node(PROJECT,legacy_id)?.is_some(),
                    "Precommit failure changed the complete selected source or published graph declarations");
                ensure!(complete_foreground_source(&ps.search, previous, "new_marker", &original_source).await? == original_pk,
                    "Prepublication refusal must preserve the original physical source revision");
                let refusal=response.as_ref().err().unwrap().to_string();
                if step=="cancellation" {
                    ensure!(refusal.contains("update cancelled before source generation publication"),"{refusal}");
                    let retry=engram.handle_update_project(serde_json::from_value(json!({
                        "project_id":PROJECT,"wait":true,"max_commits":1,"index_antipatterns":false
                    }))?).await?;
                    let current=engram.state.registry.active_source_generation(PROJECT)?;
                    ensure!(retry.is_error!=Some(true) && current>previous
                        && !ps.search.lexical_search(&query(current,"foreground_published_marker"))?.is_empty()
                        && engram.state.graph.get_node(PROJECT,legacy_id)?.is_none(),
                        "A public retry must converge after cancellation without retaining stale declarations");
                    complete_foreground_source(&ps.search, current, "foreground_published_marker", replacement_source).await?;
                } else {
                    ensure!(refusal.contains("source generation changed while this update was staged"),"{refusal}");
                    ensure!(!engram.state.registry.source_head_is_current(&head)?
                        && engram.state.registry.get_project(PROJECT)?.unwrap().directory!=original_project.directory,
                        "A rejected CAS must preserve the competing identity correction, never restore an old token by ABA");
                }
            }
            eprintln!("FOREGROUND_PUBLICATION_COMPLETENESS {}", json!({
                "native_vectors":cfg!(feature="vector"),"after_commit":after_commit,"step":step,
                "previous_generation":previous,"final_generation":engram.state.registry.active_source_generation(PROJECT)?
            }));
            finish_owned_jobs(&engram).await;
            drop(ps);drop(engram);
            Ok::<(),anyhow::Error>(())
        })
    };
    let cleanup=owned.close();
    match (outcome,cleanup) {
        (Err(error),Err(cleanup))=>Err(error.context(format!("Owned fixture cleanup failed: {cleanup}"))),
        (Err(error),_)=>Err(error),(Ok(()),cleanup)=>cleanup.map_err(Into::into),
    }
}

#[test]
fn foreground_precommit_cancellation_retries_and_stale_cas_preserves_old_source() -> anyhow::Result<()> {
    for step in ["cancellation","rejected CAS"] {foreground_publication_case(false,step)?;}
    Ok(())
}

#[test]
fn foreground_postcommit_interruptions_report_committed_generation_and_exact_incomplete_step() -> anyhow::Result<()> {
    for step in ["cancellation","source purge","purge settlement","scoped graph cleanup","App_Code derived-edge refresh"] {
        foreground_publication_case(true,step)?;
    }
    Ok(())
}
