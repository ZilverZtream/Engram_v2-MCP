use crate::models::UpdateMemoryBankRequest;
use crate::services::{ingest_service, project_service};
use crate::state::{ProjectInfo, ProjectState};
use crate::tools::Engram;
use crate::utils::files::exts_for_project_type;
use crate::utils::now_ms;
use engram_core::{Checkpoint, JobPhase, JobRecord, ProjectRecord};
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
    rels.iter().map(|r| root.join(r)).collect()
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
}
