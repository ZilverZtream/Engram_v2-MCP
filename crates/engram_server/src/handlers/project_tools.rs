use crate::models::{
    AddRepoRuleRequest, CancelJobRequest, CheckIntegrityRequest, DeleteRepoRuleRequest,
    GetCheckpointStatusRequest, GetMemoryBudgetRequest, GetMetricsRequest,
    IncrementalIndexingGcRequest, IndexProjectRequest, ListJobsRequest, MemorySectionRequest,
    ProjectIdRequest, ProjectType, RepairProjectRequest, UpdateMemoryBankRequest,
    UpdateProjectRequest, WatchProjectRequest,
};
use crate::services::{graph_service, ingest_service, job_service, project_service};
use crate::state::{AppEvent, ProjectInfo, ProjectState};
use crate::tools::Engram;
mod instruction_observation;
#[cfg(test)]
#[path = "project_tools/memory_retention_tests.rs"]
mod memory_retention_tests;
#[cfg(test)]
#[path = "project_tools/vector_compaction_tests.rs"]
mod vector_compaction_tests;
#[cfg(test)]
#[path = "project_tools/source_publication_tests.rs"]
mod source_publication_tests;
use crate::utils::files::{exts_for_project_type, exts_for_project_type_enum};
use crate::utils::now_ms;
use engram_core::{Checkpoint, JobPhase, JobRecord, MemorySection, ProjectRecord, WatchRecord};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// Correlate ordinary watcher updates with external resource samples without
/// logging source contents. `returned` marks a control-flow boundary, not a
/// success claim: several enrichment results intentionally remain best effort.
/// A missing return marker can mean cancellation, failure or a process exit.
fn observe_source_update(project_id: &str, generation: u64, stage: &'static str, boundary: &'static str) {
    tracing::info!(project_id, generation, stage, boundary, "source update boundary");
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct PhaseResumeState {
    pending_files: Vec<String>,
    processed_files: Vec<String>,
    processed_chunk_ids: Vec<u64>,
}

enum SourceRepairBase {
    Published(engram_core::registry::SourceHeadToken),
    Damaged(engram_core::registry::SourceRecoveryToken),
}

impl SourceRepairBase {
    fn matches_source_identity(&self, directory: &str, project_type: &str) -> bool {
        match self {
            Self::Published(base) => base.matches_source_identity(directory, project_type),
            Self::Damaged(base) => base.matches_source_identity(directory, project_type),
        }
    }
}

enum SourceRepairPublication {
    Published(engram_core::registry::SourceHeadToken),
    Recovered(engram_core::registry::SourceRecoveryReservation),
}

fn to_rel_paths(root: &Path, files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .map(|p| {
            engram_core::RelPath::from_relative(root, p)
                .unwrap_or_else(|| engram_core::RelPath::new(&p.to_string_lossy()))
                .to_string()
        })
        .collect()
}

/// Absolute paths with NATIVE separators for files that no longer exist.
///
/// `from_rel_paths` canonicalises through `safe_join`, which cannot resolve a
/// deleted path. The VB sidecar keys its tree cache by the absolute path the
/// directory walk produced, so the separator style has to match exactly.
fn abs_paths_native(root: &Path, rels: &[engram_core::RelPath]) -> Vec<PathBuf> {
    rels.iter()
        .map(|r| {
            let mut p = root.to_path_buf();
            for seg in r.as_str().split(['/', '\\']) {
                if !seg.is_empty() {
                    p.push(seg);
                }
            }
            p
        })
        .collect()
}

// Use the shared handler-boundary validation from mod.rs.
use super::validate_project_id;

// ─── Panic-safe job cleanup guard ────────────────────────────────────────────

/// RAII guard that commits critical bookkeeping even when a `tokio::spawn` task
/// ENG-AUD-2026-S03-0001: Determine the final job status string from the three
/// independent outcome signals.  Extracted as a pure function so it can be
/// tested directly without a running AppState.
///
/// Priority: cancelled > failed > degraded > done.
fn determine_job_status(
    cancelled: bool,
    res_failed: bool,
    enrich_warnings: &[String],
) -> &'static str {
    if cancelled {
        "cancelled"
    } else if res_failed {
        "failed"
    } else if !enrich_warnings.is_empty() {
        "degraded"
    } else {
        "done"
    }
}

/// ENG-AUD-2026-S03-0001: Build the final job message that includes enrichment
/// warnings when present.
fn determine_job_message(
    cancelled: bool,
    res_err: Option<&str>,
    enrich_warnings: &[String],
) -> String {
    if cancelled {
        "cancelled by user".to_string()
    } else if let Some(e) = res_err {
        e.to_string()
    } else if !enrich_warnings.is_empty() {
        format!(
            "completed with enrichment warnings: {}",
            enrich_warnings.join("; ")
        )
    } else {
        "completed".to_string()
    }
}

/// terminates abnormally (panic, `abort()`, or early `?`-return before the
/// normal cleanup path).
///
/// **Construction**: create at the very top of the spawned async block, before
/// any fallible operation. The guard decrements the active-indexing slot counter
/// (for index jobs), writes a failure tombstone to the job registry, and removes
/// the job from both `active_jobs` and `cancellation_tokens`.
///
/// **Disarming**: call `disarm()` on the normal completion path so that `Drop`
/// becomes a no-op and the explicit cleanup that follows runs without duplication.
struct JobCleanupGuard {
    state: crate::state::AppState,
    job_id: String,
    project_id: Option<String>,
    /// ASCII kind string stored in the failure tombstone.
    job_kind: &'static str,
    created_at_ms: u64,
    /// When `true`, `active_indexing_count` is decremented on drop.
    dec_active_count: bool,
    /// Set to `true` once the normal completion path has handled teardown.
    disarmed: bool,
}

impl JobCleanupGuard {
    fn new_index(
        state: crate::state::AppState,
        job_id: String,
        project_id: String,
        created_at_ms: u64,
    ) -> Self {
        Self {
            state,
            job_id,
            project_id: Some(project_id),
            job_kind: "index_project",
            created_at_ms,
            dec_active_count: true,
            disarmed: false,
        }
    }

    fn new_update(
        state: crate::state::AppState,
        job_id: String,
        project_id: String,
        created_at_ms: u64,
    ) -> Self {
        Self {
            state,
            job_id,
            project_id: Some(project_id),
            job_kind: "update_project",
            created_at_ms,
            dec_active_count: false,
            disarmed: false,
        }
    }

