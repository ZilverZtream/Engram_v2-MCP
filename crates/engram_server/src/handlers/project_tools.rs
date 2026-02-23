use crate::models::{
    IndexProjectRequest, ProjectIdRequest, RepairProjectRequest, UpdateMemoryBankRequest,
    UpdateProjectRequest, WatchProjectRequest,
};
use crate::services::{graph_service, ingest_service, project_service};
use crate::state::{AppEvent, ProjectInfo, ProjectState};
use crate::tools::Engram;
use crate::utils::files::exts_for_project_type;
use crate::utils::{dir_size_bytes, format_bytes, now_ms};
use engram_core::{Checkpoint, JobPhase, JobRecord, ProjectRecord, WatchRecord};
use rmcp::model::{CallToolResult, Content};
use rmcp::{ErrorData as McpError, handler::server::tool::Parameters};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    rels.iter()
        .filter(|r| {
            // Reject absolute paths and any path that escapes the root via `..` components.
            // These originate from checkpoint state, but we validate defensively.
            let p = std::path::Path::new(r.as_str());
            !p.is_absolute()
                && !r.contains("..")
                && !r.starts_with('/')
                && !r.starts_with('\\')
        })
        .map(|r| root.join(r))
        .collect()
}

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

    /// Generate a confidence footer for WebForms files.
    ///
    /// Returns a string like `"\n---\nextraction_confidence: medium (0.65) | ..."` for
    /// WebForms languages (aspx, vb, cs) or an empty string for non-WebForms files.
    /// When the score is below the configured threshold, adds a warning.
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

        // Heuristic confidence based on file extension signals.
        // We use lightweight signals here — full content-based scoring
        // is available via the `get_extraction_confidence` tool.
        let has_codebehind_ext = path_str.ends_with(".aspx.vb")
            || path_str.ends_with(".aspx.cs")
            || path_str.ends_with(".ascx.vb")
            || path_str.ends_with(".ascx.cs");
        let is_markup = path_str.ends_with(".aspx")
            || path_str.ends_with(".ascx")
            || path_str.ends_with(".master");

        let score = engram_index::confidence::score_event_wiring(
            is_markup,          // has_inherits_directive (markup files typically have it)
            has_codebehind_ext, // has_codebehind_file
            has_codebehind_ext, // has_matching_handler (if codebehind exists, likely)
            true,               // handler_signature_valid (assume standard)
            is_markup,          // control_id_explicit (markup has explicit IDs)
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
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // File may disappear between discovery and indexing.
                    }
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
        let _ = tokio::task::spawn_blocking(move || store.put(&cp)).await;
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
        // Single admission point for parse/chunking. This bounds spawn_blocking +
        // Rayon fan-out inside engram_index::HybridSearchEngine::index_files.
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
                    "\u{274C} {e}"
                ))]));
            }
        };

        // Dedupe + registry insert in a single Redb write transaction.
        let dir_str = dir.to_string_lossy().to_string();
        let project_id = Uuid::new_v4().to_string();
        let project_name = req.project_name.clone();
        let project_type = req.project_type.clone();
        let now = now_ms();
        let rec_candidate = ProjectRecord {
            project_id: project_id.clone(),
            project_name: project_name.clone(),
            project_type: project_type.clone(),
            directory: dir_str.clone(),
            created_at_ms: now,
            updated_at_ms: now,
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
                "\u{2705} Already indexed.\nproject_id: {}\nproject_name: {}\ndirectory: {}",
                p.project_id, p.project_name, p.directory
            ))]));
        }
        let project_root = self.state.cfg.data_dir.join("projects").join(&project_id);
        let tantivy_dir = project_root.join("tantivy");
        let lancedb_dir = project_root.join("lancedb");
        tokio::fs::create_dir_all(&tantivy_dir).await.ok();
        tokio::fs::create_dir_all(&lancedb_dir).await.ok();

        let search = engram_index::HybridSearchEngine::new_with_budget(
            tantivy_dir.clone(),
            lancedb_dir.clone(),
            &self.state.cfg,
            Some(self.state.memory_budget.clone()),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let search = std::sync::Arc::new(search);

        // Runtime cache
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
            let exts = exts_for_project_type(&info.project_type);
            let cancel = tokio_util::sync::CancellationToken::new();

            let files = engram_index::ingest::iter_files(&dir, &exts);
            if let Err(e) = self.enforce_project_byte_budget(&files).await {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{274C} {e}"
                ))]));
            }
            if let Some(limit) = self.state.cfg.max_project_files
                && files.len() as u64 > limit
            {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{274C} Too many files: {} > limit {}",
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

            // Post-ingest graph enrichment passes
            {
                let graph = self.state.graph.clone();
                let pid = project_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = graph_service::resolve_app_code_globals(&graph, &pid, 1);
                    let _ = graph_service::link_binding_fields_to_columns(&graph, &pid, 1);
                })
                .await;
            }

            // Link unresolved edges
            let graph = self.state.graph.clone();
            let pid = project_id.clone();
            tokio::task::spawn_blocking(move || graph.resolve_symbol_edges(&pid))
                .await
                .ok();

            let report = self.generate_indexing_report(&stats);
            let _ = self
                .update_memory_bank(Parameters(UpdateMemoryBankRequest {
                    project_id: project_id.clone(),
                    section_id: Some("engram/index_report".into()),
                    section: "Indexing Report".into(),
                    content: report.clone(),
                }))
                .await;

            return Ok(CallToolResult::success(vec![Content::text(format!(
                "\u{2705} Indexed project_id: {project_id}\n\n{report}"
            ))]));
        }

        // Background job
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
            "\u{1F7E1} Index job started.\njob_id: {job_id}\nproject_id: {project_id}"
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
        tokio::task::spawn_blocking({
            let job = job.clone();
            move || reg.put_job(&job)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("failed to persist job: {e}"), None))?
        .map_err(|e| {
            McpError::internal_error(format!("failed to persist job record: {e}"), None)
        })?;

        let reg2 = self.state.registry.clone();
        let projects_cache = self.state.projects.clone();
        let active_jobs = self.state.active_jobs.clone();
        let cancellation_tokens = self.state.cancellation_tokens.clone();
        let state_for_spawn = self.state.clone();
        let project_id_for_job = project_id.clone();
        let job_id_for_job = job_id.clone();

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

        let token = tokio_util::sync::CancellationToken::new();
        {
            let mut m = cancellation_tokens.write().await;
            m.insert(job_id.clone(), token.clone());
        }

        let handle = tokio::spawn(async move {
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
                    let exts = exts_for_project_type(&project_type);
                    let job_id_for_cb = job_id_for_job.clone();
                    let reg_for_cb = reg2.clone();
                    let engram = Engram::new(state_for_spawn.clone());
                    let files = engram_index::ingest::iter_files(&directory, &exts);
                    let resume = engram.resumable_checkpoint(&project_id_for_job, 1).await;
                    let (resume_cp, resume_state) = resume.unwrap_or((
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
                    if resume_cp.updated_at_ms > 0 {
                        engram_core::metrics::metrics().checkpoints_resumed.inc();
                    }
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

            state_for_spawn
                .active_indexing_count
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            // Fix #10: now that one indexing job has finished, evict any projects
            // that overshot MAX_CACHED_PROJECTS while all cache slots were busy.
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
                        Some(PhaseResumeState {
                            processed_chunk_ids: (0..stats.chunks as u64).collect(),
                            ..PhaseResumeState::default()
                        }),
                    )
                    .await;

                if let Err(e) = engram.process_ingest_stats(&pid, 1, stats).await {
                    status = "failed";
                    msg = format!("Graph processing failed: {}", e);
                    progress = 0;
                } else {
                    engram
                        .write_checkpoint(
                            &job_id_for_job,
                            &project_id_for_job,
                            1,
                            &directory,
                            JobPhase::GraphBuilding,
                            stats.symbols.len() as u64,
                            stats.symbols.len() as u64,
                            None,
                        )
                        .await;
                    let _ = crate::services::graph_service::resolve_app_code_globals(
                        &engram.state.graph,
                        &pid,
                        1,
                    );
                    let _ = crate::services::graph_service::link_binding_fields_to_columns(
                        &engram.state.graph,
                        &pid,
                        1,
                    );
                    let _ = engram.state.graph.resolve_symbol_edges(&pid);

                    let report = engram.generate_indexing_report(stats);
                    let _ = engram
                        .update_memory_bank(Parameters(UpdateMemoryBankRequest {
                            project_id: pid.clone(),
                            section_id: Some("engram/index_report".into()),
                            section: "Indexing Report".into(),
                            content: report,
                        }))
                        .await;

                    engram
                        .write_checkpoint(
                            &job_id_for_job,
                            &project_id_for_job,
                            1,
                            &directory,
                            JobPhase::GraphBuilding,
                            stats.symbols.len() as u64,
                            stats.symbols.len() as u64,
                            None,
                        )
                        .await;
                    engram
                        .write_checkpoint(
                            &job_id_for_job,
                            &project_id_for_job,
                            1,
                            &directory,
                            JobPhase::PostProcessing,
                            1,
                            1,
                            None,
                        )
                        .await;
                    status = "done";
                }
            } else if let Err(e) = res {
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
                status = "failed";
                msg = e.to_string();
                progress = 0;
            }

            engram
                .write_checkpoint(
                    &job_id_for_job,
                    &project_id_for_job,
                    1,
                    &directory,
                    JobPhase::Completed,
                    1,
                    1,
                    None,
                )
                .await;
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
            let reg_final = reg2.clone();
            let _ = tokio::task::spawn_blocking(move || reg_final.put_job(&jr)).await;

            {
                let mut m = active_jobs.write().await;
                m.remove(&job_id_for_job);
            }
            {
                let mut m = cancellation_tokens.write().await;
                m.remove(&job_id_for_job);
            }

            let _ = projects_cache;
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

        let max_jobs = state.cfg.max_concurrent_jobs;
        if state
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

        let token = tokio_util::sync::CancellationToken::new();
        {
            let mut m = self.state.cancellation_tokens.write().await;
            m.insert(job_id.clone(), token.clone());
        }

        let handle = tokio::spawn(async move {
            let mut status = "done";
            let mut msg = "completed".to_string();
            let mut progress = 100;

            let res = async {
                let ps = {
                    let project_root = state
                        .cfg
                        .data_dir
                        .join("projects")
                        .join(&project_id_for_job);
                    let tantivy_dir = project_root.join("tantivy");
                    let lancedb_dir = project_root.join("lancedb");
                    tokio::fs::create_dir_all(&tantivy_dir).await?;
                    tokio::fs::create_dir_all(&lancedb_dir).await?;
                    let search = engram_index::HybridSearchEngine::new_with_budget(
                        tantivy_dir.clone(),
                        lancedb_dir.clone(),
                        &state.cfg,
                        Some(state.memory_budget.clone()),
                    )
                    .await?;
                    let rec = state
                        .registry
                        .get_project(&project_id_for_job)?
                        .ok_or_else(|| anyhow::anyhow!("missing project"))?;
                    ProjectState {
                        info: ProjectInfo {
                            project_id: project_id_for_job.clone(),
                            project_name: rec.project_name,
                            project_type: rec.project_type,
                            directory: rec.directory,
                            tantivy_dir,
                            lancedb_dir,
                        },
                        search: std::sync::Arc::new(search),
                    }
                };

                if token.is_cancelled() {
                    return Ok(());
                }

                let dir = PathBuf::from(&ps.info.directory);
                let exts = exts_for_project_type(&ps.info.project_type);

                let job_id_for_cb = job_id_for_job.clone();
                let reg_for_cb = state.registry.clone();

                let engram = Engram::new(state.clone());
                let (changed, deleted) = engram
                    .get_incremental_changes(&project_id_for_job, &dir, &exts)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                let resume = engram
                    .resumable_checkpoint(&project_id_for_job, new_gen)
                    .await;
                let changed = if let Some((_cp, rs)) = resume {
                    if !rs.pending_files.is_empty() {
                        engram_core::metrics::metrics().checkpoints_resumed.inc();
                        from_rel_paths(&dir, &rs.pending_files)
                    } else {
                        changed
                    }
                } else {
                    changed
                };
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
                engram.enforce_project_byte_budget(&changed).await?;

                if !deleted.is_empty() {
                    ps.search
                        .delete_files(&project_id_for_job, "memory", &deleted)
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;
                }

                let reg_for_progress = reg_for_cb.clone();
                let job_id_for_progress = job_id_for_cb.clone();
                let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
                let stats = Engram::new(state.clone())
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
                                100
                            } else {
                                ((curr as f32 / total as f32) * 100.0) as u8
                            };
                            let prev = last_pct.load(std::sync::atomic::Ordering::Relaxed);
                            if pct.saturating_sub(prev) < 5 && curr != total {
                                return;
                            }
                            last_pct.store(pct, std::sync::atomic::Ordering::Relaxed);
                            if let Ok(Some(mut job)) =
                                reg_for_progress.get_job(&job_id_for_progress)
                            {
                                job.progress_pct = pct;
                                job.message = format!("Indexing: {}/{} files", curr, total);
                                job.updated_at_ms = now_ms();
                                let _ = reg_for_progress.put_job(&job);
                            }
                        },
                    )
                    .await?;

                if token.is_cancelled() {
                    return Ok(());
                }

                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        new_gen,
                        &dir,
                        JobPhase::VectorIndexing,
                        stats.chunks as u64,
                        stats.chunks as u64,
                        Some(PhaseResumeState {
                            processed_chunk_ids: (0..stats.chunks as u64).collect(),
                            ..PhaseResumeState::default()
                        }),
                    )
                    .await;
                if let Err(e) = engram
                    .process_ingest_stats(&project_id_for_job, new_gen, &stats)
                    .await
                {
                    let reg_err = reg_for_cb.clone();
                    let jid_err = job_id_for_cb.clone();
                    let err_msg = format!("Graph processing failed: {}", e);
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(Some(mut job)) = reg_err.get_job(&jid_err) {
                            job.status = "failed".into();
                            job.message = err_msg;
                            job.updated_at_ms = now_ms();
                            let _ = reg_err.put_job(&job);
                        }
                    })
                    .await;
                    return Err(anyhow::anyhow!("Graph processing failed: {}", e));
                }

                let _ = crate::services::graph_service::resolve_app_code_globals(
                    &engram.state.graph,
                    &project_id_for_job,
                    new_gen,
                );
                let _ = crate::services::graph_service::link_binding_fields_to_columns(
                    &engram.state.graph,
                    &project_id_for_job,
                    new_gen,
                );
                let _ = engram.state.graph.resolve_symbol_edges(&project_id_for_job);
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        new_gen,
                        &dir,
                        JobPhase::PostProcessing,
                        1,
                        1,
                        None,
                    )
                    .await;

                let reg_for_git = state.registry.clone();
                let jid_for_git = job_id_for_job.clone();
                let _ = engram
                    .git_update_stream(
                        &project_id_for_job,
                        &ps.info.directory,
                        new_gen,
                        max_commits,
                        index_antipatterns,
                        engram_git::history::MergeCommitPolicy::AllParents,
                        &token,
                        Box::new(move |curr, total| {
                            if let Ok(Some(mut job)) = reg_for_git.get_job(&jid_for_git)
                                && (curr % 20 == 0 || curr == total)
                            {
                                job.message =
                                    format!("Analyzing history: {}/{} commits", curr, total);
                                job.updated_at_ms = now_ms();
                                let _ = reg_for_git.put_job(&job);
                            }
                        }),
                    )
                    .await;

                Ok::<(), anyhow::Error>(())
            }
            .await;

            if token.is_cancelled() {
                let engram = Engram::new(state.clone());
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        new_gen,
                        &PathBuf::from(
                            state
                                .registry
                                .get_project(&project_id_for_job)
                                .ok()
                                .flatten()
                                .map(|p| p.directory)
                                .unwrap_or_default(),
                        ),
                        JobPhase::Failed,
                        0,
                        0,
                        None,
                    )
                    .await;
                status = "cancelled";
                msg = "cancelled by user".to_string();
                progress = 0;
            } else if let Err(e) = res {
                status = "failed";
                msg = e.to_string();
                progress = 0;
            } else {
                let engram = Engram::new(state.clone());
                engram
                    .write_checkpoint(
                        &job_id_for_job,
                        &project_id_for_job,
                        new_gen,
                        &PathBuf::from(
                            state
                                .registry
                                .get_project(&project_id_for_job)
                                .ok()
                                .flatten()
                                .map(|p| p.directory)
                                .unwrap_or_default(),
                        ),
                        JobPhase::Completed,
                        1,
                        1,
                        None,
                    )
                    .await;
                let _ = state.registry.set_meta(
                    &project_id_for_job,
                    "active_generation",
                    &new_gen.to_string(),
                );
            }

            let now = now_ms();
            let jr2 = JobRecord {
                job_id: job_id_for_job.clone(),
                kind: "update_project".into(),
                project_id: Some(project_id_for_job.clone()),
                status: status.into(),
                message: msg,
                progress_pct: progress,
                estimated_time_remaining_ms: None,
                created_at_ms: jr.created_at_ms,
                updated_at_ms: now,
            };
            let reg_final = state.registry.clone();
            let _ = tokio::task::spawn_blocking(move || reg_final.put_job(&jr2)).await;

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
            "\u{1F7E1} Update job started.\njob_id: {job_id}\nproject_id: {}",
            req.project_id
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

        let exts = exts_for_project_type(&ps.info.project_type);
        let pid = project_id.to_string();
        let dir = PathBuf::from(&ps.info.directory);
        let old_gen = new_gen.saturating_sub(1);

        let (changed, deleted) = self
            .get_incremental_changes(project_id, &dir, &exts)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        self.enforce_project_byte_budget(&changed).await?;

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

        let max_chunks = self.state.cfg.max_chunks_per_file;
        let stats = self
            .index_files_with_parse_guard(
                &ps.search,
                &pid,
                "memory",
                new_gen,
                &dir,
                changed,
                max_chunks,
                cancel,
                |_, _| {},
            )
            .await?;

        self.process_ingest_stats(project_id, new_gen, &stats)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            let generation = new_gen;
            let _ = tokio::task::spawn_blocking(move || {
                graph_service::link_sql_to_schema(&graph, &pid, generation)
            })
            .await;
        }

        {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            let generation = new_gen;
            let _ = tokio::task::spawn_blocking(move || {
                graph_service::resolve_app_code_globals(&graph, &pid, generation)
            })
            .await;
        }

        {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            let generation = new_gen;
            let _ = tokio::task::spawn_blocking(move || {
                graph_service::link_binding_fields_to_columns(&graph, &pid, generation)
            })
            .await;
        }

        let graph = self.state.graph.clone();
        let pid_clone = project_id.to_string();
        tokio::task::spawn_blocking(move || graph.resolve_symbol_edges(&pid_clone))
            .await
            .ok();

        let git_summary = self
            .git_update_stream(
                project_id,
                &ps.info.directory,
                new_gen,
                max_commits,
                index_antipatterns,
                engram_git::history::MergeCommitPolicy::AllParents,
                cancel,
                Box::new(|_, _| {}),
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;

        {
            let reg = self.state.registry.clone();
            let pid_clone = project_id.to_string();
            tokio::task::spawn_blocking(move || {
                reg.set_meta(&pid_clone, "active_generation", &new_gen.to_string())
            })
            .await
            .map_err(|e| anyhow::anyhow!("set_meta join error: {e}"))?
            .map_err(|e| anyhow::anyhow!("set_meta failed: {e}"))?;
        }

        ps.search
            .purge_old_generations(project_id, new_gen)
            .await
            .ok();

        Ok(format!(
            "\u{2705} Updated project_id: {}\nactive_generation: {}\nfiles={} chunks={} bytes={}\n{}\n",
            project_id, new_gen, stats.files, stats.chunks, stats.bytes, git_summary
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
                "No projects indexed yet.".to_string(),
            )]));
        }

        let mut out = String::new();
        for p in list {
            out.push_str(&format!(
                "- {} | {} | {} | {}\n",
                p.project_id, p.project_name, p.project_type, p.directory
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    pub async fn handle_project_info(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id;
        let reg = self.state.registry.clone();
        let pid_clone = pid.clone();
        let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid_clone))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some(rec) = rec else {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "\u{274C} Unknown project_id: {pid}"
            ))]));
        };

        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);
        Ok(CallToolResult::success(vec![Content::text(format!(
            "project_id: {}\nname: {}\ntype: {}\ndirectory: {}\nactive_generation: {}",
            rec.project_id, rec.project_name, rec.project_type, rec.directory, gen_
        ))]))
    }

    pub async fn handle_project_health(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id;
        let ps = self.ensure_project_runtime(&pid).await?;
        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);

        // Graph stats (blocking — redb reads)
        let graph = self.state.graph.clone();
        let pid_clone = pid.clone();
        let (graph_nodes, graph_edges, node_type_counts, _edge_kind_counts) =
            tokio::task::spawn_blocking(move || {
                let nodes = graph.count_nodes(&pid_clone).unwrap_or(0);
                let edges = graph.count_edges(&pid_clone).unwrap_or(0);
                let ntc = graph.count_nodes_by_type(&pid_clone).unwrap_or_default();
                let ekc = graph.count_edges_by_kind(&pid_clone).unwrap_or_default();
                (nodes, edges, ntc, ekc)
            })
            .await
            .unwrap_or_default();

        // Per-namespace doc counts
        let ns_counts = ps.search.count_docs_by_namespace(&pid).unwrap_or_default();
        let total_docs: usize = ns_counts.values().sum();
        let memory_docs = ns_counts.get("memory").copied().unwrap_or(0);
        let history_docs = ns_counts.get("history").copied().unwrap_or(0);
        let antipattern_docs = ns_counts.get("antipattern").copied().unwrap_or(0);

        // Language breakdown for quick reference
        let lang_counts = ps.search.count_docs_by_language(&pid).unwrap_or_default();

        // Vector row count
        let lancedb_rows = ps.search.count_vectors(&pid).await.unwrap_or(0);

        // Disk usage: measure project data directory
        let data_dir = self.state.cfg.data_dir.join("projects").join(&pid);
        let disk_usage = tokio::task::spawn_blocking(move || dir_size_bytes(&data_dir))
            .await
            .unwrap_or(0);

        // Active jobs for this project
        let reg = self.state.registry.clone();
        let pid_for_jobs = pid.clone();
        let active_jobs = tokio::task::spawn_blocking(move || {
            reg.list_jobs(Some(&pid_for_jobs)).unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        let running_jobs: Vec<_> = active_jobs
            .iter()
            .filter(|j| j.status == "running" || j.status == "pending")
            .collect();

        // Active indexing count (global, for context)
        let indexing_count = self
            .state
            .active_indexing_count
            .load(std::sync::atomic::Ordering::Relaxed);

        // Build output
        let mut out = String::with_capacity(2048);
        out.push_str(&format!("Project Health: {}\n", pid));
        out.push_str(&format!("directory: {}\n", ps.info.directory));
        out.push_str(&format!("project_type: {}\n", ps.info.project_type));
        out.push_str(&format!("active_generation: {}\n", gen_));

        out.push_str("\n--- Index Stats ---\n");
        out.push_str(&format!("graph_nodes: {}\n", graph_nodes));
        out.push_str(&format!("graph_edges: {}\n", graph_edges));
        out.push_str(&format!("tantivy_docs_total: {}\n", total_docs));
        out.push_str(&format!("  memory: {}\n", memory_docs));
        out.push_str(&format!("  history: {}\n", history_docs));
        out.push_str(&format!("  antipattern: {}\n", antipattern_docs));
        for (ns, count) in &ns_counts {
            if ns != "memory" && ns != "history" && ns != "antipattern" {
                out.push_str(&format!("  {}: {}\n", ns, count));
            }
        }
        out.push_str(&format!("lancedb_vectors: {}\n", lancedb_rows));
        out.push_str(&format!("disk_usage: {}\n", format_bytes(disk_usage)));

        // Symbol type breakdown (top 5)
        if !node_type_counts.is_empty() {
            let mut nts: Vec<_> = node_type_counts.iter().collect();
            nts.sort_by(|a, b| b.1.cmp(a.1));
            out.push_str("\n--- Symbol Types (top 5) ---\n");
            for (ntype, count) in nts.iter().take(5) {
                out.push_str(&format!("  {}: {}\n", ntype, count));
            }
        }

        // Language breakdown (top 5)
        if !lang_counts.is_empty() {
            let mut ls: Vec<_> = lang_counts.iter().collect();
            ls.sort_by(|a, b| b.1.cmp(a.1));
            out.push_str("\n--- Languages (top 5) ---\n");
            for (lang, count) in ls.iter().take(5) {
                out.push_str(&format!("  {}: {}\n", lang, count));
            }
        }

        // Job status
        if !running_jobs.is_empty() {
            out.push_str(&format!("\n--- Active Jobs ({}) ---\n", running_jobs.len()));
            for j in &running_jobs {
                out.push_str(&format!("  {} [{}] {}\n", j.job_id, j.status, j.kind));
            }
        }
        if indexing_count > 0 {
            out.push_str(&format!(
                "global_active_indexing_tasks: {}\n",
                indexing_count
            ));
        }

        // Integrity warnings with actionable suggestions
        let mut warnings: Vec<String> = Vec::new();
        if graph_nodes == 0 && memory_docs > 0 {
            warnings.push(
                "Graph is empty but Tantivy has docs — symbol extraction may have failed. Suggested: repair_project(scope='graph_only').".into(),
            );
        }
        if memory_docs > 0 && lancedb_rows == 0 {
            warnings.push(
                "Vector index is empty but Tantivy has docs — embeddings may have failed. Suggested: repair_project(scope='vector_only').".into(),
            );
        }
        if lancedb_rows > 0 && total_docs == 0 {
            warnings.push(
                "Tantivy is empty but LanceDB has rows — Tantivy may be corrupted. Suggested: repair_project(scope='tantivy_only').".into(),
            );
        }
        if gen_ > 1 && total_docs == 0 && graph_nodes == 0 {
            warnings.push(
                "Generation > 1 but all indexes empty — project may need full re-indexing. Suggested: repair_project(wipe_and_reindex=true).".into(),
            );
        }
        if memory_docs > 0 && graph_nodes > 0 {
            // Check ratio: if graph nodes are disproportionately low vs docs
            let ratio = graph_nodes as f64 / memory_docs as f64;
            if ratio < 0.01 {
                warnings.push(format!(
                    "Graph/doc ratio is very low ({:.4}) — graph may be stale. Suggested: repair_project(scope='graph_only').",
                    ratio
                ));
            }
        }
        // Check vector/doc ratio for embedding health
        if memory_docs > 10 && lancedb_rows > 0 {
            let vec_ratio = lancedb_rows as f64 / memory_docs as f64;
            if vec_ratio < 0.5 {
                warnings.push(format!(
                    "Vector coverage is low ({:.0}%) — some chunks may not have embeddings. Suggested: repair_project(scope='vector_only').",
                    vec_ratio * 100.0
                ));
            }
        }
        // Edge kind health: if graph has nodes but no edges
        if graph_nodes > 10 && graph_edges == 0 {
            warnings.push(
                "Graph has nodes but no edges — edge extraction may have failed. Suggested: repair_project(scope='graph_only').".into(),
            );
        }
        // Check for stale jobs
        let stale_jobs: Vec<_> = active_jobs
            .iter()
            .filter(|j| j.status == "running")
            .collect();
        if stale_jobs.len() > 2 {
            warnings.push(format!(
                "{} jobs are still running — possible stale jobs. Check list_jobs and cancel_job if needed.",
                stale_jobs.len()
            ));
        }

        if warnings.is_empty() {
            out.push_str("\n--- Status ---\nHealthy\n");
        } else {
            out.push_str(&format!("\n--- Warnings ({}) ---\n", warnings.len()));
            for w in &warnings {
                out.push_str(&format!("  ! {}\n", w));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_repair_project(
        &self,
        req: RepairProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        let max_commits = req.sanitized_max_commits();
        let pid = req.project_id;
        let scope = req.scope.to_lowercase();
        let wipe_and_reindex = req.wipe_and_reindex;
        let index_antipatterns = req.index_antipatterns;
        let ps = self.ensure_project_runtime(&pid).await?;
        let active_gen = self.get_active_generation(&pid).await?;
        let mut steps: Vec<String> = Vec::new();

        if wipe_and_reindex {
            // Full wipe: delete graph data, purge search data, then re-index from scratch
            let graph = self.state.graph.clone();
            let pid_g = pid.clone();
            tokio::task::spawn_blocking(move || graph.delete_project_data(&pid_g))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            steps.push("Wiped graph data.".into());

            // Delete on-disk Tantivy + LanceDB directories and recreate
            let tantivy_dir = ps.info.tantivy_dir.clone();
            let lance_dir = ps.info.lancedb_dir.clone();
            if let Err(e) = tokio::fs::remove_dir_all(&tantivy_dir).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("repair_project: could not remove tantivy dir: {e}");
                }
            }
            if let Err(e) = tokio::fs::remove_dir_all(&lance_dir).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("repair_project: could not remove lance dir: {e}");
                }
            }
            steps.push("Wiped Tantivy and LanceDB data.".into());

            // Evict cached engine so it recreates on next access
            self.state.projects.remove(&pid);
            self.state.project_lru.remove(&pid);

            // Reset generation to 1 and perform full index
            let reg = self.state.registry.clone();
            let pid_r = pid.clone();
            let _ =
                tokio::task::spawn_blocking(move || reg.set_meta(&pid_r, "active_generation", "1"))
                    .await;

            let cancel = tokio_util::sync::CancellationToken::new();
            let summary = self
                .update_project_impl(&pid, 1, max_commits, index_antipatterns, &cancel)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            steps.push(format!("Full re-index completed.\n{}", summary));
        } else {
            match scope.as_str() {
                "graph_only" => {
                    // Purge and rebuild graph only (keep Tantivy/LanceDB intact)
                    let graph = self.state.graph.clone();
                    let pid_g = pid.clone();
                    tokio::task::spawn_blocking(move || graph.delete_project_data(&pid_g))
                        .await
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    steps.push("Purged graph data.".into());

                    // Re-index to rebuild graph
                    let new_gen = active_gen.saturating_add(1);
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let summary = self
                        .update_project_impl(
                            &pid,
                            new_gen,
                            max_commits,
                            index_antipatterns,
                            &cancel,
                        )
                        .await
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    steps.push(format!("Graph rebuilt via re-index.\n{}", summary));
                }
                "tantivy_only" => {
                    // Wipe Tantivy directory and re-index
                    let tantivy_dir = ps.info.tantivy_dir.clone();
                    if let Err(e) = tokio::fs::remove_dir_all(&tantivy_dir).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!("repair_project: could not remove tantivy dir: {e}");
                        }
                    }
                    steps.push("Wiped Tantivy directory.".into());

                    // Evict cache to force recreation
                    self.state.projects.remove(&pid);
                    self.state.project_lru.remove(&pid);

                    let new_gen = active_gen.saturating_add(1);
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let summary = self
                        .update_project_impl(
                            &pid,
                            new_gen,
                            max_commits,
                            index_antipatterns,
                            &cancel,
                        )
                        .await
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    steps.push(format!("Tantivy rebuilt via re-index.\n{}", summary));
                }
                "vector_only" => {
                    // Wipe LanceDB directory and re-index vectors
                    let lance_dir = ps.info.lancedb_dir.clone();
                    if let Err(e) = tokio::fs::remove_dir_all(&lance_dir).await {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            tracing::warn!("repair_project: could not remove lance dir: {e}");
                        }
                    }
                    steps.push("Wiped LanceDB directory.".into());

                    // Evict cache to force recreation
                    self.state.projects.remove(&pid);
                    self.state.project_lru.remove(&pid);

                    let new_gen = active_gen.saturating_add(1);
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let summary = self
                        .update_project_impl(
                            &pid,
                            new_gen,
                            max_commits,
                            index_antipatterns,
                            &cancel,
                        )
                        .await
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    steps.push(format!("Vectors rebuilt via re-index.\n{}", summary));
                }
                _ => {
                    // Full scope (default): GC + incremental re-index
                    let graph = self.state.graph.clone();
                    let pid_gc = pid.clone();
                    tokio::task::spawn_blocking(move || {
                        graph.purge_old_generations(&pid_gc, active_gen)
                    })
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    steps.push("Purged stale graph generations.".into());

                    ps.search
                        .purge_old_generations(&pid, active_gen)
                        .await
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    steps.push("Purged stale search generations.".into());

                    let new_gen = active_gen.saturating_add(1);
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let summary = self
                        .update_project_impl(
                            &pid,
                            new_gen,
                            max_commits,
                            index_antipatterns,
                            &cancel,
                        )
                        .await
                        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                    steps.push(format!("Incremental re-index completed.\n{}", summary));
                }
            }
        }

        let mut out = String::with_capacity(512);
        out.push_str(&format!(
            "\u{2705} Project repaired (scope: {}).\n",
            if wipe_and_reindex {
                "wipe_and_reindex"
            } else {
                scope.as_str()
            },
        ));
        for (i, step) in steps.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, step));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    pub async fn handle_delete_project(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id;
        project_service::validate_project_id(&pid).map_err(McpError::from)?;

        // Cancel active jobs for this project (best-effort).

        if let Ok(Ok(list)) = tokio::task::spawn_blocking({
            let reg = self.state.registry.clone();

            let pid = pid.clone();

            move || reg.list_jobs(Some(&pid))
        })
        .await
        {
            for j in list {
                let _ = self.cancel_job_internal(&j.job_id).await;
            }
        }

        // Remove cache entry (DashMap — no async lock needed)
        self.state.projects.remove(&pid);
        self.state.project_lru.remove(&pid);

        // Remove registry record + all metadata (memory bank, rules, watches, jobs, etc.)
        {
            let reg = self.state.registry.clone();
            let pid2 = pid.clone();
            tokio::task::spawn_blocking(move || reg.delete_all_for_project(&pid2))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        // Purge graph nodes/edges
        {
            let graph = self.state.graph.clone();
            let pid2 = pid.clone();
            tokio::task::spawn_blocking(move || graph.delete_project_data(&pid2))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        // Delete on-disk project dir. Use tokio::fs to avoid blocking the async
        // executor — large LanceDB/Tantivy directories can take hundreds of ms.
        let proj_dir = self.state.cfg.data_dir.join("projects").join(&pid);
        if let Err(e) = tokio::fs::remove_dir_all(&proj_dir).await {
            // Not an error if the directory never existed.
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("delete_project: could not remove {proj_dir:?}: {e}");
            }
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Deleted project_id: {pid}"
        ))]))
    }

    pub async fn handle_watch_project(
        &self,
        req: WatchProjectRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id.clone();
        let rec = self.ensure_project_record(&pid).await?;

        let watch = WatchRecord {
            watch_id: "default".into(),
            directory: rec.directory.clone(),
            enabled: req.enabled,
            updated_at_ms: now_ms(),
        };

        let reg = self.state.registry.clone();
        let pid2 = pid.clone();
        tokio::task::spawn_blocking(move || reg.put_watch(&pid2, &watch))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Notify watcher actor
        if let Err(e) = self.state.events_tx.send(AppEvent::WatchUpdate {
            project_id: pid.clone(),
            directory: rec.directory.clone(),
            enabled: req.enabled,
        }) {
            tracing::warn!("Failed to send WatchUpdate event: {e}");
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} watch_project: {}\nenabled: {}",
            pid, req.enabled
        ))]))
    }

    pub async fn handle_unwatch_project(
        &self,
        req: ProjectIdRequest,
    ) -> Result<CallToolResult, McpError> {
        let pid = req.project_id;
        let watch = WatchRecord {
            watch_id: "default".into(),
            directory: "".into(),
            enabled: false,
            updated_at_ms: now_ms(),
        };
        let reg = self.state.registry.clone();
        let pid_clone = pid.clone();
        tokio::task::spawn_blocking(move || reg.put_watch(&pid_clone, &watch))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Notify watcher actor
        if let Err(e) = self.state.events_tx.send(AppEvent::WatchUpdate {
            project_id: pid.clone(),
            directory: "".into(),
            enabled: false,
        }) {
            tracing::warn!("Failed to send WatchUpdate event: {e}");
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} unwatch_project: {pid}"
        ))]))
    }
}
