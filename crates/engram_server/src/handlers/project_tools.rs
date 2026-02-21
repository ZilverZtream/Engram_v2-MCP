use crate::models::UpdateMemoryBankRequest;
use crate::services::{ingest_service, project_service};
use crate::state::{ProjectInfo, ProjectState};
use crate::tools::Engram;
use crate::utils::files::exts_for_project_type;
use crate::utils::now_ms;
use engram_core::{JobRecord, ProjectRecord};
use rmcp::{ErrorData as McpError, handler::server::tool::Parameters};
use std::path::{Path, PathBuf};
use uuid::Uuid;

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
            let search_init = engram_index::HybridSearchEngine::new(
                tantivy_dir,
                lancedb_dir,
                &state_for_spawn.cfg,
            )
            .await;

            let res = match search_init {
                Ok(search) => {
                    let exts = exts_for_project_type(&project_type);
                    let job_id_for_cb = job_id_for_job.clone();
                    let reg_for_cb = reg2.clone();
                    let files = engram_index::ingest::iter_files(&directory, &exts);

                    if let Err(e) = Engram::new(state_for_spawn.clone())
                        .enforce_project_byte_budget(&files)
                        .await
                    {
                        Err(e)
                    } else if let Some(limit) = state_for_spawn.cfg.max_project_files {
                        if files.len() as u64 > limit {
                            Err(anyhow::anyhow!(
                                "Too many files: {} > limit {}",
                                files.len(),
                                limit
                            ))
                        } else {
                            let last_pct = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));
                            search
                                .index_files(
                                    &project_id_for_job,
                                    "memory",
                                    1,
                                    &directory,
                                    files,
                                    2000,
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
                        search
                            .index_files(
                                &project_id_for_job,
                                "memory",
                                1,
                                &directory,
                                files,
                                2000,
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

            if token.is_cancelled() {
                status = "cancelled";
                msg = "cancelled by user".to_string();
                progress = 0;
            } else if let Ok(stats) = &res {
                let engram = Engram::new(state_for_spawn.clone());
                let pid = project_id_for_job.clone();

                if let Err(e) = engram.process_ingest_stats(&pid, 1, stats).await {
                    status = "failed";
                    msg = format!("Graph processing failed: {}", e);
                    progress = 0;
                } else {
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

                    status = "done";
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
                    let search = engram_index::HybridSearchEngine::new(
                        tantivy_dir.clone(),
                        lancedb_dir.clone(),
                        &state.cfg,
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
                let stats = ps
                    .search
                    .index_files(
                        &project_id_for_job,
                        "memory",
                        new_gen,
                        &dir,
                        changed,
                        2000,
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
                status = "cancelled";
                msg = "cancelled by user".to_string();
                progress = 0;
            } else if let Err(e) = res {
                status = "failed";
                msg = e.to_string();
                progress = 0;
            } else {
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