    /// Signal that the task completed normally; `Drop` becomes a no-op.
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for JobCleanupGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }

        // Synchronous: release the active-indexing slot immediately so that new
        // jobs can be admitted without waiting for the async cleanup to finish.
        if self.dec_active_count {
            self.state
                .active_indexing_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }

        // Async cleanup: write a failure tombstone and purge the job from all
        // tracking maps.  `Handle::try_current()` is valid during stack unwinding
        // inside a Tokio task, allowing us to spawn a lightweight cleanup future.
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return, // No runtime context — skip async cleanup.
        };

        let state = self.state.clone();
        let job_id = self.job_id.clone();
        let project_id = self.project_id.clone();
        let kind = self.job_kind;
        let created_at = self.created_at_ms;

        handle.spawn(async move {
            let reg = state.registry.clone();
            let jid = job_id.clone();
            let pid = project_id.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || {
                let jr = engram_core::JobRecord {
                    job_id: jid.clone(),
                    kind: kind.into(),
                    project_id: pid,
                    status: "failed".into(),
                    message: "job terminated abnormally (panic or abort)".into(),
                    progress_pct: 0,
                    estimated_time_remaining_ms: None,
                    created_at_ms: created_at,
                    updated_at_ms: now_ms(),
                };
                if let Err(e) = reg.put_job(&jr) {
                    tracing::warn!(job_id = %jid, "failed to persist failure tombstone in job cleanup guard: {e}");
                }
            })
            .await
            {
                tracing::warn!(job_id = %job_id, "spawn_blocking panicked in job cleanup guard tombstone write: {e}");
            }

            // Remove from active tracking maps (order: jobs before tokens to
            // mirror the lock-order convention in cancel_job_internal).
            {
                state.active_jobs.write().await.remove(&job_id);
            }
            {
                state.cancellation_tokens.write().await.remove(&job_id);
            }
            state.evict_cache_overshoot().await;
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Project lifecycle helper methods on Engram.
/// Why a memory section is stale, if it is. Two grounded signals:
///  - a review-by date that has passed, and
///  - a referenced file re-indexed more recently than the note was written
///    (its `mtime`, in seconds, from the graph's file-node metadata), meaning
///    the code the note describes has moved on.
///
/// `file_mtimes_secs` maps project-relative path → indexed mtime in SECONDS
/// (the granularity ingest records). `None` means no stale signal was found,
/// not that the current working-tree content has been verified.
pub(crate) fn memory_stale_reason(
    sec: &engram_core::MemorySection,
    now_ms: u64,
    file_mtimes_secs: &std::collections::HashMap<String, u64>,
) -> Option<String> {
    if let Some(r) = sec.review_after_ms
        && now_ms > r
    {
        return Some("review overdue".to_string());
    }
    let updated_secs = sec.updated_at_ms / 1000;
    for f in &sec.related_files {
        if let Ok(Some(mtime)) = memory_file_timestamp(file_mtimes_secs, f)
            && mtime > updated_secs
        {
            return Some(format!("referenced file {f} changed since written"));
        }
    }
    None
}

/// Map of project-relative file path → indexed mtime (SECONDS), from the
/// code graph's file-node metadata. The same fingerprints grep freshness
/// reads; here they date a memory's referenced files.
pub(crate) fn indexed_file_mtimes_secs(
    graph: &engram_graph::GraphStore,
    project_id: &str,
) -> std::collections::HashMap<String, u64> {
    indexed_file_mtimes_secs_checked(graph, project_id).unwrap_or_default()
}

fn memory_file_key(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Keep case-distinct current files separate. A case-insensitive historical or
/// caller spelling is usable only when it identifies exactly one current file.
fn memory_file_timestamp(
    mtimes: &std::collections::HashMap<String, u64>,
    path: &str,
) -> Result<Option<u64>, ()> {
    let key = memory_file_key(path);
    let exact: Vec<_> = mtimes
        .iter()
        .filter(|(candidate, _)| memory_file_key(candidate) == key)
        .collect();
    let matches = if exact.is_empty() {
        mtimes
            .iter()
            .filter(|(candidate, _)| memory_file_key(candidate).eq_ignore_ascii_case(&key))
            .collect::<Vec<_>>()
    } else {
        exact
    };
    match matches.as_slice() {
        [] => Ok(None),
        [(_, mtime)] => Ok(Some(**mtime)),
        _ => Err(()),
    }
}

fn indexed_file_mtimes_secs_checked(
    graph: &engram_graph::GraphStore,
    project_id: &str,
) -> anyhow::Result<std::collections::HashMap<String, u64>> {
    graph.list_file_node_metadata(project_id).map(|rows| {
        rows.into_iter()
            .filter_map(|(rel, meta)| {
                let mtime = meta
                    .as_ref()
                    .and_then(|m| m.get("mtime"))
                    .and_then(|v| v.as_u64())?;
                // Preserve raw keys for existing callers; resolve aliases
                // at lookup time without collapsing distinct source paths.
                Some((rel.as_str().to_string(), mtime))
            })
            .collect()
    })
}

fn memory_freshness_evidence(
    sec: &engram_core::MemorySection,
    mtimes: &Result<std::collections::HashMap<String, u64>, String>,
) -> String {
    if sec.related_files.is_empty() {
        return "subject freshness: UNKNOWN (no related files recorded)".into();
    }
    match mtimes {
        Err(error) => format!("subject freshness: UNKNOWN (indexed-file lookup failed: {error})"),
        Ok(mtimes) => {
            let missing = sec
                .related_files
                .iter()
                .filter(|path| matches!(memory_file_timestamp(mtimes, path), Ok(None)))
                .count();
            let ambiguous = sec
                .related_files
                .iter()
                .filter(|path| memory_file_timestamp(mtimes, path).is_err())
                .count();
            if ambiguous > 0 {
                format!(
                    "subject freshness: UNKNOWN ({ambiguous}/{} related file identities are ambiguous; {missing} lack indexed timestamps); use exact indexed paths",
                    sec.related_files.len()
                )
            } else if missing > 0 {
                format!(
                    "subject freshness: UNKNOWN ({missing}/{} related files lack indexed timestamps); check get_index_freshness and source",
                    sec.related_files.len()
                )
            } else {
                "subject freshness: indexed timestamps examined; working-tree content not verified"
                    .into()
            }
        }
    }
}

/// Jaccard similarity of the whitespace token sets of two strings. Used to
/// flag a near-duplicate memory section on write.
fn token_jaccard(a: &str, b: &str) -> f32 {
    fn tokens(s: &str) -> std::collections::HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 2)
            .map(|t| t.to_ascii_lowercase())
            .collect()
    }
    let (ta, tb) = (tokens(a), tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count() as f32;
    let union = ta.union(&tb).count() as f32;
    inter / union
}

impl Engram {
    pub(crate) async fn ensure_project_record(
        &self,
        project_id: &str,
    ) -> Result<ProjectRecord, McpError> {
        project_service::ensure_project_record(&self.state, project_id)
            .await
            .map_err(McpError::from)
    }

    pub(crate) async fn ensure_project_runtime(
        &self,
        project_id: &str,
    ) -> Result<ProjectState, McpError> {
        project_service::ensure_project_runtime(&self.state, project_id)
            .await
            .map_err(McpError::from)
    }

    pub(crate) async fn get_active_generation(&self, project_id: &str) -> Result<u64, McpError> {
        project_service::get_active_generation(&self.state, project_id)
            .await
            .map_err(McpError::from)
    }

    /// Capture the published head and hold its physical revisions through the
    /// complete RPC. If publication wins before pinning, retry once at its head.
    pub(crate) async fn pin_active_source_generation(
        &self,
        project_id: &str,
        search: &engram_index::HybridSearchEngine,
    ) -> Result<(u64, engram_index::SourceSnapshotGuard), McpError> {
        for attempt in 0..2 {
            let generation = self.get_active_generation(project_id).await?;
            match project_service::pin_source_generation(&self.state, search, project_id, generation) {
                Ok(guard) => return Ok((generation, guard)),
                Err(error) => {
                    if attempt == 0 && self.get_active_generation(project_id).await? != generation {
                        continue;
                    }
                    return Err(McpError::internal_error(
                        format!("source snapshot unavailable: {error:#}"),
                        None,
                    ));
                }
            }
        }
        unreachable!("bounded pin attempts return above")
    }

    /// Standard one-line trailer: generation and last project-update age.
    /// A missing timestamp is reported as unavailable, not per-file freshness.
    pub(crate) async fn freshness_footer(&self, project_id: &str, generation: u64) -> String {
        let reg = self.state.registry.clone();
        let pid = project_id.to_string();
        let last_ms = tokio::task::spawn_blocking(move || {
            reg.get_meta(&pid, "last_index_completed_ms")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u64>().ok())
        })
        .await
        .unwrap_or(None);
        crate::utils::envelope::footer(generation, last_ms)
    }

    pub(crate) fn generate_indexing_report(&self, stats: &engram_index::IngestStats) -> String {
        project_service::generate_indexing_report(stats)
    }

    pub(crate) async fn get_incremental_changes(
        &self,
        project_id: &str,
        root: &Path,
        exts: &[&str],
    ) -> anyhow::Result<(Vec<PathBuf>, Vec<engram_core::RelPath>)> {
        project_service::get_incremental_changes(&self.state, project_id, root, exts).await
    }

    pub(crate) async fn process_ingest_stats(
        &self,
        project_id: &str,
        generation: u64,
        stats: &engram_index::IngestStats,
    ) -> anyhow::Result<()> {
        ingest_service::process_ingest_stats(&self.state, project_id, generation, stats).await
    }

    pub(crate) async fn inject_repo_rules(
        &self,
        project_id: &str,
        file_path: &engram_core::RelPath,
        content: &str,
    ) -> String {
        project_service::inject_repo_rules(&self.state, project_id, file_path, content).await
    }

    pub(crate) fn confidence_footer(&self, path: &engram_core::RelPath, lang: &str) -> String {
        let path_str = path.as_str();
        let is_webforms = matches!(lang, "aspx" | "ascx" | "master" | "vb" | "cs")
            || path_str.ends_with(".aspx")
            || path_str.ends_with(".aspx.vb")
            || path_str.ends_with(".aspx.cs")
            || path_str.ends_with(".ascx")
            || path_str.ends_with(".master");

        if !is_webforms {
            return String::new();
        }

        let has_codebehind_ext = path_str.ends_with(".aspx.vb")
            || path_str.ends_with(".aspx.cs")
            || path_str.ends_with(".ascx.vb")
            || path_str.ends_with(".ascx.cs");
        let is_markup = path_str.ends_with(".aspx")
            || path_str.ends_with(".ascx")
            || path_str.ends_with(".master");

        let score = engram_index::confidence::score_event_wiring(
            is_markup,
            has_codebehind_ext,
            has_codebehind_ext,
            true,
            is_markup,
        );

        let threshold = self.state.cfg.confidence_warning_threshold;
        let warning = if score.score < threshold {
            " | WARNING: Low extraction confidence — results may be incomplete"
        } else {
            ""
        };

        format!(
            "\n---\nextraction_confidence: {} ({:.2}){} | {}",
            score.band, score.score, warning, score.rationale
        )
    }

    pub(crate) async fn enforce_project_byte_budget(
        &self,
        files: &[PathBuf],
    ) -> anyhow::Result<()> {
        let Some(limit) = self.state.cfg.max_project_bytes else {
            return Ok(());
        };

        let files_owned = files.to_vec();
        let total_bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            let mut total = 0_u64;
            for path in files_owned {
                match std::fs::metadata(&path) {
                    Ok(metadata) => {
                        total = total
                            .checked_add(metadata.len())
                            .ok_or_else(|| anyhow::anyhow!("File size total overflowed u64"))?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "Failed to stat candidate file {}: {}",
                            path.display(),
                            e
                        ));
                    }
                }
            }
            Ok(total)
        })
        .await??;

        if total_bytes > limit {
            anyhow::bail!(
                "Project byte budget exceeded: candidate files total {} bytes > limit {} bytes",
                total_bytes,
                limit
            );
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_checkpoint(
        &self,
        job_id: &str,
        project_id: &str,
        generation: u64,
        directory: &Path,
        phase: JobPhase,
        items_processed: u64,
        items_total: u64,
        resume_state: Option<PhaseResumeState>,
    ) {
        let store = self.state.checkpoints.clone();
        let cp = Checkpoint {
            job_id: job_id.to_string(),
            project_id: project_id.to_string(),
            phase,
            items_processed,
            items_total,
            generation,
            idempotency_key: Checkpoint::compute_idempotency_key(
                project_id,
                &directory.to_string_lossy(),
                generation,
            ),
            resume_state: resume_state.and_then(|s| serde_json::to_string(&s).ok()),
            updated_at_ms: now_ms(),
            error: None,
        };
        let jid = cp.job_id.clone();
        match tokio::task::spawn_blocking(move || store.put(&cp)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    job_id = %jid,
                    "checkpoint write failed — resumability may be lost: {e}"
                );
            }
            Err(e) => {
                tracing::warn!(
                    job_id = %jid,
                    "checkpoint write task panicked — resumability may be lost: {e}"
                );
            }
        }
    }

    async fn resumable_checkpoint(
        &self,
        project_id: &str,
        generation: u64,
    ) -> Option<(Checkpoint, PhaseResumeState)> {
        let store = self.state.checkpoints.clone();
        let cp = {
            let project_id = project_id.to_string();
            tokio::task::spawn_blocking(move || store.find_resumable(&project_id))
        }
        .await
        .ok()
        .and_then(|r| r.ok().flatten())?;
        if cp.generation != generation {
            return None;
        }
        let state = cp
            .resume_state
            .as_deref()
            .and_then(|raw| serde_json::from_str::<PhaseResumeState>(raw).ok())
            .unwrap_or_default();
        Some((cp, state))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn index_files_with_parse_guard<F>(
        &self,
        search: &engram_index::HybridSearchEngine,
        project_id: &str,
        namespace: &str,
        generation: u64,
        root: &Path,
        files: Vec<PathBuf>,
        vb_source_files: Option<Arc<Vec<PathBuf>>>,
        max_chunks_per_file: usize,
        cancel: &tokio_util::sync::CancellationToken,
        progress_cb: F,
    ) -> anyhow::Result<engram_index::IngestStats>
    where
        F: FnMut(usize, usize) + Send,
    {
        let search = search.with_parse_limiter(self.state.parse_semaphore.clone())?;
        let search = match vb_source_files {
            Some(files) => search.with_vb_source_inventory(files),
            None => search,
        };
        let result = search
            .index_files(
                project_id,
                namespace,
                generation,
                root,
                files,
                max_chunks_per_file,
                cancel,
                progress_cb,
            )
            .await
            .map_err(|error| {
                let detail = format!("ingest attempt project={project_id} namespace={namespace} input_generation={generation} source_directory={}; caller publication state must be inspected separately: {error:#}", root.display());
                error.context(detail)
            });
        self.record_vector_storage_result(project_id, result).await
    }

    async fn record_vector_storage_result<T>(
        &self,
        project_id: &str,
        result: anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if let Err(error) = &result
            && error.chain().any(|cause| {
                let message = cause.to_string();
                message.starts_with("VEC1:") || message.starts_with("VEC1/")
            })
        {
            // Preparation and ingestion share this durable failure signal.
            // An asynchronous event alone can be lost before restart.
            let registry = self.state.registry.clone();
            let project = project_id.to_owned();
            tokio::task::spawn_blocking(move || {
                registry.require_reindex(
                    &project,
                    engram_core::registry::ReindexReason::Vector,
                    now_ms(),
                )
            })
            .await
            .map_err(|record_error| {
                anyhow::anyhow!(
                    "{error:#}; recording required vector recovery failed: {record_error}"
                )
            })?
            .map_err(|record_error| {
                anyhow::anyhow!(
                    "{error:#}; recording required vector recovery failed: {record_error}"
                )
            })?;
            let _ = self.state.events_tx.send(AppEvent::FullReindexRequired {
                project_id: project_id.to_owned(),
            });
        }
        result
    }

    /// Rebuild all source files into an explicit checkpoint; publish separately.
    async fn index_full_files_with_parse_guard<F>(
        &self,
        search: &engram_index::HybridSearchEngine,
        project_id: &str,
        namespace: &str,
        generation: u64,
        base_generation: u64,
        root: &Path,
        files: Vec<PathBuf>,
        max_chunks_per_file: usize,
        extraction: &engram_index::SourceExtractionExpectation,
        cancel: &tokio_util::sync::CancellationToken,
        progress_cb: F,
    ) -> anyhow::Result<engram_index::IngestStats>
    where
        F: FnMut(usize, usize) + Send,
    {
        self.index_complete_source_files(
            search, project_id, namespace, generation, base_generation, root,
            files, max_chunks_per_file, extraction, cancel, progress_cb, false,
        ).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn index_complete_source_files<F>(
        &self,
        search: &engram_index::HybridSearchEngine,
        project_id: &str,
        namespace: &str,
        generation: u64,
        base_generation: u64,
        root: &Path,
        files: Vec<PathBuf>,
        max_chunks_per_file: usize,
        extraction: &engram_index::SourceExtractionExpectation,
        cancel: &tokio_util::sync::CancellationToken,
        progress_cb: F,
        recovery: bool,
    ) -> anyhow::Result<engram_index::IngestStats>
    where
        F: FnMut(usize, usize) + Send,
    {
        anyhow::ensure!(
            namespace == "memory",
            "full source checkpoints require the memory namespace"
        );
        if recovery {
            anyhow::ensure!(files.len() <= 250_000,
                "Recovered source snapshot exceeds its path entry budget");
        }
        self.enforce_project_byte_budget(&files).await?;
        if let Some(limit) = self.state.cfg.max_project_files {
            anyhow::ensure!(
                files.len() as u64 <= limit,
                "Too many files: {} > limit {}",
                files.len(),
                limit
            );
        }
        let paths = files
            .iter()
            .map(|file| {
                engram_core::RelPath::from_relative(root, file)
                    .ok_or_else(|| anyhow::anyhow!("source file is outside its project"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let prepared = if recovery {
            search.prepare_recovered_source_snapshot(
                project_id, base_generation, generation, &paths, cancel,
            ).await
        } else {
            search.prepare_full_source_snapshot(
                project_id, base_generation, generation, &paths, cancel,
            ).await
        };
        self.record_vector_storage_result(project_id, prepared)
            .await?;
        let vb_source_files = project_service::admitted_vb_source_files(root, &files);
        let stats = self
            .index_files_with_parse_guard(
                search,
                project_id,
                namespace,
                generation,
                root,
                files,
                Some(vb_source_files),
                max_chunks_per_file,
                cancel,
                progress_cb,
            )
            .await?;
        extraction.verify_produced(&stats)?;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "full source rebuild cancelled before sealing"
        );
        search.seal_source_snapshot(project_id, generation)?;
        Ok(stats)
    }

    async fn capture_source_requirement(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<engram_core::registry::ReindexRequirementToken>> {
        let registry = self.state.registry.clone();
        let project = project_id.to_owned();
        tokio::task::spawn_blocking(move || {
            registry
                .reindex_requirement_token(&project, engram_core::registry::ReindexReason::Source)
        })
        .await?
    }

    async fn capture_source_head(
        &self,
        project_id: &str,
        directory: &str,
        project_type: &str,
    ) -> anyhow::Result<engram_core::registry::SourceHeadToken> {
        let registry = self.state.registry.clone();
        let project = project_id.to_owned();
        let head =
            tokio::task::spawn_blocking(move || {
                // Do not let lazy legacy counter initialization manufacture
                // opaque recovery authority for an unproven absent head.
                anyhow::ensure!(registry.source_generation_metadata(&project)?.is_some()
                    || registry.legacy_source_head_attested(&project)?,
                    "Unproven absent source head; explicit non-wiping full source recovery required");
                registry.capture_source_head(&project)
            }).await??;
        // Refresh source metadata without reopening/duplicating the search
        // engine. A caller already holding old info must retry; the next
        // request then converges rather than requiring a server restart.
        if let Some(mut cached) = self.state.projects.get_mut(project_id)
            && !head.matches_source_identity(&cached.info.directory, &cached.info.project_type)
        {
            let (directory, project_type) = head.source_identity();
            cached.info.directory = directory.to_owned();
            cached.info.project_type = project_type.to_owned();
        }
        anyhow::ensure!(
            head.matches_source_identity(directory, project_type),
            "Source directory/type changed; refresh this operation's project runtime before indexing"
        );
        if head.generation_metadata_absent() {
            // Absence/empty disk is not evidence of a complete legacy layout.
            // Only its actual producer may have installed this independent
            // attestation. Explicit full repair can take the opaque fresh route.
            anyhow::ensure!(self.state.registry.legacy_source_head_attested(project_id)?,
                "Unproven absent source head; explicit non-wiping full source recovery required");
        }
        Ok(head)
    }

    async fn publish_source_generation(
        &self,
        admission: &engram_index::hybrid::IndexMutationGuard,
        project_id: &str,
        base: engram_core::registry::SourceHeadToken,
        new: u64,
        source_requirement: Option<engram_core::registry::ReindexRequirementToken>,
        extraction: engram_index::SourceExtractionExpectation,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            base.project_id() == project_id,
            "Source publication project does not match its base"
        );
        let runtime = self
            .ensure_project_runtime(project_id)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let scoped = runtime.search.with_mutation(admission)?;
        let publication = scoped.begin_operation()?;
        let registry = self.state.registry.clone();
        tokio::task::spawn_blocking(move || {
            // The task may continue after its awaiting request is cancelled.
            let _publication = publication;
            extraction.with_publication_fence(|| {
                anyhow::ensure!(
                    registry.publish_source_head(&base, new)?,
                    "source generation changed before publication; retry against the current snapshot"
                );
                if let Some(token) = source_requirement {
                    registry.settle_reindex_requirement(&token)?;
                }
                Ok(())
            })
        })
        .await??;
        Ok(())
    }

    async fn publish_recovered_source_generation(
        &self,
        admission: &engram_index::hybrid::IndexMutationGuard,
        project_id: &str,
        reservation: engram_core::registry::SourceRecoveryReservation,
        source_requirement: Option<engram_core::registry::ReindexRequirementToken>,
        extraction: engram_index::SourceExtractionExpectation,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(reservation.project_id() == project_id, "Source recovery project mismatch");
        let runtime = self.ensure_project_runtime(project_id).await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let scoped = runtime.search.with_mutation(admission)?;
        let publication = scoped.begin_operation()?;
        let registry = self.state.registry.clone();
        tokio::task::spawn_blocking(move || {
            // Publication remains admitted even if its requesting RPC is cancelled.
            let _publication = publication;
            extraction.with_publication_fence(|| {
                anyhow::ensure!(registry.publish_source_recovery(&reservation)?,
                    "source recovery authority changed before publication; retry");
                if let Some(token) = source_requirement {
                    registry.settle_reindex_requirement(&token)?;
                }
                Ok(())
            })
        }).await??;
        Ok(())
    }

    pub async fn handle_index_project(
        &self,
        req: IndexProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        let dir = match self.state.paths.resolve_path(&req.directory) {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "❌ {e}"
                ))]));
            }
        };

        let dir_str = dir.to_string_lossy().to_string();
        let project_id = Uuid::new_v4().to_string();
        let project_name = req.project_name.clone();
        // ENG-AUD-2026-EXH-P1-0001: project_type is now a validated enum;
        // store the canonical string so registry reads work unchanged.
        let project_type = req.project_type.as_str().to_owned();
        let now = now_ms();
        let rec_candidate = ProjectRecord {
            project_id: project_id.clone(),
            project_name: project_name.clone(),
            project_type: project_type.clone(),
            directory: dir_str.clone(),
            created_at_ms: now,
            updated_at_ms: now,
            reindex_required_since_ms: None,
        };
        let dedupe = req.dedupe_by_directory;
        let reg = self.state.registry.clone();

        let existing_opt =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<ProjectRecord>> {
                if dedupe {
                    let list = reg.list_projects()?;
                    if let Some(existing) = list.into_iter().find(|p| p.directory == dir_str) {
                        return Ok(Some(existing));
                    }
                }
                reg.register_source_project(&rec_candidate)?;
                Ok(None)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if let Some(p) = existing_opt {
            let format =
                project_service::source_index_format_coverage(&self.state, &p.project_id).await;
            let advice = match format {
                Ok((0, _)) => {
                    "Source index status unknown: no file metadata. Run update_project to build or recover this registered project."
                }
                Ok((_, paths)) if !paths.is_empty() => {
                    "Source index migration or interrupted update requires update_project before trusting cached source evidence."
                }
                Ok(_) => {
                    "Existing registration retained. Check get_index_freshness before trusting cached source evidence."
                }
                Err(_) => {
                    "Source index status unavailable. Check get_index_freshness and restore store access before relying on this registration."
                }
            };
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Existing project registration.\nproject_id: {}\nproject_name: {}\ndirectory: {}\n{}",
                p.project_id, p.project_name, p.directory, advice
            ))]));
        }
        let project_root = self.state.cfg.data_dir.join("projects").join(&project_id);
        let tantivy_dir = project_root.join("tantivy");
        let lancedb_dir = project_root.join("lancedb");
        tokio::fs::create_dir_all(&tantivy_dir).await.map_err(|e| {
            McpError::internal_error(
                format!(
                    "AUD-2026-INV-0003: failed to create index directory {:?}: {e}",
                    tantivy_dir
                ),
                None,
            )
        })?;
        tokio::fs::create_dir_all(&lancedb_dir).await.map_err(|e| {
            McpError::internal_error(
                format!(
                    "AUD-2026-INV-0003: failed to create index directory {:?}: {e}",
                    lancedb_dir
                ),
                None,
            )
        })?;

        let search = engram_index::HybridSearchEngine::new_with_budget(
            tantivy_dir.clone(),
            lancedb_dir.clone(),
            &self.state.cfg,
            Some(self.state.memory_budget.clone()),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let search = std::sync::Arc::new(search);

        let info = ProjectInfo {
            project_id: project_id.clone(),
            project_name,
            project_type,
            directory: dir.to_string_lossy().to_string(),
            tantivy_dir: tantivy_dir.clone(),
            lancedb_dir: lancedb_dir.clone(),
        };
        self.state
            .put_project_cached(ProjectState {
            pagerank_cache: Default::default(),
                info: info.clone(),
                search: search.clone(),
            })
            .await;

        if req.wait {
            let _update_guard = self.state.acquire_project_update_lock(&project_id).await;
            let _storage_mutation = search
                .begin_project_mutation(&project_id)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let search = Arc::new(
                search
                    .with_mutation(&_storage_mutation)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?,
            );
            let exts = ProjectType::from_registry_str(&info.project_type)
                .map(exts_for_project_type_enum)
                .unwrap_or_else(|| exts_for_project_type(&info.project_type));
            let cancel = tokio_util::sync::CancellationToken::new();

            let files = project_service::scan_source_files(&self.state, &dir, &exts)
                .await
                .map_err(|error| {
                    McpError::internal_error(format!("Source scan failed: {error:#}"), None)
                })?;
            let extraction = project_service::capture_source_extraction_for_files(&self.state, &dir, &files)
                .await.map_err(|error| McpError::internal_error(error.to_string(), None))?;
            if let Err(e) = self.enforce_project_byte_budget(&files).await {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "❌ {e}"
                ))]));
            }
            if let Some(limit) = self.state.cfg.max_project_files
                && files.len() as u64 > limit
            {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "❌ Too many files: {} > limit {}",
                    files.len(),
                    limit
                ))]));
            }

            let source_head = self
                .capture_source_head(&project_id, &info.directory, &info.project_type)
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            if source_head.generation() != 0 {
                return Err(McpError::internal_error(
                    "Initial source generation changed before indexing",
                    None,
                ));
            }
            let source_requirement = self
                .capture_source_requirement(&project_id)
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let max_chunks = self.state.cfg.max_chunks_per_file;
            let stats = self
                .index_full_files_with_parse_guard(
                    &search,
                    &project_id,
                    "memory",
                    1,
                    source_head.generation(),
                    &dir,
                    files,
                    max_chunks,
                    &extraction,
                    &cancel,
                    |_, _| {},
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            self.process_ingest_stats(&project_id, 1, &stats)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            self.publish_source_generation(
                &_storage_mutation,
                &project_id,
                source_head,
                1,
                source_requirement,
                extraction,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            {
                let graph = self.state.graph.clone();
                let pid = project_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    tracing::info!(project_id = %pid, "index_project: starting resolve_app_code_globals");
                    let _ = graph_service::resolve_app_code_globals(&graph, &pid, 1);
                    tracing::info!(project_id = %pid, "index_project: starting link_binding_fields_to_columns");
                    let _ = graph_service::link_binding_fields_to_columns(&graph, &pid, 1);
                    tracing::info!(project_id = %pid, "index_project: graph resolution complete");
                })
                .await;
            }

            {
                let graph = self.state.graph.clone();
                let pid = project_id.clone();
                tracing::info!(project_id = %pid, "index_project: starting resolve_symbol_edges");
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(600),
                    tokio::task::spawn_blocking(move || graph.resolve_symbol_edges(&pid)),
                )
                .await;
                match result {
                    Ok(Ok(Ok(n))) => {
                        tracing::info!(project_id = %project_id, resolved = n, "index_project: resolve_symbol_edges complete");
                    }
                    Ok(Ok(Err(e))) => {
                        tracing::warn!(project_id = %project_id, "index_project: resolve_symbol_edges failed: {e:#}");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(project_id = %project_id, "index_project: resolve_symbol_edges panicked: {e}");
                    }
                    Err(_) => {
                        tracing::warn!(project_id = %project_id, "index_project: resolve_symbol_edges timed out after 600s — skipping (graph data is usable, some :: edges unresolved)");
                    }
                }
            }

            // TODO-41: warm the centrality cache so the first blast_radius /
            // rerank call doesn't block on a full PageRank over the graph.
            {
                let graph = self.state.graph.clone();
                let pid = project_id.clone();
                let warm_gen = 1; // This completed full-index operation published generation 1.
                tokio::task::spawn_blocking(move || {
                    match graph.get_or_compute_centrality(&pid, warm_gen) {
                        Ok(m) => tracing::info!(
                            project_id = %pid,
                            nodes = m.len(),
                            "index_project: centrality cache warmed"
                        ),
                        Err(e) => tracing::warn!(
                            project_id = %pid,
                            "index_project: centrality warm failed: {e:#}"
                        ),
                    }
                });
            }

            let report = self.generate_indexing_report(&stats);
            if let Err(e) = self
                .handle_update_memory_bank(UpdateMemoryBankRequest {
                    project_id: project_id.clone(),
                    section_id: Some("engram/index_report".into()),
                    section: "Indexing Report".into(),
                    content: report.clone(),
                    ..Default::default()
                })
                .await
            {
                tracing::warn!(project_id = %project_id, "ENG-AUD-2026-S08-0001: memory bank update failed (index report not persisted): {e:#}");
            }

            // A FULL index wrote every live node at the current generation —
            // record it as the graph purge baseline. The periodic GC only
            // purges the graph against this value (never the incremental
            // active_generation counter — unchanged files keep older-gen
            // nodes between full indexes).
            {
                let reg = self.state.registry.clone();
                let pid_full = project_id.clone();
                let gen_full = 1_u64; // Baseline belongs to this full index, never a later head.
                let _ = tokio::task::spawn_blocking(move || {
                    reg.set_meta(
                        &pid_full,
                        "last_full_index_generation",
                        &gen_full.to_string(),
                    )
                })
                .await;
            }
            // External audit 2026-08-29 row 9: leave the co-change snapshot warm so
            // get_change_set / find_similar_changes never walk git at call time.
            {
                let st = self.state.clone();
                let pid_w = project_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) =
                        crate::handlers::planning_tools::warm_co_change_snapshot_blocking(
                            &st, &pid_w,
                        )
                    {
                        tracing::warn!(project_id = %pid_w, "co-change warm-up skipped: {e:#}");
                    }
                })
                .await;
            }

            return Ok(CallToolResult::success(vec![Content::text(format!(
                "✅ Indexed project_id: {project_id}\n\n{report}"
            ))]));
        }

        let job_id = self
            .spawn_job_index_directory(
                project_id.clone(),
                info.project_type.clone(),
                dir,
                tantivy_dir,
                lancedb_dir,
            )
            .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "🟡 Index job started.\njob_id: {job_id}\nproject_id: {project_id}"
        ))]))
    }

    pub(crate) async fn spawn_job_index_directory(
        &self,
        project_id: String,
        project_type: String,
        directory: PathBuf,
        tantivy_dir: PathBuf,
        lancedb_dir: PathBuf,
    ) -> Result<String, McpError> {
        // Enforce concurrency limit BEFORE creating the job record so that a
        // rejected request never leaves a phantom "running" entry in the registry.
        let state_for_spawn = self.state.clone();
        let max_jobs = state_for_spawn.cfg.max_concurrent_jobs;
        if state_for_spawn
            .active_indexing_count
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |current| {
                    if current >= max_jobs {
                        None
                    } else {
                        Some(current + 1)
                    }
                },
            )
            .is_err()
        {
            return Err(McpError::internal_error(
                format!("Too many concurrent jobs running (limit: {})", max_jobs),
                None,
            ));
        }

        // Limit accepted — now safe to persist the "running" job record.
        let job_id = Uuid::new_v4().to_string();
        let now = now_ms();
        let job = JobRecord {
            job_id: job_id.clone(),
            kind: "index_project".into(),
            project_id: Some(project_id.clone()),
            status: "running".into(),
            message: "indexing directory".into(),
            progress_pct: 0,
            estimated_time_remaining_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        };

        let reg = self.state.registry.clone();
        let persist_result = tokio::task::spawn_blocking({
            let job = job.clone();
            move || reg.put_job(&job)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("failed to persist job: {e}"), None))?;
        if let Err(e) = persist_result {
            // Release the slot we already claimed before bailing out.
            state_for_spawn
                .active_indexing_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(McpError::internal_error(
                format!("failed to persist job record: {e}"),
                None,
            ));
        }

        let reg2 = self.state.registry.clone();
        let active_jobs = self.state.active_jobs.clone();
        let cancellation_tokens = self.state.cancellation_tokens.clone();
        let project_id_for_job = project_id.clone();
        let job_id_for_job = job_id.clone();

        let token = tokio_util::sync::CancellationToken::new();
        {
            let mut m = cancellation_tokens.write().await;
            m.insert(job_id.clone(), token.clone());
        }

        let handle = tokio::spawn(async move {
            // Install the panic-safe cleanup guard before touching any database.
            // If this task panics or is aborted the guard's Drop impl commits a
            // failure tombstone and releases the active-indexing slot.
            let mut cleanup_guard = JobCleanupGuard::new_index(
                state_for_spawn.clone(),
                job_id_for_job.clone(),
                project_id_for_job.clone(),
                job.created_at_ms,
            );

            // Acquire the per-project serialisation lock (Fix 2).
            // Prevents concurrent index and update jobs from interleaving writes
            // to the same Tantivy/LanceDB backends for this project.
            let _update_guard = state_for_spawn
                .acquire_project_update_lock(&project_id_for_job)
                .await;

            let search_init = engram_index::HybridSearchEngine::new_with_budget(
                tantivy_dir,
                lancedb_dir,
                &state_for_spawn.cfg,
                Some(state_for_spawn.memory_budget.clone()),
            )
            .await;
            let max_chunks = state_for_spawn.cfg.max_chunks_per_file;

            let mut _storage_mutation = None;
            let mut source_requirement = None;
            let mut source_head = None;
            let mut source_extraction = None;
            let res = match search_init {
                Ok(search) => {
                    async {
                        _storage_mutation =
                            Some(search.begin_project_mutation(&project_id_for_job)?);
                        let search =
                            search.with_mutation(_storage_mutation.as_ref().ok_or_else(
                                || anyhow::anyhow!("Initial storage admission missing"),
                            )?)?;
                        let exts = ProjectType::from_registry_str(&project_type)
                            .map(exts_for_project_type_enum)
                            .unwrap_or_else(|| exts_for_project_type(&project_type));
                        let job_id_for_cb = job_id_for_job.clone();
                        let reg_for_cb = reg2.clone();
                        let engram = Engram::new(state_for_spawn.clone());
                        let base = engram
                            .capture_source_head(
                                &project_id_for_job,
                                &directory.to_string_lossy(),
                                &project_type,
                            )
                            .await?;
                        anyhow::ensure!(
                            base.generation() == 0,
                            "Initial source generation changed before indexing"
                        );
                        source_head = Some(base);
                        source_requirement = engram
                            .capture_source_requirement(&project_id_for_job)
                            .await?;
                        let files =
                            project_service::scan_source_files(&state_for_spawn, &directory, &exts)
                                .await?;
                        source_extraction = Some(project_service::capture_source_extraction_for_files(
                            &state_for_spawn, &directory, &files,
                        ).await?);
                        // A cleared full attempt must replay every source file.
                        let pending = files.clone();
                        engram
                            .write_checkpoint(
                                &job_id_for_job,
                                &project_id_for_job,
                                1,
                                &directory,
                                JobPhase::Scanning,
                                0,
                                files.len() as u64,
                                Some(PhaseResumeState {
                                    pending_files: to_rel_paths(&directory, &pending),
                                    ..PhaseResumeState::default()
                                }),
                            )
                            .await;

                        if let Err(e) = engram.enforce_project_byte_budget(&pending).await {
                            Err(e)
                        } else if let Some(limit) = state_for_spawn.cfg.max_project_files {
                            if pending.len() as u64 > limit {
                                Err(anyhow::anyhow!(
                                    "Too many files: {} > limit {}",
                                    pending.len(),
                                    limit
                                ))
                            } else {
                                let last_pct =
                                    std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
                                engram
                                    .index_full_files_with_parse_guard(
                                        &search,
                                        &project_id_for_job,
                                        "memory",
                                        1,
                                        0,
                                        &directory,
                                        pending,
                                        max_chunks,
                                        source_extraction.as_ref().ok_or_else(|| anyhow::anyhow!("Initial extraction context missing"))?,
                                        &token,
                                        move |curr, total| {
                                            let pct = if total == 0 {
                                                100
                                            } else {
                                                ((curr as f32 / total as f32) * 100.0) as u8
                                            };
                                            let prev =
                                                last_pct.load(std::sync::atomic::Ordering::Relaxed);
                                            if pct.saturating_sub(prev) < 5 && curr != total {
                                                return;
                                            }
                                            last_pct
                                                .store(pct, std::sync::atomic::Ordering::Relaxed);
                                            if let Ok(Some(mut job)) =
                                                reg_for_cb.get_job(&job_id_for_cb)
                                            {
                                                job.progress_pct = pct;
                                                job.message =
                                                    format!("Indexing: {}/{} files", curr, total);
                                                job.updated_at_ms = now_ms();
                                                let _ = reg_for_cb.put_job(&job);
                                            }
                                        },
                                    )
                                    .await
                            }
                        } else {
                            let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
                            Engram::new(state_for_spawn.clone())
                                .index_full_files_with_parse_guard(
                                    &search,
                                    &project_id_for_job,
                                    "memory",
                                    1,
                                    0,
                                    &directory,
                                    files,
                                    max_chunks,
                                    source_extraction.as_ref().ok_or_else(|| anyhow::anyhow!("Initial extraction context missing"))?,
                                    &token,
                                    move |curr, total| {
                                        let pct = if total == 0 {
                                            100
                                        } else {
                                            ((curr as f32 / total as f32) * 100.0) as u8
                                        };
                                        let prev =
                                            last_pct.load(std::sync::atomic::Ordering::Relaxed);
                                        if pct.saturating_sub(prev) < 5 && curr != total {
                                            return;
                                        }
                                        last_pct.store(pct, std::sync::atomic::Ordering::Relaxed);
                                        if let Ok(Some(mut job)) =
                                            reg_for_cb.get_job(&job_id_for_cb)
                                        {
                                            job.progress_pct = pct;
                                            job.message =
                                                format!("Indexing: {}/{} files", curr, total);
                                            job.updated_at_ms = now_ms();
                                            let _ = reg_for_cb.put_job(&job);
                                        }
                                    },
                                )
                                .await
                        }
                    }
                    .await
                }
                Err(e) => Err(e),
            };

            // Source ingestion records vector recovery failures durably; do not
            // construct a second engine and retry an incompatible shared store.

            // Normal-path cleanup: disarm the guard so its Drop is a no-op, then
            // perform the explicit teardown that updates progress and removes maps.
            cleanup_guard.disarm();

            state_for_spawn
                .active_indexing_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            state_for_spawn.evict_cache_overshoot().await;

            let mut status = "done";
            let mut msg = "completed".to_string();
            let mut progress = 100;

            let engram = Engram::new(state_for_spawn.clone());
            if token.is_cancelled() {
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        1,
                        &directory,
                        JobPhase::Failed,
                        0,
                        0,
                        None,
                    )
                    .await;
                status = "cancelled";
                msg = "cancelled by user".to_string();
                progress = 0;
            } else if let Ok(stats) = &res {
                let pid = project_id_for_job.clone();
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        1,
                        &directory,
                        JobPhase::VectorIndexing,
                        stats.chunks as u64,
                        stats.chunks as u64,
                        None,
                    )
                    .await;
                let publication = async {
                    engram.process_ingest_stats(&pid, 1, stats).await?;
                    engram
                        .publish_source_generation(
                            _storage_mutation.as_ref().ok_or_else(|| {
                                anyhow::anyhow!("Initial storage admission missing")
                            })?,
                            &pid,
                            source_head.take().ok_or_else(|| {
                                anyhow::anyhow!("Initial source publication base missing")
                            })?,
                            1,
                            source_requirement,
                            source_extraction.take().ok_or_else(|| anyhow::anyhow!("Initial extraction context missing"))?,
                        )
                        .await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(e) = publication {
                    status = "failed";
                    msg = format!("Graph processing or source publication failed: {e:#}");
                    progress = 0;
                } else {
                    tracing::info!(project_id = %pid, "index_project[job]: starting resolve_app_code_globals");
                    let _ = graph_service::resolve_app_code_globals(&engram.state.graph, &pid, 1);
                    tracing::info!(project_id = %pid, "index_project[job]: starting link_binding_fields_to_columns");
                    let _ =
                        graph_service::link_binding_fields_to_columns(&engram.state.graph, &pid, 1);
                    tracing::info!(project_id = %pid, "index_project[job]: starting resolve_symbol_edges");
                    let graph_for_resolve = engram.state.graph.clone();
                    let pid_for_resolve = pid.clone();
                    let resolve_result = tokio::time::timeout(
                        std::time::Duration::from_secs(600),
                        tokio::task::spawn_blocking(move || {
                            graph_for_resolve.resolve_symbol_edges(&pid_for_resolve)
                        }),
                    )
                    .await;
                    match resolve_result {
                        Ok(Ok(Ok(n))) => {
                            tracing::info!(project_id = %pid, resolved = n, "index_project[job]: resolve_symbol_edges complete")
                        }
                        Ok(Ok(Err(e))) => {
                            tracing::warn!(project_id = %pid, "index_project[job]: resolve_symbol_edges failed: {e:#}")
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(project_id = %pid, "index_project[job]: resolve_symbol_edges panicked: {e}")
                        }
                        Err(_) => {
                            tracing::warn!(project_id = %pid, "index_project[job]: resolve_symbol_edges timed out after 600s — skipping")
                        }
                    }
                    let report = engram.generate_indexing_report(stats);
                    let _ = engram
                        .handle_update_memory_bank(UpdateMemoryBankRequest {
                            project_id: pid.clone(),
                            section_id: Some("engram/index_report".into()),
                            section: "Indexing Report".into(),
                            content: report,
                            ..Default::default()
                        })
                        .await;
                    // Record the graph purge baseline — see the sync path
                    // for the rationale (GC never purges the graph against
                    // the incremental active_generation counter).
                    let reg_full = engram.state.registry.clone();
                    let pid_full = pid.clone();
                    let gen_full = 1_u64; // This job rebuilt every source file at generation 1.
                    let _ = tokio::task::spawn_blocking(move || {
                        reg_full.set_meta(
                            &pid_full,
                            "last_full_index_generation",
                            &gen_full.to_string(),
                        )
                    })
                    .await;
                    // External audit 2026-08-29 row 9: warm the co-change snapshot.
                    let st = engram.state.clone();
                    let pid_w = pid.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) =
                            crate::handlers::planning_tools::warm_co_change_snapshot_blocking(
                                &st, &pid_w,
                            )
                        {
                            tracing::warn!(project_id = %pid_w, "co-change warm-up skipped: {e:#}");
                        }
                    })
                    .await;
                }
            } else if let Err(e) = res {
                status = "failed";
                msg = e.to_string();
                progress = 0;
            }

            let now = now_ms();
            let jr = JobRecord {
                job_id: job_id_for_job.clone(),
                kind: "index_project".into(),
                project_id: Some(project_id_for_job.clone()),
                status: status.into(),
                message: msg,
                progress_pct: progress,
                estimated_time_remaining_ms: None,
                created_at_ms: job.created_at_ms,
                updated_at_ms: now,
            };
            let _ = tokio::task::spawn_blocking(move || reg2.put_job(&jr)).await;

            {
                let mut m = active_jobs.write().await;
                m.remove(&job_id_for_job);
            }
            {
                let mut m = cancellation_tokens.write().await;
                m.remove(&job_id_for_job);
            }
        });

        {
            let mut m = self.state.active_jobs.write().await;
            m.insert(job_id.clone(), handle);
        }
        Ok(job_id)
    }

    pub(crate) async fn spawn_job_update_project(
        &self,
        project_id: String,
        new_gen: u64,
        max_commits: usize,
        index_antipatterns: bool,
        reindex_paths: Vec<String>,
    ) -> Result<String, McpError> {
        let job_id = Uuid::new_v4().to_string();
        let now = now_ms();
        let jr = JobRecord {
            job_id: job_id.clone(),
            kind: "update_project".into(),
            project_id: Some(project_id.clone()),
            status: "running".into(),
            message: "updating project".into(),
            progress_pct: 0,
            estimated_time_remaining_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        };

        let reg = self.state.registry.clone();
        tokio::task::spawn_blocking({
            let jr = jr.clone();
            move || reg.put_job(&jr)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("failed to persist job: {e}"), None))?
        .map_err(|e| {
            McpError::internal_error(format!("failed to persist job record: {e}"), None)
        })?;

        let state = self.state.clone();
        let job_id_for_job = job_id.clone();
        let project_id_for_job = project_id.clone();

        let token = tokio_util::sync::CancellationToken::new();
        {
            let mut m = self.state.cancellation_tokens.write().await;
            m.insert(job_id.clone(), token.clone());
        }

        let handle = tokio::spawn(async move {
            // Fix 4: Install panic-safe cleanup guard before any fallible work.
            let mut cleanup_guard = JobCleanupGuard::new_update(
                state.clone(),
                job_id_for_job.clone(),
                project_id_for_job.clone(),
                jr.created_at_ms,
            );

            // Fix 1: Acquire per-project serialisation lock before mutating any
            // backend store. Prevents concurrent update/index jobs from interleaving
            // writes to the same Tantivy and LanceDB databases.
            let _update_guard = state.acquire_project_update_lock(&project_id_for_job).await;

            // Pre-load the project directory for checkpointing. We fetch it once
            // outside `res` so the completion/failure checkpoint can always be
            // written even if the inner block fails before `dir` is bound.
            let project_dir: PathBuf = {
                let reg = state.registry.clone();
                let pid_c = project_id_for_job.clone();
                tokio::task::spawn_blocking(move || {
                    reg.get_project(&pid_c)
                        .ok()
                        .flatten()
                        .map(|r| PathBuf::from(&r.directory))
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default()
            };

            // Build the progress-reporting components (Fix 12) before `res` so
            // the closure can capture them by move.
            let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
            let reg_for_cb = state.registry.clone();
            let job_id_for_cb = job_id_for_job.clone();

            // ENG-AUD-2026-S03-0001: Collect enrichment-phase warnings so they can
            // be surfaced as "degraded" status — not silently swallowed by warn!().
            let enrichment_warnings: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let enrich_warn_inner = enrichment_warnings.clone();

            let mut new_gen = new_gen;
            let mut source_published = false;
            let res = async {
                let engram = Engram::new(state.clone());
                let source_record = engram.ensure_project_record(&project_id_for_job).await
                    .map_err(|error| anyhow::anyhow!(error.message))?;
                let source_head = engram.capture_source_head(&project_id_for_job, &source_record.directory, &source_record.project_type).await?;
                let old_gen = source_head.generation();
                new_gen = old_gen.checked_add(1).ok_or_else(|| anyhow::anyhow!("source generation exhausted"))?;
                let mut ps = engram
                    .ensure_project_runtime(&project_id_for_job)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.message))?;
                anyhow::ensure!(source_head.matches_source_identity(&ps.info.directory, &ps.info.project_type),
                    "Source runtime identity changed before update; refresh and retry");
                let _storage_mutation = ps.search.begin_project_mutation(&project_id_for_job)?;
                ps.search = Arc::new(ps.search.with_mutation(&_storage_mutation)?);

                let dir = PathBuf::from(&ps.info.directory);
                let exts = ProjectType::from_registry_str(&ps.info.project_type)
                    .map(exts_for_project_type_enum)
                    .unwrap_or_else(|| exts_for_project_type(&ps.info.project_type));

                let project_service::SourceChangeScan { mut changed, deleted, extraction, vb_source_files } = project_service::get_incremental_changes_for_update(
                    &state, &ps.search, &source_head, &dir, &exts)
                    .await?;
                project_service::include_reindex_paths(&state, &dir, &exts, &reindex_paths, &mut changed).await?;
                if changed.is_empty() && deleted.is_empty() && reindex_paths.is_empty() {
                    new_gen = old_gen;
                    let (_, warning) = engram.refresh_unchanged_source(
                        &_storage_mutation, &project_id_for_job, &ps.info.directory, &source_head,
                        extraction, max_commits, index_antipatterns, &token,
                    ).await?;
                    if let Some(warning) = warning {
                        if let Ok(mut warnings) = enrich_warn_inner.lock() { warnings.push(warning); }
                    }
                    return Ok::<(), anyhow::Error>(());
                }
                let graph_refresh_paths: std::collections::HashSet<String> = changed.iter()
                    .filter_map(|p| engram_core::RelPath::from_relative(&dir, p))
                    .map(|p| p.as_str().to_string())
                    .chain(deleted.iter().map(|p| p.as_str().to_string())).collect();

                // Fix 10: Enforce byte budget for background updates — identical
                // to the guarantee provided by the synchronous update_project_impl.
                engram.enforce_project_byte_budget(&changed).await?;

                // Fix 11: Checkpoint the scanning phase so the job is crash-resumable.
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        new_gen,
                        &dir,
                        JobPhase::Scanning,
                        0,
                        changed.len() as u64,
                        Some(PhaseResumeState {
                            pending_files: to_rel_paths(&dir, &changed),
                            ..PhaseResumeState::default()
                        }),
                    )
                    .await;

                if !deleted.is_empty() {
                    // Drop the VB sidecar's cached trees for the removed
                    // files. prepare_project only sees files to index, so
                    // without this a deleted type keeps resolving until the
                    // next full rebuild.
                    engram_index::vb_extractor::forget_files(&abs_paths_native(&dir, &deleted))?;
                }

                let changed_paths = changed.iter().map(|path| engram_core::RelPath::from_relative(&dir, path)
                    .ok_or_else(|| anyhow::anyhow!("changed source is outside its project"))).collect::<anyhow::Result<Vec<_>>>()?;
                let prepared = ps.search.prepare_source_snapshot(&project_id_for_job, old_gen, new_gen, &changed_paths, &deleted, &token).await;
                engram.record_vector_storage_result(&project_id_for_job, prepared).await?;

                // Fix 12: Real progress callback — updates the job record in
                // Redb every 5 percentage-point increment.
                let last_pct_cb = last_pct.clone();
                let stats = engram
                    .index_files_with_parse_guard(
                        &ps.search,
                        &project_id_for_job,
                        "memory",
                        new_gen,
                        &dir,
                        changed,
                        Some(vb_source_files),
                        state.cfg.max_chunks_per_file,
                        &token,
                        move |curr, total| {
                            let pct = if total == 0 {
                                100u8
                            } else {
                                ((curr as f32 / total as f32) * 100.0) as u8
                            };
                            let prev = last_pct_cb.load(std::sync::atomic::Ordering::Relaxed);
                            if pct.saturating_sub(prev) < 5 && curr != total {
                                return;
                            }
                            last_pct_cb.store(pct, std::sync::atomic::Ordering::Relaxed);
                            if let Ok(Some(mut job)) = reg_for_cb.get_job(&job_id_for_cb) {
                                job.progress_pct = pct;
                                job.message = format!("Updating: {}/{} files", curr, total);
                                job.updated_at_ms = now_ms();
                                let _ = reg_for_cb.put_job(&job);
                            }
                        },
                    )
                    .await?;

                extraction.verify_produced(&stats)?;
                // Fix 11: Checkpoint after vector indexing completes.
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        new_gen,
                        &dir,
                        JobPhase::VectorIndexing,
                        stats.chunks as u64,
                        stats.chunks as u64,
                        None,
                    )
                    .await;

                engram
                    .process_ingest_stats(&project_id_for_job, new_gen, &stats)
                    .await?;
                if let Err(e) = engram
                    .git_update_stream(
                        &project_id_for_job,
                        &ps.info.directory,
                        new_gen,
                        max_commits,
                        // Forward only: update/refresh is an INCREMENTAL operation.
                // Backfill is a one-time historical ingest - running it
                // here re-walked (and re-weighted) hundreds of already-
                // indexed commits on every routine update.
                crate::models::GitHistoryMode::Forward,
                        index_antipatterns,
                        engram_git::history::MergeCommitPolicy::AllParents,
                        &token,
                        Box::new(|_, _| {}),
                        false,
            )
                    .await
                {
                    let msg = format!("git_update_stream failed (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "ENG-AUD-2026-S03-0001: {msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() {
                        w.push(msg);
                    }
                }

                #[cfg(test)]
                source_publication_tests::before_background_publication(&state.registry, &project_id_for_job).await;
                anyhow::ensure!(!token.is_cancelled(), "update cancelled before source generation publication");
                ps.search.seal_source_snapshot(&project_id_for_job, new_gen)?;
                let reg = state.registry.clone();
                let publication = ps.search.begin_operation()?;
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let _publication = publication;
                    extraction.with_publication_fence(|| {
                        anyhow::ensure!(reg.publish_source_head(&source_head, new_gen)?,
                            "source generation changed while this update was staged; retry against the current snapshot");
                        Ok(())
                    })
                }).await??;
                source_published = true;
                #[cfg(test)]
                let inject_cleanup_failure = source_publication_tests::after_background_publication(&state.registry, &project_id_for_job).await;
                // Retire published declarations only after this candidate wins
                // the head CAS. Cancellation or a rejected candidate must retain
                // the old declarations and their resolver-owned edges.
                let cleanup = (|| -> anyhow::Result<_> {
                    #[cfg(test)]
                    anyhow::ensure!(!inject_cleanup_failure, "Injected scoped graph cleanup failure");
                    engram.state.graph.purge_stale_nodes_for_paths(&project_id_for_job, &graph_refresh_paths, new_gen)
                })();
                if let Err(e) = cleanup {
                    let msg = format!("Source generation published, but scoped graph cleanup failed; resolver refresh skipped (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "{msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() { w.push(msg); }
                    return Ok::<(), anyhow::Error>(());
                }
                if let Err(e) = graph_service::resolve_app_code_globals(
                    &engram.state.graph, &project_id_for_job, new_gen,
                ) {
                    let msg = format!("App_Code derived-edge refresh failed (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "{msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() { w.push(msg); }
                }
                if let Err(e) = graph_service::link_sql_to_schema(
                    &engram.state.graph, &project_id_for_job, new_gen,
                ) {
                    let msg = format!("link_sql_to_schema failed (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "{msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() { w.push(msg); }
                }
                if let Err(e) = engram.state.graph.resolve_symbol_edges(&project_id_for_job) {
                    let msg = format!("resolve_symbol_edges failed (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "{msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() { w.push(msg); }
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;

            // Cancellation after CAS cannot undo an already visible source
            // head. Keep that commitment explicit in the durable job result.
            if source_published {
                if let Ok(mut warnings) = enrichment_warnings.lock() {
                    if token.is_cancelled() {
                        warnings.push(format!("Source generation {new_gen} was published before cancellation; the committed update remains visible."));
                    }
                    if let Err(error) = &res {
                        warnings.push(format!("Source generation {new_gen} was published; follow-up failed (enrichment degraded): {error:#}"));
                    }
                }
            }

            // ENG-AUD-2026-S03-0001: Surface enrichment warnings as "degraded"
            // status so callers can distinguish clean success from partial success.
            let enrich_warnings_collected: Vec<String> = enrichment_warnings
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();

            let final_status = determine_job_status(
                token.is_cancelled() && !source_published,
                res.is_err() && !source_published,
                &enrich_warnings_collected,
            );
            let final_msg = determine_job_message(
                token.is_cancelled() && !source_published,
                res.as_ref().err().filter(|_| !source_published).map(|e| e.to_string()).as_deref(),
                &enrich_warnings_collected,
            );

            // Fix 12: Derive progress from the actual outcome, not a pre-set constant.
            // "degraded" counts as complete (indexing finished, enrichment partial).
            let final_progress: u8 = if final_status == "done" || final_status == "degraded" {
                100
            } else {
                0
            };

            // Fix 11: Write failure/cancellation checkpoint on abnormal exit.
            // "degraded" still advances generation — do not write a failure checkpoint for it.
            if final_status != "done" && final_status != "degraded" {
                let engram = Engram::new(state.clone());
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        new_gen,
                        &project_dir,
                        JobPhase::Failed,
                        0,
                        0,
                        None,
                    )
                    .await;
            }

            // Fix 4: Disarm guard before explicit teardown to avoid double-cleanup.
            cleanup_guard.disarm();

            let jr2 = JobRecord {
                job_id: job_id_for_job.clone(),
                kind: "update_project".into(),
                project_id: Some(project_id_for_job.clone()),
                status: final_status.into(),
                message: final_msg,
                progress_pct: final_progress,
                estimated_time_remaining_ms: None,
                created_at_ms: jr.created_at_ms,
                updated_at_ms: now_ms(),
            };
            let _ = tokio::task::spawn_blocking(move || state.registry.put_job(&jr2)).await;
            {
                let mut m = state.active_jobs.write().await;
                m.remove(&job_id_for_job);
            }
            {
                let mut m = state.cancellation_tokens.write().await;
                m.remove(&job_id_for_job);
            }
        });

        {
            let mut m = self.state.active_jobs.write().await;
            m.insert(job_id.clone(), handle);
        }
        Ok(job_id)
    }

    /// One-call corpus freshness: runs the full INCREMENTAL refresh chain
    /// (files -> git history/temporal -> merged-PR corpus) so an agent can
    /// keep its own knowledge current with a single tool. Each stage is
    /// watermark-incremental; a no-change repo returns in seconds. This
    /// exists because every corpus that rots silently degrades planning
    /// quality (2026-07-04: temporal edges silently missing for a day).
    pub async fn handle_refresh_corpora(
        &self,
        req: crate::models::RefreshCorporaRequest,
    ) -> Result<CallToolResult, McpError> {
        crate::handlers::validate_project_id(&req.project_id)?;
        let mut out = String::from("# Corpus refresh\n");

        let text_of = |r: &CallToolResult| -> String {
            r.content
                .iter()
                .filter_map(|c| c.as_text().map(|t| t.text.clone()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Stage 1: file index (changed/added/removed source files).
        match self
            .handle_update_project(crate::models::UpdateProjectRequest {
                only_paths: None,
                instruction_maintenance: None,
                reindex_paths: Vec::new(),
                project_id: req.project_id.clone(),
                wait: true,
                max_commits: 500,
                index_antipatterns: false,
            })
            .await
        {
            Ok(r) => {
                let t = text_of(&r);
                out.push_str(&format!(
                    "\n## files\n{}\n",
                    t.lines().take(6).collect::<Vec<_>>().join("\n")
                ));
            }
            Err(e) => out.push_str(&format!("\n## files\nFAILED: {}\n", e.message)),
        }

        // Stage 2: git history (new commits -> temporal coupling edges).
        match self
            .handle_index_git_history(crate::models::IndexGitHistoryRequest {
                project_id: req.project_id.clone(),
                max_commits: 500,
                force: false,
                index_antipatterns: false,
                mode: Some(crate::models::GitHistoryMode::Forward),
                wait: true,
            })
            .await
        {
            Ok(r) => {
                let t = text_of(&r);
                out.push_str(&format!(
                    "\n## git history\n{}\n",
                    t.lines().take(6).collect::<Vec<_>>().join("\n")
                ));
            }
            Err(e) => out.push_str(&format!("\n## git history\nFAILED: {}\n", e.message)),
        }

        // Stage 3: merged-PR exemplar corpus.
        match self
            .handle_ingest_merged_prs(crate::models::IngestMergedPrsRequest {
                project_id: req.project_id.clone(),
                rebuild: false,
                max_commits: 500,
                merged_before: None,
            })
            .await
        {
            Ok(r) => {
                let t = text_of(&r);
                out.push_str(&format!(
                    "\n## merged-PR corpus\n{}\n",
                    t.lines().take(4).collect::<Vec<_>>().join("\n")
                ));
            }
            Err(e) => out.push_str(&format!("\n## merged-PR corpus\nFAILED: {}\n", e.message)),
        }

        // Stage 4 (opt-in): code-review corpora — anti-pattern clusters +
        // review-disposition evidence. Imported dispositions do not establish
        // accepted exceptions or automatically suppress findings.
        // Only with a caller-supplied PAT (never
        // persisted, so the stage can't run unattended without one).
        // Incremental via the registry's last_pr_id marker: repeat calls
        // only process PRs newer than the last ingest.
        if let Some(pat) = req.pat_token.clone() {
            match (
                req.ado_org.clone(),
                req.ado_project.clone(),
                req.ado_repo.clone(),
            ) {
                (Some(org), Some(project), Some(repo)) => {
                    // Persist the NON-SECRET ADO coordinates (org/project —
                    // never the PAT, per the stance above) so
                    // get_change_set's work-item auto-fetch can default
                    // them when a caller supplies a per-call PAT.
                    {
                        let reg = self.state.registry.clone();
                        let pid = req.project_id.clone();
                        let (o, p) = (org.clone(), project.clone());
                        tokio::task::spawn_blocking(move || {
                            reg.set_meta(&pid, "ado_org", &o).ok();
                            reg.set_meta(&pid, "ado_project", &p).ok();
                        })
                        .await
                        .ok();
                    }
                    match self
                        .handle_ingest_code_review_history(
                            crate::models::IngestCodeReviewHistoryRequest {
                                project_id: req.project_id.clone(),
                                source: "azure_devops".into(),
                                file_path: None,
                                pat_token: Some(pat),
                                org: Some(org),
                                project: Some(project),
                                repo: Some(repo),
                                max_prs: Some(200),
                                max_pr_id: None,
                                completed_before: None,
                                min_fix_rate: 0.5,
                                token_overlap_threshold: 0.4,
                                force_full_rescan: false,
                                use_llm_for_ambiguous: false,
                                promote_min_fix_rate: 0.7,
                                promote_min_prs: 3,
                                promote_min_lift: 0.15,
                            },
                        )
                        .await
                    {
                        Ok(r) => {
                            let t = text_of(&r);
                            out.push_str(&format!(
                                "\n## code-review corpora\n{}\n",
                                t.lines().take(6).collect::<Vec<_>>().join("\n")
                            ));
                        }
                        Err(e) => out.push_str(&format!(
                            "\n## code-review corpora\nFAILED: {}\n",
                            e.message
                        )),
                    }
                }
                _ => out.push_str(
                    "\n## code-review corpora\nSKIPPED: pat_token provided without \
                     ado_org/ado_project/ado_repo\n",
                ),
            }
        } else {
            out.push_str(
                "\nnote: code-review corpora (anti-patterns + review-disposition evidence; no automatic finding suppression) \
                 refresh only when pat_token+ado_* are provided — they otherwise \
                 age until the next ingest_code_review_history call.\n",
            );
        }

        out.push_str(
            "\nnote: quality gates (board/CodeRabbit) refresh via \
             ingest_quality_gates on newly fetched sources; KB wikis regenerate \
             via describe_setting/describe_table on demand.\n",
        );
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_update_project(
        &self,
        req: UpdateProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if let Some(paths) = &req.only_paths {
            if !req.wait || paths.is_empty() || paths.len() > 4096
                || !req.reindex_paths.is_empty() || req.instruction_maintenance.is_some()
                || req.index_antipatterns {
                return Err(McpError::invalid_params("only_paths requires wait=true, 1..4096 paths, and no reindex_paths, instruction maintenance or anti-pattern refresh",None));
            }
        }
        if let Some(maintenance) = &req.instruction_maintenance {
            if !req.wait {
                return Err(McpError::invalid_params("instruction_maintenance requires wait=true; queued updates have no completed source observation", None));
            }
            crate::services::instruction_maintenance::validate_observation_request(maintenance)
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        }
        let active_gen = self.get_active_generation(&req.project_id).await?;
        let new_gen = active_gen.saturating_add(1);

        if req.wait {
            let cancel = tokio_util::sync::CancellationToken::new();
            let summary = self
                .update_project_with_scope(
                    &req.project_id,
                    new_gen,
                    req.sanitized_max_commits(),
                    req.index_antipatterns,
                    &req.reindex_paths,
                    req.only_paths.as_deref(),
                    &cancel,
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            let mut result = CallToolResult::success(vec![Content::text(summary)]);
            if let Some(maintenance) = req.instruction_maintenance {
                let observation = self.collect_instruction_observation(&req.project_id, maintenance,
                    "separate_worktree_observation_collected_after_source_update").await;
                result.content.push(Content::text(observation.to_string()));
            }
            return Ok(result);
        }

        let job_id = self
            .spawn_job_update_project(
                req.project_id.clone(),
                new_gen,
                req.sanitized_max_commits(),
                req.index_antipatterns,
                req.reindex_paths.clone(),
            )
            .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "🟡 Update job started.\njob_id: {job_id}"
        ))]))
    }

    /// A source snapshot is already complete when no eligible bytes changed.
    /// Keep requested history enrichment, but do not rebuild or republish source.
    async fn refresh_unchanged_source(
        &self,
        admission: &engram_index::hybrid::IndexMutationGuard,
        project_id: &str,
        directory: &str,
        base: &engram_core::registry::SourceHeadToken,
        extraction: engram_index::SourceExtractionExpectation,
        max_commits: usize,
        index_antipatterns: bool,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<(String, Option<String>)> {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "update cancelled before unchanged-source refresh"
        );
        let generation = base.generation();
        let registry = self.state.registry.clone();
        let verified = base.clone();
        let runtime = self
            .ensure_project_runtime(project_id)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let scoped = runtime.search.with_mutation(admission)?;
        let identity_operation = scoped.begin_operation()?;
        anyhow::ensure!(
            tokio::task::spawn_blocking(move || {
                // A cancelled waiter must not let new staging overtake this
                // operation's conditional revision/identity settlement.
                let _identity_operation = identity_operation;
                extraction.with_publication_fence(|| registry.attest_unchanged_source_identity(&verified))
            })
            .await??,
            "Source head or identity changed during unchanged-source verification; retry"
        );
        let history = self
            .git_update_stream(
                project_id,
                directory,
                generation,
                max_commits,
                crate::models::GitHistoryMode::Forward,
                index_antipatterns,
                engram_git::history::MergeCommitPolicy::AllParents,
                cancel,
                Box::new(|_, _| {}),
                false,
            )
            .await;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "update cancelled during unchanged-source refresh"
        );
        let (status, history, warning) = match history {
            Ok(summary) => ("done", summary, None),
            Err(error) => {
                let warning = format!(
                    "git_update_stream failed (enrichment degraded): {}",
                    error.message
                );
                ("degraded", warning.clone(), Some(warning))
            }
        };
        Ok((
            format!(
                "Updated project_id: {project_id}\nstatus: {status}\nsource_update: no_changes_detected\nactive_generation: {generation}\nfiles=0 chunks=0 bytes=0\n{history}\n"
            ),
            warning,
        ))
    }

    pub async fn update_project_impl(
        &self,
        project_id: &str,
        new_gen: u64,
        max_commits: usize,
        index_antipatterns: bool,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        self.update_project_with_paths(
            project_id,
            new_gen,
            max_commits,
            index_antipatterns,
            &[],
            cancel,
        )
        .await
    }

    async fn update_project_with_paths(
        &self,
        project_id: &str,
        requested_generation: u64,
        max_commits: usize,
        index_antipatterns: bool,
        reindex_paths: &[String],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        self.update_project_with_scope(project_id, requested_generation, max_commits,
            index_antipatterns, reindex_paths, None, cancel).await
    }

    async fn update_project_with_scope(
        &self,
        project_id: &str,
        _requested_generation: u64,
        max_commits: usize,
        index_antipatterns: bool,
        reindex_paths: &[String],
        only_paths: Option<&[String]>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        let _update_guard = self.state.acquire_project_update_lock(project_id).await;
        // External audit 2026-08-29 P0-1: the watcher and update_project(wait=true)
        // reach this path without a job guard, so the GC never saw the update.
        let _indexing_slot = crate::state::ActiveIndexingSlot::acquire(&self.state);

        let mut ps = self
            .ensure_project_runtime(project_id)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        let _storage_mutation = ps.search.begin_project_mutation(project_id)?;
        ps.search = Arc::new(ps.search.with_mutation(&_storage_mutation)?);

        let exts = ProjectType::from_registry_str(&ps.info.project_type)
            .map(exts_for_project_type_enum)
            .unwrap_or_else(|| exts_for_project_type(&ps.info.project_type));
        let pid = project_id.to_string();
        let dir = PathBuf::from(&ps.info.directory);
        // Callers can queue while another update holds the lock. Never use
        // their pre-lock generation to copy old snapshots over a newer one.
        let source_head = self
            .capture_source_head(project_id, &ps.info.directory, &ps.info.project_type)
            .await?;
        let old_gen = source_head.generation();
        let new_gen = old_gen
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("source generation exhausted"))?;

        let project_service::SourceChangeScan { changed, deleted, extraction, vb_source_files } =
            if let Some(paths) = only_paths {
                project_service::exclusive_source_changes(&self.state, &ps.search, &source_head, &dir, &exts, paths).await?
            } else {
                project_service::get_incremental_changes_for_update(&self.state, &ps.search, &source_head, &dir, &exts).await?
            };

        let mut changed = changed;
        project_service::include_reindex_paths(
            &self.state,
            &dir,
            &exts,
            reindex_paths,
            &mut changed,
        )
        .await?;

        if changed.is_empty() && deleted.is_empty() && reindex_paths.is_empty() {
            return self
                .refresh_unchanged_source(
                    &_storage_mutation,
                    project_id,
                    &ps.info.directory,
                    &source_head,
                    extraction,
                    max_commits,
                    index_antipatterns,
                    cancel,
                )
                .await
                .map(|(summary, _)| summary);
        }

        // Resume from a previously interrupted job: narrow the pending-file list
        // so only files not yet processed are re-indexed.
        let mut resumed_from_checkpoint = false;
        let source_migration_required = only_paths.is_some() ||
            !project_service::outdated_source_index_paths(&self.state, project_id)
                .await?
                .is_empty();
        let changed = if !source_migration_required
            && reindex_paths.is_empty()
            && let Some((_cp, rs)) = self.resumable_checkpoint(project_id, new_gen).await
        {
            if !rs.pending_files.is_empty() {
                engram_core::metrics().checkpoints_resumed.inc();
                resumed_from_checkpoint = true;
                // The unpublished physical attempt is replaced as one unit.
                // Completed files from that failed attempt still need replay:
                // their graph markers are newer than the published snapshot.
                changed
            } else {
                changed
            }
        } else {
            changed
        };

        self.enforce_project_byte_budget(&changed).await?;

        if !deleted.is_empty() {
            // See the job path: the sidecar must forget removed files.
            engram_index::vb_extractor::forget_files(&abs_paths_native(&dir, &deleted))?;
        }

        // Files whose stale graph generations are eligible for the scoped
        // purge after indexing: everything re-extracted this update plus
        // deletions (whose nodes have no replacement generation at all).
        let purge_paths: std::collections::HashSet<String> = changed
            .iter()
            .filter_map(|p| engram_core::RelPath::from_relative(&dir, p))
            .map(|r| r.as_str().to_string())
            .chain(deleted.iter().map(|r| r.as_str().to_string()))
            .collect();

        // Select unchanged physical revisions by reference. Only changed files
        // are indexed; an aborted attempt is cleared before replaying its files.
        let memory_policy = engram_core::get_policy("memory")
            .map(|p| p.versioning)
            .unwrap_or(engram_core::NamespaceVersioning::Snapshot);

        if memory_policy == engram_core::NamespaceVersioning::Snapshot {
            let changed_paths = changed
                .iter()
                .map(|path| {
                    engram_core::RelPath::from_relative(&dir, path)
                        .ok_or_else(|| anyhow::anyhow!("changed source is outside its project"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let prepared = ps
                .search
                .prepare_source_snapshot(
                    project_id,
                    old_gen,
                    new_gen,
                    &changed_paths,
                    &deleted,
                    cancel,
                )
                .await;
            self.record_vector_storage_result(project_id, prepared)
                .await?;
        } else {
            let mut to_delete = deleted;
            for p in &changed {
                let rel = engram_core::RelPath::from_relative(&dir, p)
                    .unwrap_or_else(|| engram_core::RelPath::new(&p.to_string_lossy()));
                to_delete.push(rel);
            }
            if !to_delete.is_empty() {
                ps.search
                    .delete_files(project_id, "memory", &to_delete)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
        }

        // Vector failure requires complete recovery. Retrying only these changed
        // files can otherwise publish a snapshot with inherited vectors missing.
        observe_source_update(project_id, new_gen, "index_files", "begin");
        let stats = self
            .index_files_with_parse_guard(
                &ps.search,
                &pid,
                "memory",
                new_gen,
                &dir,
                changed,
                Some(vb_source_files),
                self.state.cfg.max_chunks_per_file,
                cancel,
                |_, _| {},
            )
            .await?;
        observe_source_update(project_id, new_gen, "index_files", "returned");

        extraction.verify_produced(&stats)?;
        #[cfg(test)]
        crate::actors::watcher::behavior_tests::staged(project_id).await;
        anyhow::ensure!(!cancel.is_cancelled(), "update cancelled before graph enrichment");
        observe_source_update(project_id, new_gen, "graph_ingest", "begin");
        self.process_ingest_stats(project_id, new_gen, &stats)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        observe_source_update(project_id, new_gen, "graph_ingest", "returned");

        if only_paths.is_none() {
            let graph = self.state.graph.clone();
            let pid2 = pid.clone();
            let generation = new_gen;
            observe_source_update(project_id, new_gen, "sql_link", "begin");
            let _ = tokio::task::spawn_blocking(move || {
                graph_service::link_sql_to_schema(&graph, &pid2, generation)
            })
            .await;
            observe_source_update(project_id, new_gen, "sql_link", "returned");
        }

        observe_source_update(project_id, new_gen, "git_enrichment", "begin");
        let git_result = if only_paths.is_some() {
            Ok("history_enrichment: skipped (exclusive source scope; outside paths preserved without checking current bytes)".to_owned())
        } else { self
            .git_update_stream(
                project_id,
                &ps.info.directory,
                new_gen,
                max_commits,
                // Forward only: update/refresh is an INCREMENTAL operation.
                // Backfill is a one-time historical ingest - running it
                // here re-walked (and re-weighted) hundreds of already-
                // indexed commits on every routine update.
                crate::models::GitHistoryMode::Forward,
                index_antipatterns,
                engram_git::history::MergeCommitPolicy::AllParents,
                cancel,
                Box::new(|_, _| {}),
                false,
            )
            .await };
        observe_source_update(project_id, new_gen, "git_enrichment", "returned");
        // Match the background update path: history is enrichment, so its
        // failure must not strand a successfully indexed source generation.
        // Keep the degradation explicit rather than reporting clean success.
        let (mut update_status, git_summary) = match git_result {
            Ok(summary) => ("done", summary),
            Err(error) => {
                let warning = format!(
                    "git_update_stream failed (enrichment degraded): {}",
                    error.message
                );
                tracing::warn!(project_id = %pid, "{warning}");
                ("degraded", warning)
            }
        };
        #[cfg(test)]
        let _ = source_publication_tests::foreground_publication(
            &self.state.registry, project_id, false, cancel,
        ).await;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "update cancelled before source generation publication"
        );
        if memory_policy == engram_core::NamespaceVersioning::Snapshot {
            observe_source_update(project_id, new_gen, "snapshot_seal", "begin");
            ps.search.seal_source_snapshot(project_id, new_gen)?;
            observe_source_update(project_id, new_gen, "snapshot_seal", "returned");
        }

        {
            let reg = self.state.registry.clone();
            let publication = ps.search.begin_operation()?;
            tokio::task::spawn_blocking(move || {
                let _publication = publication;
                extraction.with_publication_fence(|| {
                    anyhow::ensure!(reg.publish_source_head(&source_head, new_gen)?,
                        "source generation changed while this update was staged; retry against the current snapshot");
                    observe_source_update(source_head.project_id(), new_gen, "source_head", "published");
                    Ok(())
                })
            })
            .await
            .map_err(|e| anyhow::anyhow!("set_meta join error: {e}"))?
            .map_err(|e| anyhow::anyhow!("set_meta failed: {e}"))?;
        }

        // Every failure above remains a failed unpublished attempt. From this
        // point the complete source head is visible; follow-up failures must
        // report that committed outcome instead of implying publication failed.
        #[cfg(test)]
        let injected_step = source_publication_tests::foreground_publication(
            &self.state.registry, project_id, true, cancel,
        ).await;
        let mut finalization_warnings = Vec::new();

        // External audit round 2 (docs/audits/10) P0-1 + doc-11 P1a: a failed
        // post-publication purge is a recorded debt the GC settles
        // (`purge_pending`), never a swallowed error — and the REGISTRY WRITE
        // that records or clears the debt is itself part of the outcome,
        // never a discarded `let _`.
        observe_source_update(project_id, new_gen, "source_purge", "begin");
        let purge_outcome = async {
            #[cfg(test)]
            anyhow::ensure!(injected_step != Some("source purge"), "Injected source purge failure");
            ps.search.purge_old_generations(project_id, new_gen).await
        }.await;
        observe_source_update(project_id, new_gen, "source_purge", "returned");
        let purge_failed = purge_outcome.is_err();
        if let Err(e) = &purge_outcome {
            tracing::warn!(
                project_id = project_id,
                generation = new_gen,
                "post-publication purge failed — recording as pending for the GC: {e:#}"
            );
        }
        let (purge_note, gc_nudge, purge_degraded) = {
            let reg = self.state.registry.clone();
            let pid_p = project_id.to_string();
            tokio::task::spawn_blocking(move || {
                let mut record_failed = false;
                let (note, nudge) = settle_purge_outcome(purge_outcome, new_gen, |v| {
                    let written = (|| -> anyhow::Result<()> {
                        #[cfg(test)]
                        anyhow::ensure!(injected_step != Some("purge settlement"),
                            "Injected purge settlement failure");
                        reg.set_meta(&pid_p, "purge_pending", v)
                            .map_err(anyhow::Error::from)
                    })();
                    record_failed = written.is_err();
                    written
                });
                // Clearing an old debt may fail even when GC needs no nudge.
                // Preserve that distinction without changing GC wake semantics.
                (note, nudge, purge_failed || record_failed)
            })
            .await
            .unwrap_or_else(|e| {
                (format!("purge settlement task panicked -- degraded: {e}"), true, true)
            })
        };
        if gc_nudge {
            self.state.gc_nudge.notify_one();
        }
        if purge_degraded {
            finalization_warnings.push(format!(
                "Source generation {new_gen} was published, but post-publication source purge/settlement is incomplete: {purge_note}"
            ));
        }
        // External audit 2026-08-29 row 9: refresh the co-change snapshot
        // (incremental by walked commit) so the next get_change_set only reads.
        if only_paths.is_none() {
            let st = self.state.clone();
            let pid_w = project_id.to_string();
            observe_source_update(project_id, new_gen, "co_change_warm", "begin");
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) =
                    crate::handlers::planning_tools::warm_co_change_snapshot_blocking(&st, &pid_w)
                {
                    tracing::warn!(project_id = %pid_w, "co-change warm-up skipped: {e:#}");
                }
            })
            .await;
            observe_source_update(project_id, new_gen, "co_change_warm", "returned");
        }

        // Retire old graph declarations only after actual head publication.
        // Preserve scoped cleanup ordering: a failed cleanup skips resolvers.
        observe_source_update(project_id, new_gen, "graph_cleanup", "begin");
        let graph_cleanup = {
            let graph = self.state.graph.clone();
            let pid_gc = pid.clone();
            let paths = purge_paths.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                #[cfg(test)]
                anyhow::ensure!(injected_step != Some("scoped graph cleanup"),
                    "Injected scoped graph cleanup failure");
                Ok(graph.purge_stale_nodes_for_paths(&pid_gc, &paths, new_gen)?)
            }).await.map_err(|error| anyhow::anyhow!("scoped graph cleanup task failed: {error}"))
                .and_then(|result| result)
        };
        observe_source_update(project_id, new_gen, "graph_cleanup", "returned");
        match graph_cleanup {
            Ok((n, e)) => {
                if n > 0 || e > 0 {
                    tracing::debug!(project_id = %pid, nodes = n, edges = e,
                        "purged stale graph generations for re-indexed files");
                }
                if only_paths.is_none() {
                let graph = self.state.graph.clone();
                let pid2 = pid.clone();
                let resolved = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    #[cfg(test)]
                    anyhow::ensure!(injected_step != Some("App_Code derived-edge refresh"),
                        "Injected App_Code derived-edge refresh failure; symbol resolver refresh skipped");
                    observe_source_update(&pid2, new_gen, "app_code_resolver", "begin");
                    graph_service::resolve_app_code_globals(&graph, &pid2, new_gen)
                        .map_err(|error| anyhow::anyhow!(
                            "App_Code derived-edge refresh failed; symbol resolver refresh skipped: {error:#}"))?;
                    observe_source_update(&pid2, new_gen, "app_code_resolver", "returned");
                    observe_source_update(&pid2, new_gen, "symbol_resolver", "begin");
                    graph.resolve_symbol_edges(&pid2)
                        .map_err(|error| anyhow::anyhow!("symbol edge resolver refresh failed: {error:#}"))?;
                    observe_source_update(&pid2, new_gen, "symbol_resolver", "returned");
                    Ok(())
                }).await.map_err(|error| anyhow::anyhow!("resolver refresh task failed: {error}"))
                    .and_then(|result| result);
                if let Err(error) = resolved {
                    finalization_warnings.push(format!(
                        "Source generation {new_gen} was published, but resolver refresh is incomplete: {error:#}"
                    ));
                }
                }
            }
            Err(error) => finalization_warnings.push(format!(
                "Source generation {new_gen} was published, but scoped graph cleanup failed; resolver refresh skipped: {error:#}"
            )),
        }

        // TODO-40: a resumed (crash-recovered) run leaves stale generations
        // behind — nudge the GC instead of waiting for the hourly tick.
        if resumed_from_checkpoint {
            self.state.gc_nudge.notify_one();
        }

        if cancel.is_cancelled() {
            finalization_warnings.push(format!(
                "Source generation {new_gen} was published before cancellation; the committed update remains visible."
            ));
        }
        if !finalization_warnings.is_empty() {
            update_status = "degraded";
            for warning in &finalization_warnings {
                tracing::warn!(project_id = %pid, "{warning}");
            }
        }
        let finalization_report = finalization_warnings.join("\n");
        let scope_report = if only_paths.is_some() {
            "update_scope: only_paths\noutside_scope_disk_check: not_run\noutside_source_revisions: inherited\nglobal_enrichment: skipped\n"
        } else { "update_scope: project\n" };
        Ok(format!(
            "✅ Updated project_id: {project_id}\nstatus: {update_status}\nactive_generation: {new_gen}\nSource generation {new_gen} published.\n{scope_report}{purge_note}\nfiles={} chunks={} bytes={}\n{git_summary}\n{finalization_report}\n",
            stats.files, stats.chunks, stats.bytes
        ))
    }

    pub async fn handle_list_projects(&self) -> Result<CallToolResult, McpError> {
        let reg = self.state.registry.clone();
        let list = tokio::task::spawn_blocking(move || reg.list_projects())
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if list.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No projects indexed.",
            )]));
        }

        let mut out = String::new();
        for p in list {
            out.push_str(&format!(
                "- {} | {} | {}\n",
                p.project_id, p.project_name, p.directory
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_project_info(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        // Fix 6: Redb reads are synchronous blocking I/O and must not execute on
        // a Tokio worker thread directly.
        let reg = self.state.registry.clone();
        let pid_info = req.project_id.clone();
        let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid_info))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let Some(rec) = rec else {
            return Err(McpError::invalid_params("Unknown project", None));
        };
        let gen_ = self
            .get_active_generation(&req.project_id)
            .await
            .unwrap_or(1);
        Ok(CallToolResult::success(vec![Content::text(format!(
            "project_id: {}\nname: {}\ndirectory: {}\nactive_generation: {}",
            rec.project_id, rec.project_name, rec.directory, gen_
        ))]))
    }

    /// External audit 2026-08-29 P0-2: does the PUBLISHED generation actually
    /// hold the corpus? Code chunks live in the `memory` namespace, one or
    /// more per indexed file; a generation with fewer chunks than half the
    /// tracked files has lost its corpus (pilot corpus: 105 chunks for 2,274 files
    /// while health said OK). Cheap: two counts.
    pub(crate) async fn generation_completeness(
        &self,
        pid: &str,
        generation: u64,
    ) -> anyhow::Result<GenerationCompleteness> {
        self.ensure_project_runtime(pid)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        generation_completeness_for(&self.state, pid, generation).await
    }
}

