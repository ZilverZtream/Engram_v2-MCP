use crate::models::*;
use crate::services::{graph_service, project_service};
use crate::state::{AppEvent, AppState, ProjectInfo, ProjectState, SearchHitLite};
use crate::utils::files::exts_for_project_type;
use crate::utils::now_ms;
use crate::utils::text::{code_to_query, stacktrace_to_query};
use engram_core::{MemorySection, ProjectRecord, RepoRule, WatchRecord};
use engram_git::GitWalker;
use engram_graph::EdgeKind;
use engram_index::{HybridQuery, IndexDoc};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::Parameters, tool::ToolRouter},
    model::*,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone)]
pub struct Engram {
    pub state: AppState,
    pub tool_router: ToolRouter<Engram>,
}

// -------------------- Tool router --------------------

#[tool_router]
impl Engram {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    pub async fn process_ingest_stats_for_test(
        &self,
        project_id: &str,
        generation: u64,
        stats: &engram_index::IngestStats,
    ) -> anyhow::Result<()> {
        self.process_ingest_stats(project_id, generation, stats)
            .await
    }

    // ---- Project lifecycle ----

    #[tool(
        description = "Index a local directory to make it searchable (v1 parity: index_project)."
    )]
    #[tracing::instrument(skip(self, params), fields(directory = %params.0.directory, project_name = %params.0.project_name))]
    pub async fn index_project(
        &self,
        params: Parameters<IndexProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;

        let dir = match self.state.paths.resolve_path(&req.directory) {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{274C} {e}"
                ))]));
            }
        };

        // Dedupe + registry insert in a single Redb write transaction.
        // Because Redb is single-writer, the list→find→put sequence is atomic:
        // no concurrent `index_project` call can sneak in between the check and
        // the write, preventing the TOCTOU duplicate-UUID corruption bug.
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
        // Returns Ok(Some(existing)) if deduped, Ok(None) if newly inserted.
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

        let search = engram_index::HybridSearchEngine::new(
            tantivy_dir.clone(),
            lancedb_dir.clone(),
            &self.state.cfg,
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
            let stats = search
                .index_files(
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

    #[tool(description = "Update a project index + git intelligence (v1 parity: update_project).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn update_project(
        &self,
        params: Parameters<UpdateProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
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
        // Serialise concurrent updates for this project. The watcher actor and a
        // direct Agent MCP call can race; without this lock they both read the same
        // generation N and write N+1, corrupting Tantivy/LanceDB data.
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

        // For Snapshot namespaces (memory): use copy-forward.
        // For GlobalMutable/AppendOnly: use delete-then-reindex.
        let memory_policy = engram_core::get_policy("memory")
            .map(|p| p.versioning)
            .unwrap_or(engram_core::NamespaceVersioning::Snapshot);

        if memory_policy == engram_core::NamespaceVersioning::Snapshot {
            // Gather ALL current disk files to identify unchanged set
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

            // Copy unchanged docs from old_gen â†’ new_gen
            if old_gen > 0 && !unchanged.is_empty() {
                ps.search
                    .copy_generation_for_paths(
                        project_id, "memory", old_gen, new_gen, &unchanged, cancel,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
            }
        } else {
            // GlobalMutable / AppendOnly: delete stale files and reindex
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

        // Acquire parse semaphore to bound concurrent parse/chunk blocking threads.
        let _parse_permit =
            self.state.parse_semaphore.acquire().await.map_err(|e| {
                McpError::internal_error(format!("Parse semaphore closed: {e}"), None)
            })?;
        let max_chunks = self.state.cfg.max_chunks_per_file;
        let stats = ps
            .search
            .index_files(
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
        drop(_parse_permit);

        self.process_ingest_stats(project_id, new_gen, &stats)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Link SQL nodes to schema tables (QueriesTable edges).
        {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            let generation = new_gen;
            let link_result: Result<anyhow::Result<usize>, _> =
                tokio::task::spawn_blocking(move || {
                    graph_service::link_sql_to_schema(&graph, &pid, generation)
                })
                .await;
            match link_result {
                Ok(Ok(_count)) => {}
                Ok(Err(e)) => {
                    tracing::warn!("link_sql_to_schema for {project_id}: {e}");
                }
                Err(e) => {
                    tracing::warn!("link_sql_to_schema task panicked for {project_id}: {e}");
                }
            }
        }

        // Resolve App_Code global FQN references (legacy WebForms).
        {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            let generation = new_gen;
            let result: Result<anyhow::Result<usize>, _> = tokio::task::spawn_blocking(move || {
                graph_service::resolve_app_code_globals(&graph, &pid, generation)
            })
            .await;
            match result {
                Ok(Ok(_count)) => {}
                Ok(Err(e)) => {
                    tracing::warn!("resolve_app_code_globals for {project_id}: {e}");
                }
                Err(e) => {
                    tracing::warn!("resolve_app_code_globals task panicked for {project_id}: {e}");
                }
            }
        }

        // Link binding_field nodes to db_column nodes.
        {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            let generation = new_gen;
            let result: Result<anyhow::Result<usize>, _> = tokio::task::spawn_blocking(move || {
                graph_service::link_binding_fields_to_columns(&graph, &pid, generation)
            })
            .await;
            match result {
                Ok(Ok(_count)) => {}
                Ok(Err(e)) => {
                    tracing::warn!("link_binding_fields_to_columns for {project_id}: {e}");
                }
                Err(e) => {
                    tracing::warn!(
                        "link_binding_fields_to_columns task panicked for {project_id}: {e}"
                    );
                }
            }
        }

        // Link unresolved edges
        let graph = self.state.graph.clone();
        let pid_clone = project_id.to_string();
        tokio::task::spawn_blocking(move || graph.resolve_symbol_edges(&pid_clone))
            .await
            .ok();

        // Stream git temporal coupling and optional anti-pattern indexing.
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

        // Commit generation. Propagate errors — if this write fails the generation
        // counter is desynchronised and future incremental updates will be wrong.
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

        // Automatic GC for Snapshot namespaces
        ps.search
            .purge_old_generations(project_id, new_gen)
            .await
            .ok();

        Ok(format!(
            "\u{2705} Updated project_id: {}\nactive_generation: {}\nfiles={} chunks={} bytes={}\n{}\n",
            project_id, new_gen, stats.files, stats.chunks, stats.bytes, git_summary
        ))
    }

    #[tool(description = "List indexed projects (v1 parity: list_projects).")]
    #[tracing::instrument(skip(self))]
    pub async fn list_projects(&self) -> Result<CallToolResult, McpError> {
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

    #[tool(description = "Get info about a project (v1 parity: project_info).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn project_info(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
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

    #[tool(description = "Quick project health check (v1 parity: project_health).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn project_health(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
        let ps = self.ensure_project_runtime(&pid).await?;
        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);

        let graph = self.state.graph.clone();
        let pid_clone = pid.clone();
        let graph_stats = tokio::task::spawn_blocking(move || {
            let nodes = graph.count_nodes(&pid_clone).unwrap_or(0);
            let edges = graph.count_edges(&pid_clone).unwrap_or(0);
            (nodes, edges)
        })
        .await
        .unwrap_or((0, 0));

        let tantivy_docs = ps.search.count_docs(&pid).unwrap_or(0);
        let lancedb_rows = ps.search.count_vectors(&pid).await.unwrap_or(0);

        Ok(CallToolResult::success(vec![Content::text(format!(
            "project_id: {}\nactive_generation: {}\ndirectory: {}\ngraph_nodes: {}\ngraph_edges: {}\ntantivy_docs: {}\nlancedb_rows: {}",
            pid, gen_, ps.info.directory, graph_stats.0, graph_stats.1, tantivy_docs, lancedb_rows
        ))]))
    }

    #[tool(description = "Repair a project index (v1 parity: repair_project).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn repair_project(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
        let ps = self.ensure_project_runtime(&pid).await?;
        let active_gen = self.get_active_generation(&pid).await?;

        // 1. Trigger manual GC for this project
        self.state
            .graph
            .purge_old_generations(&pid, active_gen)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        ps.search
            .purge_old_generations(&pid, active_gen)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // 2. Perform a fresh update (same generation to overwrite/fix, or new? Let's use a new one to be safe)
        let new_gen = active_gen.saturating_add(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let summary = self
            .update_project_impl(&pid, new_gen, 500, true, &cancel)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Project repaired.\n{}",
            summary
        ))]))
    }

    #[tool(description = "Delete a project and its stored data (v1 parity: delete_project).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn delete_project(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
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

    #[tool(description = "Enable/disable watching a project directory (v1 parity: watch_project).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, enabled = %params.0.enabled))]
    pub async fn watch_project(
        &self,
        params: Parameters<WatchProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
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

    #[tool(description = "Disable watching a project directory (v1 parity: unwatch_project).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn unwatch_project(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
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

    // ---- Search + chunks ----

    #[tool(description = "Search the indexed code/docs (v1 parity: search_memory).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, query = %params.0.query))]
    pub async fn search_memory(
        &self,
        params: Parameters<SearchMemoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // 1. Fetch PageRank centrality for boosting (project-wide)
        let graph = self.state.graph.clone();
        let pid_for_centrality = req.project_id.clone();
        let active_gen = gen_;
        let centrality = tokio::task::spawn_blocking(move || {
            engram_graph::analysis::compute_pagerank(&graph, &pid_for_centrality, active_gen)
        })
        .await
        .ok()
        .and_then(|r| r.ok());

        // 2. Perform Hybrid Search with Boost
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: req.namespace.clone(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: req.sanitized_max_results(),
                    fts_mode: req.fts_mode.clone(),
                    include_path_prefixes: req.include_path_prefixes.clone(),
                    exclude_path_prefixes: req.exclude_path_prefixes.clone(),
                    language_filters: req.language_filters.clone(),
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: req.use_mmr,
                },
                centrality.as_ref().map(|c| &c.pagerank),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Feed the dreamer co-occurrence graph (non-blocking).
        let lite: Vec<SearchHitLite> = hits
            .iter()
            .map(|h| SearchHitLite {
                pk: h.pk.clone(),
                doc_id: h.doc_id.clone(),
                path: h.path.clone(),
                chunk_id: Some(h.chunk_id),
            })
            .collect();
        let _ = self.state.events_tx.send(AppEvent::SearchSession {
            project_id: req.project_id.clone(),
            hits: lite,
        });

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text("No hits.")]));
        }

        let mut out = String::new();
        out.push_str(&format!("active_generation: {gen_}\n"));
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "\n#{}\ndoc_id: {}\nchunk_id: {}\npath: {}\nscore: {:.3}\n",
                i + 1,
                h.doc_id,
                h.chunk_id,
                h.path,
                h.score
            ));

            if req.include_content {
                if let Ok(Some((_, _, content, _, _))) =
                    ps.search
                        .get_doc_by_doc_id(&req.project_id, &req.namespace, gen_, &h.doc_id)
                {
                    out.push_str("content:\n");
                    let limit = req.sanitized_max_content_chars_per_result();
                    if content.chars().count() > limit {
                        out.push_str(&content.chars().take(limit).collect::<String>());
                        out.push_str("... (truncated)");
                    } else {
                        out.push_str(&content);
                    }
                    out.push('\n');
                }
            } else if let Some(sn) = &h.snippet {
                out.push_str("snippet:\n");
                out.push_str(sn);
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Fetch full content for a chunk (v1 parity: get_chunk).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, doc_id = %params.0.doc_id))]
    pub async fn get_chunk(
        &self,
        params: Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let doc = ps
            .search
            .get_doc_by_doc_id(&req.project_id, &req.namespace, gen_, &req.doc_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some((path, lang, content, start_line, end_line)) = doc else {
            return Ok(CallToolResult::success(vec![Content::text("Not found.")]));
        };

        // Inject repo rules if requested.
        let display_content = if req.inject_rules {
            self.inject_repo_rules(&req.project_id, &path, &content)
                .await
        } else {
            content.to_string()
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "path: {}\ndoc_id: {}\nnamespace: {}\nlanguage: {}\nlines: {}-{}\nactive_generation: {}\n\n{}",
            path, req.doc_id, req.namespace, lang, start_line, end_line, gen_, display_content
        ))]))
    }

    // ---- Memory bank + repo rules ----

    #[tool(description = "Create/update a memory bank section (v1 parity: update_memory_bank).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, section = %params.0.section))]
    pub async fn update_memory_bank(
        &self,
        params: Parameters<UpdateMemoryBankRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_record(&req.project_id).await?;

        let section_id = req.section_id.unwrap_or_else(|| req.section.clone());
        let sec = MemorySection {
            section_id: section_id.clone(),
            title: req.section,
            content: req.content.clone(),
            updated_at_ms: now_ms(),
        };

        // Persist
        {
            let reg = self.state.registry.clone();
            let pid = req.project_id.clone();
            let sec_clone = sec.clone();
            tokio::task::spawn_blocking(move || reg.put_memory_section(&pid, &sec_clone))
                .await
                .ok();
        }

        // Index (namespace = memory_bank)
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

        // Escape colons in section_id to prevent ambiguity in chunk identity parsing.
        let safe_section_id = section_id.replace(':', "_");
        let mb_path = format!("{}:{}", namespace, safe_section_id);
        let mut chunks = engram_index::chunking::chunk_lines(&sec.content, 2000);
        for c in &mut chunks {
            c.set_doc_id(&mb_path);
        }
        ps.search
            .delete_files(
                &req.project_id,
                namespace,
                &[engram_core::RelPath::new(&mb_path)],
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mb_path_str = mb_path.clone();
        let docs: Vec<IndexDoc> = chunks
            .into_iter()
            .map(|c| {
                let chunk_id = engram_index::chunk_id_from_content_hash(&c.content_hash);
                IndexDoc {
                    generation: effective_gen,
                    chunk_id,
                    path: mb_path_str.clone().into(),
                    language: "text".into(),
                    content: c.content,
                    namespace: namespace.into(),
                    author: None,
                    timestamp: None,
                    start_line: c.start_line,
                    end_line: c.end_line,
                    doc_id: c.doc_id.0,
                    content_hash: c.content_hash.0,
                }
            })
            .collect();
        let cancel = tokio_util::sync::CancellationToken::new();
        ps.search
            .index_docs(&req.project_id, &docs, &cancel)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} memory_bank updated\nsection_id: {section_id}\nchunks_indexed: {}",
            docs.len()
        ))]))
    }

    #[tool(description = "List memory bank sections (v1 parity: list_memory_bank).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn list_memory_bank(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
        let reg = self.state.registry.clone();
        let secs = tokio::task::spawn_blocking(move || reg.list_memory_sections(&pid))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if secs.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No memory bank sections.",
            )]));
        }
        let mut out = String::new();
        for s in secs {
            out.push_str(&format!("- {} | {}\n", s.section_id, s.title));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Read a memory bank section (v1 parity: read_memory_bank).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, section = %params.0.section))]
    pub async fn read_memory_bank(
        &self,
        params: Parameters<MemorySectionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let reg = self.state.registry.clone();
        let sec = tokio::task::spawn_blocking(move || {
            reg.get_memory_section(&req.project_id, &req.section)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some(sec) = sec else {
            return Ok(CallToolResult::success(vec![Content::text("Not found.")]));
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "section_id: {}\ntitle: {}\nupdated_at_ms: {}\n\n{}",
            sec.section_id, sec.title, sec.updated_at_ms, sec.content
        ))]))
    }

    #[tool(description = "Delete a memory bank section (v1 parity: delete_memory_bank).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, section = %params.0.section))]
    pub async fn delete_memory_bank(
        &self,
        params: Parameters<MemorySectionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let reg = self.state.registry.clone();
        let pid_clone = req.project_id.clone();
        let sid_clone = req.section.clone();
        tokio::task::spawn_blocking(move || reg.delete_memory_section(&pid_clone, &sid_clone))
            .await
            .ok();

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} deleted section_id: {}",
            req.section
        ))]))
    }

    #[tool(
        description = "Add a repo rule/constraint injected into chunk reads (v1 parity: add_repo_rule)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, file_pattern = %params.0.file_pattern))]
    pub async fn add_repo_rule(
        &self,
        params: Parameters<AddRepoRuleRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_record(&req.project_id).await?;

        let rule_id = req.rule_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let rule = RepoRule {
            rule_id: rule_id.clone(),
            file_pattern: req.file_pattern,
            rule_text: req.rule_text,
            priority: req.priority,
            updated_at_ms: now_ms(),
        };

        let reg = self.state.registry.clone();
        let pid = req.project_id.clone();
        tokio::task::spawn_blocking(move || reg.put_repo_rule(&pid, &rule))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} repo rule added\nrule_id: {rule_id}"
        ))]))
    }

    #[tool(description = "List repo rules (v1 parity: list_repo_rules).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn list_repo_rules(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
        let reg = self.state.registry.clone();
        let rules = tokio::task::spawn_blocking(move || reg.list_repo_rules(&pid))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if rules.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No repo rules.",
            )]));
        }
        let mut out = String::new();
        for r in rules {
            out.push_str(&format!(
                "- {} | {} (p={}) | {}\n",
                r.rule_id, r.file_pattern, r.priority, r.rule_text
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Delete a repo rule (v1 parity: delete_repo_rule).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, rule_id = %params.0.rule_id))]
    pub async fn delete_repo_rule(
        &self,
        params: Parameters<DeleteRepoRuleRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let reg = self.state.registry.clone();
        let pid_clone = req.project_id.clone();
        let rid_clone = req.rule_id.clone();
        tokio::task::spawn_blocking(move || reg.delete_repo_rule(&pid_clone, &rid_clone))
            .await
            .ok();

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} deleted rule_id: {}",
            req.rule_id
        ))]))
    }

    // ---- Graph tools ----

    #[tool(description = "Query graph nodes by substring (v1 parity: query_graph_nodes).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, name_pattern = %params.0.name_pattern))]
    pub async fn query_graph_nodes(
        &self,
        params: Parameters<QueryGraphNodesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let nodes = graph
                .query_nodes(
                    &req.project_id,
                    Some(&req.node_type),
                    Some(&req.name_pattern),
                    Some(&req.file_path),
                    req.limit,
                )
                .map_err(|e| e.to_string())?;

            if nodes.is_empty() {
                return Ok(String::new());
            }

            let mut out = String::new();
            for n in nodes {
                out.push_str(&format!(
                    "- {} | {} | {} (lines {}-{} | gen {})\n",
                    n.node_id, n.node_type, n.file_path, n.start_line, n.end_line, n.generation
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        if out.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matching nodes.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    #[tool(description = "Find graph references from a node (v1 parity: find_references).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, node_id = %params.0.node_id))]
    pub async fn find_references(
        &self,
        params: Parameters<FindReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let kind = match req.edge_kind.as_deref() {
            Some("co_occurrence") => Some(EdgeKind::CoOccurrence),
            Some("temporal_coupling") => Some(EdgeKind::TemporalCoupling),
            Some("insight") => Some(EdgeKind::Insight),
            Some("dependency") => Some(EdgeKind::Dependency),
            Some("anti_pattern") => Some(EdgeKind::AntiPattern),
            Some("contains") => Some(EdgeKind::Contains),
            Some("imports") => Some(EdgeKind::Imports),
            _ => None,
        };

        let graph = self.state.graph.clone();
        let edge_kind_str = req.edge_kind.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let mut out = String::new();

            if req.direction == "in" || req.direction == "both" {
                let incoming = graph
                    .find_incoming_edges(&req.project_id, kind.clone(), &req.node_id, 100)
                    .map_err(|e| e.to_string())?;
                if !incoming.is_empty() {
                    let header = match edge_kind_str.as_deref() {
                        Some("contains") => "Containers (Incoming 'contains'):\n",
                        Some("imports") => "Imported by (Incoming 'imports'):\n",
                        Some(k) => &format!("Incoming references (kind='{}'):\n", k),
                        None => "Incoming references (all kinds):\n",
                    };
                    out.push_str(header);
                    for (n, w) in incoming {
                        out.push_str(&format!("- {} (weight={})\n", n, w));
                    }
                }
            }

            if req.direction == "out" || req.direction == "both" {
                let search_kind = kind.unwrap_or(EdgeKind::Dependency);
                let outgoing = graph
                    .neighbors(&req.project_id, search_kind, &req.node_id, 100)
                    .map_err(|e| e.to_string())?;
                if !outgoing.is_empty() {
                    let header = match edge_kind_str.as_deref() {
                        Some("contains") => "Members (Outgoing 'contains'):\n",
                        Some("imports") => "Imports (Outgoing 'imports'):\n",
                        Some(k) => &format!("Outgoing references (kind='{}'):\n", k),
                        None => "Outgoing references (dependencies):\n",
                    };
                    out.push_str(header);
                    for (n, w) in outgoing {
                        out.push_str(&format!("- {} (weight={})\n", n, w));
                    }
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        if out.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No references found.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    #[tool(description = "Graph search by node name/id substring (v1 parity: graph_search).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, query = %params.0.query))]
    pub async fn graph_search(
        &self,
        params: Parameters<GraphSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // 1. Hybrid search for initial candidates
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: req.sanitized_max_results(),
                    fts_mode: "strict".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // 2. Expand via graph neighbors
        let mut expanded_nodes = std::collections::HashMap::new();
        for h in &hits {
            let node_id = format!("file:{}", h.path);
            let score = h.score;
            expanded_nodes.insert(node_id.clone(), score);

            // Fetch neighbors — neighbor score is always lower than the originating hit's score
            // to prevent graph neighbors from outranking actual search results.
            if let Ok(neighbors) =
                self.state
                    .graph
                    .neighbors(&req.project_id, EdgeKind::Dependency, &node_id, 5)
            {
                for (neigh_id, weight) in neighbors {
                    // Neighbor score decays from parent: base * boost_factor, capped at parent score
                    let decay = 0.5 + (weight.min(10) as f32 * req.symbol_boost * 0.05);
                    let neigh_score = score * decay.min(0.95); // Never exceed 95% of parent
                    let entry = expanded_nodes.entry(neigh_id).or_insert(0.0);
                    if neigh_score > *entry {
                        *entry = neigh_score;
                    }
                }
            }
        }

        if expanded_nodes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No graph matches found.",
            )]));
        }

        // 3. Sort and format
        let mut sorted: Vec<_> = expanded_nodes.into_iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut out = String::new();
        out.push_str(&format!("Graph search results for '{}':\n", req.query));
        for (id, score) in sorted.iter().take(req.sanitized_max_results()) {
            out.push_str(&format!("- {} (score={:.3})\n", id, score));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    #[tool(description = "Multi-hop graph traversal from a start node (BFS).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, node_id = %params.0.node_id, max_hops = %params.0.max_hops))]
    pub async fn traverse_graph(
        &self,
        params: Parameters<TraverseGraphRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;

        let kinds = req.edge_kinds.as_ref().map(|v| {
            v.iter()
                .filter_map(|s| EdgeKind::parse(s.as_str()))
                .collect::<Vec<_>>()
        });

        let results = self
            .state
            .graph
            .traverse(
                &req.project_id,
                &req.node_id,
                req.sanitized_max_hops(),
                kinds,
                &req.direction,
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No connected nodes found within constraints.",
            )]));
        }

        let mut out = String::new();
        out.push_str(&format!(
            "Traversal results from {} (max_hops={}):\n",
            req.node_id, req.max_hops
        ));
        for (n, dist) in results {
            out.push_str(&format!(
                "- [{}] {} | {} | {}\n",
                dist, n.node_id, n.node_type, n.file_path
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    // ---- Git/history tools ----

    #[tool(
        description = "Index git history for temporal coupling + anti-patterns (v1 parity: index_git_history)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn index_git_history(
        &self,
        params: Parameters<IndexGitHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;

        if req.wait {
            let active_gen = self.get_active_generation(&req.project_id).await?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let summary = self
                .git_update_stream(
                    &req.project_id,
                    &ps.info.directory,
                    active_gen,
                    req.sanitized_max_commits(),
                    req.index_antipatterns,
                    engram_git::history::MergeCommitPolicy::AllParents,
                    &cancel,
                    Box::new(|_, _| {}),
                )
                .await?;
            return Ok(CallToolResult::success(vec![Content::text(summary)]));
        }

        let pid_clone = req.project_id.clone();
        let job_id = self
            .spawn_job_git_history(
                pid_clone,
                req.sanitized_max_commits(),
                req.index_antipatterns,
            )
            .await?;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{1F7E1} Git history job started.\njob_id: {job_id}"
        ))]))
    }

    #[tool(
        description = "Ingest a folder of zip snapshots as pseudo-history for temporal coupling."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, directory = %params.0.directory))]
    pub async fn ingest_zip_history(
        &self,
        params: Parameters<IngestZipHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let active_gen = self.get_active_generation(&req.project_id).await?;

        let dir = self
            .state
            .paths
            .resolve_path(&req.directory)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;

        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();

        if req.wait {
            let summary = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
                let mut zip_files: Vec<_> = std::fs::read_dir(&dir)?
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .map(|s| s.to_lowercase())
                            == Some("zip".to_string())
                    })
                    .collect();

                // Sort numerically — extract the leading digit sequence from the
                // filename for chronological ordering.
                // Handles "2.zip", "10.zip", "snapshot-1.zip", "backup_20.zip", etc.
                // For filenames with no digits (e.g. "commit-abc.zip") the numeric
                // key is u64::MAX and the secondary alphabetical key is used, which
                // produces correct results only when filenames are alphabetically
                // chronological. Warn the caller when all files are non-numeric so
                // they can rename them if the ordering is wrong.
                fn extract_first_number(s: &str) -> u64 {
                    let digits: String = s
                        .chars()
                        .skip_while(|c| !c.is_ascii_digit())
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    digits.parse().unwrap_or(u64::MAX)
                }
                zip_files.sort_by_cached_key(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let num = extract_first_number(&name);
                    (num, name)
                });

                if zip_files.len() < 2 {
                    return Ok("Need at least 2 zip files to compute pseudo-history.".to_string());
                }

                // Warn if no numeric ordering is available — alphabetical fallback
                // may not reflect chronological order.
                let all_non_numeric = zip_files.iter().all(|e| {
                    extract_first_number(&e.file_name().to_string_lossy()) == u64::MAX
                });
                if all_non_numeric {
                    tracing::warn!(
                        "ingest_zip_history: no numeric prefixes found in zip filenames — \
                         falling back to alphabetical ordering which may not be chronological. \
                         Rename files as 01_name.zip, 02_name.zip … for correct ordering."
                    );
                }

                let mut temporal_edges = 0;
                let mut prev_fingerprints: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();

                let mut skipped_zips = 0usize;
                for (i, entry) in zip_files.iter().enumerate() {
                    let path = entry.path();
                    let file = match std::fs::File::open(&path) {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "Skipping unreadable zip file");
                            skipped_zips += 1;
                            continue;
                        }
                    };
                    let mut archive = match zip::ZipArchive::new(file) {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "Skipping corrupt/invalid zip file");
                            skipped_zips += 1;
                            continue;
                        }
                    };

                    let mut current_fingerprints = std::collections::HashMap::new();
                    let mut changed_files = Vec::new();

                    for j in 0..archive.len() {
                        let mut f = archive.by_index(j)?;
                        if f.is_file() {
                            let name = f.name().to_string();
                            // Compute a quick hash of the file in zip
                            let mut hasher = blake3::Hasher::new();
                            std::io::copy(&mut f, &mut hasher)?;
                            let hash = hasher.finalize().to_hex().to_string();

                            current_fingerprints.insert(name.clone(), hash.clone());

                            if i > 0 {
                                if let Some(prev_hash) = prev_fingerprints.get(&name) {
                                    if *prev_hash != hash {
                                        changed_files.push(engram_core::RelPath::new(&name));
                                    }
                                } else {
                                    // New file
                                    changed_files.push(engram_core::RelPath::new(&name));
                                }
                            }
                        }
                    }

                    if !changed_files.is_empty() {
                        let pairs = engram_git::temporal::file_pairs(&changed_files, 100);
                        let batch: Vec<(engram_graph::EdgeKind, String, String, u32)> = pairs
                            .iter()
                            .map(|(a, b)| {
                                (
                                    engram_graph::EdgeKind::TemporalCoupling,
                                    format!("file:{}", a),
                                    format!("file:{}", b),
                                    1u32,
                                )
                            })
                            .collect();
                        temporal_edges += batch.len();
                        graph.batch_increment_undirected_edges(
                            &project_id,
                            engram_core::namespaces::NAMESPACE_HISTORY,
                            "text",
                            active_gen,
                            &batch,
                        )?;
                    }

                    prev_fingerprints = current_fingerprints;
                }

                let mut summary = format!(
                    "\u{2705} Ingested {} snapshots, added {} temporal edges.",
                    zip_files.len().saturating_sub(skipped_zips),
                    temporal_edges
                );
                if skipped_zips > 0 {
                    summary.push_str(&format!("\n\u{26a0}\u{fe0f} {} zip files were skipped (corrupt or unreadable).", skipped_zips));
                }
                Ok(summary)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            return Ok(CallToolResult::success(vec![Content::text(summary)]));
        }

        Err(McpError::internal_error(
            "Background job for ingest_zip_history not implemented yet. Use wait=true.",
            None,
        ))
    }

    #[tool(description = "Search history (commits and diffs) (v1 parity: search_history).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, query = %params.0.query))]
    pub async fn search_history(
        &self,
        params: Parameters<SearchHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // Map filters
        let include_path_prefixes = req.file_filter.clone().map(|f| vec![f]);

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "history".into(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: req.sanitized_limit(),
                    fts_mode: "strict".into(),
                    include_path_prefixes,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: req.author_filter,
                    date_after: req.date_after,
                    date_before: req.date_before,
                    use_mmr: false,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No history hits found.",
            )]));
        }

        let mut out = String::new();
        out.push_str(&format!("History search results (gen {gen_}):\n"));
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "\n#{}\nscore: {:.3}\npath: {}\n",
                i + 1,
                h.score,
                h.path
            ));

            if let Ok(Some((_, _, content, _, _))) =
                ps.search
                    .get_doc_by_doc_id(&req.project_id, "history", gen_, &h.doc_id)
            {
                out.push_str("content:\n");
                let limit = 800;
                if content.chars().count() > limit {
                    out.push_str(&content.chars().take(limit).collect::<String>());
                    out.push_str("... (truncated)");
                } else {
                    out.push_str(&content);
                }
                out.push('\n');
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Analyze temporal couplings for a file (v1 parity: analyze_temporal_couplings)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, file_path = ?params.0.file_path))]
    pub async fn analyze_temporal_couplings(
        &self,
        params: Parameters<AnalyzeTemporalCouplingsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;

        let couplings = if let Some(ref file_path) = req.file_path {
            // Focused search
            let node_id = if file_path.starts_with("file:") {
                file_path.clone()
            } else {
                format!("file:{file_path}")
            };
            engram_graph::algorithms::coupling::file_temporal_couplings(
                &self.state.graph,
                &req.project_id,
                &node_id,
                req.sanitized_min_frequency() as u32,
                req.sanitized_limit(),
            )
        } else {
            // Global search
            engram_graph::algorithms::coupling::top_project_couplings(
                &self.state.graph,
                &req.project_id,
                req.sanitized_limit(),
            )
        }
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if couplings.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No temporal neighbors found for the given criteria.",
            )]));
        }

        let mut out = String::new();
        if let Some(ref fp) = req.file_path {
            out.push_str(&format!("Temporal couplings for {fp}:\n"));
        } else {
            out.push_str("Top temporal couplings:\n");
        }

        for c in couplings {
            out.push_str(&format!(
                "- {} <-> {} (weight={})\n",
                c.file_node_id, c.neighbor_node_id, c.weight
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Analyze reverts (The Immune System): detects reverted commits and creates anti-pattern rules (v1 parity: analyze_reverts)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn analyze_reverts(
        &self,
        params: Parameters<AnalyzeRevertsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let active_gen = self.get_active_generation(&req.project_id).await?;

        // Phase 1 (CPU-bound): walk commits and build anti-pattern docs in a blocking thread.
        // We intentionally do NOT call the async index_docs here to avoid the
        // `block_on`-inside-`spawn_blocking` anti-pattern which can deadlock under load.
        let cancel = tokio_util::sync::CancellationToken::new();
        let (anti_docs, reverts_found, rules_added) = tokio::task::spawn_blocking({
            let directory = ps.info.directory.clone();
            let project_id = req.project_id.clone();
            let registry = self.state.registry.clone();
            let max_commits = req.max_commits;
            let cancel_clone = cancel.clone();

            move || -> anyhow::Result<(Vec<engram_index::IndexDoc>, usize, usize)> {
                let repo = GitWalker::open_repo(Path::new(&directory))?;

                let mut reverts_found = 0;
                let mut rules_added = 0;
                let mut anti_docs = Vec::new();

                GitWalker::walk_commits_streaming(
                    &repo,
                    None,
                    max_commits,
                    engram_git::history::MergeCommitPolicy::AllParents,
                    &cancel_clone,
                    |oid, _curr, _total| {
                    let docs = GitWalker::extract_antipatterns_from_reverts(&repo, oid, 50_000)?;
                    if !docs.is_empty() {
                        reverts_found += 1;
                        for doc in docs {
                            let rule_id = format!("immune_{}", doc.original_commit);
                            let rule = RepoRule {
                                rule_id: rule_id.clone(),
                                file_pattern: format!("**/{}", doc.file_path),
                                rule_text: format!("AVOID: This pattern was previously reverted in commit {}. It caused issues that required a rollback.", doc.original_commit),
                                priority: 10,
                                updated_at_ms: now_ms(),
                            };
                            registry.put_repo_rule(&project_id, &rule)?;
                            rules_added += 1;

                            // Convert to IndexDoc for Tantivy
                            let immune_content_hash = engram_core::ContentHash::compute(doc.diff_text.as_bytes());
                            let immune_doc_id_str = engram_core::DocIdStr::compute(
                                doc.file_path.as_str(), 0, 0,
                                &immune_content_hash,
                            ).0;

                            anti_docs.push(engram_index::IndexDoc {
                                generation: active_gen,
                                chunk_id: engram_index::chunk_id_from_content_hash(&immune_content_hash),
                                doc_id: immune_doc_id_str,
                                content_hash: immune_content_hash.0,
                                path: doc.file_path.clone(),
                                language: "code".into(),
                                content: doc.diff_text,
                                namespace: "antipattern".into(),
                                author: None,
                                timestamp: None,
                                start_line: 0,
                                end_line: 0,
                            });
                        }
                    }
                    Ok(())
                })?;

                Ok((anti_docs, reverts_found, rules_added))
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Phase 2 (async): index the anti-pattern docs in the outer async context,
        // eliminating the block_on-inside-spawn_blocking deadlock risk.
        if !anti_docs.is_empty() {
            ps.search
                .index_docs(&req.project_id, &anti_docs, &cancel)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        let summary = format!(
            "\u{2705} Immune System active.\nReverts analyzed: {reverts_found}\nAnti-patterns indexed: {}\nRepo rules generated: {rules_added}",
            anti_docs.len()
        );

        Ok(CallToolResult::success(vec![Content::text(summary)]))
    }

    // ---- Agent / cognitive tools ----

    #[tool(description = "Analyze what might break if a file or symbol is changed.")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn impact_analysis(
        &self,
        params: Parameters<ImpactAnalysisRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        // Validate early — no graph access needed
        if req.symbol_fqn.is_none() && req.file_path.is_none() {
            return Err(McpError::invalid_params(
                "Either file_path or symbol_fqn must be provided.",
                None,
            ));
        }

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            // 1. Resolve target node
            let target_id = if let Some(ref fqn) = req.symbol_fqn {
                if fqn.starts_with("sql:") || fqn.starts_with("table:") || fqn.starts_with("state:")
                {
                    fqn.clone()
                } else {
                    let table_id = engram_core::ids::NodeId::table(fqn).0;
                    if graph
                        .get_node(&req.project_id, &table_id)
                        .map_err(|e| e.to_string())?
                        .is_some()
                    {
                        table_id
                    } else if let Ok(candidates) =
                        graph.query_nodes(&req.project_id, None, None, None, 500)
                        && let Some(n) = candidates.iter().find(|n| {
                            n.metadata
                                .as_ref()
                                .and_then(|m| m.get("fqn"))
                                .and_then(|v| v.as_str())
                                == Some(fqn)
                        })
                    {
                        n.node_id.clone()
                    } else {
                        // Fallback: search by short name with higher limit to avoid FQN truncation.
                        // 500 covers large codebases where many symbols share a common short name
                        // (e.g., "Page_Load" in WebForms apps with hundreds of pages).
                        let short = fqn.split('.').next_back().unwrap_or(fqn);
                        if let Ok(candidates) =
                            graph.query_nodes(&req.project_id, None, Some(short), None, 500)
                            && !candidates.is_empty()
                        {
                            if let Some(exact) = candidates.iter().find(|n| {
                                n.metadata
                                    .as_ref()
                                    .and_then(|m| m.get("fqn"))
                                    .and_then(|v| v.as_str())
                                    == Some(fqn)
                            }) {
                                exact.node_id.clone()
                            } else {
                                candidates[0].node_id.clone()
                            }
                        } else {
                            return Ok(format!("Symbol '{fqn}' not found in graph."));
                        }
                    }
                }
            } else if let Some(ref path) = req.file_path {
                engram_core::ids::NodeId::file(path).0
            } else {
                unreachable!()
            };

            // 2. Find incoming edges (who depends on this?)
            // Cap the limit to prevent unbounded memory allocation and LLM context overflow.
            let capped_limit = req.limit.clamp(1, 1000);
            let incoming = graph
                .find_incoming_edges_with_kind(&req.project_id, None, &target_id, capped_limit)
                .map_err(|e| e.to_string())?;

            if incoming.is_empty() {
                return Ok(format!("No dependent nodes found for {target_id}."));
            }

            // 3. Format results
            let mut out = format!("Impact Analysis for {target_id}:\n\n");
            out.push_str("Nodes that depend on or are related to this:\n");

            let mut grouped: std::collections::HashMap<String, (Vec<engram_graph::EdgeKind>, u32)> =
                std::collections::HashMap::new();
            for (src_id, kind, weight) in incoming {
                let entry = grouped.entry(src_id).or_insert((Vec::new(), 0));
                entry.0.push(kind);
                if weight > entry.1 {
                    entry.1 = weight;
                }
            }

            let mut sorted: Vec<_> = grouped.into_iter().collect();
            sorted.sort_by(|a, b| b.1.1.cmp(&a.1.1));

            for (src_id, (kinds, weight)) in sorted {
                let Some(src_node) = graph
                    .get_node(&req.project_id, &src_id)
                    .map_err(|e| e.to_string())?
                else {
                    continue;
                };

                let mut reasons = Vec::new();
                for ek in kinds {
                    let r = match ek {
                        engram_graph::EdgeKind::Dependency => "Calls/Uses this",
                        engram_graph::EdgeKind::Contains => "Contains this",
                        engram_graph::EdgeKind::Imports => "Imports this",
                        engram_graph::EdgeKind::SqlCalls => "Executes this SQL",
                        engram_graph::EdgeKind::CoOccurrence => {
                            "Often searched with this (Co-occurrence)"
                        }
                        engram_graph::EdgeKind::TemporalCoupling => {
                            "Often changed with this (Temporal coupling)"
                        }
                        engram_graph::EdgeKind::QueriesTable => "Queries this table",
                        engram_graph::EdgeKind::ReadsState => "Reads this state",
                        engram_graph::EdgeKind::WritesState => "Writes this state",
                        engram_graph::EdgeKind::HasColumn => "Has column",
                        engram_graph::EdgeKind::ForeignKey => "Foreign key reference",
                        _ => "Related",
                    };
                    reasons.push(r);
                }
                reasons.sort();
                reasons.dedup();

                let reason_str = if reasons.is_empty() {
                    "Dependent".to_string()
                } else {
                    reasons.join(", ")
                };

                out.push_str(&format!(
                    "- {} [{}] (weight: {weight}) - {reason_str}\n",
                    src_node.node_id, src_node.node_type
                ));
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Get schema info for a database table: DDL, columns, foreign keys, and referencing code."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, table = %params.0.table_name))]
    pub async fn get_table_schema(
        &self,
        params: Parameters<GetTableSchemaRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let table_id = engram_core::ids::NodeId::table(&req.table_name).0;

            // 1. Find the table node.
            let table_node = graph
                .get_node(&req.project_id, &table_id)
                .map_err(|e| e.to_string())?;

            let Some(table_node) = table_node else {
                let candidates = graph
                    .query_nodes(
                        &req.project_id,
                        Some("db_table"),
                        Some(&req.table_name),
                        None,
                        10,
                    )
                    .map_err(|e| e.to_string())?;
                if candidates.is_empty() {
                    return Ok(format!(
                        "Table '{}' not found. Make sure the project has .sql DDL files indexed.",
                        req.table_name
                    ));
                }
                let names: Vec<_> = candidates.iter().map(|n| n.name.as_str()).collect();
                return Ok(format!(
                    "Table '{}' not found exactly. Did you mean one of: {}?",
                    req.table_name,
                    names.join(", ")
                ));
            };

            let mut out = format!("## Table: {}\n\n", table_node.name);

            // 2. Show DDL from metadata.
            if let Some(ref meta) = table_node.metadata {
                if let Some(ddl) = meta.get("ddl").and_then(|v| v.as_str()) {
                    out.push_str("### DDL\n```sql\n");
                    out.push_str(ddl);
                    out.push_str("\n```\n\n");
                }
            }

            // 3. Find columns via outgoing HasColumn edges.
            let columns = graph
                .neighbors(&req.project_id, EdgeKind::HasColumn, &table_id, 200)
                .map_err(|e| e.to_string())?;

            if !columns.is_empty() {
                out.push_str("### Columns\n");
                for (col_id, _weight) in &columns {
                    if let Ok(Some(col_node)) = graph.get_node(&req.project_id, col_id) {
                        let data_type = col_node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("data_type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        let nullable = col_node
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("nullable"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("?");
                        out.push_str(&format!(
                            "- **{}** {} (nullable: {})\n",
                            col_node.name, data_type, nullable
                        ));
                    }
                }
                out.push('\n');
            }

            // 4. Find foreign keys from column nodes.
            let mut fk_lines = Vec::new();
            for (col_id, _) in &columns {
                let fks = graph
                    .neighbors(&req.project_id, EdgeKind::ForeignKey, col_id, 50)
                    .map_err(|e| e.to_string())?;
                for (ref_col_id, _) in fks {
                    fk_lines.push(format!("- {} -> {}", col_id, ref_col_id));
                }
            }
            if !fk_lines.is_empty() {
                out.push_str("### Foreign Keys\n");
                for line in &fk_lines {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }

            // 5. Find incoming QueriesTable edges (SQL nodes that reference this table).
            let referencing = graph
                .find_incoming_edges(&req.project_id, Some(EdgeKind::QueriesTable), &table_id, 50)
                .map_err(|e| e.to_string())?;

            if !referencing.is_empty() {
                out.push_str("### Referenced by SQL Nodes\n");
                for (sql_id, weight) in &referencing {
                    let callers = graph
                        .find_incoming_edges(&req.project_id, Some(EdgeKind::SqlCalls), sql_id, 20)
                        .map_err(|e| e.to_string())?;
                    let caller_strs: Vec<_> = callers.iter().map(|(id, _)| id.as_str()).collect();
                    if caller_strs.is_empty() {
                        out.push_str(&format!("- {} (weight: {})\n", sql_id, weight));
                    } else {
                        out.push_str(&format!(
                            "- {} (weight: {}) <- called from: {}\n",
                            sql_id,
                            weight,
                            caller_strs.join(", ")
                        ));
                    }
                }
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Trace all readers/writers of a global state key (Session, ViewState, Application, Cache)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, state_type = %params.0.state_type, state_key = %params.0.state_key))]
    pub async fn trace_state_usage(
        &self,
        params: Parameters<TraceStateUsageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            let state_id = engram_core::ids::NodeId::state(&req.state_type, &req.state_key).0;

            // Propagate real DB errors as Err so the outer handler surfaces them
            // as McpError rather than silently returning a "not found" string that
            // makes the LLM think the code simply doesn't exist.
            let state_node = graph
                .get_node(&req.project_id, &state_id)
                .map_err(|e| format!("DB error looking up state node: {e}"))?;

            if state_node.is_none() {
                let candidates = graph
                    .query_nodes(
                        &req.project_id,
                        Some("global_state"),
                        Some(&req.state_key),
                        None,
                        20,
                    )
                    .map_err(|e| format!("DB error querying state candidates: {e}"))?;
                if candidates.is_empty() {
                    return Ok(format!(
                        "State key '{}[\"{}\"]' not found in the graph.\nMake sure the project has C#/VB files with {} access indexed.",
                        req.state_type, req.state_key, req.state_type
                    ));
                }
                let names: Vec<_> = candidates.iter().map(|n| n.name.as_str()).collect();
                return Ok(format!(
                    "State key '{}:{}' not found exactly. Similar keys found: {}",
                    req.state_type,
                    req.state_key,
                    names.join(", ")
                ));
            }

            let mut out = format!(
                "## State Usage: {}[\"{}\"]\n\n",
                req.state_type, req.state_key
            );

            let writers = graph
                .find_incoming_edges(
                    &req.project_id,
                    Some(EdgeKind::WritesState),
                    &state_id,
                    req.limit,
                )
                .map_err(|e| format!("DB error querying writers: {e}"))?;

            if !writers.is_empty() {
                out.push_str("### Writers\n");
                for (writer_id, weight) in &writers {
                    if let Ok(Some(node)) = graph.get_node(&req.project_id, writer_id) {
                        out.push_str(&format!(
                            "- {} [{}] in {} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            weight
                        ));
                    } else {
                        out.push_str(&format!("- {} (weight: {})\n", writer_id, weight));
                    }
                }
                out.push('\n');
            } else {
                out.push_str("### Writers\nNo writers found.\n\n");
            }

            let readers = graph
                .find_incoming_edges(
                    &req.project_id,
                    Some(EdgeKind::ReadsState),
                    &state_id,
                    req.limit,
                )
                .map_err(|e| format!("DB error querying readers: {e}"))?;

            if !readers.is_empty() {
                out.push_str("### Readers\n");
                for (reader_id, weight) in &readers {
                    if let Ok(Some(node)) = graph.get_node(&req.project_id, reader_id) {
                        out.push_str(&format!(
                            "- {} [{}] in {} (weight: {})\n",
                            node.name,
                            node.node_type,
                            node.file_path.as_str(),
                            weight
                        ));
                    } else {
                        out.push_str(&format!("- {} (weight: {})\n", reader_id, weight));
                    }
                }
            } else {
                out.push_str("### Readers\nNo readers found.\n");
            }

            Ok(out)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Trace paths from UI (aspx page + control ID) to SQL nodes.")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn trace_ui_event(
        &self,
        params: Parameters<TraceUiEventRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        // 1. Resolve start node ID
        let mut start_id = if let Some(ref ctrl) = req.control_id {
            engram_core::ids::NodeId::control(&req.page_path, ctrl).0
        } else if let Some(ref handler) = req.handler_fqn {
            engram_core::ids::NodeId::symbol("function", Some(handler), &req.page_path, "", 0).0
        } else {
            engram_core::ids::NodeId::page(&req.page_path).0
        };

        // If the start_id doesn't exist, try to find a candidate page if only path was given
        if self
            .state
            .graph
            .get_node(&req.project_id, &start_id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .is_none()
            && let Some(ref ctrl) = req.control_id
            && let Ok(candidates) =
                self.state
                    .graph
                    .query_nodes(&req.project_id, Some("control"), Some(ctrl), None, 1)
            && !candidates.is_empty()
        {
            start_id = candidates[0].node_id.clone();
        }

        // 2. Find paths to SQL
        let paths = self
            .state
            .graph
            .find_ui_paths(
                &req.project_id,
                &start_id,
                req.max_hops as usize,
                req.max_paths,
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if paths.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No paths found from {start_id} to any SQL nodes within {} hops.",
                req.max_hops
            ))]));
        }

        // 3. Format output
        let mut out = format!("Found {} path(s) to SQL:\n", paths.len());
        for (i, path) in paths.iter().enumerate() {
            out.push_str(&format!("\nPath #{}:\n", i + 1));
            for (step, node) in path.iter().enumerate() {
                let label = match node.node_type.as_str() {
                    "page" => "ASPX Page",
                    "control" => "UI Control",
                    "function" => "Code-Behind Handler",
                    "stored_proc" => "Stored Procedure",
                    "inline_sql" => "Inline SQL",
                    _ => &node.node_type,
                };

                let justification = if step == 0 {
                    "Starting point"
                } else {
                    let prev = &path[step - 1];
                    match (prev.node_type.as_str(), node.node_type.as_str()) {
                        ("page", "class") => "Inherits class",
                        ("control", "function") => "Event wiring (OnClick/Handles)",
                        ("function", "function") => "Method call",
                        (_, "inline_sql") | (_, "stored_proc") => "Executes SQL",
                        _ => "Dependency",
                    }
                };

                let indent = "  ".repeat(step);
                out.push_str(&format!(
                    "{indent}Step {}: {} [{}] ({}) - {}\n",
                    step + 1,
                    node.name,
                    label,
                    node.node_id,
                    justification
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Trace a UI action to its code-behind handler and call chain (legacy .NET support)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, query = %params.0.query))]
    pub async fn trace_ui_action(
        &self,
        params: Parameters<TraceUiActionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // 1. Find candidate starting nodes (controls, pages, handlers)
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: 10,
                    fts_mode: "loose".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut start_nodes = Vec::new();
        for h in &hits {
            // Find nodes associated with this file/chunk
            let nodes = self
                .state
                .graph
                .query_nodes(&req.project_id, None, None, Some(h.path.as_str()), 10)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for n in nodes {
                if matches!(n.node_type.as_str(), "control" | "page" | "function") {
                    start_nodes.push(n.node_id);
                }
            }
        }
        start_nodes.sort();
        start_nodes.dedup();

        if start_nodes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No UI controls or handlers found for the query.",
            )]));
        }

        // 2. Expand graph paths
        let mut out = format!("UI Trace results for '{}':\n", req.query);
        let mut paths_found = 0;

        let edge_kinds = vec![
            engram_graph::EdgeKind::Contains, // Page -> Class, Control -> Event
            engram_graph::EdgeKind::Dependency, // Control -> Handler, Func -> Func
        ];

        for start_id in start_nodes {
            if paths_found >= req.max_paths {
                break;
            }

            let paths = self
                .state
                .graph
                .traverse(
                    &req.project_id,
                    &start_id,
                    req.max_depth as usize,
                    Some(edge_kinds.clone()),
                    "out",
                )
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            if paths.len() > 1 {
                // More than just the start node
                paths_found += 1;
                out.push_str(&format!("\nPath starting at {}:\n", start_id));
                for (n, depth) in paths {
                    let indent = "  ".repeat(depth);
                    out.push_str(&indent);
                    out.push_str(&format!(
                        "- {} | {} | {} (lines {}-{})\n",
                        n.node_id, n.node_type, n.file_path, n.start_line, n.end_line
                    ));
                }
            }
        }

        if paths_found == 0 {
            return Ok(CallToolResult::success(vec![Content::text(
                "No call chains found from identified UI elements.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    #[tool(description = "Export a comprehensive 'capture pack' (zip) for agentic usage.")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn export_capture_pack(
        &self,
        params: Parameters<ExportCapturePackRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let pid = req.project_id.clone();
        let _ps = self.ensure_project_runtime(&pid).await?;
        let _active_gen = self.get_active_generation(&pid).await.unwrap_or(1);

        // Fetch overview before entering spawn_blocking (it's async)
        let overview = self
            .get_codebase_overview(Parameters(ProjectIdRequest {
                project_id: pid.clone(),
            }))
            .await?;
        let overview_text = match &overview.content[0].raw {
            RawContent::Text(t) => t.text.clone(),
            _ => String::new(),
        };

        // Stream the zip directly to disk instead of building a Vec<u8> in memory.
        // This prevents OOM for large projects with thousands of graph nodes.
        let timestamp = now_ms();
        let data_dir = self.state.cfg.data_dir.clone();
        let exports_dir = data_dir.join("exports").join(&pid);
        tokio::fs::create_dir_all(&exports_dir)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let zip_path = exports_dir.join(format!("{}.zip", timestamp));

        let graph = self.state.graph.clone();
        let pid_clone = pid.clone();
        let zip_path_clone = zip_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let file = std::fs::File::create(&zip_path_clone).map_err(|e| e.to_string())?;
            let writer = std::io::BufWriter::new(file);
            let mut zip = zip::ZipWriter::new(writer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            // 1. overview.md
            zip.start_file("overview.md", options)
                .map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut zip, overview_text.as_bytes())
                .map_err(|e| e.to_string())?;

            // 2. graph_topology.json — stream directly into zip via serde_json::to_writer
            let all_nodes = graph
                .query_nodes(&pid_clone, None, None, None, 1000)
                .unwrap_or_default();
            let total_node_count = graph.count_nodes(&pid_clone).unwrap_or(all_nodes.len());
            let topo = serde_json::json!({
                "node_count": total_node_count,
                "nodes": all_nodes.iter().map(|n| {
                    serde_json::json!({
                        "id": n.node_id,
                        "type": n.node_type,
                        "name": n.name,
                        "path": n.file_path,
                        "language": n.language
                    })
                }).collect::<Vec<_>>()
            });
            zip.start_file("graph_topology.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &topo).map_err(|e| e.to_string())?;

            // 3. ui_wiring.json
            let ui_nodes = graph
                .query_nodes(&pid_clone, Some("control"), None, None, 5000)
                .unwrap_or_default();
            let mut wiring = Vec::new();
            for ctrl in ui_nodes {
                let deps = graph
                    .neighbors(
                        &pid_clone,
                        engram_graph::EdgeKind::Dependency,
                        &ctrl.node_id,
                        10,
                    )
                    .unwrap_or_default();
                wiring.push(serde_json::json!({
                    "control": ctrl.node_id,
                    "handlers": deps.iter().map(|(id, _)| id).collect::<Vec<_>>()
                }));
            }
            zip.start_file("ui_wiring.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &wiring).map_err(|e| e.to_string())?;

            // 4. sql_map.json
            let sql_edges = graph
                .list_edges_by_kind(&pid_clone, engram_graph::EdgeKind::SqlCalls, 5000)
                .unwrap_or_default();
            zip.start_file("sql_map.json", options)
                .map_err(|e| e.to_string())?;
            serde_json::to_writer_pretty(&mut zip, &sql_edges).map_err(|e| e.to_string())?;

            zip.finish().map_err(|e| e.to_string())?;
            Ok(())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Capture pack exported to: {}",
            zip_path.to_string_lossy()
        ))]))
    }

    #[tool(
        description = "Get a JSON tree of the UI layout for a WebForms (.aspx/.ascx) or WinForms (.Designer.vb/.Designer.cs) file. Returns container hierarchy, child controls with labels, grid positions, logical groupings, and tab order."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, file_path = %params.0.file_path))]
    pub async fn get_ui_blueprint(
        &self,
        params: Parameters<GetUiBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_runtime(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            // 1. Find all ui_container nodes for this file.
            let all_containers = graph
                .query_nodes(
                    &req.project_id,
                    Some("ui_container"),
                    None,
                    Some(&req.file_path),
                    500,
                )
                .map_err(|e| e.to_string())?;

            if all_containers.is_empty() {
                return Ok(format!(
                    "No UI layout data found for '{}'. Ensure the file has been indexed and contains container elements (Panel, Table, GroupBox, div).",
                    req.file_path
                ));
            }

            // 2. For each container, find its children via ContainsUi edges.
            let mut tree = serde_json::Map::new();
            tree.insert("file".into(), serde_json::Value::String(req.file_path.clone()));

            let mut containers_json = Vec::new();
            for container in &all_containers {
                let mut cobj = serde_json::Map::new();
                cobj.insert("id".into(), serde_json::Value::String(container.name.clone()));
                cobj.insert("node_id".into(), serde_json::Value::String(container.node_id.clone()));

                if let Some(ref meta) = container.metadata {
                    if let Some(ct) = meta.get("container_type").and_then(|v| v.as_str()) {
                        cobj.insert("container_type".into(), serde_json::Value::String(ct.to_string()));
                    }
                    if let Some(ls) = meta.get("layout_style").and_then(|v| v.as_str()) {
                        cobj.insert("layout_style".into(), serde_json::Value::String(ls.to_string()));
                    }
                    if let Some(lg) = meta.get("logical_grouping").and_then(|v| v.as_str()) {
                        cobj.insert("logical_grouping".into(), serde_json::Value::String(lg.to_string()));
                    }
                    if let Some(css) = meta.get("css_class").and_then(|v| v.as_str()) {
                        cobj.insert("css_class".into(), serde_json::Value::String(css.to_string()));
                    }
                }

                // Find children via ContainsUi edges
                let children = graph
                    .neighbors(
                        &req.project_id,
                        EdgeKind::ContainsUi,
                        &container.node_id,
                        200,
                    )
                    .unwrap_or_default();

                let mut children_json = Vec::new();
                for (child_id, _weight) in &children {
                    let mut child_obj = serde_json::Map::new();
                    child_obj.insert("node_id".into(), serde_json::Value::String(child_id.clone()));

                    if let Ok(Some(child_node)) = graph.get_node(&req.project_id, child_id) {
                        child_obj.insert("name".into(), serde_json::Value::String(child_node.name.clone()));
                        child_obj.insert("type".into(), serde_json::Value::String(child_node.node_type.clone()));

                        if let Some(ref meta) = child_node.metadata {
                            if let Some(label) = meta.get("ui_label").and_then(|v| v.as_str()) {
                                child_obj.insert("ui_label".into(), serde_json::Value::String(label.to_string()));
                            }
                            if let Some(row) = meta.get("row").and_then(|v| v.as_str()) {
                                child_obj.insert("row".into(), serde_json::Value::String(row.to_string()));
                            }
                            if let Some(col) = meta.get("col").and_then(|v| v.as_str()) {
                                child_obj.insert("col".into(), serde_json::Value::String(col.to_string()));
                            }
                            if let Some(lg) = meta.get("logical_grouping").and_then(|v| v.as_str()) {
                                child_obj.insert("logical_grouping".into(), serde_json::Value::String(lg.to_string()));
                            }
                            if let Some(x) = meta.get("x").and_then(|v| v.as_str()) {
                                child_obj.insert("x".into(), serde_json::Value::String(x.to_string()));
                            }
                            if let Some(y) = meta.get("y").and_then(|v| v.as_str()) {
                                child_obj.insert("y".into(), serde_json::Value::String(y.to_string()));
                            }
                        }

                        // Find tab-order neighbors for this child
                        let neighbors = graph
                            .neighbors(
                                &req.project_id,
                                EdgeKind::UiLayoutNeighbor,
                                child_id,
                                5,
                            )
                            .unwrap_or_default();
                        if !neighbors.is_empty() {
                            let next_ids: Vec<serde_json::Value> = neighbors
                                .iter()
                                .map(|(nid, _)| serde_json::Value::String(nid.clone()))
                                .collect();
                            child_obj.insert("next_in_tab_order".into(), serde_json::Value::Array(next_ids));
                        }
                    }

                    children_json.push(serde_json::Value::Object(child_obj));
                }

                cobj.insert("children".into(), serde_json::Value::Array(children_json));
                containers_json.push(serde_json::Value::Object(cobj));
            }

            tree.insert("containers".into(), serde_json::Value::Array(containers_json));
            tree.insert("container_count".into(), serde_json::Value::Number(all_containers.len().into()));

            serde_json::to_string_pretty(&tree).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Get a lightweight codebase overview (v1 parity: get_codebase_overview).")]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn get_codebase_overview(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
        let rec = self.ensure_project_record(&pid).await?;
        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);
        let ps = self.ensure_project_runtime(&pid).await?;

        let rules = self.state.registry.clone();
        let pid_clone = pid.clone();
        let rule_count = tokio::task::spawn_blocking(move || rules.list_repo_rules(&pid_clone))
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|v| v.len())
            .unwrap_or(0);

        // Stats from search index
        let tantivy_docs = ps.search.count_docs(&pid).unwrap_or(0);
        let lancedb_rows = ps.search.count_vectors(&pid).await.unwrap_or(0);

        // Compute PageRank for the overview
        let graph = self.state.graph.clone();
        let pid_clone2 = pid.clone();
        let active_gen = self.get_active_generation(&pid).await.unwrap_or(1);
        let centrality = tokio::task::spawn_blocking(move || {
            engram_graph::analysis::compute_pagerank(&graph, &pid_clone2, active_gen)
        })
        .await
        .ok()
        .and_then(|r| r.ok());

        let mut out = format!("Codebase Overview for {}\n", rec.project_name);
        out.push_str(&format!("project_id: {}\n", rec.project_id));
        out.push_str(&format!("directory: {}\n", rec.directory));
        out.push_str(&format!("active_generation: {}\n", gen_));
        out.push_str(&format!("repo_rules: {}\n", rule_count));
        out.push_str(&format!("chunks_indexed: {}\n", tantivy_docs));
        out.push_str(&format!("vectors_stored: {}\n", lancedb_rows));

        if let Some(metrics) = centrality {
            let mut top_nodes: Vec<_> = metrics.pagerank.into_iter().collect();
            top_nodes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            out.push_str("\nTop central nodes (PageRank):\n");
            for (id, score) in top_nodes.iter().take(8) {
                out.push_str(&format!("- {} ({:.4})\n", id, score));
            }
        }

        // Database tables overview
        let table_nodes = self
            .state
            .graph
            .query_nodes(&pid, Some("db_table"), None, None, 100)
            .unwrap_or_default();
        if !table_nodes.is_empty() {
            out.push_str(&format!("\nDatabase Tables ({}): ", table_nodes.len()));
            let names: Vec<_> = table_nodes
                .iter()
                .take(15)
                .map(|n| n.name.as_str())
                .collect();
            out.push_str(&names.join(", "));
            if table_nodes.len() > 15 {
                out.push_str(&format!(" ... and {} more", table_nodes.len() - 15));
            }
            out.push('\n');
        }

        // Top global state keys (Session, ViewState, etc.)
        let state_nodes = self
            .state
            .graph
            .query_nodes(&pid, Some("global_state"), None, None, 100)
            .unwrap_or_default();
        if !state_nodes.is_empty() {
            // Count incoming edges (reads + writes) per state node for ranking.
            let mut state_usage: Vec<(&str, usize, usize)> = Vec::new();
            for sn in &state_nodes {
                let reads = self
                    .state
                    .graph
                    .find_incoming_edges(&pid, Some(EdgeKind::ReadsState), &sn.node_id, 200)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let writes = self
                    .state
                    .graph
                    .find_incoming_edges(&pid, Some(EdgeKind::WritesState), &sn.node_id, 200)
                    .map(|v| v.len())
                    .unwrap_or(0);
                state_usage.push((&sn.name, reads, writes));
            }
            state_usage.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

            out.push_str(&format!(
                "\nGlobal State Keys ({} total):\n",
                state_nodes.len()
            ));
            for (name, reads, writes) in state_usage.iter().take(10) {
                out.push_str(&format!(
                    "- {} (reads={}, writes={})\n",
                    name, reads, writes
                ));
            }
            if state_nodes.len() > 10 {
                out.push_str(&format!("  ... and {} more\n", state_nodes.len() - 10));
            }
        }

        // Also fetch some top temporal couplings if any
        let couplings =
            engram_graph::algorithms::coupling::top_project_couplings(&self.state.graph, &pid, 5)
                .unwrap_or_default();
        if !couplings.is_empty() {
            out.push_str("\nTop Temporal Couplings:\n");
            for c in couplings {
                out.push_str(&format!(
                    "- {} <-> {} (w={})\n",
                    c.file_node_id, c.neighbor_node_id, c.weight
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Find symbol references using graph + lexical fallback (v1 parity: find_symbol_references)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, symbol_name = %params.0.symbol_name))]
    pub async fn find_symbol_references(
        &self,
        params: Parameters<FindSymbolReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // 1. Try to find the symbol in the graph
        let nodes = self
            .state
            .graph
            .query_nodes(&req.project_id, None, Some(&req.symbol_name), None, 10)
            .unwrap_or_default();

        let mut out = String::new();
        let mut found_in_graph = false;

        for node in nodes {
            // Only consider nodes that match the name exactly (query_nodes is substring)
            if node.name != req.symbol_name {
                continue;
            }

            let incoming = self
                .state
                .graph
                .find_incoming_edges(
                    &req.project_id,
                    Some(engram_graph::EdgeKind::Dependency),
                    &node.node_id,
                    100,
                )
                .unwrap_or_default();

            if !incoming.is_empty() {
                found_in_graph = true;
                out.push_str(&format!(
                    "Graph references for {} ({}, {}):\n",
                    node.name, node.node_type, node.file_path
                ));
                for (src_id, weight) in incoming {
                    out.push_str(&format!("- {} (weight={})\n", src_id, weight));
                }
                out.push('\n');
            }
        }

        if found_in_graph {
            return Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]));
        }

        // 2. Fallback: Lexical search
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: req.symbol_name.clone(),
                    top_k: 20,
                    fts_mode: "strict".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No references found.",
            )]));
        }

        let mut out = String::new();
        out.push_str(&format!("Lexical references to: {}\n", req.symbol_name));
        for h in hits {
            out.push_str(&format!(
                "- {} (chunk_id={}, score={:.3})\n",
                h.path, h.chunk_id, h.score
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Analyze an error stacktrace and suggest likely files (v1 parity: analyze_error_stack)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn analyze_error_stack(
        &self,
        params: Parameters<AnalyzeErrorStackRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let q = stacktrace_to_query(&req.traceback);

        // 1. Hybrid search for initial candidates, using MMR for diversity.
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: q,
                    top_k: 15,
                    fts_mode: "loose".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: true,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No likely matches found in the codebase.",
            )]));
        }

        // 2. Generate hypothesis based on top hits and graph centrality
        let mut out = String::new();
        out.push_str("Error Stacktrace Analysis:\n\n");

        out.push_str("Hypothesis: The error likely originates from or affects the following components, sorted by relevance and architectural significance.\n\n");

        for (i, h) in hits.iter().enumerate().take(5) {
            let centrality_note = if h.centrality > 0.5 {
                " (Architectural Hub)"
            } else if h.centrality > 0.2 {
                " (Common Utility)"
            } else {
                ""
            };

            out.push_str(&format!(
                "#{}: {}{} (score: {:.3})\n",
                i + 1,
                h.path,
                centrality_note,
                h.score
            ));

            if let Ok(Some((_, _, content, _, _))) =
                ps.search
                    .get_doc_by_doc_id(&req.project_id, "memory", gen_, &h.doc_id)
            {
                let snippet: String = content.lines().take(3).collect::<Vec<_>>().join("\n");
                out.push_str("   > ");
                out.push_str(&snippet.replace('\n', "\n   > "));
                out.push_str("\n\n");
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    #[tool(
        description = "Dream: generate insights from co-occurrence clusters (v1 parity: dream_project)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn dream_project(
        &self,
        params: Parameters<DreamProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let pid = req.project_id.clone();
        let _ = self.ensure_project_record(&pid).await?;

        if req.wait {
            let insights = crate::actors::dreamer::dream_once(
                &self.state,
                &pid,
                2, // min_edge_weight
                3, // min_cluster_size
                req.sanitized_max_pairs(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            return Ok(CallToolResult::success(vec![Content::text(format!(
                "ðŸ§  Dream completed for project_id: {pid}\ninsights_generated: {insights}"
            ))]));
        }

        if let Err(e) = self.state.events_tx.send(AppEvent::TriggerDream {
            project_id: pid.clone(),
        }) {
            tracing::warn!("Failed to send TriggerDream event: {e}");
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "ðŸ§  Dream requested for project_id: {pid} (max_pairs={})",
            req.max_pairs
        ))]))
    }

    #[tool(description = "Alias for dreaming: trigger a REM cycle (v1 parity: trigger_rem_cycle).")]
    pub async fn trigger_rem_cycle(
        &self,
        params: Parameters<DreamProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.dream_project(params).await
    }

    #[tool(
        description = "Analyze coding style of a file or directory based on recent history (v1 parity: analyze_file_coding_style)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, file_path = %params.0.file_path))]
    pub async fn analyze_file_coding_style(
        &self,
        params: Parameters<AnalyzeFileCodingStyleRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;

        let abs_path = if std::path::Path::new(&req.file_path).is_absolute() {
            std::path::PathBuf::from(&req.file_path)
        } else {
            std::path::Path::new(&ps.info.directory).join(&req.file_path)
        };

        let resolved = self
            .state
            .paths
            .resolve_path(&abs_path)
            .map_err(|e| McpError::invalid_request(e.to_string(), None))?;
        let is_dir = resolved.is_dir();

        // Get latest OID for caching
        let latest_oid = tokio::task::spawn_blocking({
            let repo_path = PathBuf::from(&ps.info.directory);
            move || -> anyhow::Result<String> {
                let repo = GitWalker::open_repo(&repo_path)?;
                let head = repo.head()?.peel_to_commit()?;
                Ok(head.id().to_string())
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let cache_subject = if let Some(rel) =
            engram_core::RelPath::from_relative(std::path::Path::new(&ps.info.directory), &resolved)
        {
            rel.as_str().to_string()
        } else {
            req.file_path.clone()
        };
        let cache_key = format!("style_guide:{}:{}", cache_subject, latest_oid);
        if let Ok(Some(cached_json)) = self.state.registry.get_meta(&req.project_id, &cache_key)
            && let Ok(guide) = serde_json::from_str::<engram_ml::StyleGuide>(&cached_json)
        {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Style Guide for {} (cached)\n\n{}",
                req.file_path,
                guide.as_text()
            ))]));
        }

        // Pull recent diffs involving this file/directory.
        let diffs = tokio::task::spawn_blocking({
            let repo_path = PathBuf::from(&ps.info.directory);
            let file_path = req.file_path.clone();
            let limit = req.sanitized_diff_limit();
            let cancel = tokio_util::sync::CancellationToken::new();
            move || -> anyhow::Result<Vec<String>> {
                let repo = GitWalker::open_repo(&repo_path)?;
                let oids = GitWalker::walk_new_commits(
                    &repo,
                    None,
                    200,
                    engram_git::history::MergeCommitPolicy::AllParents,
                    &cancel,
                )?; // check last 200 commits
                let mut blobs = Vec::new();
                let target_rel = engram_core::RelPath::new(&file_path);
                let target_str = target_rel.as_str();

                for oid in oids {
                    let parts = GitWalker::diff_text_for_commit(&repo, oid, 200_000)?;
                    for (p, diff) in parts {
                        let p_str = p.as_str();
                        let matches = if is_dir {
                            p_str.starts_with(target_str)
                        } else {
                            p == target_rel
                        };

                        if matches {
                            blobs.push(diff);
                        }
                    }
                    if blobs.len() >= limit {
                        break;
                    }
                }
                Ok(blobs)
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Wrap blocking file I/O in spawn_blocking to avoid async executor starvation
        let current_content = {
            let resolved_clone = resolved.clone();
            tokio::task::spawn_blocking(move || -> Option<String> {
                const MAX_FILE_SIZE: u64 = 1_048_576; // 1MB
                if !is_dir {
                    // Skip files larger than 1MB (e.g. minified JS, generated code)
                    if let Ok(m) = std::fs::metadata(&resolved_clone) {
                        if m.len() > MAX_FILE_SIZE {
                            return None;
                        }
                    }
                    std::fs::read_to_string(&resolved_clone).ok()
                } else {
                    // For directory, sample sorted files for deterministic results
                    let mut sampled_text = String::new();
                    if let Ok(entries) = std::fs::read_dir(&resolved_clone) {
                        let mut file_paths: Vec<PathBuf> = entries
                            .flatten()
                            .map(|e| e.path())
                            .filter(|p| p.is_file())
                            .collect();
                        // Sort for deterministic sampling across OSs/runs
                        file_paths.sort();
                        let mut count = 0;
                        const MAX_AGGREGATED_BYTES: usize = 1_500_000;
                        for path in file_paths {
                            // Skip files larger than 1MB
                            if let Ok(m) = std::fs::metadata(&path) {
                                if m.len() > MAX_FILE_SIZE {
                                    continue;
                                }
                            }
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                if sampled_text.len() + content.len() > MAX_AGGREGATED_BYTES {
                                    break;
                                }
                                sampled_text.push_str(&content);
                                sampled_text.push('\n');
                                count += 1;
                            } else {
                                tracing::warn!(
                                    "Failed to read file for style analysis: {:?}",
                                    path
                                );
                            }
                            if count >= 5 {
                                break;
                            }
                        }
                    }
                    if sampled_text.is_empty() {
                        None
                    } else {
                        Some(sampled_text)
                    }
                }
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        let guide = self
            .state
            .mimicry
            .analyze(&diffs, current_content.as_deref());

        // Cache the result
        if let Ok(json) = serde_json::to_string(&guide) {
            let _ = self
                .state
                .registry
                .set_meta(&req.project_id, &cache_key, &json);
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Style Guide for {}\n\n{}",
            req.file_path,
            guide.as_text()
        ))]))
    }

    #[tool(description = "List background jobs (v1 parity: list_jobs).")]
    #[tracing::instrument(skip(self, params))]
    pub async fn list_jobs(
        &self,
        params: Parameters<ListJobsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let reg = self.state.registry.clone();
        let pid_clone = req.project_id.clone();
        let jobs = tokio::task::spawn_blocking(move || reg.list_jobs(pid_clone.as_deref()))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if jobs.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text("No jobs.")]));
        }

        let mut out = String::new();
        for j in jobs {
            out.push_str(&format!(
                "- {} | {} | {} | {}\n",
                j.job_id, j.kind, j.status, j.message
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(description = "Cancel a background job (v1 parity: cancel_job).")]
    #[tracing::instrument(skip(self, params), fields(job_id = %params.0.job_id))]
    pub async fn cancel_job(
        &self,
        params: Parameters<CancelJobRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ok = self.cancel_job_internal(&req.job_id).await;
        Ok(CallToolResult::success(vec![Content::text(if ok {
            format!("\u{2705} cancelled job_id: {}", req.job_id)
        } else {
            format!("\u{26A0}\u{FE0F} job_id not active: {}", req.job_id)
        })]))
    }

    #[tool(description = "Get status and progress of a background job.")]
    #[tracing::instrument(skip(self, params), fields(job_id = %params.0.job_id))]
    pub async fn get_job_status(
        &self,
        params: Parameters<CancelJobRequest>, // Reuse Request struct with job_id
    ) -> Result<CallToolResult, McpError> {
        let jid = params.0.job_id.clone();
        let reg = self.state.registry.clone();
        let job_id_for_msg = jid.clone();
        let job = tokio::task::spawn_blocking(move || reg.get_job(&jid))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let Some(job) = job else {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "\u{274C} Unknown job_id: {}",
                job_id_for_msg
            ))]));
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "job_id: {}\nkind: {}\nstatus: {}\nprogress: {}%\nmessage: {}",
            job.job_id, job.kind, job.status, job.progress_pct, job.message
        ))]))
    }

    // ---- Immune system (v2 extra) ----

    #[tool(
        description = "Immune system check: compare a draft against anti-pattern index (v2 extra: immune_check)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn immune_check(
        &self,
        params: Parameters<ImmuneCheckRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // For now: lexical search in namespace "antipattern".
        let q = code_to_query(&req.code);
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q,
                    top_k: req.sanitized_top_k(),
                    fts_mode: "strict".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let (similarity, snippet) = if let Some(best) = hits.first() {
            (best.score, best.snippet.clone())
        } else {
            (0.0, None)
        };

        // Tunable thresholds from meta
        let warn_t = self
            .state
            .registry
            .get_meta(&req.project_id, "immune_warn_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(self.state.immune.warn_threshold);
        let block_t = self
            .state
            .registry
            .get_meta(&req.project_id, "immune_block_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(self.state.immune.block_threshold);

        let engine = engram_ml::ImmuneEngine::new(warn_t, block_t);
        let decision = engine.decide(similarity, snippet.as_deref());
        let mut out = String::new();
        match decision {
            engram_ml::ImmuneDecision::Allow => {
                out.push_str("\u{2705} ALLOW\n");
            }
            engram_ml::ImmuneDecision::Warn {
                message,
                confidence,
            } => {
                out.push_str(&format!(
                    "\u{26A0}\u{FE0F} WARN (confidence={confidence:.2})\n{message}\n"
                ));
            }
            engram_ml::ImmuneDecision::Block {
                message,
                confidence,
            } => {
                out.push_str(&format!(
                    "\u{26D4} BLOCK (confidence={confidence:.2})\n{message}\n"
                ));
            }
        }

        if !hits.is_empty() {
            out.push_str("\nTop anti-pattern matches:\n");
            for h in hits.iter().take(5) {
                out.push_str(&format!(
                    "- score={:.3} path={} chunk_id={}\n",
                    h.score, h.path, h.chunk_id
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Anti-pattern guard: score a code snippet against the anti-pattern index and suggest alternatives."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn anti_pattern_guard(
        &self,
        params: Parameters<AntiPatternGuardRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // Use strict FTS for deterministic signature matching
        let q = code_to_query(&req.code);
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q,
                    top_k: req.sanitized_limit(),
                    fts_mode: "strict".into(),
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: false,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "✅ No matching anti-patterns found. The code snippet looks safe based on history.",
            )]));
        }

        let mut out = String::new();
        let best = &hits[0];

        // Tunable thresholds from meta
        let warn_t = self
            .state
            .registry
            .get_meta(&req.project_id, "immune_warn_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(self.state.immune.warn_threshold);
        let block_t = self
            .state
            .registry
            .get_meta(&req.project_id, "immune_block_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(self.state.immune.block_threshold);

        let engine = engram_ml::ImmuneEngine::new(warn_t, block_t);
        // Fetch full content for better context
        let snippet = if let Ok(Some((_, _, content, _, _))) =
            ps.search
                .get_doc_by_doc_id(&req.project_id, "antipattern", gen_, &best.doc_id)
        {
            Some(content)
        } else {
            best.snippet.clone()
        };

        let decision = engine.decide(best.score, snippet.as_deref());
        match decision {
            engram_ml::ImmuneDecision::Allow => {
                out.push_str("✅ ALLOW\n");
            }
            engram_ml::ImmuneDecision::Warn {
                message,
                confidence,
            } => {
                out.push_str(&format!(
                    "⚠️ WARN (confidence={confidence:.2})\n{message}\n"
                ));
            }
            engram_ml::ImmuneDecision::Block {
                message,
                confidence,
            } => {
                out.push_str(&format!(
                    "🚫 BLOCK (confidence={confidence:.2})\n{message}\n"
                ));
            }
        }

        out.push_str("\nWhy it's risky:\n");
        out.push_str("This code snippet matches a pattern that was previously reverted in the project's history. Reverted code usually indicates bugs, performance issues, or architectural violations that were later corrected.");

        out.push_str("\n\nSuggested safer alternative pattern:\n");
        if let Some(sn) = &snippet {
            if sn.contains("Reverted in Commit: ") {
                if let Some(line) = sn.lines().find(|l| l.contains("Reverted in Commit: ")) {
                    let commit = line.split(':').nth(1).unwrap_or("unknown").trim();
                    out.push_str(&format!("Review the changes in commit {} to see how this pattern was corrected or replaced. Usually, the correct approach is the inverse of the reverted diff or following the pattern established in the reverting commit.", commit));
                }
            } else {
                out.push_str("Review the project's history for similar files to identify the current best practices. Avoid the logic shown in the matched snippet.");
            }
        } else {
            out.push_str("Consult the project's documentation or lead engineers to identify the preferred pattern for this functionality.");
        }

        if !hits.is_empty() {
            out.push_str("\n\nTop anti-pattern matches:\n");
            for h in hits.iter().take(req.sanitized_limit()) {
                out.push_str(&format!(
                    "- score={:.3} path={} chunk_id={}\n",
                    h.score, h.path, h.chunk_id
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Generate a minimal instrumentation snippet for legacy .NET apps to log runtime events."
    )]
    pub async fn get_instrumentation_pack(
        &self,
        params: Parameters<GetInstrumentationPackRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let lang = req.language.to_lowercase();

        let (snippet, instructions) = match lang.as_str() {
            "csharp" | "cs" => {
                let s = r#"
// Add to Global.asax.cs or a base Page class
protected void LogEngramEvent(string eventName, string controlId, string sqlText = null) {
    string path = Request.AppRelativeCurrentExecutionFilePath;
    string sqlHash = string.IsNullOrEmpty(sqlText) ? "" : BitConverter.ToString(System.Security.Cryptography.SHA1.Create().ComputeHash(System.Text.Encoding.UTF8.GetBytes(sqlText))).Replace("-", "").ToLower().Substring(0, 12);
    string logLine = $"{DateTime.UtcNow:s}|{path}|{eventName}|{controlId}|{sqlHash}";
    System.Diagnostics.Trace.WriteLine($"ENGRAM_LOG|{logLine}");
}
"#;
                let i = "1. Add the LogEngramEvent method to your base Page class or Global.asax.\n2. Call it in your event handlers: LogEngramEvent(\"OnClick\", btn.ID);\n3. If you have a DAL, pass the SQL: LogEngramEvent(\"SqlCall\", \"\", cmd.CommandText);\n4. Capture the Trace output and ingest it using ingest_instrumentation_logs.";
                (s, i)
            }
            "vb" | "vbnet" | "vb.net" => {
                let s = r#"
' Add to Global.asax.vb or a base Page class
Protected Sub LogEngramEvent(eventName As String, controlId As String, Optional sqlText As String = Nothing)
    Dim path As String = Request.AppRelativeCurrentExecutionFilePath
    Dim sqlHash As String = ""
    If Not String.IsNullOrEmpty(sqlText) Then
        Using sha1 = System.Security.Cryptography.SHA1.Create()
            Dim hashBytes = sha1.ComputeHash(System.Text.Encoding.UTF8.GetBytes(sqlText))
            sqlHash = BitConverter.ToString(hashBytes).Replace("-", "").ToLower().Substring(0, 12)
        End Using
    End If
    Dim logLine As String = String.Format("{0:s}|{1}|{2}|{3}|{4}", DateTime.UtcNow, path, eventName, controlId, sqlHash)
    System.Diagnostics.Trace.WriteLine("ENGRAM_LOG|" & logLine)
End Sub
"#;
                let i = "1. Add the LogEngramEvent sub to your base Page class or Global.asax.\n2. Call it in your event handlers: LogEngramEvent(\"OnClick\", btn.ID)\n3. If you have a DAL, pass the SQL: LogEngramEvent(\"SqlCall\", \"\", cmd.CommandText)\n4. Capture the Trace output and ingest it using ingest_instrumentation_logs.";
                (s, i)
            }
            _ => {
                return Err(McpError::invalid_params(
                    "Unsupported language. Use 'csharp' or 'vb'.",
                    None,
                ));
            }
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Instrumentation Snippet ({lang}):\n{snippet}\n\nInstructions:\n{instructions}"
        ))]))
    }

    #[tool(
        description = "Suggest microservice/bounded-context migration boundaries from temporal coupling clusters, shared state, and SQL table references. Uses LLM when available, falls back to directory-prefix grouping."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn suggest_migration_boundaries(
        &self,
        params: Parameters<SuggestMigrationBoundariesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_record(&req.project_id).await?;

        let boundaries = self
            .cognitive_suggest_boundaries(
                &req.project_id,
                req.sanitized_min_frequency(),
                req.sanitized_max_clusters(),
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if boundaries.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No temporal coupling data found. Index git history first (index_git_history) to populate coupling edges.",
            )]));
        }

        let json =
            serde_json::to_string_pretty(&boundaries).unwrap_or_else(|_| format!("{boundaries:?}"));

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Migration Boundary Suggestions ({} contexts):\n\n{json}",
            boundaries.len()
        ))]))
    }

    #[tool(
        description = "Ingest instrumentation logs from a legacy .NET app to record runtime events and SQL calls."
    )]
    pub async fn ingest_instrumentation_logs(
        &self,
        params: Parameters<IngestInstrumentationLogsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ps = self.ensure_project_runtime(&req.project_id).await?;
        let active_gen = self.get_active_generation(&req.project_id).await?;

        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();

        let count = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut edge_batch: Vec<(engram_graph::EdgeKind, String, String, u32)> = Vec::new();
            for line in req.log_content.lines() {
                if !line.contains("ENGRAM_LOG|") {
                    continue;
                }
                let Some(log_part) = line.split("ENGRAM_LOG|").nth(1).filter(|s| !s.is_empty())
                else {
                    continue; // Skip malformed log lines (e.g. "ENGRAM_LOG|" with no data)
                };
                let parts: Vec<&str> = log_part.split('|').collect();
                if parts.len() < 5 {
                    continue;
                }

                let _timestamp = parts[0];
                let path = parts[1]; // e.g. ~/Default.aspx
                let _event_name = parts[2];
                let control_id = parts[3];
                let sql_hash = parts[4];

                // Normalize path: strip ASP.NET virtual path prefixes (`~/`, `/`,
                // absolute Windows paths) and reject any traversal or unsafe chars.
                let rel_path = path
                    .trim_start_matches("~/")
                    .trim_start_matches('/')
                    .trim_start_matches('\\');

                // Security: use RelPath normalization which strips .., control chars,
                // and NUL bytes. Also reject if the result is empty (root path) or
                // still contains backslashes (possible Windows absolute path leak).
                let safe = engram_core::RelPath::new(rel_path);
                if safe.is_empty() {
                    tracing::warn!(path = %path, "Rejecting instrumentation log line with empty normalized path");
                    continue;
                }
                let rel_path = safe.as_str();

                let source_id = if !control_id.is_empty() {
                    engram_core::ids::NodeId::control(rel_path, control_id).0
                } else {
                    engram_core::ids::NodeId::page(rel_path).0
                };

                if !sql_hash.is_empty() {
                    let target_id = format!("sql:inline:{}", sql_hash);
                    edge_batch.push((
                        engram_graph::EdgeKind::SqlCalls,
                        source_id,
                        target_id,
                        1,
                    ));
                }
            }
            let edges_added = edge_batch.len();
            if !edge_batch.is_empty() {
                graph.batch_increment_edges(
                    &project_id,
                    engram_core::namespaces::NAMESPACE_HISTORY,
                    "text",
                    active_gen,
                    &edge_batch,
                )?;
            }
            Ok(edges_added)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Ingested logs, added {} runtime SQL call edges.",
            count
        ))]))
    }

    // ---- Migration slicer ----

    #[tool(
        description = "Compile a vertical-slice migration blueprint for a legacy entry point (WebForms page, JS file, VB class). Traverses the graph to collect all frontend scripts, backend methods, state mutations, database dependencies, component wiring, lifecycle info, and side-effects into one structured dossier. Use this BEFORE rewriting any legacy feature."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, entry_node = %params.0.entry_node))]
    pub async fn generate_migration_blueprint(
        &self,
        params: Parameters<GenerateMigrationBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let max_depth = req.sanitized_max_depth();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let entry_raw = req.entry_node.clone();

        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            // Try exact match first, then fuzzy-find by substring
            let entry_node_id = if graph
                .get_node(&project_id, &entry_raw)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                entry_raw.clone()
            } else {
                // Try common prefixes: file:, sym:class:, sym:function:
                let candidates = [
                    format!("file:{entry_raw}"),
                    entry_raw.clone(),
                ];
                let mut found = None;
                for cand in &candidates {
                    if graph
                        .get_node(&project_id, cand)
                        .map_err(|e| e.to_string())?
                        .is_some()
                    {
                        found = Some(cand.clone());
                        break;
                    }
                }
                if found.is_none() {
                    // Substring search across all nodes
                    let nodes = graph
                        .query_nodes(&project_id, None, Some(&entry_raw), None, 10)
                        .map_err(|e| e.to_string())?;
                    if let Some(n) = nodes.first() {
                        found = Some(n.node_id.clone());
                    }
                }
                match found {
                    Some(id) => id,
                    None => {
                        return Err(format!(
                            "No node found matching '{}'. Try query_graph_nodes to discover node IDs.",
                            entry_raw
                        ));
                    }
                }
            };

            let slice = graph_service::compile_migration_slice(
                &graph,
                &project_id,
                &entry_node_id,
                max_depth,
            )
            .map_err(|e| e.to_string())?;

            Ok(graph_service::format_migration_blueprint(&slice))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}

// -------------------- Server handler --------------------

#[tool_handler]
impl ServerHandler for Engram {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Engram MCP v2 (Rust) - hybrid code search + graph cognition + git intelligence."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Run the server (STDIO).
pub async fn run_stdio(state: AppState) -> anyhow::Result<()> {
    let service = Engram::new(state)
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}
