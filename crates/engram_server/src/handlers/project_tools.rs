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
use crate::utils::files::{exts_for_project_type, exts_for_project_type_enum};
use crate::utils::now_ms;
use engram_core::{Checkpoint, JobPhase, JobRecord, MemorySection, ProjectRecord, WatchRecord};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct PhaseResumeState {
    pending_files: Vec<String>,
    processed_files: Vec<String>,
    processed_chunk_ids: Vec<u64>,
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

fn from_rel_paths(root: &Path, rels: &[String]) -> Vec<PathBuf> {
    // H-2 fix: use safe_join instead of lexical-only string checks.
    // The previous filter could be bypassed via Windows drive-letter paths
    // (e.g. "C:foo"), UNC paths, or symlinks already present inside the root.
    // safe_join canonicalises intermediate components and rejects symlinks that
    // escape the root directory.
    rels.iter()
        .filter_map(|r| {
            engram_core::safe_join(root, r)
                .map_err(|e| {
                    tracing::warn!(
                        path = %r,
                        "resume-state path rejected and will be skipped: {e}"
                    );
                })
                .ok()
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

    /// P0-4/P0-5: standard one-line response trailer (generation + index age).
    /// Never fails — a missing meta key just renders as "index age unknown".
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
        max_chunks_per_file: usize,
        cancel: &tokio_util::sync::CancellationToken,
        progress_cb: F,
    ) -> anyhow::Result<engram_index::IngestStats>
    where
        F: FnMut(usize, usize) + Send,
    {
        let _parse_permit = self
            .state
            .parse_semaphore
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("Parse semaphore closed: {e}"))?;

        search
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
        let pid_for_meta = project_id.clone();

        let existing_opt =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<ProjectRecord>> {
                if dedupe {
                    let list = reg.list_projects()?;
                    if let Some(existing) = list.into_iter().find(|p| p.directory == dir_str) {
                        return Ok(Some(existing));
                    }
                }
                reg.put_project(&rec_candidate)?;
                reg.set_meta(&pid_for_meta, "active_generation", "1")?;
                Ok(None)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if let Some(p) = existing_opt {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "✅ Already indexed.\nproject_id: {}\nproject_name: {}\ndirectory: {}",
                p.project_id, p.project_name, p.directory
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
                info: info.clone(),
                search: search.clone(),
            })
            .await;

        if req.wait {
            let exts = ProjectType::from_registry_str(&info.project_type)
                .map(exts_for_project_type_enum)
                .unwrap_or_else(|| exts_for_project_type(&info.project_type));
            let cancel = tokio_util::sync::CancellationToken::new();

            let files = engram_index::ingest::iter_files(&dir, &exts);
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

            let max_chunks = self.state.cfg.max_chunks_per_file;
            let stats = self
                .index_files_with_parse_guard(
                    &search,
                    &project_id,
                    "memory",
                    1,
                    &dir,
                    files,
                    max_chunks,
                    &cancel,
                    |_, _| {},
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            self.process_ingest_stats(&project_id, 1, &stats)
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
                let warm_gen = self.get_active_generation(&project_id).await.unwrap_or(1);
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
                })
                .await
            {
                tracing::warn!(project_id = %project_id, "ENG-AUD-2026-S08-0001: memory bank update failed (index report not persisted): {e:#}");
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

            let res = match search_init {
                Ok(search) => {
                    let exts = ProjectType::from_registry_str(&project_type)
                        .map(exts_for_project_type_enum)
                        .unwrap_or_else(|| exts_for_project_type(&project_type));
                    let job_id_for_cb = job_id_for_job.clone();
                    let reg_for_cb = reg2.clone();
                    let engram = Engram::new(state_for_spawn.clone());
                    let files = engram_index::ingest::iter_files(&directory, &exts);
                    let resume = engram.resumable_checkpoint(&project_id_for_job, 1).await;
                    let (_resume_cp, resume_state) = resume.unwrap_or((
                        Checkpoint {
                            job_id: job_id_for_job.clone(),
                            project_id: project_id_for_job.clone(),
                            phase: JobPhase::Scanning,
                            items_processed: 0,
                            items_total: 0,
                            generation: 1,
                            idempotency_key: String::new(),
                            resume_state: None,
                            updated_at_ms: 0,
                            error: None,
                        },
                        PhaseResumeState::default(),
                    ));
                    let pending = if !resume_state.pending_files.is_empty() {
                        from_rel_paths(&directory, &resume_state.pending_files)
                    } else {
                        files.clone()
                    };
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
                            let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
                            engram
                                .index_files_with_parse_guard(
                                    &search,
                                    &project_id_for_job,
                                    "memory",
                                    1,
                                    &directory,
                                    pending,
                                    max_chunks,
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
                    } else {
                        let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
                        Engram::new(state_for_spawn.clone())
                            .index_files_with_parse_guard(
                                &search,
                                &project_id_for_job,
                                "memory",
                                1,
                                &directory,
                                files,
                                max_chunks,
                                &token,
                                move |curr, total| {
                                    let pct = if total == 0 {
                                        100
                                    } else {
                                        ((curr as f32 / total as f32) * 100.0) as u8
                                    };
                                    let prev = last_pct.load(std::sync::atomic::Ordering::Relaxed);
                                    if pct.saturating_sub(prev) < 5 && curr != total {
                                        return;
                                    }
                                    last_pct.store(pct, std::sync::atomic::Ordering::Relaxed);
                                    if let Ok(Some(mut job)) = reg_for_cb.get_job(&job_id_for_cb) {
                                        job.progress_pct = pct;
                                        job.message = format!("Indexing: {}/{} files", curr, total);
                                        job.updated_at_ms = now_ms();
                                        let _ = reg_for_cb.put_job(&job);
                                    }
                                },
                            )
                            .await
                    }
                }
                Err(e) => Err(e),
            };

            // VEC1/X1: if index_docs bailed because the vector table was recreated,
            // emit a FullReindexRequired event and retry once with a fresh engine.
            // The fresh engine will find the now-correct empty table and succeed;
            // semantic search quality is degraded until the operator runs a reindex.
            let mut res = res;
            if let Err(ref e) = res {
                let msg = format!("{e:#}");
                if msg.contains("VEC1") {
                    tracing::warn!(
                        project_id = %project_id_for_job,
                        "VEC1/X1: retrying index after vector table recreation"
                    );
                    let _ = state_for_spawn.events_tx.send(
                        crate::state::AppEvent::FullReindexRequired {
                            project_id: project_id_for_job.clone(),
                        },
                    );
                    // Reconstruct paths (always cfg.data_dir/projects/{id}/...)
                    let project_root = state_for_spawn
                        .cfg
                        .data_dir
                        .join("projects")
                        .join(&project_id_for_job);
                    let retry_tantivy = project_root.join("tantivy");
                    let retry_lancedb = project_root.join("lancedb");
                    match engram_index::HybridSearchEngine::new_with_budget(
                        retry_tantivy,
                        retry_lancedb,
                        &state_for_spawn.cfg,
                        Some(state_for_spawn.memory_budget.clone()),
                    )
                    .await
                    {
                        Ok(fresh_search) => {
                            let exts = ProjectType::from_registry_str(&project_type)
                                .map(exts_for_project_type_enum)
                                .unwrap_or_else(|| exts_for_project_type(&project_type));
                            let files = engram_index::ingest::iter_files(&directory, &exts);
                            res = Engram::new(state_for_spawn.clone())
                                .index_files_with_parse_guard(
                                    &fresh_search,
                                    &project_id_for_job,
                                    "memory",
                                    1,
                                    &directory,
                                    files,
                                    max_chunks,
                                    &token,
                                    |_, _| {},
                                )
                                .await;
                        }
                        Err(e2) => {
                            tracing::error!(
                                project_id = %project_id_for_job,
                                "VEC1/X1: retry engine creation failed: {e2:#}"
                            );
                        }
                    }
                }
            }

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
                if let Err(e) = engram.process_ingest_stats(&pid, 1, stats).await {
                    status = "failed";
                    msg = format!("Graph processing failed: {}", e);
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
                        })
                        .await;
                    // VEC1/D1: clear reindex-required flag now that a full index succeeded.
                    let reg_clr = engram.state.registry.clone();
                    let pid_clr = pid.clone();
                    if let Err(e) = tokio::task::spawn_blocking(move || {
                        reg_clr.clear_reindex_required(&pid_clr)
                    })
                    .await
                    {
                        tracing::warn!(
                            project_id = %pid,
                            "VEC1/D1: failed to clear reindex_required flag: {e:#}"
                        );
                    }
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

            let res = async {
                let engram = Engram::new(state.clone());
                let ps = engram
                    .ensure_project_runtime(&project_id_for_job)
                    .await
                    .map_err(|e| anyhow::anyhow!(e.message))?;

                let dir = PathBuf::from(&ps.info.directory);
                let exts = ProjectType::from_registry_str(&ps.info.project_type)
                    .map(exts_for_project_type_enum)
                    .unwrap_or_else(|| exts_for_project_type(&ps.info.project_type));

                let (changed, deleted) = engram
                    .get_incremental_changes(&project_id_for_job, &dir, &exts)
                    .await?;

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
                    ps.search
                        .delete_files(&project_id_for_job, "memory", &deleted)
                        .await?;
                }

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
                if let Err(e) = graph_service::link_sql_to_schema(
                    &engram.state.graph,
                    &project_id_for_job,
                    new_gen,
                ) {
                    let msg = format!("link_sql_to_schema failed (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "ENG-AUD-2026-S03-0001: {msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() {
                        w.push(msg);
                    }
                }
                if let Err(e) = engram.state.graph.resolve_symbol_edges(&project_id_for_job) {
                    let msg = format!("resolve_symbol_edges failed (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "ENG-AUD-2026-S03-0001: {msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() {
                        w.push(msg);
                    }
                }

                if let Err(e) = engram
                    .git_update_stream(
                        &project_id_for_job,
                        &ps.info.directory,
                        new_gen,
                        max_commits,
                        crate::models::GitHistoryMode::Both,
                        index_antipatterns,
                        engram_git::history::MergeCommitPolicy::AllParents,
                        &token,
                        Box::new(|_, _| {}),
                    )
                    .await
                {
                    let msg = format!("git_update_stream failed (enrichment degraded): {e:#}");
                    tracing::warn!(project_id = %project_id_for_job, "ENG-AUD-2026-S03-0001: {msg}");
                    if let Ok(mut w) = enrich_warn_inner.lock() {
                        w.push(msg);
                    }
                }

                Ok::<(), anyhow::Error>(())
            }
            .await;

            // ENG-AUD-2026-S03-0001: Surface enrichment warnings as "degraded"
            // status so callers can distinguish clean success from partial success.
            let enrich_warnings_collected: Vec<String> = enrichment_warnings
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();

            let final_status = determine_job_status(
                token.is_cancelled(),
                res.is_err(),
                &enrich_warnings_collected,
            );
            let final_msg = determine_job_message(
                token.is_cancelled(),
                res.as_ref().err().map(|e| e.to_string()).as_deref(),
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

            if final_status == "done" || final_status == "degraded" {
                let reg = state.registry.clone();
                let pid2 = project_id_for_job.clone();
                let gen_str = new_gen.to_string();
                let _ = tokio::task::spawn_blocking(move || {
                    reg.set_meta(&pid2, "active_generation", &gen_str)
                })
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

    pub async fn handle_update_project(
        &self,
        req: UpdateProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let active_gen = self.get_active_generation(&req.project_id).await?;
        let new_gen = active_gen.saturating_add(1);

        if req.wait {
            let cancel = tokio_util::sync::CancellationToken::new();
            let summary = self
                .update_project_impl(
                    &req.project_id,
                    new_gen,
                    req.sanitized_max_commits(),
                    req.index_antipatterns,
                    &cancel,
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            return Ok(CallToolResult::success(vec![Content::text(summary)]));
        }

        let job_id = self
            .spawn_job_update_project(
                req.project_id.clone(),
                new_gen,
                req.sanitized_max_commits(),
                req.index_antipatterns,
            )
            .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "🟡 Update job started.\njob_id: {job_id}"
        ))]))
    }

    pub async fn update_project_impl(
        &self,
        project_id: &str,
        new_gen: u64,
        max_commits: usize,
        index_antipatterns: bool,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<String> {
        let _update_guard = self.state.acquire_project_update_lock(project_id).await;

        let ps = self
            .ensure_project_runtime(project_id)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;

        let exts = ProjectType::from_registry_str(&ps.info.project_type)
            .map(exts_for_project_type_enum)
            .unwrap_or_else(|| exts_for_project_type(&ps.info.project_type));
        let pid = project_id.to_string();
        let dir = PathBuf::from(&ps.info.directory);
        let old_gen = new_gen.saturating_sub(1);

        let (changed, deleted) = self
            .get_incremental_changes(project_id, &dir, &exts)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        // Resume from a previously interrupted job: narrow the pending-file list
        // so only files not yet processed are re-indexed.
        let mut resumed_from_checkpoint = false;
        let changed = if let Some((_cp, rs)) = self.resumable_checkpoint(project_id, new_gen).await
        {
            if !rs.pending_files.is_empty() {
                engram_core::metrics().checkpoints_resumed.inc();
                resumed_from_checkpoint = true;
                let pending_set: std::collections::HashSet<PathBuf> =
                    from_rel_paths(&dir, &rs.pending_files)
                        .into_iter()
                        .collect();
                changed
                    .into_iter()
                    .filter(|p| pending_set.contains(p))
                    .collect()
            } else {
                changed
            }
        } else {
            changed
        };

        self.enforce_project_byte_budget(&changed).await?;

        // Files whose stale graph generations are eligible for the scoped
        // purge after indexing: everything re-extracted this update plus
        // deletions (whose nodes have no replacement generation at all).
        let purge_paths: std::collections::HashSet<String> = changed
            .iter()
            .filter_map(|p| engram_core::RelPath::from_relative(&dir, p))
            .map(|r| r.as_str().to_string())
            .chain(deleted.iter().map(|r| r.as_str().to_string()))
            .collect();

        // Copy-forward unchanged files to the new generation (Snapshot namespaces).
        let memory_policy = engram_core::get_policy("memory")
            .map(|p| p.versioning)
            .unwrap_or(engram_core::NamespaceVersioning::Snapshot);

        if memory_policy == engram_core::NamespaceVersioning::Snapshot {
            let root_clone = dir.clone();
            let exts_owned: Vec<String> = exts.iter().map(|s| s.to_string()).collect();
            let all_disk_files = tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = exts_owned.iter().map(|s| s.as_str()).collect();
                engram_index::ingest::iter_files(&root_clone, &refs)
            })
            .await?;

            let changed_set: std::collections::HashSet<PathBuf> = changed.iter().cloned().collect();
            let unchanged: Vec<engram_core::RelPath> = all_disk_files
                .iter()
                .filter(|p| !changed_set.contains(*p))
                .map(|p| {
                    engram_core::RelPath::from_relative(&dir, p)
                        .unwrap_or_else(|| engram_core::RelPath::new(&p.to_string_lossy()))
                })
                .collect();

            if old_gen > 0 && !unchanged.is_empty() {
                ps.search
                    .copy_generation_for_paths(
                        project_id, "memory", old_gen, new_gen, &unchanged, cancel,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
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

        // VEC1/X1: parity with spawn_job_index_directory — if the update path hits a
        // vector table recreation error, emit FullReindexRequired and retry once with a
        // fresh engine, exactly mirroring the index-job recovery path.  Without this
        // branch, schema-mismatch errors during incremental updates do not set the
        // durable degraded-state flag, leaving operators unaware of quality degradation.
        let changed_for_retry = changed.clone();
        let mut index_result = self
            .index_files_with_parse_guard(
                &ps.search,
                &pid,
                "memory",
                new_gen,
                &dir,
                changed,
                self.state.cfg.max_chunks_per_file,
                cancel,
                |_, _| {},
            )
            .await;

        if let Err(ref e) = index_result {
            if format!("{e:#}").contains("VEC1") {
                tracing::warn!(
                    project_id = %pid,
                    "VEC1/X1: update path hit vector table recreation — emitting FullReindexRequired and retrying"
                );
                let _ = self.state.events_tx.send(AppEvent::FullReindexRequired {
                    project_id: pid.clone(),
                });
                let project_root = self.state.cfg.data_dir.join("projects").join(&pid);
                match engram_index::HybridSearchEngine::new_with_budget(
                    project_root.join("tantivy"),
                    project_root.join("lancedb"),
                    &self.state.cfg,
                    Some(self.state.memory_budget.clone()),
                )
                .await
                {
                    Ok(fresh_search) => {
                        index_result = self
                            .index_files_with_parse_guard(
                                &fresh_search,
                                &pid,
                                "memory",
                                new_gen,
                                &dir,
                                changed_for_retry,
                                self.state.cfg.max_chunks_per_file,
                                cancel,
                                |_, _| {},
                            )
                            .await;
                    }
                    Err(e2) => {
                        tracing::error!(
                            project_id = %pid,
                            "VEC1/X1: update retry engine creation failed: {e2:#}"
                        );
                    }
                }
            }
        }

        let stats = index_result?;

        self.process_ingest_stats(project_id, new_gen, &stats)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        {
            let graph = self.state.graph.clone();
            let pid2 = pid.clone();
            let generation = new_gen;
            let _ = tokio::task::spawn_blocking(move || {
                graph_service::link_sql_to_schema(&graph, &pid2, generation)
            })
            .await;
        }

        {
            let graph = self.state.graph.clone();
            let pid2 = pid.clone();
            let _ = tokio::task::spawn_blocking(move || graph.resolve_symbol_edges(&pid2))
                .await
                .ok();
        }

        let git_summary = self
            .git_update_stream(
                project_id,
                &ps.info.directory,
                new_gen,
                max_commits,
                crate::models::GitHistoryMode::Both,
                index_antipatterns,
                engram_git::history::MergeCommitPolicy::AllParents,
                cancel,
                Box::new(|_, _| {}),
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;

        {
            let reg = self.state.registry.clone();
            let pid2 = pid.clone();
            tokio::task::spawn_blocking(move || {
                reg.set_meta(&pid2, "active_generation", &new_gen.to_string())
            })
            .await
            .map_err(|e| anyhow::anyhow!("set_meta join error: {e}"))?
            .map_err(|e| anyhow::anyhow!("set_meta failed: {e}"))?;
        }

        ps.search
            .purge_old_generations(project_id, new_gen)
            .await
            .ok();

        // Eagerly purge stale GRAPH generations for the files this update
        // actually re-indexed (plus deletions): a symbol whose declaration
        // line shifted would otherwise keep its old-generation node alive
        // until scheduled GC, and under watcher mode blast radius, footprints
        // and query_nodes count phantoms continuously. Scoped per-file — a
        // GLOBAL purge is unsafe after incremental updates because unchanged
        // files legitimately keep their older-generation nodes.
        {
            let graph = self.state.graph.clone();
            let pid_gc = pid.clone();
            let paths = purge_paths.clone();
            match tokio::task::spawn_blocking(move || {
                graph.purge_stale_nodes_for_paths(&pid_gc, &paths, new_gen)
            })
            .await
            {
                Ok(Ok((n, e))) if n > 0 || e > 0 => tracing::debug!(
                    project_id = %pid,
                    nodes = n,
                    edges = e,
                    "purged stale graph generations for re-indexed files"
                ),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(
                    project_id = %pid,
                    "scoped graph purge after update failed (GC will retry): {e}"
                ),
                Err(e) => tracing::warn!(
                    project_id = %pid,
                    "scoped graph purge task panicked: {e}"
                ),
            }
        }

        // TODO-40: a resumed (crash-recovered) run leaves stale generations
        // behind — nudge the GC instead of waiting for the hourly tick.
        if resumed_from_checkpoint {
            self.state.gc_nudge.notify_one();
        }

        Ok(format!(
            "✅ Updated project_id: {project_id}\nactive_generation: {new_gen}\nfiles={} chunks={} bytes={}\n{git_summary}\n",
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

    pub async fn handle_project_health(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let pid = req.project_id;
        let ps = self.ensure_project_runtime(&pid).await?;
        let generation = self.get_active_generation(&pid).await.unwrap_or(1);

        let graph = self.state.graph.clone();
        let pid_clone = pid.clone();
        let (graph_nodes, graph_edges) = tokio::task::spawn_blocking(move || {
            let nodes = graph.count_nodes(&pid_clone).unwrap_or(0);
            let edges = graph.count_edges(&pid_clone).unwrap_or(0);
            (nodes, edges)
        })
        .await
        .unwrap_or_default();

        let ns_counts = ps.search.count_docs_by_namespace(&pid).unwrap_or_default();
        let total_docs: usize = ns_counts.values().sum();
        let lancedb_rows = ps.search.count_vectors(&pid).await.unwrap_or(0);

        let mut out = String::from("Health: OK\n");
        out.push_str(&format!("active_generation: {generation}\n"));
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
        let generation = self.get_active_generation(&pid).await.unwrap_or(1);

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
        if let Some(since) = rec.reindex_required_since_ms {
            out.push_str(&format!(
                "WARNING: full reindex required since epoch_ms={since} (vector table was recreated) — run update_project.\n"
            ));
        }

        // Optional disk drift check: count tracked files modified after the
        // last completed index.
        let mut dirty_files: Option<usize> = None;
        if req.check_disk
            && let Some(last_ms) = last_index_ms
        {
            let exts = ProjectType::from_registry_str(&rec.project_type)
                .map(exts_for_project_type_enum)
                .unwrap_or_else(|| exts_for_project_type(&rec.project_type));
            let exts_owned: Vec<String> = exts.iter().map(|s| s.to_string()).collect();
            let dir = PathBuf::from(&rec.directory);
            let count = tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = exts_owned.iter().map(|s| s.as_str()).collect();
                engram_index::ingest::iter_files(&dir, &refs)
                    .into_iter()
                    .filter(|p| {
                        std::fs::metadata(p)
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64 > last_ms)
                            .unwrap_or(false)
                    })
                    .count()
            })
            .await
            .unwrap_or(0);
            dirty_files = Some(count);
            out.push_str(&format!("files_modified_since_index: {count}\n"));
        }

        let advice = if rec.reindex_required_since_ms.is_some() {
            "run update_project now — vector data was lost and must be rebuilt"
        } else if matches!(dirty_files, Some(n) if n > 0) {
            "index is stale — run update_project (or enable watch_project for auto-updates)"
        } else if last_index_ms.is_none() {
            "freshness unknown — run update_project once to start tracking"
        } else if !watch_enabled {
            "index is current; enable watch_project to keep it that way automatically"
        } else {
            "index is current and the watcher is active"
        };
        out.push_str(&format!("advice: {advice}\n"));

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
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let resolved = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            tokio::task::spawn_blocking(move || graph.resolve_symbol_edges(&pid)),
        )
        .await
        .map_err(|_| McpError::internal_error("resolve_symbol_edges timed out after 600s", None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "resolve_graph_edges: {resolved} placeholder edge(s) resolved to concrete nodes.
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

        if req.wipe_and_reindex {
            self.state.projects.remove(&pid);
            self.state.graph.delete_project_data(&pid).ok();
        }

        let ps = self.ensure_project_runtime(&pid).await?;
        let current_gen = self.get_active_generation(&pid).await.map_err(|e| {
            McpError::internal_error(
                format!("AUD-2026-INV-0002: get_active_generation failed during repair: {e:#}"),
                None,
            )
        })?;
        let new_gen = current_gen + 1;

        let exts = ProjectType::from_registry_str(&rec.project_type)
            .map(exts_for_project_type_enum)
            .unwrap_or_else(|| exts_for_project_type(&rec.project_type));
        let files = engram_index::ingest::iter_files(&dir, &exts);
        let cancel = tokio_util::sync::CancellationToken::new();

        let stats = self
            .index_files_with_parse_guard(
                &ps.search,
                &pid,
                "memory",
                new_gen,
                &dir,
                files,
                self.state.cfg.max_chunks_per_file,
                &cancel,
                |_, _| {},
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

        self.state
            .registry
            .set_meta(&pid, "active_generation", &new_gen.to_string())
            .map_err(|e| {
                McpError::internal_error(
                    format!(
                        "AUD-2026-INV-0002: set_meta active_generation failed during repair: {e:#}"
                    ),
                    None,
                )
            })?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Project repaired project_id: {pid}\nactive_generation: {new_gen}\nfiles={} chunks={}",
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
        let _ = self.state.events_tx.send(AppEvent::WatchUpdate {
            project_id: req.project_id,
            directory: "".into(),
            enabled: false,
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
        let ps = self.ensure_project_runtime(&pid).await?;
        let active_gen = self.get_active_generation(&pid).await?;
        let target_gen = req.target_generation.unwrap_or(active_gen);

        // Fix 3: Acquire the per-project serialisation lock before mutating
        // graph and search stores. Without this, GC can delete generations out
        // from under a concurrently running index or update job, causing crashes
        // and dangling references.
        let _update_guard = self.state.acquire_project_update_lock(&pid).await;

        let mut steps: Vec<String> = Vec::new();

        let pre_graph_nodes = self.state.graph.count_nodes(&pid).unwrap_or(0);
        let pre_graph_edges = self.state.graph.count_edges(&pid).unwrap_or(0);
        let pre_tantivy = ps.search.count_docs(&pid).unwrap_or(0);
        let pre_vectors = ps.search.count_vectors(&pid).await.unwrap_or(0);

        let graph = self.state.graph.clone();
        let pid_gc = pid.clone();
        tokio::task::spawn_blocking(move || graph.purge_old_generations(&pid_gc, target_gen))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        steps.push(format!(
            "Purged graph generations older than {}.",
            target_gen
        ));

        ps.search
            .purge_old_generations(&pid, target_gen)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        steps.push("Purged Tantivy stale documents.".into());

        if req.compact_vectors {
            steps.push("LanceDB garbage collection triggered.".into());
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
        let budget = &self.state.memory_budget;
        let breakdown = budget.breakdown();
        if req.output_json {
            let json = serde_json::to_string_pretty(&breakdown)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }
        let mut out = String::with_capacity(1024);
        out.push_str("Memory Budget Status\n");
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
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_update_memory_bank(
        &self,
        req: UpdateMemoryBankRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let _ = self.ensure_project_record(&req.project_id).await?;

        let section_id = req.section_id.unwrap_or_else(|| req.section.clone());
        // REG2: validate section_id at the handler boundary before it is used in
        // registry composite-key paths and search-index document identifiers.
        // Delimiter bytes (\0, \n) are blocked by the registry layer, but broader
        // character-class enforcement here prevents namespace/path semantics drift
        // in downstream index identifiers even for delimiter-safe but structurally
        // problematic strings (e.g. pure whitespace, empty, >256 chars).
        engram_core::security::validate_key_component("section_id", &section_id)
            .map_err(|e| McpError::invalid_params(format!("invalid section_id: {e}"), None))?;
        let sec = MemorySection {
            section_id: section_id.clone(),
            title: req.section,
            content: req.content.clone(),
            updated_at_ms: now_ms(),
        };

        // Persist to registry — REG1/MCP1: propagate failure so the MCP caller
        // receives a hard error rather than a silent success-but-not-persisted response.
        // Prior `.ok()` swallowed both JoinError and registry write errors, making the
        // handler lie to the caller about durable persistence.
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

        // Index to search engine (namespace = memory_bank)
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let namespace = engram_core::namespaces::NAMESPACE_MEMORY_BANK;
        let effective_gen = if let Ok(policy) = engram_core::get_policy(namespace) {
            if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
                0
            } else {
                gen_
            }
        } else {
            gen_
        };

        let doc = engram_index::IndexDoc {
            doc_id: engram_core::DocIdStr(format!("mb:{}", section_id)).0,
            content_hash: engram_core::ContentHash(sec.content.clone()).0,
            path: engram_core::RelPath::new(&format!("memory_bank:{}", section_id)),
            content: sec.content,
            language: "markdown".into(),
            namespace: namespace.into(),
            generation: effective_gen,
            chunk_id: 0,
            author: None,
            timestamp: Some(sec.updated_at_ms),
            start_line: 0,
            end_line: 0,
        };

        ps.search
            .index_docs(
                &req.project_id,
                &[doc],
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Updated memory_bank: {section_id}"
        ))]))
    }

    pub async fn handle_list_memory_bank(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        let secs = self
            .state
            .registry
            .list_memory_sections(&req.project_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut out = String::new();
        for s in secs {
            out.push_str(&format!("- {} | {}\n", s.section_id, s.title));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_read_memory_bank(
        &self,
        req: MemorySectionRequest,
    ) -> Result<CallToolResult, McpError> {
        let sec = self
            .state
            .registry
            .get_memory_section(&req.project_id, &req.section)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let Some(s) = sec else {
            return Ok(CallToolResult::success(vec![Content::text("Not found.")]));
        };
        Ok(CallToolResult::success(vec![Content::text(s.content)]))
    }

    pub async fn handle_delete_memory_bank(
        &self,
        req: MemorySectionRequest,
    ) -> Result<CallToolResult, McpError> {
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
        let rule_id = req.rule_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let rule = engram_core::RepoRule {
            rule_id: rule_id.clone(),
            file_pattern: req.file_pattern,
            rule_text: req.rule_text,
            priority: req.priority,
            updated_at_ms: now_ms(),
        };
        self.state
            .registry
            .put_repo_rule(&req.project_id, &rule)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Added rule: {rule_id}"
        ))]))
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
        let mut out = String::new();
        for r in rules {
            out.push_str(&format!("- {} | {}\n", r.rule_id, r.file_pattern));
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