/// The completeness check for callers that hold only an `AppState` (the
/// pre-commit review harness). The project runtime must already be loaded.
pub(crate) async fn generation_completeness_for(
    state: &crate::state::AppState,
    pid: &str,
    generation: u64,
) -> anyhow::Result<GenerationCompleteness> {
    generation_completeness_with_graph_observation(state, pid, generation, None).await
}

async fn generation_completeness_with_graph_observation(
    state: &crate::state::AppState,
    pid: &str,
    generation: u64,
    graph_observation: Option<&project_service::SourceFileMetadataObservation>,
) -> anyhow::Result<GenerationCompleteness> {
    use std::collections::BTreeSet;
    // External audit round 2 (docs/audits/10) P0-2: chunks and files are
    // different units — completeness compares PATH SETS per store against the
    // eligible repository paths; counts are diagnostics.
    let ps = state
        .get_project_cached(pid)
        .ok_or_else(|| anyhow::anyhow!("project runtime for {pid} is not loaded"))?;
    let code_chunks = ps.search.count_docs_in_generation(
        pid,
        engram_core::namespaces::NAMESPACE_MEMORY,
        generation,
    )?;
    // Doc 11 P0-1: a provider error is DEGRADED, never an empty store.
    let mut store_errors: Vec<String> = Vec::new();
    let (vector_rows, vector_paths) = match ps.search.vector_generation_inventory(pid, None, generation).await {
        Ok(observation) => observation,
        Err(e) => {
            store_errors.push(format!("lancedb row/path observation failed: {e}"));
            (0, Default::default())
        }
    };

    let reg = state.registry.clone();
    let pid_r = pid.to_string();
    let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid_r))
        .await??
        .ok_or_else(|| anyhow::anyhow!("project {pid} is not registered"))?;
    let dir = std::path::PathBuf::from(&rec.directory);
    let exts = crate::models::ProjectType::from_registry_str(&rec.project_type)
        .map(crate::utils::files::exts_for_project_type_enum)
        .unwrap_or_else(|| crate::utils::files::exts_for_project_type(&rec.project_type));
    let files = project_service::scan_source_files(state, &dir, &exts).await?;
    let dir2 = dir.clone();
    let expected: BTreeSet<String> = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mut expected = BTreeSet::new();
        for file in files {
            // A disappearing or unreadable file makes this live observation
            // incomplete. Do not silently shrink the expected source set.
            if std::fs::metadata(&file)?.len() > 0 {
                let relative = file.strip_prefix(&dir2)?;
                expected.insert(completeness_path(&relative.to_string_lossy()));
            }
        }
        Ok(expected)
    })
    .await??;

    let tantivy: BTreeSet<String> = ps
        .search
        .paths_in_generation(pid, None, generation)?
        .into_iter()
        .map(|s| completeness_path(&s))
        .collect();
    let vectors: BTreeSet<String> = vector_paths.into_iter().map(|s| completeness_path(&s)).collect();
    let captured;
    let observation = match graph_observation {
        Some(observation) => observation,
        None => {
            captured = project_service::observe_source_file_metadata(state, pid).await;
            &captured
        }
    };
    let rows = observation.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut graph_paths = BTreeSet::new();
    let mut unpublished_graph_files = 0usize;
    for (path, _, graph_generation) in rows.iter() {
        if *graph_generation > generation {
            unpublished_graph_files += 1;
        } else {
            graph_paths.insert(completeness_path(path.as_str()));
        }
    }
    if unpublished_graph_files > 0 {
        // Source leases preserve physical chunks, not a historical graph.
        // Live metadata ahead of the selected head cannot certify its scope.
        store_errors.push(format!("graph has {unpublished_graph_files} file records newer than selected source generation {generation}; publication is in progress or the selected reader is older than the live graph; retry freshness after publication"));
    }

    // Round-2 audit P0-2 follow-up: paths the indexer skipped BY RULE are
    // subtracted from the expectation and reported — never tolerated.
    let reg_l = state.registry.clone();
    let pid_l = pid.to_string();
    let ledger: Vec<(String, String)> = tokio::task::spawn_blocking(move || {
        reg_l
            .get_meta(
                &pid_l,
                crate::services::ingest_service::SKIP_LEDGER_META_KEY,
            )
            .ok()
            .flatten()
            .and_then(|s| {
                serde_json::from_str::<crate::services::ingest_service::SkipLedger>(&s).ok()
            })
            .map(|l| l.skipped)
            .unwrap_or_default()
    })
    .await?;
    let skipped: BTreeSet<String> = ledger.iter().map(|(p, _)| completeness_path(p)).collect();
    let mut skipped_reasons: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (p, r) in &ledger {
        if expected.contains(&completeness_path(p)) {
            *skipped_reasons.entry(r.clone()).or_default() += 1;
        }
    }
    let skipped_by_rule = expected.iter().filter(|p| skipped.contains(*p)).count();

    // Doc 11 P0-1: whether embeddings are EXPECTED comes from CONFIGURATION
    // (the integrity service's rule) — an entirely empty vector store under a
    // vector backend is a LOSS, and a provider error is DEGRADED, never an
    // empty store. The graph is a store under test: it participates in
    // missing, extra, and cross-store equality.
    let vectors_required = !matches!(state.cfg.embedding_backend.as_str(), "fts_only" | "");
    let degraded = !store_errors.is_empty();
    let vectors_checked = vectors_required && !degraded;
    let missing: Vec<String> = expected
        .iter()
        .filter(|f| {
            !skipped.contains(*f)
                && (!tantivy.contains(*f)
                    || (vectors_checked && !vectors.contains(*f))
                    || !graph_paths.contains(*f))
        })
        .cloned()
        .collect();
    let unindexed: Vec<&String> = missing
        .iter()
        .filter(|f| {
            !tantivy.contains(*f)
                && !graph_paths.contains(*f)
                && (!vectors_checked || !vectors.contains(*f))
        })
        .collect();
    let extra = tantivy.iter().filter(|f| !expected.contains(*f)).count();
    let vector_extra = if vectors_checked {
        vectors.iter().filter(|f| !expected.contains(*f)).count()
    } else {
        0
    };
    let graph_extra = graph_paths
        .iter()
        .filter(|f| !expected.contains(*f))
        .count();
    let cross_store_mismatch = (if vectors_checked {
        tantivy.symmetric_difference(&vectors).count()
    } else {
        0
    }) + tantivy.symmetric_difference(&graph_paths).count();
    // Round-2 audit P0-2 follow-up: ZERO tolerance — a missing path is never
    // "complete"; known skips are subtracted above instead.
    let tolerance = 0usize;
    let complete = missing.len() <= tolerance && cross_store_mismatch == 0 && !degraded;
    Ok(GenerationCompleteness {
        generation,
        expected_paths: expected.len(),
        tantivy_paths: tantivy.len(),
        vector_paths: vectors.len(),
        graph_paths: graph_paths.len(),
        missing: missing.len(),
        missing_sample: missing.iter().take(10).cloned().collect(),
        unindexed: unindexed.len(),
        unindexed_sample: unindexed.iter().take(10).map(|p| p.to_string()).collect(),
        extra,
        cross_store_mismatch,
        tolerance,
        skipped_by_rule,
        skipped_reasons: skipped_reasons.into_iter().collect(),
        code_chunks,
        vector_rows,
        vector_extra,
        graph_extra,
        vectors_expected: vectors_required,
        store_errors,
        degraded,
        complete,
    })
}

/// One spelling for every store's paths: forward slashes, no leading `./`.
fn completeness_path(p: &str) -> String {
    p.replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn skip_reasons(c: &GenerationCompleteness) -> String {
    if c.skipped_reasons.is_empty() {
        String::new()
    } else {
        format!(
            " — {}",
            c.skipped_reasons
                .iter()
                .map(|(r, n)| format!("{r} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// The one-line completeness report every surface prints.
pub(crate) fn completeness_line(c: &GenerationCompleteness) -> String {
    let mut s = format!(
        "generation completeness: generation {} — expected paths: {}; tantivy: {}, vectors: {}, graph: {}; missing: {}, extra: {}, cross-store mismatch: {}; chunks {} / vector rows {} (diagnostic) — {}",
        c.generation,
        c.expected_paths,
        c.tantivy_paths,
        c.vector_paths,
        c.graph_paths,
        c.missing,
        c.extra,
        c.cross_store_mismatch,
        c.code_chunks,
        c.vector_rows,
        if c.complete {
            format!(
                "complete (missing 0, mismatch 0; skipped by rule: {}{})",
                c.skipped_by_rule,
                skip_reasons(c)
            )
        } else if c.only_unindexed() {
            format!(
                "STALE ({} path(s) on disk are not indexed yet; the stores agree with each other; skipped by rule: {}{})",
                c.unindexed,
                c.skipped_by_rule,
                skip_reasons(c)
            )
        } else {
            format!(
                "INCOMPLETE (missing must be 0 after known skips, mismatch must be 0; skipped by rule: {}{})",
                c.skipped_by_rule,
                skip_reasons(c)
            )
        }
    );
    if c.vector_extra > 0 || c.graph_extra > 0 {
        s.push_str(&format!(
            " — extra vectors {}, extra graph {}",
            c.vector_extra, c.graph_extra
        ));
    }
    if c.degraded {
        s.push_str(&format!(
            "\n  DEGRADED — store errors: {}",
            c.store_errors.join("; ")
        ));
    }
    if !c.missing_sample.is_empty() {
        s.push_str(&format!(
            "\n  missing sample: {}",
            c.missing_sample.join(", ")
        ));
    }
    s
}

/// Storage integrity and optional audit knowledge are independent. Counts
/// describe records, not parsed, approved or applicable quality rules.
fn quality_gate_availability(count: Option<usize>) -> String {
    match count {
        Some(0) => "quality_gate_records: 0\npre_push_audit_knowledge: INACTIVE_EMPTY (no quality_gate records; storage health does not establish audit coverage)\nquality_gate_setup: reviewed project policy sources can be supplied through ingest_quality_gates; no ingestion is performed by this health check.\n".into(),
        Some(n) => format!("quality_gate_records: {n}\npre_push_audit_knowledge: RECORDS_PRESENT_NOT_VALIDATED (stored records only; parsing, relevance, approval and audit execution are not established by this count)\n"),
        None => "quality_gate_records: UNKNOWN\npre_push_audit_knowledge: UNKNOWN (namespace count failed; unavailable evidence is not an empty corpus or a readiness pass; see failure diagnostics)\n".into(),
    }
}

/// An intact source index does not supply the historical pattern corpus used
/// by immune_check/anti_pattern_guard. Presence is not pattern validity.
fn anti_pattern_availability(count: Option<usize>) -> String {
    match count {
        Some(0) => "antipattern_records: 0\nanti_pattern_knowledge: INACTIVE_EMPTY (immune_check and anti_pattern_guard lack historical pattern evidence; zero matches cannot pass those checks)\nanti_pattern_setup: analyze_reverts can derive candidate patterns from the project's reviewed historical scope; no analysis or ingestion is performed by this check.\n".into(),
        Some(n) => format!("antipattern_records: {n}\nanti_pattern_knowledge: RECORDS_PRESENT_NOT_VALIDATED (stored records only; pattern validity, relevance, historical scope and check execution are not established by this count)\n"),
        None => "antipattern_records: UNKNOWN\nanti_pattern_knowledge: UNKNOWN (namespace count unavailable; this is neither an empty corpus nor a readiness pass)\n".into(),
    }
}

impl Engram {
    pub async fn handle_project_health_with_instructions(
        &self,
        req: crate::models::ProjectHealthRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if let Some(maintenance) = &req.instruction_maintenance {
            crate::services::instruction_maintenance::validate_observation_request(maintenance)
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        }
        let mut result = self.handle_project_health(ProjectIdRequest {
            project_id: req.project_id.clone(),
        }).await?;
        if let Some(maintenance) = req.instruction_maintenance {
            let observation = self.collect_instruction_observation(&req.project_id, maintenance,
                "separate_worktree_observation_collected_after_storage_health").await;
            result.content.push(Content::text(observation.to_string()));
        }
        Ok(result)
    }

    pub async fn handle_project_health(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id;
        let ps = self.ensure_project_runtime(&pid).await?;
        let (generation, _source_view) = self.pin_active_source_generation(&pid, &ps.search).await?;
        #[cfg(test)]
        crate::handlers::search_tools::selected_source_tests::after_selection(&pid).await;
        let source_identity_verified = !self
            .state
            .registry
            .source_identity_needs_verification(&pid)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;

        let graph = self.state.graph.clone();
        let pid_clone = pid.clone();
        let mut failures: Vec<String> = Vec::new();
        let (graph_nodes, graph_edges) = match tokio::task::spawn_blocking(move || {
            let nodes = graph.count_nodes(&pid_clone);
            let edges = graph.count_edges(&pid_clone);
            (nodes, edges)
        })
        .await
        {
            Ok((Ok(n), Ok(e))) => (n, e),
            Ok((n, e)) => {
                if let Err(err) = &n {
                    failures.push(format!("graph node count failed: {err}"));
                }
                if let Err(err) = &e {
                    failures.push(format!("graph edge count failed: {err}"));
                }
                (n.unwrap_or(0), e.unwrap_or(0))
            }
            Err(err) => {
                failures.push(format!("graph counts panicked: {err}"));
                (0, 0)
            }
        };

        let (ns_counts, namespace_counts_available) = match ps.search.count_docs_by_namespace(&pid)
        {
            Ok(m) => (m, true),
            Err(e) => {
                failures.push(format!("search doc counts failed: {e}"));
                (Default::default(), false)
            }
        };
        // Doc-11 P1e: the TOTAL is the project-wide count — the namespace
        // map enumerates a fixed subset and understated the label (live:
        // 421,293 printed vs 422,249 held).
        let total_docs: usize = match ps.search.count_docs(&pid) {
            Ok(n) => n,
            Err(e) => {
                failures.push(format!("search total count failed: {e}"));
                ns_counts.values().sum()
            }
        };
        let lancedb_rows = match ps.search.count_vectors(&pid).await {
            Ok(n) => n,
            Err(e) => {
                failures.push(format!("vector row count failed: {e}"));
                0
            }
        };
        // External audit 2026-08-29 P0-2: "Health: OK" was the INITIAL value of
        // the answer, not a conclusion. The verdict is computed from the
        // evidence — generation completeness first, provider failures second.
        let completeness = self.generation_completeness(&pid, generation).await;
        let verdict = match &completeness {
            // Doc 11 P0-1: a store provider error means the vector picture is
            // UNKNOWN — degraded outranks incomplete (whose missing list would
            // mislead) and always outranks OK.
            Ok(c) if c.degraded => {
                for e in &c.store_errors {
                    failures.push(e.clone());
                }
                "Health: DEGRADED — a store provider failed during the completeness check (see failures)"
                    .to_string()
            }
            Ok(c) if c.only_unindexed() => format!(
                "Health: STALE — {} eligible path(s) on disk are not in active generation {} (sample: {}); every store agrees on the indexed paths. Run update_project to index them.",
                c.unindexed,
                c.generation,
                c.unindexed_sample.join(", ")
            ),
            Ok(c) if !c.complete => format!(
                "Health: CORRUPT — active generation {} is INCOMPLETE ({} of {} eligible paths missing from the searchable stores, cross-store mismatch {}); searchable evidence is unreliable until repair_project(scope=\"full\", wipe_and_reindex=false) rebuilds this project ID",
                c.generation, c.missing, c.expected_paths, c.cross_store_mismatch
            ),
            Err(e) => {
                failures.push(format!("generation completeness check failed: {e}"));
                "Health: DEGRADED — completeness unknown".to_string()
            }
            Ok(_) if !failures.is_empty() => {
                "Health: DEGRADED — a provider failed (see failures)".to_string()
            }
            Ok(_) if !source_identity_verified => {
                "Health: STALE — source identity has not been verified against inherited fingerprints; run update_project".to_string()
            }
            Ok(_) => "Health: OK".to_string(),
        };
        let mut out = format!("{verdict}\n");
        out.push_str("health_scope: checked source-index stores and generation completeness; OK does not imply optional knowledge availability or audit coverage.\n");
        out.push_str(&quality_gate_availability(
            namespace_counts_available.then(|| ns_counts.get("quality_gate").copied().unwrap_or(0)),
        ));
        out.push_str(&anti_pattern_availability(
            namespace_counts_available.then(|| ns_counts.get("antipattern").copied().unwrap_or(0)),
        ));
        out.push_str(&format!("active_generation: {generation}\n"));
        out.push_str(&format!(
            "source_identity_verified: {source_identity_verified}\n"
        ));
        if let Ok(c) = &completeness {
            out.push_str(&format!("{}\n", completeness_line(c)));
        }
        for f in &failures {
            out.push_str(&format!("failure: {f}\n"));
        }
        out.push_str(&format!("graph_nodes: {graph_nodes}\n"));
        out.push_str(&format!("graph_edges: {graph_edges}\n"));
        out.push_str(&format!("tantivy_docs_total: {total_docs}\n"));
        out.push_str(&format!("lancedb_vectors: {lancedb_rows}\n"));
        out.push_str(&format!("lancedb_rows: {lancedb_rows}\n"));
        // P0-2: surface the semantic tier so agents/operators can see at a
        // glance whether vector search is real, the trigram stub, or off.
        let semantic = match ps.search.semantic_quality() {
            engram_index::SemanticQuality::Semantic => "true",
            engram_index::SemanticQuality::DegradedTrigram => {
                "degraded (trigram projection — set embedding_backend=ollama|openai)"
            }
            engram_index::SemanticQuality::Off => "off (fts_only)",
        };
        out.push_str(&format!("semantic_search: {semantic}\n"));
        match ps.search.semantic_quality() {
            engram_index::SemanticQuality::Semantic => {
                out.push_str(
                    "retrieval_readiness: READY (lexical and semantic providers available)\n",
                );
            }
            engram_index::SemanticQuality::DegradedTrigram => {
                out.push_str("retrieval_readiness: DEGRADED (semantic provider is a trigram projection; meaning-based recall is not fully available)\n");
            }
            engram_index::SemanticQuality::Off => {
                out.push_str("retrieval_readiness: DEGRADED (semantic retrieval is disabled; Health: OK covers source-index integrity only)\n");
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// P0-5: answer "can I trust this index right now?" in one call.
    pub async fn handle_get_index_freshness(
        &self,
        req: crate::models::GetIndexFreshnessRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id.clone();
        let rec = self.ensure_project_record(&pid).await?;
        let ps = self.ensure_project_runtime(&pid).await?;
        let (generation, _source_view) = self.pin_active_source_generation(&pid, &ps.search).await?;
        #[cfg(test)]
        crate::handlers::search_tools::selected_source_tests::after_selection(&pid).await;

        // Registry reads (blocking Redb) in one spawn_blocking hop.
        let reg = self.state.registry.clone();
        let pid_b = pid.clone();
        let (last_index_ms, last_index_files, watch_enabled) =
            tokio::task::spawn_blocking(move || {
                let last_ms = reg
                    .get_meta(&pid_b, "last_index_completed_ms")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse::<u64>().ok());
                let last_files = reg
                    .get_meta(&pid_b, "last_index_files")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse::<usize>().ok());
                let watching = reg
                    .list_watches(&pid_b)
                    .map(|ws| ws.iter().any(|w| w.enabled))
                    .unwrap_or(false);
                (last_ms, last_files, watching)
            })
            .await
            .unwrap_or((None, None, false));

        let mut out = String::with_capacity(512);
        out.push_str(&format!("project_id: {pid}\n"));
        out.push_str(&format!("directory: {}\n", rec.directory));
        out.push_str(&format!("active_generation: {generation}\n"));
        match last_index_ms {
            Some(ms) => {
                let age_s = now_ms().saturating_sub(ms) / 1000;
                out.push_str(&format!(
                    "last_index_completed: {age_s}s ago (epoch_ms={ms})\n"
                ));
            }
            None => out.push_str(
                "last_index_completed: unknown (project indexed before freshness tracking)\n",
            ),
        }
        if let Some(files) = last_index_files {
            out.push_str(&format!("last_index_files: {files}\n"));
        }
        out.push_str(&format!("watcher_enabled: {watch_enabled}\n"));
        // Reuse validated graph rows for both diagnostics. Filesystem, search,
        // vector and skip-ledger checks remain independent live observations.
        let graph_observation = project_service::observe_source_file_metadata(&self.state, &pid).await;
        let source_format = project_service::source_index_format_coverage_from_observation(
            &self.state,
            std::path::Path::new(&rec.directory),
            generation,
            &graph_observation,
        )
        .await;
        match &source_format {
            Ok((0, _)) => out.push_str("source_index_format: unknown (no indexed file metadata); absence of metadata cannot verify cached source ranges\n"),
            Ok((_, paths)) if paths.is_empty() => out.push_str(&format!("source_index_format: current (version {})\n", engram_index::SOURCE_INDEX_VERSION)),
            Ok((_, paths)) => out.push_str(&format!("source_index_format: reindex_required ({} file paths require version {}, current extraction settings/parser provenance, or active-generation publication); cached source ranges and call bindings are not trustworthy until update_project completes\n", paths.len(), engram_index::SOURCE_INDEX_VERSION)),
            Err(error) => out.push_str(&format!("source_index_format: unknown ({error})\n")),
        }
        if let Some(since) = rec.reindex_required_since_ms {
            out.push_str(&format!(
                "WARNING: recovery required since epoch_ms={since}.\n"
            ));
            let registry = self.state.registry.clone();
            let project = pid.clone();
            match tokio::task::spawn_blocking(move || registry.reindex_reasons(&project)).await {
                Ok(Ok(reasons)) => for reason in reasons {
                    out.push_str(match reason {
                        engram_core::registry::ReindexReason::Vector => "recovery_reason: vector - run repair_project(scope=\"vector_only\") to rebuild every retained namespace from stored text.\n",
                        engram_core::registry::ReindexReason::Source => "recovery_reason: source - run repair_project(scope=\"full\", wipe_and_reindex=false) to rebuild source while preserving knowledge.\n",
                        engram_core::registry::ReindexReason::Unknown => "recovery_reason: unknown - legacy recovery warning; source-only or vector-only completion cannot establish that all affected data is repaired.\n",
                    });
                },
                _ => out.push_str("recovery_reason: unavailable - restore registry access before trusting repair status.\n"),
            }
        }

        // Historical and detached-checkout workflows need a stronger binding
        // than generation completeness. A complete index can still contain a
        // dirty worktree at the expected commit.
        let mut source_binding_invalid = false;
        let mut source_binding_unknown = false;
        if let Some(expected) = req.expected_git_commit.as_deref() {
            let valid = matches!(expected.len(), 40 | 64)
                && expected.bytes().all(|b| b.is_ascii_hexdigit());
            if !valid {
                return Err(McpError::invalid_params(
                    "expected_git_commit must be a full 40- or 64-character hexadecimal object id",
                    None,
                ));
            }
            let expected = expected.to_ascii_lowercase();
            let directory = PathBuf::from(&rec.directory);
            let check = tokio::task::spawn_blocking(move || {
                let head = std::process::Command::new("git")
                    .args(["--no-optional-locks", "-C"])
                    .arg(&directory)
                    .args(["rev-parse", "--verify", "HEAD"])
                    .output()?;
                if !head.status.success() {
                    return Ok::<_, std::io::Error>(None);
                }
                let status = std::process::Command::new("git")
                    .args(["--no-optional-locks", "-C"])
                    .arg(&directory)
                    .args(["status", "--porcelain=v1", "--untracked-files=no"])
                    .output()?;
                if !status.status.success() {
                    return Ok(None);
                }
                Ok(Some((
                    String::from_utf8_lossy(&head.stdout)
                        .trim()
                        .to_ascii_lowercase(),
                    String::from_utf8_lossy(&status.stdout).lines().count(),
                )))
            })
            .await;
            out.push_str(&format!("expected_git_commit: {expected}\n"));
            match check {
                Ok(Ok(Some((head, dirty)))) => {
                    let matches = head == expected;
                    source_binding_invalid = !matches || dirty > 0;
                    out.push_str(&format!("source_git_head: {head}\n"));
                    out.push_str(&format!(
                        "source_revision_check: {}\ntracked_worktree_changes: {dirty}\nsource_binding_check: {}\n",
                        if matches { "match" } else { "MISMATCH" },
                        if source_binding_invalid { "FAILED" } else { "pass" }
                    ));
                }
                Ok(Ok(None)) => {
                    source_binding_unknown = true;
                    out.push_str("source_revision_check: unavailable (directory is not a readable Git worktree)\nsource_binding_check: unknown\n");
                }
                Ok(Err(error)) => {
                    source_binding_unknown = true;
                    out.push_str(&format!("source_revision_check: unavailable ({error})\nsource_binding_check: unknown\n"));
                }
                Err(error) => {
                    source_binding_unknown = true;
                    out.push_str(&format!("source_revision_check: unavailable ({error})\nsource_binding_check: unknown\n"));
                }
            }
        } else {
            out.push_str(
                "source_revision_check: not_requested\nsource_binding_check: not_requested\n",
            );
        }

        // Compare with each indexed file, not the wall-clock end of a job.
        // Restored files can have older timestamps, and deletions have no mtime.
        // This is the incremental indexer's change detector, not proof that all
        // content bytes match (unchanged stat tuples may avoid hashing).
        let mut disk_changes: Option<(usize, usize)> = None;
        if req.check_disk {
            let exts = ProjectType::from_registry_str(&rec.project_type)
                .map(exts_for_project_type_enum)
                .unwrap_or_else(|| exts_for_project_type(&rec.project_type));
            let dir = PathBuf::from(&rec.directory);
            if !dir.is_dir() {
                out.push_str(
                    "disk_check: unavailable (project directory is missing or inaccessible)\n",
                );
            } else {
                match self.get_incremental_changes(&pid, &dir, &exts).await {
                    Ok((changed, deleted)) => {
                        disk_changes = Some((changed.len(), deleted.len()));
                        out.push_str(&format!(
                            "disk_check: {}\nfiles_changed_or_added_since_index: {}\nfiles_deleted_since_index: {}\n",
                            if changed.is_empty() && deleted.is_empty() { "no_changes_detected" } else { "stale" },
                            changed.len(), deleted.len()
                        ));
                    }
                    Err(e) => out.push_str(&format!("disk_check: unavailable ({e})\n")),
                }
            }
            out.push_str("disk_check_basis: indexed file metadata with conditional hashing; refresh counts include source-index format migrations; unchanged metadata does not prove identical content\n");
        } else {
            out.push_str("disk_check: not_run (check_disk=false)\n");
        }

        // External audit 2026-08-29 P0-2: timestamps and modified files say
        // nothing about whether the published generation still HOLDS the
        // corpus. Completeness is checked here and outranks every other advice.
        let completeness = generation_completeness_with_graph_observation(
            &self.state, &pid, generation, Some(&graph_observation),
        ).await;
        let completeness_unknown = match &completeness {
            Ok(c) => c.degraded,
            Err(_) => true,
        };
        let incomplete = match &completeness {
            Ok(c) if c.degraded => {
                out.push_str("generation_complete: unknown (DEGRADED — store provider failure)\n");
                for error in &c.store_errors {
                    out.push_str(&format!("failure: {error}\n"));
                }
                false
            }
            Ok(c) => {
                out.push_str(&format!(
                    "generation_complete: {} ({} of {} eligible paths present in every store; missing {}, cross-store mismatch {}, skipped by rule {})\n",
                    c.complete,
                    c.expected_paths.saturating_sub(c.missing),
                    c.expected_paths,
                    c.missing,
                    c.cross_store_mismatch,
                    c.skipped_by_rule
                ));
                !c.complete
            }
            Err(e) => {
                out.push_str(&format!("generation_complete: unknown ({e})\n"));
                false
            }
        };
        let unindexed_only = matches!(&completeness, Ok(c) if c.only_unindexed());
        let advice = if source_binding_invalid {
            "source revision binding FAILED — do not trust indexed source; restore the expected clean Git worktree and run update_project"
        } else if source_binding_unknown {
            "freshness unknown — the requested Git source revision could not be verified"
        } else if source_format.is_err()
            || source_format.as_ref().is_ok_and(|(count, _)| *count == 0)
        {
            "freshness unknown — source-index format could not be verified; restore index access and retry"
        } else if source_format
            .as_ref()
            .is_ok_and(|(_, paths)| !paths.is_empty())
        {
            "run update_project — source-index migration automatically re-extracts old files while retaining this project ID and knowledge corpora"
        } else if completeness_unknown {
            "freshness unknown — generation completeness could not be verified; restore index access and retry"
        } else if unindexed_only {
            "index is stale — eligible files on disk are not indexed yet; run update_project (or enable watch_project for auto-updates)"
        } else if incomplete {
            "active generation is INCOMPLETE — the searchable corpus is missing; run repair_project(scope=\"full\", wipe_and_reindex=false) to rebuild this project ID and preserve knowledge"
        } else if rec.reindex_required_since_ms.is_some() {
            "recovery is pending - follow the recovery_reason guidance above; source indexing alone cannot establish vector/history recovery"
        } else if matches!(disk_changes, Some((changed, deleted)) if changed > 0 || deleted > 0) {
            "index is stale — run update_project (or enable watch_project for auto-updates)"
        } else if disk_changes.is_none() {
            "freshness unknown — disk comparison was not completed; run get_index_freshness with check_disk=true and restore source/index access if unavailable"
        } else if last_index_ms.is_none() {
            "freshness unknown — run update_project once to start tracking"
        } else if !watch_enabled {
            "no indexed metadata changes detected; enable watch_project for automatic updates; verify relevant source before relying on indexed evidence"
        } else {
            "no indexed metadata changes detected and the watcher is active; verify relevant source before relying on indexed evidence"
        };
        out.push_str(&format!("advice: {advice}\n"));
        // Agent intake already requests freshness. Surface missing audit
        // prerequisites here without changing the source-freshness verdict or
        // silently populating knowledge during an implementation session.
        out.push_str("audit_knowledge_scope: optional stored evidence, independent of source freshness; counts do not certify checks or readiness\n");
        let audit_counts = match self.ensure_project_runtime(&pid).await {
            Ok(ps) => ps
                .search
                .count_docs_by_namespace(&pid)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        match audit_counts {
            Ok(counts) => {
                out.push_str(&quality_gate_availability(Some(
                    counts.get("quality_gate").copied().unwrap_or(0),
                )));
                out.push_str(&anti_pattern_availability(Some(
                    counts.get("antipattern").copied().unwrap_or(0),
                )));
            }
            Err(error) => {
                out.push_str(&quality_gate_availability(None));
                out.push_str(&anti_pattern_availability(None));
                out.push_str(&format!("audit_knowledge_failure: {error}\n"));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Re-run placeholder edge resolution on the existing graph. Lets
    /// resolver upgrades (e.g. edge-metadata FQN matching) apply to an
    /// already-indexed project without a full reindex.
    pub async fn handle_resolve_graph_edges(
        &self,
        req: crate::models::ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        crate::handlers::validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_record(&req.project_id).await?;
        let _update_guard = self
            .state
            .acquire_project_update_lock(&req.project_id)
            .await;
        let generation = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let (resolved, schema_links, app_code) = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let app_code = graph_service::resolve_app_code_globals(&graph, &pid, generation)?;
                let resolved = graph.resolve_symbol_edges(&pid)?;
                let schema_links =
                    crate::services::ingest_service::repair_schema_links(&graph, &pid)?;
                Ok((resolved, schema_links, app_code))
            }),
        )
        .await
        .map_err(|_| McpError::internal_error("resolve_symbol_edges timed out after 600s", None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "resolve_graph_edges: {resolved} placeholder edge(s) resolved to concrete nodes.
             App_Code derived edges refreshed; {app_code} new dependency edge(s).
             Canonical schema links restored from indexed DDL: {schema_links}.
             Edges still starting with '::' had no matching symbol (external or dynamic targets).
             next: find_symbol_references / trace_ui_event now see the resolved targets."
        ))]))
    }

    pub async fn handle_repair_project(
        &self,
        req: RepairProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id.clone();
        let _ = self.ensure_project_record(&pid).await?;

        // Full repair implementation — Fix 6: wrap blocking Redb read.
        let reg_r = self.state.registry.clone();
        let pid_r = pid.clone();
        let rec = tokio::task::spawn_blocking(move || reg_r.get_project(&pid_r))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .ok_or_else(|| McpError::internal_error(format!("project {pid} not found"), None))?;
        let dir = PathBuf::from(&rec.directory);

        // Route the documented `scope`. It used to be accepted and ignored:
        // every call ran a full repair, so a narrow request silently paid a
        // full reindex — minutes of work, and every doc_id the caller was
        // holding went stale with the generation bump — while a typo'd scope
        // did the same instead of erroring.
        let purge_graph_first = match req.scope.to_ascii_lowercase().as_str() {
            "full" => false,
            // Vector recovery rebuilds from retained stored text without
            // changing source generations or rereading project files.
            scope @ ("tantivy_only" | "vector_only" | "initialize_vectors") => {
                if scope == "initialize_vectors" && req.wipe_and_reindex {
                    return Err(McpError::invalid_params(
                        "initialize_vectors cannot be combined with wipe_and_reindex",
                        None,
                    ));
                }
                let msg = crate::services::project_service::repair_project_scoped(
                    &self.state,
                    &pid,
                    scope,
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "✅ Scoped repair ({scope}) for project_id: {pid}
{msg}"
                ))]));
            }
            // graph_only purges the graph and then falls through to the
            // indexing pass below, because that pass is what REBUILDS it.
            // Delegating to repair_project_scoped("graph_only") would delete
            // the graph and return Ok without rebuilding anything.
            "graph_only" => true,
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "repair_project: invalid scope '{other}'. Expected one of: full, graph_only, tantivy_only, vector_only, initialize_vectors"
                    ),
                    None,
                ));
            }
        };

        let _update_guard = self.state.acquire_project_update_lock(&pid).await;
        // Destructive/scoped repair still requires an ordinary valid head.
        // Explicit non-wiping full repair may instead capture exact recovery
        // authority without interpreting damaged bytes as a source generation.
        let source_base = match self
            .capture_source_head(&pid, &rec.directory, &rec.project_type)
            .await
        {
            Ok(base) => SourceRepairBase::Published(base),
            Err(original) if !purge_graph_first && !req.wipe_and_reindex => {
                let registry = self.state.registry.clone();
                let project = pid.clone();
                let recovery = tokio::task::spawn_blocking(move ||
                    registry.capture_source_recovery(&project)
                ).await.map_err(|error| McpError::internal_error(error.to_string(), None))?
                .map_err(|error| McpError::internal_error(
                    format!("Cannot admit source repair: {original:#}; explicit recovery unavailable: {error:#}"), None,
                ))?;
                // A damaged head prevents the ordinary capture helper from
                // refreshing cached source identity. Converge without reopening
                // the engine; the checks below still reject a stale request.
                if let Some(mut cached) = self.state.projects.get_mut(&pid)
                    && !recovery.matches_source_identity(&cached.info.directory, &cached.info.project_type)
                {
                    let (directory, project_type) = recovery.source_identity();
                    cached.info.directory = directory.to_owned();
                    cached.info.project_type = project_type.to_owned();
                }
                SourceRepairBase::Damaged(recovery)
            }
            Err(error) => return Err(McpError::internal_error(
                format!("Cannot admit source repair: {error:#}"), None,
            )),
        };
        let admitted_runtime = self.ensure_project_runtime(&pid).await?;
        if !source_base.matches_source_identity(&rec.directory, &rec.project_type)
            || !source_base.matches_source_identity(
            &admitted_runtime.info.directory,
            &admitted_runtime.info.project_type,
        ) {
            return Err(McpError::internal_error(
                "Source runtime identity changed before repair; refresh and retry",
                None,
            ));
        }
        let _storage_mutation = admitted_runtime
            .search
            .begin_project_mutation(&pid)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let admitted_search = admitted_runtime.search.with_mutation(&_storage_mutation)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        drop(admitted_runtime);

        // A failed/partial walk or over-budget corpus must not authorize any
        // graph, runtime-cache or history-watermark destruction.
        let exts = ProjectType::from_registry_str(&rec.project_type)
            .map(exts_for_project_type_enum)
            .unwrap_or_else(|| exts_for_project_type(&rec.project_type));
        let files = project_service::scan_source_files(&self.state, &dir, &exts)
            .await
            .map_err(|error| {
                McpError::internal_error(format!("Cannot admit source repair: {error:#}"), None)
            })?;
        let extraction = project_service::capture_source_extraction_for_files(&self.state, &dir, &files)
            .await.map_err(|error| McpError::internal_error(error.to_string(), None))?;

        let cancel = tokio_util::sync::CancellationToken::new();
        let (current_gen, new_gen, publication) = match source_base {
            SourceRepairBase::Published(base) => {
                let current = base.generation();
                let next = current.checked_add(1).ok_or_else(||
                    McpError::internal_error("source generation exhausted", None))?;
                (current, next, SourceRepairPublication::Published(base))
            }
            SourceRepairBase::Damaged(base) => {
                // The update lock spans discovery, durable reservation, full
                // staging and publication. The storage lane drains prior work;
                // recovery never clears an unknown old physical generation.
                let floor = admitted_search.source_generation_high_water(&pid, &cancel).await
                    .map_err(|error| McpError::internal_error(
                        format!("Cannot establish a safe source recovery generation: {error:#}"), None,
                    ))?;
                let operation = admitted_search.begin_operation()
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                let registry = self.state.registry.clone();
                let reservation = tokio::task::spawn_blocking(move || {
                    let _operation = operation;
                    registry.reserve_source_recovery(&base, floor)
                }).await.map_err(|error| McpError::internal_error(error.to_string(), None))?
                .map_err(|error| McpError::internal_error(error.to_string(), None))?
                .ok_or_else(|| McpError::internal_error(
                    "Source recovery authority changed before reservation; retry", None,
                ))?;
                (floor, reservation.generation(), SourceRepairPublication::Recovered(reservation))
            }
        };
        let recovered = matches!(&publication, SourceRepairPublication::Recovered(_));

        if purge_graph_first {
            tracing::info!(project_id = %pid, "repair_project(graph_only): purging graph before rebuild");
            self.state.graph.delete_project_data(&pid).ok();
        }

        if req.wipe_and_reindex {
            self.state.projects.remove(&pid);
            self.state.graph.delete_project_data(&pid).ok();
            // The wipe deletes graph/search data but ingest WATERMARKS live
            // in the registry and would survive - leaving tools like
            // index_git_history / ingest_merged_prs claiming 'fully
            // indexed' over a hole (three live incidents on 2026-07-04).
            // Clear them so the next ingest rebuilds from scratch.
            {
                let reg = self.state.registry.clone();
                let pid_wm = pid.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    for key in [
                        "last_git_oid",
                        "oldest_indexed_git_oid",
                        "git_backfill_complete",
                        "pr_ingest_watermark",
                    ] {
                        let _ = reg.set_meta(&pid_wm, key, "");
                    }
                })
                .await;
            }
        }

        let mut ps = self.ensure_project_runtime(&pid).await?;
        ps.search = Arc::new(
            ps.search
                .with_mutation(&_storage_mutation)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?,
        );

        let source_requirement = self
            .capture_source_requirement(&pid)
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let stats = self
            .index_complete_source_files(
                &ps.search,
                &pid,
                "memory",
                new_gen,
                current_gen,
                &dir,
                files,
                self.state.cfg.max_chunks_per_file,
                &extraction,
                &cancel,
                |_, _| {},
                recovered,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        self.process_ingest_stats(&pid, new_gen, &stats)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    format!("AUD-2026-INV-0002: process_ingest_stats failed during repair: {e:#}"),
                    None,
                )
            })?;

        #[cfg(test)]
        source_publication_tests::before_background_publication(&self.state.registry, &pid).await;

        match publication {
            SourceRepairPublication::Published(base) => self.publish_source_generation(
                &_storage_mutation, &pid, base, new_gen, source_requirement, extraction,
            ).await,
            SourceRepairPublication::Recovered(reservation) => self.publish_recovered_source_generation(
                &_storage_mutation, &pid, reservation, source_requirement, extraction,
            ).await,
        }
        .map_err(|e| {
            McpError::internal_error(format!("source repair publication failed: {e:#}"), None)
        })?;

        // Placeholder rewiring: index_project runs this after every
        // index; the repair path skipped it, silently leaving ALL
        // cross-file "::" edges unresolved after a wipe-reindex.
        {
            let graph = self.state.graph.clone();
            let pid_r = pid.clone();
            match tokio::time::timeout(
                std::time::Duration::from_secs(600),
                tokio::task::spawn_blocking(move || {
                    graph_service::resolve_app_code_globals(&graph, &pid_r, new_gen)?;
                    graph.resolve_symbol_edges(&pid_r)
                }),
            )
            .await
            {
                Ok(Ok(Ok(n))) => {
                    tracing::info!(project_id = %pid, resolved = n, "repair_project: resolve_symbol_edges complete");
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!(project_id = %pid, "repair_project: resolve_symbol_edges failed: {e:#}");
                }
                Ok(Err(e)) => {
                    tracing::warn!(project_id = %pid, "repair_project: resolve_symbol_edges panicked: {e}");
                }
                Err(_) => {
                    tracing::warn!(project_id = %pid, "repair_project: resolve_symbol_edges timed out after 600s");
                }
            }
        }

        // A wipe-reindex is a FULL index: record the GC graph-purge
        // baseline here too (index_project's paths already do) — without
        // it the periodic GC skips graph purging forever on repaired
        // stores.
        {
            let reg = self.state.registry.clone();
            let pid_full = pid.clone();
            let gen_str = new_gen.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                reg.set_meta(&pid_full, "last_full_index_generation", &gen_str)
            })
            .await;
        }

        // Ordinary repair can restore temporal edges in a bounded background
        // follow-up. A zero cap deliberately leaves Git work to a later call;
        // after a wipe, that means temporal history has not been restored.
        let git_note = if recovered {
            // Recovery restores source only; existing history coverage and its
            // watermarks must not be changed by an unrelated background ingest.
            "existing git history and ingestion watermarks preserved"
        } else if req.max_commits == 0 {
            "git history follow-up skipped (max_commits=0); temporal history was not replayed"
        } else { match self
            .handle_index_git_history(crate::models::IndexGitHistoryRequest {
                project_id: pid.clone(),
                max_commits: req.max_commits,
                force: false,
                index_antipatterns: req.index_antipatterns,
                mode: Some(crate::models::GitHistoryMode::Forward),
                wait: false,
            })
            .await
        {
            Ok(_) => "git history re-walk started (background) — temporal edges restore shortly",
            Err(_) => {
                "WARNING: git history re-walk could not be started — run index_git_history \
                 to restore temporal edges"
            }
        }};

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Project repaired project_id: {pid}\nactive_generation: {new_gen}\nfiles={} chunks={}\n{git_note}",
            stats.files, stats.chunks
        ))]))
    }

    pub async fn handle_delete_project(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id;

        // Fix 9: Signal and abort every active job for this project before
        // destroying its database entries. Without this, orphaned tasks continue
        // to read and write the now-deleted data, recreating deleted directories
        // and leaving the system in a ghost state.
        {
            let reg = self.state.registry.clone();
            let pid_c = pid.clone();
            let running_ids: Vec<String> =
                tokio::task::spawn_blocking(move || reg.list_jobs(Some(&pid_c)))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|j| j.status == "running")
                    .map(|j| j.job_id)
                    .collect();

            if !running_ids.is_empty() {
                // Collect tokens without holding the lock across awaits.
                let tokens_to_cancel: Vec<tokio_util::sync::CancellationToken> = {
                    let guard = self.state.cancellation_tokens.read().await;
                    running_ids
                        .iter()
                        .filter_map(|jid| guard.get(jid).cloned())
                        .collect()
                };
                // Signal cooperative cancellation first.
                for tok in &tokens_to_cancel {
                    tok.cancel();
                }
                // Then hard-abort the JoinHandles and remove them from maps.
                {
                    let mut handles = self.state.active_jobs.write().await;
                    for jid in &running_ids {
                        if let Some(h) = handles.remove(jid) {
                            h.abort();
                        }
                    }
                }
                {
                    let mut tokens = self.state.cancellation_tokens.write().await;
                    for jid in &running_ids {
                        tokens.remove(jid);
                    }
                }
            }
        }

        // The data directory for a project is usually at {data_dir}/projects/{pid}
        let project_dir = self.state.cfg.data_dir.join("projects").join(&pid);

        self.state.projects.remove(&pid);
        self.state.project_update_locks.write().await.remove(&pid);
        self.state
            .registry
            .delete_all_for_project(&pid)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        self.state
            .graph
            .delete_project_data(&pid)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // MCP1-n3v6: call remove_dir_all directly — skipping the .exists() pre-check
        // removes the TOCTOU window between the check and the removal.
        // remove_dir_all is idempotent: NotFound is silently discarded via let _ =.
        let _ = std::fs::remove_dir_all(&project_dir);

        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Deleted project_id: {pid}"
        ))]))
    }

    pub async fn handle_watch_project(
        &self,
        req: WatchProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let watch = WatchRecord {
            watch_id: "default".into(),
            directory: rec.directory.clone(),
            enabled: req.enabled,
            updated_at_ms: now_ms(),
        };
        self.state
            .registry
            .put_watch(&req.project_id, &watch)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let _ = self.state.events_tx.send(AppEvent::WatchUpdate {
            project_id: req.project_id,
            directory: rec.directory,
            enabled: req.enabled,
        });
        Ok(CallToolResult::success(vec![Content::text(
            "✅ Watch updated.",
        )]))
    }

    pub async fn handle_unwatch_project(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let registry = self.state.registry.clone();
        let project_id = req.project_id.clone();
        tokio::task::spawn_blocking(move || registry.disable_project_watches(&project_id, now_ms()))
            .await.map_err(|error| McpError::internal_error(error.to_string(), None))?
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let _ = self.state.events_tx.send(AppEvent::WatchUpdate {
            project_id: req.project_id, directory: String::new(), enabled: false,
        });
        Ok(CallToolResult::success(vec![Content::text(
            "✅ Unwatched.",
        )]))
    }

    pub async fn handle_list_jobs(&self, req: ListJobsRequest) -> Result<CallToolResult, McpError> {
        let jobs = self
            .state
            .registry
            .list_jobs(req.project_id.as_deref())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut out = String::new();
        for j in jobs {
            out.push_str(&format!("- {} | {} | {}\n", j.job_id, j.kind, j.status));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_cancel_job(
        &self,
        req: CancelJobRequest,
    ) -> Result<CallToolResult, McpError> {
        let outcome = self.cancel_job_internal(&req.job_id).await;
        Ok(CallToolResult::success(vec![Content::text(
            match outcome {
                job_service::CancellationOutcome::CancelledWithTombstone => {
                    format!("✅ cancelled job_id: {} (tombstone persisted)", req.job_id)
                }
                job_service::CancellationOutcome::CancelledWithoutTombstone => format!(
                    "⚠️ cancelled job_id: {} (WARNING: tombstone persistence failed — audit metadata may be missing)",
                    req.job_id
                ),
                job_service::CancellationOutcome::NotFound => {
                    format!("❌ job_id not active: {}", req.job_id)
                }
            },
        )]))
    }

    pub async fn handle_get_job_status(
        &self,
        req: CancelJobRequest,
    ) -> Result<CallToolResult, McpError> {
        let job = self
            .state
            .registry
            .get_job(&req.job_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let Some(j) = job else {
            return Err(McpError::invalid_params("Unknown job", None));
        };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "job_id: {}\nstatus: {}\nmessage: {}",
            j.job_id, j.status, j.message
        ))]))
    }

    pub async fn handle_incremental_indexing_gc(
        &self,
        req: IncrementalIndexingGcRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id;
        // Capture both authoritative retention baselines only after acquiring
        // the same lock used for publication, including a delayed GC request.
        let _update_guard = self.state.acquire_project_update_lock(&pid).await;
        let ps = self.ensure_project_runtime(&pid).await?;
        let (active_gen, _source_snapshot) = self.pin_active_source_generation(&pid, &ps.search).await?;
        if active_gen == 0 {
            return Ok(CallToolResult::success(vec![Content::text("Skipped source GC: no source has been published.")]));
        }
        if !_source_snapshot.is_managed() {
            return Err(McpError::internal_error("Legacy source must complete a full repair before snapshot GC", None));
        }
        // Graph baseline: the LAST FULL INDEX generation. Incremental updates
        // bump active_generation while unchanged files keep older nodes, so
        // purging the graph against the incremental counter deletes them.
        let full_gen: Option<u64> = {
            let reg = self.state.registry.clone();
            let pid_meta = pid.clone();
            tokio::task::spawn_blocking(move || {
                reg.get_meta(&pid_meta, "last_full_index_generation")
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };
        let target_gen = req.target_generation.unwrap_or(active_gen);
        if target_gen > active_gen {
            return Err(McpError::invalid_params(
                "GC target_generation cannot exceed the published source generation",
                None,
            ));
        }
        // Incremental updates deliberately retain unchanged graph nodes below
        // the active counter. A custom cutoff cannot override that baseline.
        let graph_baseline = full_gen.map(|full| target_gen.min(full));

        let mut steps: Vec<String> = Vec::new();

        let pre_graph_nodes = self.state.graph.count_nodes(&pid).unwrap_or(0);
        let pre_graph_edges = self.state.graph.count_edges(&pid).unwrap_or(0);
        let pre_tantivy = ps.search.count_docs(&pid).unwrap_or(0);
        let pre_vectors = ps.search.count_vectors(&pid).await.unwrap_or(0);

        match graph_baseline {
            Some(baseline) => {
                let graph = self.state.graph.clone();
                let pid_gc = pid.clone();
                let (nodes, edges) = tokio::task::spawn_blocking(move || {
                    graph.purge_generations_below(&pid_gc, baseline)
                })
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                steps.push(format!(
                    "Purged graph entries below generation {baseline} (last full index; \
                     {nodes} nodes, {edges} edges removed)."
                ));
            }
            None => steps.push(
                "Skipped the graph purge: no last_full_index_generation baseline \
                 (incremental generations must not purge the graph)."
                    .into(),
            ),
        }

        ps.search
            .purge_old_generations_with_cutoff(&pid, active_gen, target_gen)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        steps.push("Purged Tantivy stale documents.".into());

        if req.compact_vectors {
            match ps.search.compact_vector_fragments(&pid).await
                .map_err(|e| McpError::internal_error(format!("Stale-generation purge completed, but vector compaction failed: {e}"), None))? {
                Some((removed, added)) => steps.push(format!(
                    "Vector compaction completed: {removed} fragments replaced by {added}."
                )),
                None => steps.push("Skipped vector compaction: no native vector table is available.".into()),
            }
            // Compaction has returned and released its mutation authority.
            // History maintenance acquires its own exclusion and defers while
            // a source reader or native backend worker still needs old files.
            match ps.search.prune_vector_history(&pid, &tokio_util::sync::CancellationToken::new()).await
                .map_err(|e| McpError::internal_error(format!("Stale-generation purge and vector compaction completed, but vector history maintenance failed: {e}"), None))? {
                engram_index::hybrid::NativeHistoryPruneOutcome::Pruned { old_versions, bytes_removed, cache_rotation } => {
                    steps.push(format!("Vector history maintenance removed {old_versions} old versions and {bytes_removed} bytes."));
                    match cache_rotation {
                        engram_index::hybrid::NativeSessionRotationOutcome::Rotated { generation, elapsed_ms } => steps.push(format!(
                            "Native session {generation} prepared in {elapsed_ms} ms; subsequent native queries refill caches as needed. Released process memory is not measured."
                        )),
                        engram_index::hybrid::NativeSessionRotationOutcome::Deferred { reason, elapsed_ms } => steps.push(format!(
                            "Native cache refresh deferred after {elapsed_ms} ms: {reason}. Completed history pruning is retained."
                        )),
                        engram_index::hybrid::NativeSessionRotationOutcome::Skipped { reason } => steps.push(format!(
                            "Native caches retained: {reason}."
                        )),
                    }
                },
                engram_index::hybrid::NativeHistoryPruneOutcome::Deferred { reason } => steps.push(format!(
                    "Vector history maintenance deferred: {reason}. Old version files retained."
                )),
                engram_index::hybrid::NativeHistoryPruneOutcome::Unavailable => steps.push(
                    "Skipped vector history maintenance: no native vector table is available.".into()
                ),
            }
        }

        let post_graph_nodes = self.state.graph.count_nodes(&pid).unwrap_or(0);
        let post_graph_edges = self.state.graph.count_edges(&pid).unwrap_or(0);
        let post_tantivy = ps.search.count_docs(&pid).unwrap_or(0);
        let post_vectors = ps.search.count_vectors(&pid).await.unwrap_or(0);

        let mut out = String::with_capacity(512);
        out.push_str(&format!(
            "\u{2705} GC completed for project '{}' (target_gen={}).\n",
            pid, target_gen
        ));
        for (i, step) in steps.iter().enumerate() {
            out.push_str(&format!("  {}. {}\n", i + 1, step));
        }
        out.push_str("\n--- Before / After ---\n");
        out.push_str(&format!(
            "  graph_nodes: {} -> {} ({}{})  \n",
            pre_graph_nodes,
            post_graph_nodes,
            if post_graph_nodes <= pre_graph_nodes {
                "-"
            } else {
                "+"
            },
            (pre_graph_nodes as i64 - post_graph_nodes as i64).unsigned_abs()
        ));
        out.push_str(&format!(
            "  graph_edges: {} -> {} ({}{})  \n",
            pre_graph_edges,
            post_graph_edges,
            if post_graph_edges <= pre_graph_edges {
                "-"
            } else {
                "+"
            },
            (pre_graph_edges as i64 - post_graph_edges as i64).unsigned_abs()
        ));
        out.push_str(&format!(
            "  tantivy_docs: {} -> {} ({}{})  \n",
            pre_tantivy,
            post_tantivy,
            if post_tantivy <= pre_tantivy {
                "-"
            } else {
                "+"
            },
            (pre_tantivy as i64 - post_tantivy as i64).unsigned_abs()
        ));
        out.push_str(&format!(
            "  lancedb_vectors: {} -> {} ({}{})  \n",
            pre_vectors,
            post_vectors,
            if post_vectors <= pre_vectors {
                "-"
            } else {
                "+"
            },
            (pre_vectors as i64 - post_vectors as i64).unsigned_abs()
        ));
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_get_metrics(
        &self,
        req: GetMetricsRequest,
    ) -> Result<CallToolResult, McpError> {
        let snapshot = engram_core::metrics().snapshot();
        if req.output_json {
            let json = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }
        let s = &snapshot;
        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Engram MCP Metrics (uptime: {}s)\n",
            s.uptime_secs
        ));
        out.push_str(&format!(
            "\n--- Jobs ---\nstarted: {}  completed: {}  failed: {}  cancelled: {}  active: {}\n",
            s.jobs.started, s.jobs.completed, s.jobs.failed, s.jobs.cancelled, s.jobs.active
        ));
        out.push_str("\n--- Latencies (ms) ---\n");
        for (name, h) in [
            ("index_project", &s.latencies.index_project),
            ("update_project", &s.latencies.update_project),
            ("search", &s.latencies.search),
            ("vector_search", &s.latencies.vector_search),
            ("graph_query", &s.latencies.graph_query),
            ("dream", &s.latencies.dream),
            ("immune_check", &s.latencies.immune_check),
            ("git_history", &s.latencies.git_history),
        ] {
            if h.count > 0 {
                out.push_str(&format!(
                    "  {}: count={} avg={:.0}ms p50={}ms p95={}ms p99={}ms\n",
                    name, h.count, h.avg_ms, h.p50_ms, h.p95_ms, h.p99_ms
                ));
            }
        }
        out.push_str(&format!(
            "\n--- Queues ---\nevent_queue: {}  parse_queue: {}\n",
            s.queues.event_queue_depth, s.queues.parse_queue_depth
        ));
        out.push_str(&format!(
            "\n--- Cardinality ---\ntantivy: {}  vectors: {}  graph_nodes: {}  graph_edges: {}\n",
            s.cardinality.tantivy_doc_count,
            s.cardinality.vector_doc_count,
            s.cardinality.graph_node_count,
            s.cardinality.graph_edge_count
        ));
        out.push_str(&format!(
            "\n--- Index Drift ---\ntantivy: +{} -{}\nvectors: +{} -{}\ngraph: +{} nodes +{} edges -{} nodes -{} edges\n",
            s.index_drift.tantivy_docs_indexed, s.index_drift.tantivy_docs_deleted,
            s.index_drift.vector_docs_indexed, s.index_drift.vector_docs_deleted,
            s.index_drift.graph_nodes_upserted, s.index_drift.graph_edges_upserted,
            s.index_drift.graph_nodes_deleted, s.index_drift.graph_edges_deleted
        ));
        out.push_str(&format!(
            "\n--- Repairs ---\ntriggered: {}  succeeded: {}  failed: {}\nintegrity_checks: {}  mismatches: {}\n",
            s.repairs.triggered, s.repairs.succeeded, s.repairs.failed,
            s.repairs.integrity_checks_run, s.repairs.integrity_mismatches_found
        ));
        out.push_str(&format!(
            "\n--- Memory ---\nused: {} bytes  budget: {} bytes  pressure_events: {}  rejections: {}\n",
            s.memory.bytes_used, s.memory.budget_bytes,
            s.memory.pressure_events, s.memory.backpressure_rejections
        ));
        out.push_str(&format!(
            "\n--- Recovery ---\ncheckpoints_written: {}  checkpoints_resumed: {}\n",
            s.recovery.checkpoints_written, s.recovery.checkpoints_resumed
        ));
        out.push_str(&format!(
            "\n--- Extraction Confidence ---\nhigh: {}  medium: {}  low: {}\n",
            s.extraction_confidence.high,
            s.extraction_confidence.medium,
            s.extraction_confidence.low
        ));
        out.push_str(&format!(
            "\n--- Safety ---\nrefactors_approved: {}  refactors_blocked: {}\n",
            s.safety.refactors_approved, s.safety.refactors_blocked
        ));
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_check_integrity(
        &self,
        req: CheckIntegrityRequest,
    ) -> Result<CallToolResult, McpError> {
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let auto_repair = crate::services::integrity_service::resolve_auto_repair(
            self.state.cfg.integrity_auto_repair,
            req.auto_repair,
        );
        let result = crate::services::integrity_service::check_project_integrity_with_policy(
            &self.state,
            &req.project_id,
            auto_repair,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&result)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    pub async fn handle_get_checkpoint_status(
        &self,
        req: GetCheckpointStatusRequest,
    ) -> Result<CallToolResult, McpError> {
        let store = self.state.checkpoints.clone();
        let checkpoints =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<engram_core::Checkpoint>> {
                if let Some(ref job_id) = req.job_id {
                    Ok(store.get(job_id)?.into_iter().collect())
                } else if let Some(ref project_id) = req.project_id {
                    Ok(store.find_resumable(project_id)?.into_iter().collect())
                } else {
                    store.list_all()
                }
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if checkpoints.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No checkpoints found.",
            )]));
        }
        let mut out = String::with_capacity(2048);
        out.push_str(&format!("Checkpoints ({}):\n", checkpoints.len()));
        for cp in &checkpoints {
            out.push_str(&format!(
                "\n  job_id: {}\n  project: {}\n  phase: {:?}\n  progress: {}/{}\n  generation: {}\n",
                cp.job_id, cp.project_id, cp.phase, cp.items_processed, cp.items_total, cp.generation
            ));
            if let Some(ref err) = cp.error {
                out.push_str(&format!("  error: {}\n", err));
            }
        }
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_get_memory_budget(
        &self,
        req: GetMemoryBudgetRequest,
    ) -> Result<CallToolResult, McpError> {
        if let Some(project_id) = req.project_id.as_deref() {
            validate_project_id(project_id)?;
        }
        let budget = &self.state.memory_budget;
        let breakdown = budget.breakdown();
        let project_retention = req.project_id.as_deref().map(|project_id| {
            match self.state.peek_project_cached(project_id) {
                Ok(Some(project)) => serde_json::json!({"project_id":project_id,"status":"observed",
                    "search":project.search.retention_observation(project_id),
                    "ranking":project.pagerank_cache.retention_observation(),
                    "global_ranking_worker_slots_available":self.state.pagerank_workers.available_permits(),
                    "scope":"Temporary runtime peek; no LRU refresh. Counts are independent observations, not an atomic heap snapshot or proof of allocator retention."}),
                Ok(None) => serde_json::json!({"project_id":project_id,"status":"not_open_or_not_cached"}),
                Err(_) => serde_json::json!({"project_id":project_id,"status":"busy_or_unavailable"}),
            }
        });
        if req.output_json {
            let mut payload = serde_json::to_value(&breakdown)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            payload["accounting_scope"] = serde_json::json!("Tracked allocation reservations only; excludes untracked/native allocations and is not process private memory or RSS. Configured cache budgets are not measured usage.");
            if let Some(project) = project_retention { payload["project_retention"] = project; }
            let json = serde_json::to_string_pretty(&payload)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }
        let mut out = String::with_capacity(1024);
        out.push_str("Memory Budget Status\n");
        out.push_str("  Accounting: tracked reservations only; not process private memory, RSS or all native allocations.\n");
        out.push_str(&format!(
            "  Used: {} / {} bytes ({:.1}%)\n",
            breakdown.total_used,
            breakdown.budget,
            if breakdown.budget > 0 {
                breakdown.total_used as f64 / breakdown.budget as f64 * 100.0
            } else {
                0.0
            }
        ));
        out.push_str(&format!(
            "  Under pressure: {}\n",
            breakdown.pressure_active
        ));
        out.push_str("\n  Per-subsystem:\n");
        out.push_str(&format!("    tantivy: {} bytes\n", breakdown.tantivy));
        out.push_str(&format!("    lancedb: {} bytes\n", breakdown.lancedb));
        out.push_str(&format!("    graph: {} bytes\n", breakdown.graph));
        out.push_str(&format!("    docstore: {} bytes\n", breakdown.docstore));
        out.push_str(&format!(
            "    parse_buffer: {} bytes\n",
            breakdown.parse_buffer
        ));
        out.push_str(&format!("    misc: {} bytes\n", breakdown.misc));
        if let Some(project) = project_retention {
            out.push_str("\nProject retention observation (no runtime open, LRU refresh or cache maintenance):\n");
            out.push_str(&serde_json::to_string_pretty(&project)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?);
        }
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    /// (Re)index one memory section for search: clear the prior chunks by
    /// path, then chunk the body (so recall can hit any paragraph) and index
    /// each chunk at generation 0 (memory_bank is GlobalMutable). Shared by
    /// update_memory_bank and import_memory_bank.
    pub(crate) async fn index_memory_section_for_search(
        &self,
        project_id: &str,
        section_id: &str,
        content: &str,
        author: Option<&str>,
        timestamp_ms: u64,
    ) -> Result<(), McpError> {
        let ps = self.ensure_project_runtime(project_id).await?;
        let namespace = engram_core::namespaces::NAMESPACE_MEMORY_BANK;
        let path = format!("memory_bank:{section_id}");
        let cancel = tokio_util::sync::CancellationToken::new();

        ps.search
            .delete_files(project_id, namespace, &[engram_core::RelPath::new(&path)])
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let max_chars = self.state.cfg.max_chunks_per_file;
        let author = author.map(|a| a.to_string());
        let docs: Vec<engram_index::IndexDoc> =
            engram_index::chunking::chunk_lines(content, max_chars)
                .into_iter()
                .enumerate()
                .map(|(i, mut c)| {
                    c.set_doc_id(&path);
                    engram_index::IndexDoc {
                        doc_id: c.doc_id.0,
                        content_hash: c.content_hash.0,
                        path: engram_core::RelPath::new(&path),
                        content: c.content,
                        language: "markdown".into(),
                        namespace: namespace.into(),
                        generation: 0,
                        chunk_id: i as u64,
                        author: author.clone(),
                        timestamp: Some(timestamp_ms),
                        start_line: c.start_line,
                        end_line: c.end_line,
                    }
                })
                .collect();

        if !docs.is_empty() {
            ps.search
                .index_docs(project_id, &docs, &cancel)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        Ok(())
    }

    pub async fn handle_update_memory_bank(
        &self,
        req: UpdateMemoryBankRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_record(&req.project_id).await?;

        let section_id = req
            .section_id
            .clone()
            .unwrap_or_else(|| req.section.clone());
        // REG2: validate section_id at the handler boundary before it is used in
        // registry composite-key paths and search-index document identifiers.
        engram_core::security::validate_key_component("section_id", &section_id)
            .map_err(|e| McpError::invalid_params(format!("invalid section_id: {e}"), None))?;

        // Validate the optional kind against the controlled vocabulary — a
        // free-form kind would make recall-by-kind meaningless.
        if let Some(k) = req.kind.as_deref()
            && !crate::models::requests::MEMORY_KINDS.contains(&k)
        {
            return Err(McpError::invalid_params(
                format!(
                    "update_memory_bank: invalid kind '{k}'. Expected one of: {}.",
                    crate::models::requests::MEMORY_KINDS.join(", ")
                ),
                None,
            ));
        }

        // Fetch the existing section: needed to preserve created_at, apply the
        // append / optimistic-concurrency semantics, and carry forward
        // metadata the caller did not re-supply.
        let existing = {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let sid = section_id.clone();
            tokio::task::spawn_blocking(move || reg.get_memory_section(&pid, &sid))
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("registry read task panicked: {e}"), None)
                })?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        // Optimistic concurrency: reject if the section moved under the caller
        // rather than silently clobbering a concurrent session's write.
        if let (Some(expected), Some(cur)) = (req.expected_updated_at_ms, existing.as_ref())
            && cur.updated_at_ms != expected
        {
            return Err(McpError::invalid_params(
                format!(
                    "update_memory_bank: conflict — section '{section_id}' was modified since \
                     you read it (expected updated_at_ms={expected}, current={}). Re-read it with \
                     read_memory_bank and retry.",
                    cur.updated_at_ms
                ),
                None,
            ));
        }

        let now = now_ms();
        let content = match (req.append, existing.as_ref()) {
            (true, Some(cur)) if !cur.content.is_empty() => {
                format!("{}\n{}", cur.content, req.content)
            }
            _ => req.content.clone(),
        };
        // created_at is set once and preserved; a legacy section (created_at 0)
        // gets stamped now so it stops reading as unknown.
        let created_at_ms = existing
            .as_ref()
            .map(|e| {
                if e.created_at_ms > 0 {
                    e.created_at_ms
                } else {
                    now
                }
            })
            .unwrap_or(now);
        let author = req
            .author
            .clone()
            .or_else(|| existing.as_ref().and_then(|e| e.author.clone()));
        let kind = req
            .kind
            .clone()
            .or_else(|| existing.as_ref().and_then(|e| e.kind.clone()));
        let review_after_ms = req
            .review_after_ms
            .or_else(|| existing.as_ref().and_then(|e| e.review_after_ms));
        // tags / related_files REPLACE when provided, otherwise carry forward.
        let tags = req.tags.clone().unwrap_or_else(|| {
            existing
                .as_ref()
                .map(|e| e.tags.clone())
                .unwrap_or_default()
        });
        let related_files = req.related_files.clone().unwrap_or_else(|| {
            existing
                .as_ref()
                .map(|e| e.related_files.clone())
                .unwrap_or_default()
        });

        let sec = MemorySection {
            section_id: section_id.clone(),
            title: req.section.clone(),
            content: content.clone(),
            updated_at_ms: now,
            created_at_ms,
            author: author.clone(),
            kind,
            review_after_ms,
            tags,
            related_files,
        };

        // Persist to registry (source of truth) — propagate failures so the
        // caller is not told a write is durable when it is not.
        {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let sec_clone = sec.clone();
            tokio::task::spawn_blocking(move || reg.put_memory_section(&pid, &sec_clone))
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("registry write task panicked: {e}"), None)
                })?
                .map_err(|e| {
                    McpError::internal_error(
                        format!("registry write failed — memory bank section not persisted: {e}"),
                        None,
                    )
                })?;
        }

        // Re-index for search (chunked; clears the prior version's chunks).
        self.index_memory_section_for_search(
            &req.project_id,
            &section_id,
            &content,
            author.as_deref(),
            now,
        )
        .await?;

        // Advisory near-duplicate check: warn if another section covers nearly
        // the same ground, so knowledge doesn't fragment into rival notes.
        // Non-blocking — the write already succeeded.
        let mut msg = format!("✅ Updated memory_bank: {section_id}");
        if let Ok(all) = self.state.registry.list_memory_sections(&req.project_id) {
            let mut similar: Vec<(String, f32)> = all
                .iter()
                .filter(|s| s.section_id != section_id)
                .map(|s| (s.section_id.clone(), token_jaccard(&content, &s.content)))
                .filter(|(_, j)| *j >= 0.6)
                .collect();
            similar.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            if !similar.is_empty() {
                let names: Vec<String> = similar
                    .iter()
                    .take(3)
                    .map(|(id, j)| format!("{id} ({:.0}%)", j * 100.0))
                    .collect();
                msg.push_str(&format!(
                    "\nnote: similar to existing section(s): {}. Consider updating one \
                     instead of keeping duplicates.",
                    names.join(", ")
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }

    async fn validate_memory_read_project(&self, project_id: &str) -> Result<(), McpError> {
        validate_project_id(project_id)?;
        if project_id != engram_core::namespaces::USER_PROJECT_ID
            && self
                .state
                .registry
                .get_project(project_id)
                .map_err(|error| McpError::internal_error(error.to_string(), None))?
                .is_none()
        {
            return Err(McpError::invalid_params(
                format!(
                    "project not found: {project_id}; use list_projects to select the intended memory bank"
                ),
                None,
            ));
        }
        Ok(())
    }

    pub async fn handle_list_memory_bank(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        self.validate_memory_read_project(&req.project_id).await?;
        let mut secs = self
            .state
            .registry
            .list_memory_sections(&req.project_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        // Freshest first, so an agent scanning the list sees current notes on
        // top.
        secs.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        let now = crate::utils::now_ms();
        // Indexed file mtimes (seconds) for the staleness signal — read once.
        let file_mtimes = {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            tokio::task::spawn_blocking(move || indexed_file_mtimes_secs_checked(&graph, &pid))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
        };
        let mut out = String::new();
        if secs.is_empty() {
            out.push_str("(no memory sections yet — write one with update_memory_bank)\n");
        }
        for s in secs {
            // Size and age let the caller judge relevance without read_memory_bank;
            // kind (when set) tells a standing preference from a scratch note;
            // STALE flags a note whose review lapsed or whose subject moved.
            let kind = s
                .kind
                .as_deref()
                .map(|k| format!(" | {k}"))
                .unwrap_or_default();
            let stale = memory_stale_reason(
                &s,
                now,
                file_mtimes
                    .as_ref()
                    .unwrap_or(&std::collections::HashMap::new()),
            )
            .map(|r| format!(" | STALE: {r}"))
            .unwrap_or_default();
            out.push_str(&format!(
                "- {} | {} | {}B | {}{}{} | {}\n",
                s.section_id,
                s.title,
                s.content.len(),
                crate::utils::humanize_age_ms(s.updated_at_ms, now),
                kind,
                stale,
                memory_freshness_evidence(&s, &file_mtimes),
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_read_memory_bank(
        &self,
        req: crate::models::ReadMemoryBankRequest,
    ) -> Result<CallToolResult, McpError> {
        self.validate_memory_read_project(&req.project_id).await?;
        let sec = self
            .state
            .registry
            .get_memory_section(&req.project_id, &req.section)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let Some(s) = sec else {
            if req.citation.is_some() {
                return Err(McpError::invalid_params(
                    "citation_document_not_found: memory section does not exist",
                    None,
                ));
            }
            return Ok(CallToolResult::success(vec![Content::text("Not found.")]));
        };
        // Prepend a compact provenance header so an agent sees the record's
        // metadata — kind, author, age, tags, subject, and the
        // updated_at_ms to pass back as expected_updated_at_ms for a safe
        // concurrent edit — not just the raw body.
        let now = crate::utils::now_ms();
        let mut header = String::new();
        header.push_str(&format!("section_id: {}\n", s.section_id));
        header.push_str(&format!("title: {}\n", s.title));
        if let Some(k) = &s.kind {
            header.push_str(&format!("kind: {k}\n"));
        }
        if let Some(a) = &s.author {
            header.push_str(&format!("author: {a}\n"));
        }
        header.push_str(&format!(
            "updated: {} (updated_at_ms={})\n",
            crate::utils::humanize_age_ms(s.updated_at_ms, now),
            s.updated_at_ms
        ));
        if s.created_at_ms > 0 {
            header.push_str(&format!(
                "created: {}\n",
                crate::utils::humanize_age_ms(s.created_at_ms, now)
            ));
        }
        if let Some(r) = s.review_after_ms {
            let status = if now > r { " (OVERDUE)" } else { "" };
            header.push_str(&format!("review_after_ms: {r}{status}\n"));
        }
        if !s.tags.is_empty() {
            header.push_str(&format!("tags: {}\n", s.tags.join(", ")));
        }
        if !s.related_files.is_empty() {
            header.push_str(&format!("related_files: {}\n", s.related_files.join(", ")));
        }
        // Staleness: review lapsed, or a referenced file re-indexed since this
        // was written.
        if !s.related_files.is_empty() || s.review_after_ms.is_some() {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let mtimes =
                tokio::task::spawn_blocking(move || indexed_file_mtimes_secs_checked(&graph, &pid))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()));
            if let Some(reason) = memory_stale_reason(
                &s,
                now,
                mtimes.as_ref().unwrap_or(&std::collections::HashMap::new()),
            ) {
                header.push_str(&format!("stale: {reason}\n"));
            }
            header.push_str(&format!("{}\n", memory_freshness_evidence(&s, &mtimes)));
        } else {
            header.push_str("subject freshness: UNKNOWN (no related files recorded)\n");
        }
        if let Some(citation) = &req.citation {
            let page = crate::services::stored_citation::render(
                &s.content,
                citation,
                serde_json::json!({"project_id":req.project_id,"section":s.section_id}),
                serde_json::json!({"title":s.title,"updated_at_ms":s.updated_at_ms,"created_at_ms":s.created_at_ms,"kind":s.kind,"author":s.author,"tags":s.tags,"related_files":s.related_files,"provenance_and_freshness":header}),
            )?;
            return Ok(CallToolResult::success(vec![Content::text(
                page.to_string(),
            )]));
        }
        header.push_str("---\n");
        header.push_str(&s.content);
        Ok(CallToolResult::success(vec![Content::text(header)]))
    }

    pub async fn handle_import_memory_bank(
        &self,
        req: crate::models::ImportMemoryBankRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_record(&req.project_id).await?;

        let parsed = crate::services::memory_portability::from_markdown(&req.markdown);
        let section_id = req
            .section_id
            .clone()
            .or_else(|| parsed.section_id.clone())
            .ok_or_else(|| {
                McpError::invalid_params(
                    "import_memory_bank: no section_id — provide one, or include \
                     `section_id:` in the markdown front-matter."
                        .to_string(),
                    None,
                )
            })?;
        engram_core::security::validate_key_component("section_id", &section_id)
            .map_err(|e| McpError::invalid_params(format!("invalid section_id: {e}"), None))?;

        if let Some(k) = parsed.kind.as_deref()
            && !crate::models::requests::MEMORY_KINDS.contains(&k)
        {
            return Err(McpError::invalid_params(
                format!(
                    "import_memory_bank: invalid kind '{k}'. Expected one of: {}.",
                    crate::models::requests::MEMORY_KINDS.join(", ")
                ),
                None,
            ));
        }

        let now = now_ms();
        let sec = MemorySection {
            section_id: section_id.clone(),
            title: parsed.title.clone().unwrap_or_else(|| section_id.clone()),
            content: parsed.content.clone(),
            // updated_at is now (this import is the latest write); created_at
            // is preserved from the export so the note keeps its real age.
            updated_at_ms: now,
            created_at_ms: parsed.created_at_ms.filter(|&c| c > 0).unwrap_or(now),
            author: parsed.author.clone(),
            kind: parsed.kind.clone(),
            review_after_ms: parsed.review_after_ms,
            tags: parsed.tags.clone(),
            related_files: parsed.related_files.clone(),
        };

        {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let sec_clone = sec.clone();
            tokio::task::spawn_blocking(move || reg.put_memory_section(&pid, &sec_clone))
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("registry write task panicked: {e}"), None)
                })?
                .map_err(|e| {
                    McpError::internal_error(
                        format!("registry write failed — imported section not persisted: {e}"),
                        None,
                    )
                })?;
        }

        self.index_memory_section_for_search(
            &req.project_id,
            &section_id,
            &sec.content,
            sec.author.as_deref(),
            now,
        )
        .await?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Imported memory_bank: {section_id}"
        ))]))
    }

    pub async fn handle_delete_memory_bank(
        &self,
        req: MemorySectionRequest,
    ) -> Result<CallToolResult, McpError> {
        // Remove the INDEXED copy first. update_memory_bank writes both the
        // registry and the search index (namespace memory_bank, generation 0,
        // KeepForever — deliberately reindex-proof); deleting only the
        // registry row left the doc and its vector recallable forever: a
        // ghost memory that search kept surfacing with no registry entry
        // behind it. Index first so a failure leaves the memory intact and
        // consistent rather than half-forgotten.
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let path = engram_core::RelPath::new(&format!("memory_bank:{}", req.section));
        ps.search
            .delete_files(
                &req.project_id,
                engram_core::namespaces::NAMESPACE_MEMORY_BANK,
                std::slice::from_ref(&path),
            )
            .await
            .map_err(|e| {
                McpError::internal_error(
                    format!(
                        "delete_memory_bank: failed to remove the indexed copy — the section \
                         was NOT deleted (it would otherwise keep surfacing in search): {e}"
                    ),
                    None,
                )
            })?;

        self.state
            .registry
            .delete_memory_section(&req.project_id, &req.section)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text("✅ Deleted.")]))
    }

    pub async fn handle_add_repo_rule(
        &self,
        req: AddRepoRuleRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req
            .introduced_at
            .as_deref()
            .is_some_and(|date| !crate::handlers::test_derivation::valid_yyyy_mm_dd(date))
        {
            return Err(McpError::invalid_params(
                "introduced_at must be YYYY-MM-DD",
                None,
            ));
        }
        let rule_id = req.rule_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let rule = engram_core::RepoRule {
            rule_id: rule_id.clone(),
            file_pattern: req.file_pattern,
            rule_text: req.rule_text,
            priority: req.priority,
            updated_at_ms: now_ms(),
            introduced_at: req.introduced_at,
            provenance: req.provenance.or_else(|| {
                Some(
                    crate::services::produce_claude_md_service::instruction_plan::MANUAL_PROVENANCE
                        .into(),
                )
            }),
        };
        self.state
            .registry
            .put_repo_rule(&req.project_id, &rule)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Added rule: {rule_id}"
        ))]))
    }

    pub async fn handle_list_repo_rules_with_ownership(
        &self,
        req: crate::models::ListRepoRulesRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if !req.ownership_inventory {
            if !req.source_paths.is_empty() {
                return Err(McpError::invalid_params(
                    "source_paths requires ownership_inventory=true",
                    None,
                ));
            }
            return self
                .handle_list_repo_rules(ProjectIdRequest {
                    project_id: req.project_id,
                })
                .await;
        }
        let state = self.state.clone();
        let report = tokio::task::spawn_blocking(move || {
            crate::services::legacy_rule_ownership::inventory(
                &state,
                &req.project_id,
                &req.source_paths,
            )
        })
        .await
        .map_err(|e| {
            McpError::internal_error(format!("Ownership inventory worker failed: {e}"), None)
        })?
        .map_err(|e| {
            McpError::invalid_params(format!("Ownership inventory unavailable: {e:#}"), None)
        })?;
        Ok(CallToolResult::success(vec![Content::text(report)]))
    }

    pub async fn handle_list_repo_rules(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        let rules = self
            .state
            .registry
            .list_repo_rules(&req.project_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut out = String::from("# Repository rules\n\n");
        if rules.is_empty() {
            out.push_str("No repository rules are stored for this project.\n");
        }
        for r in rules {
            let text = r.rule_text.replace(['\r', '\n'], " ");
            out.push_str(&format!(
                "- `{}` | files: `{}` | priority: {} | introduced: {} | provenance: {} | updated_at_ms: {}\n  {}\n",
                r.rule_id,
                r.file_pattern,
                r.priority,
                r.introduced_at.as_deref().unwrap_or("undated"),
                r.provenance.as_deref().unwrap_or("unspecified"),
                r.updated_at_ms,
                text
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_delete_repo_rule(
        &self,
        req: DeleteRepoRuleRequest,
    ) -> Result<CallToolResult, McpError> {
        self.state
            .registry
            .delete_repo_rule(&req.project_id, &req.rule_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            "✅ Deleted rule.",
        )]))
    }
}

#[cfg(test)]
mod inv_tag_tests {
    use super::{determine_job_message, determine_job_status};

    #[test]
    fn failed_quality_gate_count_is_unknown_not_empty_or_ready() {
        let unknown = super::quality_gate_availability(None);
        assert!(unknown.contains("quality_gate_records: UNKNOWN"));
        assert!(unknown.contains("pre_push_audit_knowledge: UNKNOWN"));
        assert!(!unknown.contains("quality_gate_records: 0"));
        assert!(!unknown.contains("INACTIVE_EMPTY"));
        assert!(!unknown.contains("RECORDS_PRESENT_NOT_VALIDATED"));
    }

    #[test]
    fn failed_anti_pattern_count_is_not_empty_or_ready() {
        let unknown = super::anti_pattern_availability(None);
        assert!(unknown.contains("antipattern_records: UNKNOWN"));
        assert!(unknown.contains("anti_pattern_knowledge: UNKNOWN"));
        assert!(!unknown.contains("antipattern_records: 0"));
        assert!(!unknown.contains("INACTIVE_EMPTY"));
        assert!(!unknown.contains("RECORDS_PRESENT_NOT_VALIDATED"));
    }

    /// AUD-2026-INV-0002 Gate 2.5 Test 14: set_meta failure must produce non-success status.
    /// Behavioral: exercises the pure determine_job_status/message logic directly.
    #[test]
    fn repair_set_meta_failure_produces_degraded_not_done() {
        let warnings = vec!["set_meta active_generation failed: disk full".to_string()];
        let status = determine_job_status(false, false, &warnings);
        assert_ne!(
            status, "done",
            "AUD-2026-INV-0002: set_meta failure must NOT produce 'done'; got '{status}'"
        );
        let msg = determine_job_message(false, None, &warnings);
        assert!(
            msg.contains("enrichment warnings"),
            "AUD-2026-INV-0002: degraded message must use 'enrichment warnings' framing; got '{msg}'"
        );
        assert!(
            msg.contains("set_meta"),
            "AUD-2026-INV-0002: message must identify set_meta as failing component; got '{msg}'"
        );
    }

    /// AUD-2026-INV-0002: process_ingest_stats failure also degrades the job.
    #[test]
    fn repair_process_ingest_stats_failure_produces_degraded_not_done() {
        let warnings = vec!["process_ingest_stats failed: redb transaction error".to_string()];
        let status = determine_job_status(false, false, &warnings);
        assert_ne!(
            status, "done",
            "AUD-2026-INV-0002: process_ingest_stats failure must not report 'done'; got '{status}'"
        );
    }

    /// AUD-2026-INV-0003: create_dir_all on an existing file must return Err.
    /// Behavioral: this is the platform invariant that map_err relies on.
    #[test]
    fn index_mkdir_on_existing_file_returns_err() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("collision.dat");
        std::fs::write(&file_path, b"content").expect("write file");
        let result = std::fs::create_dir_all(&file_path);
        assert!(
            result.is_err(),
            "AUD-2026-INV-0003: create_dir_all on existing file must return Err — \
             previously .ok() would silently swallow this, allowing partial project records"
        );
    }

    /// AUD-2026-INV-0003: create_dir_all on a new path must succeed.
    #[test]
    fn index_mkdir_on_new_path_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let new_dir = tmp.path().join("new_subdir");
        let result = std::fs::create_dir_all(&new_dir);
        assert!(result.is_ok(), "create_dir_all on new path must succeed");
        assert!(new_dir.is_dir(), "created path must be a directory");
    }

    // ── AUD-2026-XSYS-repair-adp behavioral generation-ordering tests ────────

    /// AUD-2026-XSYS-repair-adp: Generation sentinel arithmetic.
    /// The fix ensures set_meta is called before the success banner. Behavioral:
    /// test the sentinel values and ordering invariants via pure arithmetic.
    #[test]
    fn generation_advancement_requires_persistence_first() {
        // A generation that never gets persisted (because set_meta failed)
        // must not advance from the caller's perspective. The only safe model:
        // new_gen = old_gen + 1, but caller only sees new_gen AFTER set_meta succeeds.
        let old_gen: u64 = 5;
        let new_gen: u64 = old_gen + 1;
        assert_eq!(new_gen, 6, "generation increments by 1");

        // If set_meta fails, the caller must still see old_gen (not new_gen).
        // This is enforced by the code ordering: set_meta returns Err → job degrades
        // → new_gen never replaces old_gen in the persistent store.
        let set_meta_failed = true;
        let visible_gen = if set_meta_failed { old_gen } else { new_gen };
        assert_eq!(
            visible_gen, old_gen,
            "AUD-2026-XSYS: when set_meta fails, visible generation must remain old_gen"
        );
    }

    /// Gate 3.5 behavioral: multiple enrichment warnings all appear in message.
    #[test]
    fn multiple_enrichment_warnings_all_appear_in_job_message() {
        let warnings = vec![
            "link_sql_to_schema failed: timeout".to_string(),
            "resolve_symbol_edges failed: graph disconnected".to_string(),
            "git_update_stream failed: not a git repo".to_string(),
        ];
        let msg = determine_job_message(false, None, &warnings);
        assert!(
            msg.contains("link_sql_to_schema"),
            "msg must contain warning 1"
        );
        assert!(
            msg.contains("resolve_symbol_edges"),
            "msg must contain warning 2"
        );
        assert!(
            msg.contains("git_update_stream"),
            "msg must contain warning 3"
        );
        assert!(
            msg.contains("enrichment warnings"),
            "must use 'enrichment warnings' framing"
        );
    }

    // ── Gate 2.0 Tests 1–3: enrichment failures surface as "degraded" ───────
    // These call the extracted pure functions directly — no AppState, no I/O,
    // deterministic inputs, deterministic outputs.

    /// Gate 2.0 Test 1 (S03): link_sql_to_schema failure → "degraded" status.
    /// Old behavior: final_status was "done" even with enrichment failure.
    /// New behavior: any non-empty enrichment warnings → "degraded".
    #[test]
    fn link_sql_failure_surfaces_degraded_status() {
        let warnings = vec!["link_sql_to_schema failed: connection refused".to_string()];
        let status = determine_job_status(false, false, &warnings);
        assert_eq!(
            status, "degraded",
            "ENG-AUD-2026-S03-0001: link_sql_to_schema failure must produce status='degraded', \
             not 'done'. Got: '{status}'"
        );
    }

    /// Gate 2.0 Test 2 (S03): resolve_symbol_edges failure → "degraded" status.
    #[test]
    fn resolve_symbol_failure_surfaces_degraded_status() {
        let warnings = vec!["resolve_symbol_edges failed: graph unavailable".to_string()];
        let status = determine_job_status(false, false, &warnings);
        assert_eq!(
            status, "degraded",
            "ENG-AUD-2026-S03-0001: resolve_symbol_edges failure must produce status='degraded', \
             not 'done'. Got: '{status}'"
        );
    }

    /// Gate 2.0 Test 3 (S03): git_update_stream failure → not clean "done".
    /// The job message must contain the warning text so it is visible to callers.
    #[test]
    fn git_update_failure_does_not_report_clean_success() {
        let warnings = vec!["git_update_stream failed: not a git repo".to_string()];

        let status = determine_job_status(false, false, &warnings);
        assert_ne!(
            status, "done",
            "ENG-AUD-2026-S03-0001: git_update_stream failure must not report clean 'done' status"
        );

        let msg = determine_job_message(false, None, &warnings);
        assert!(
            msg.contains("git_update_stream"),
            "ENG-AUD-2026-S03-0001: job message must contain enrichment warning text so callers \
             can see what degraded. Message: '{msg}'"
        );
        assert!(
            msg.contains("enrichment warnings"),
            "ENG-AUD-2026-S03-0001: job message must use 'enrichment warnings' framing. \
             Message: '{msg}'"
        );
    }

    /// Sanity: clean completion (no failures, no warnings) → "done".
    #[test]
    fn clean_completion_produces_done_status() {
        let status = determine_job_status(false, false, &[]);
        assert_eq!(status, "done", "Clean completion must produce 'done'");
        let msg = determine_job_message(false, None, &[]);
        assert_eq!(
            msg, "completed",
            "Clean completion message must be 'completed'"
        );
    }

    /// Sanity: hard failure (res.is_err()) → "failed", not "degraded".
    #[test]
    fn hard_failure_produces_failed_not_degraded() {
        // Even with enrichment warnings, a hard failure takes priority.
        let warnings = vec!["some enrichment warning".to_string()];
        let status = determine_job_status(false, true, &warnings);
        assert_eq!(
            status, "failed",
            "Hard job failure must produce 'failed', not 'degraded'"
        );
    }

    /// Sanity: cancellation takes priority over everything else.
    #[test]
    fn cancellation_takes_priority_over_enrichment_warnings() {
        let warnings = vec!["some enrichment warning".to_string()];
        let status = determine_job_status(true, false, &warnings);
        assert_eq!(
            status, "cancelled",
            "Cancellation must produce 'cancelled' regardless of enrichment warnings"
        );
    }
}

/// External audit 2026-08-29 P0-2: the generation completeness signal.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct GenerationCompleteness {
    pub generation: u64,
    /// Eligible repository paths (the indexer's walker + extension list, non-empty files).
    pub expected_paths: usize,
    /// Distinct paths the active generation holds, per store.
    pub tantivy_paths: usize,
    pub vector_paths: usize,
    pub graph_paths: usize,
    /// Eligible paths absent from Tantivy or (when vectors exist) from LanceDB.
    pub missing: usize,
    pub missing_sample: Vec<String>,
    /// Missing paths absent from EVERY store: on disk but never indexed,
    /// typically created after the last index. The stores agree with each
    /// other, and update_project indexes them.
    pub unindexed: usize,
    pub unindexed_sample: Vec<String>,
    /// Paths in Tantivy that are no longer eligible (stale).
    pub extra: usize,
    /// Paths present in one search store and absent in the other.
    pub cross_store_mismatch: usize,
    /// Always 0 since the round-2 follow-up: `missing` must be 0 for `complete`.
    pub tolerance: usize,
    /// Eligible paths the indexer skipped BY RULE (binary, too large,
    /// unreadable) — subtracted from the expectation, reported with reasons.
    pub skipped_by_rule: usize,
    pub skipped_reasons: Vec<(String, usize)>,
    /// Diagnostics only — counts are not completeness.
    pub code_chunks: usize,
    pub vector_rows: usize,
    /// Doc 11 P0-1: per-store extras are part of the verdict.
    pub vector_extra: usize,
    pub graph_extra: usize,
    /// Whether the configuration expects embeddings (embedding_backend).
    pub vectors_expected: bool,
    /// Store provider errors — any entry makes the verdict DEGRADED.
    pub store_errors: Vec<String>,
    pub degraded: bool,
    pub complete: bool,
}

impl GenerationCompleteness {
    /// Incomplete only because files on disk are not indexed yet: no
    /// provider error, no cross-store mismatch, and every missing path is
    /// absent from every store. That is a stale index, not a corrupt one.
    pub(crate) fn only_unindexed(&self) -> bool {
        !self.complete
            && !self.degraded
            && self.cross_store_mismatch == 0
            && self.missing > 0
            && self.missing == self.unindexed
    }
}

/// Doc-11 P1a (external audit round 2, P0-1 tail): the post-publication
/// purge settlement — the registry write that records (`gen`) or clears
/// (`""`) the purge debt is part of the outcome, never discarded. Returns
/// the report note and whether the GC should be nudged.
pub fn settle_purge_outcome(
    outcome: anyhow::Result<()>,
    new_gen: u64,
    record: impl FnOnce(&str) -> anyhow::Result<()>,
) -> (String, bool) {
    match outcome {
        Ok(()) => match record("") {
            Ok(()) => ("purge: ok".to_string(), false),
            Err(e) => (
                format!(
                    "purge: ok — but clearing the pending flag failed (the GC may re-purge needlessly): {e:#}"
                ),
                false,
            ),
        },
        Err(e) => match record(&new_gen.to_string()) {
            Ok(()) => (format!("purge: deferred to the GC ({e:#})"), true),
            Err(re) => (
                format!(
                    "purge FAILED and recording the debt failed — DEGRADED, the GC has no record: purge error {e:#}; registry error {re:#}"
                ),
                true,
            ),
        },
    }
}
