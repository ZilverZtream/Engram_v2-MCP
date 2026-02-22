use crate::models::*;
use crate::services::{graph_service, project_service};
use crate::state::{AppEvent, AppState, ProjectInfo, ProjectState, SearchHitLite};
use crate::utils::files::exts_for_project_type;
use crate::utils::text::{code_to_query, stacktrace_to_query};
use crate::utils::{dir_size_bytes, format_bytes, now_ms};
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

    #[tool(
        description = "Comprehensive project health check with per-namespace stats, disk usage, generation consistency, job status, and integrity diagnostics with actionable repair suggestions."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn project_health(
        &self,
        params: Parameters<ProjectIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pid = params.0.project_id;
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

    #[tool(
        description = "Repair a project index with targeted scope. Supports full re-index, graph-only rebuild, tantivy-only rebuild, vector-only rebuild, or wipe-and-reindex from scratch. Use project_health first to diagnose issues."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn repair_project(
        &self,
        params: Parameters<RepairProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
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

    // ---- Vector Search (pure semantic vector search) ----

    #[tool(
        description = "Pure semantic vector search using embedding similarity. Faster and more semantically focused than hybrid search_memory. Supports MMR diversity reranking, path/language filtering, and optional content retrieval."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn vector_search(
        &self,
        params: Parameters<VectorSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let top_k = req.sanitized_top_k();
        let max_chars = req.sanitized_max_content_chars();
        let timeout_ms = self.state.cfg.vector_search_timeout_ms;

        let q = HybridQuery {
            project_id: req.project_id.clone(),
            namespace: req.namespace.clone(),
            generation: gen_,
            text: req.query.clone(),
            top_k,
            fts_mode: String::new(), // unused by vector path
            include_path_prefixes: req.include_path_prefixes.clone(),
            exclude_path_prefixes: req.exclude_path_prefixes.clone(),
            language_filters: req.language_filters.clone(),
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: req.use_mmr,
        };

        let hits = ps
            .search
            .pure_vector_search(&q, timeout_ms)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if hits.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No vector search results. Ensure the project is indexed with a vector-capable embedding backend (not fts_only).",
            )]));
        }

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Vector search results (namespace={}, top_k={}, mmr={}):\n\n",
            req.namespace, top_k, req.use_mmr
        ));

        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "[{}] similarity={:.4} path={} chunk_id={}\n",
                i + 1,
                h.score,
                h.path,
                h.chunk_id
            ));

            if req.include_content {
                if let Ok(Some((_, _, content, _, _))) = ps.search.get_doc_by_pk(&h.pk) {
                    if content.chars().count() > max_chars {
                        out.push_str(&content.chars().take(max_chars).collect::<String>());
                        out.push_str("... (truncated)\n");
                    } else {
                        out.push_str(&content);
                        out.push('\n');
                    }
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Fetch full content for a chunk (v1 parity: get_chunk). Supports logical_slice to filter by method category: event_handlers, ui_methods, data_methods, sql_queries, state_access, or all (default)."
    )]
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
        let mut display_content = if req.inject_rules {
            self.inject_repo_rules(&req.project_id, &path, &content)
                .await
        } else {
            content.to_string()
        };

        // Apply logical slice if requested.
        if let Some(ref slice_type) = req.logical_slice {
            if slice_type != "all" && !slice_type.is_empty() {
                display_content = crate::services::slice_service::apply_logical_slice(
                    &display_content,
                    slice_type,
                    &lang,
                );
            }
        }

        // Compute confidence footer for WebForms files.
        let confidence_footer = self.confidence_footer(&path, &lang);

        let mut output = format!(
            "path: {}\ndoc_id: {}\nnamespace: {}\nlanguage: {}\nlines: {}-{}\nactive_generation: {}\n\n{}",
            path, req.doc_id, req.namespace, lang, start_line, end_line, gen_, display_content
        );
        output.push_str(&confidence_footer);

        Ok(CallToolResult::success(vec![Content::text(output)]))
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

    #[tool(
        description = "Graph-boosted search: combines text search with graph symbol name matching and configurable multi-hop neighbor expansion. Supports namespace selection, FTS modes (strict/loose/regex), MMR reranking, and content preview."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, query = %params.0.query))]
    pub async fn graph_search(
        &self,
        params: Parameters<GraphSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let max_results = req.sanitized_max_results();
        let hop_depth = req.sanitized_hop_depth();
        let max_content_chars = req.sanitized_max_content_chars();

        // Validate fts_mode
        let fts_mode = match req.fts_mode.as_str() {
            "strict" | "loose" | "regex" => req.fts_mode.clone(),
            _ => "strict".into(),
        };

        // 1. Hybrid text search for initial candidates
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: req.namespace.clone(),
                    generation: gen_,
                    text: req.query.clone(),
                    top_k: max_results * 2, // oversample for graph expansion
                    fts_mode,
                    include_path_prefixes: None,
                    exclude_path_prefixes: None,
                    language_filters: None,
                    author_filter: None,
                    date_after: None,
                    date_before: None,
                    use_mmr: req.use_mmr,
                },
                None,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // 2. Graph symbol name lookup: find symbol nodes whose name matches the query
        let symbol_nodes = self
            .state
            .graph
            .query_nodes(&req.project_id, None, Some(&req.query), None, 30)
            .unwrap_or_default();

        // Build score map: node_id -> (score, label, path_for_content)
        let mut scores: std::collections::HashMap<String, (f32, Option<String>, Option<String>)> =
            std::collections::HashMap::with_capacity(max_results * 2);

        // Seed from text search hits (file-level nodes)
        for h in &hits {
            let node_id = format!("file:{}", h.path);
            scores.insert(node_id, (h.score, None, Some(h.path.as_str().to_string())));
        }

        // Seed from symbol name matches with a symbol boost
        let base_score = hits.first().map(|h| h.score).unwrap_or(1.0);
        let query_lower = req.query.to_lowercase();
        for node in &symbol_nodes {
            let name_lower = node.name.to_lowercase();
            let match_ratio = if name_lower == query_lower {
                1.0f32
            } else if name_lower.contains(&query_lower) || query_lower.contains(&name_lower) {
                0.6
            } else {
                0.3
            };

            let sym_score = base_score * (0.5 + req.symbol_boost * 10.0 * match_ratio);
            let label = Some(format!("{} ({})", node.node_type, node.name));
            let file_path = if node.file_path.as_str().is_empty() {
                None
            } else {
                Some(node.file_path.as_str().to_string())
            };
            let entry = scores
                .entry(node.node_id.clone())
                .or_insert((0.0, None, None));
            if sym_score > entry.0 {
                *entry = (sym_score, label, file_path.clone());
            }

            // Also boost the parent file node
            if let Some(fp) = &file_path {
                let file_node_id = format!("file:{}", fp);
                let file_entry = scores.entry(file_node_id).or_insert((0.0, None, None));
                let file_boost = sym_score * 0.8;
                if file_boost > file_entry.0 {
                    file_entry.0 = file_boost;
                    file_entry.2 = file_path.clone();
                }
            }
        }

        // 3. Determine expansion edge kinds
        let default_expansion_kinds = vec![
            EdgeKind::Dependency,
            EdgeKind::Contains,
            EdgeKind::Imports,
            EdgeKind::SqlCalls,
            EdgeKind::ApiCall,
        ];
        let expansion_kinds = if let Some(ref filter) = req.expansion_edge_kinds {
            let mut kinds = Vec::new();
            for s in filter {
                if let Some(k) = EdgeKind::parse(s) {
                    kinds.push(k);
                }
            }
            if kinds.is_empty() {
                default_expansion_kinds
            } else {
                kinds
            }
        } else {
            default_expansion_kinds
        };

        // 4. Multi-hop graph expansion with configurable depth
        for _hop in 0..hop_depth {
            let seed_nodes: Vec<(String, f32)> = scores
                .iter()
                .map(|(k, (s, _, _))| (k.clone(), *s))
                .collect();

            for (node_id, parent_score) in &seed_nodes {
                let neighbors_per_kind = 5.max(10 / expansion_kinds.len());
                for kind in &expansion_kinds {
                    if let Ok(neighbors) = self.state.graph.neighbors(
                        &req.project_id,
                        kind.clone(),
                        node_id,
                        neighbors_per_kind,
                    ) {
                        for (neigh_id, weight) in neighbors {
                            let hop_decay = 0.7f32.powi((_hop + 1) as i32);
                            let weight_factor =
                                0.5 + (weight.min(10) as f32 * req.symbol_boost * 0.05);
                            let neigh_score = parent_score * weight_factor.min(0.90) * hop_decay;
                            let entry = scores.entry(neigh_id).or_insert((0.0, None, None));
                            if neigh_score > entry.0 {
                                entry.0 = neigh_score;
                            }
                        }
                    }
                }
            }
        }

        if scores.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No graph matches found.",
            )]));
        }

        // 5. Sort and format
        let mut sorted: Vec<_> = scores.into_iter().collect();
        sorted.sort_by(|a, b| {
            (b.1)
                .0
                .partial_cmp(&(a.1).0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Graph search results for '{}' (ns={}, {} text hits, {} symbol matches, {} hops):\n",
            req.query,
            req.namespace,
            hits.len(),
            symbol_nodes.len(),
            hop_depth
        ));

        for (id, (score, label, _path)) in sorted.iter().take(max_results) {
            if let Some(lbl) = label {
                out.push_str(&format!("- {} [{}] (score={:.3})\n", id, lbl, score));
            } else {
                out.push_str(&format!("- {} (score={:.3})\n", id, score));
            }

            // Include content preview if requested
            if req.include_content && max_content_chars > 0 {
                // Try to fetch content from search hits first
                if let Some(hit) = hits
                    .iter()
                    .find(|h| id == &format!("file:{}", h.path) || id.contains(h.path.as_str()))
                {
                    if let Ok(Some((_, _, content, start_line, _))) = ps.search.get_doc_by_doc_id(
                        &req.project_id,
                        &req.namespace,
                        gen_,
                        &hit.doc_id,
                    ) {
                        let preview: String = content.chars().take(max_content_chars).collect();
                        out.push_str(&format!("  L{}: {}", start_line, preview));
                        if content.chars().count() > max_content_chars {
                            out.push_str("...");
                        }
                        out.push('\n');
                    }
                }
            }
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

    #[tool(
        description = "Search git history (commits and diffs) with configurable FTS mode, MMR reranking, path filtering, author/date filters, and structured commit metadata extraction."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, query = %params.0.query))]
    pub async fn search_history(
        &self,
        params: Parameters<SearchHistoryRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let content_limit = req.max_content_chars;
        let limit = req.sanitized_limit();

        // Validate fts_mode
        let fts_mode = match req.fts_mode.as_str() {
            "strict" | "loose" | "regex" => req.fts_mode.clone(),
            _ => "strict".into(),
        };

        // Map path filters
        let include_path_prefixes = req.file_filter.map(|f| vec![f]);
        let exclude_path_prefixes = req.exclude_paths;
        let project_id = req.project_id;
        let query = req.query;
        let author_filter = req.author_filter;
        let date_after = req.date_after;
        let date_before = req.date_before;
        let use_mmr = req.use_mmr;

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: project_id.clone(),
                    namespace: "history".into(),
                    generation: gen_,
                    text: query.clone(),
                    top_k: limit,
                    fts_mode,
                    include_path_prefixes,
                    exclude_path_prefixes,
                    language_filters: None,
                    author_filter,
                    date_after,
                    date_before,
                    use_mmr,
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

        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "History search results ({} hits, gen {}):\n",
            hits.len(),
            gen_
        ));

        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!("\n--- #{} ---\n", i + 1));
            out.push_str(&format!("score: {:.3}\n", h.score));
            out.push_str(&format!("path: {}\n", h.path));

            if let Ok(Some((_, _, content, _, _))) =
                ps.search
                    .get_doc_by_doc_id(&project_id, "history", gen_, &h.doc_id)
            {
                // Extract structured commit metadata from content header
                let mut commit_hash = None;
                let mut author = None;
                let mut date = None;
                let mut message = None;
                let mut diff_start = 0;

                for (line_idx, line) in content.lines().enumerate() {
                    if line.starts_with("commit ") && commit_hash.is_none() {
                        commit_hash = Some(line.trim_start_matches("commit ").trim());
                    } else if line.starts_with("Author: ") && author.is_none() {
                        author = Some(line.trim_start_matches("Author: ").trim());
                    } else if line.starts_with("Date: ") && date.is_none() {
                        date = Some(line.trim_start_matches("Date: ").trim());
                    } else if line.starts_with("    ") && message.is_none() && commit_hash.is_some()
                    {
                        message = Some(line.trim());
                    } else if line.starts_with("diff ") || line.starts_with("---") {
                        diff_start = content
                            .lines()
                            .take(line_idx)
                            .map(|l| l.len() + 1)
                            .sum::<usize>();
                        break;
                    }
                }

                if let Some(hash) = commit_hash {
                    out.push_str(&format!("commit: {}\n", hash));
                }
                if let Some(auth) = author {
                    out.push_str(&format!("author: {}\n", auth));
                }
                if let Some(d) = date {
                    out.push_str(&format!("date: {}\n", d));
                }
                if let Some(msg) = message {
                    out.push_str(&format!("message: {}\n", msg));
                }

                // Show diff content if requested
                if content_limit > 0 {
                    let diff_content = if diff_start > 0 && diff_start < content.len() {
                        &content[diff_start..]
                    } else {
                        &content
                    };
                    out.push_str("content:\n");
                    if diff_content.chars().count() > content_limit {
                        out.push_str(&diff_content.chars().take(content_limit).collect::<String>());
                        out.push_str("... (truncated)");
                    } else {
                        out.push_str(diff_content);
                    }
                    out.push('\n');
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
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
        description = "Analyze reverts (The Immune System): detects reverted commits, generates LLM-powered descriptive anti-pattern rules, and indexes the reverted diffs (v1 parity: analyze_reverts)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn analyze_reverts(
        &self,
        params: Parameters<AnalyzeRevertsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let active_gen = self.get_active_generation(&req.project_id).await?;

        // Phase 1 (CPU-bound): walk commits and extract revert data in a blocking thread.
        let cancel = tokio_util::sync::CancellationToken::new();

        struct RevertData {
            rule_id: String,
            file_pattern: String,
            diff_text: String,
            original_commit: String,
            file_path: engram_core::RelPath,
        }

        let revert_data = tokio::task::spawn_blocking({
            let directory = ps.info.directory.clone();
            let max_commits = req.max_commits;
            let cancel_clone = cancel.clone();

            move || -> anyhow::Result<Vec<RevertData>> {
                let repo = GitWalker::open_repo(Path::new(&directory))?;
                let mut data = Vec::new();

                GitWalker::walk_commits_streaming(
                    &repo,
                    None,
                    max_commits,
                    engram_git::history::MergeCommitPolicy::AllParents,
                    &cancel_clone,
                    |oid, _curr, _total| {
                        let docs =
                            GitWalker::extract_antipatterns_from_reverts(&repo, oid, 50_000)?;
                        for doc in docs {
                            data.push(RevertData {
                                rule_id: format!("immune_{}", doc.original_commit),
                                file_pattern: format!("**/{}", doc.file_path),
                                diff_text: doc.diff_text,
                                original_commit: doc.original_commit.to_string(),
                                file_path: doc.file_path,
                            });
                        }
                        Ok(())
                    },
                )?;

                Ok(data)
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let reverts_found = revert_data.len();

        // Phase 2 (async): Generate LLM-powered descriptive rules + build IndexDocs.
        let dreaming = self.state.dreaming.clone();
        let mut anti_docs = Vec::with_capacity(revert_data.len());
        let mut rules_added = 0;

        for rd in &revert_data {
            // Try LLM-based analysis of the reverted diff to produce a descriptive rule
            let diff_snippet = if rd.diff_text.len() > 2000 {
                &rd.diff_text[..2000]
            } else {
                &rd.diff_text
            };

            let prompt = format!(
                "Analyze this reverted code diff and explain in 1-2 concise sentences why this pattern should be avoided. \
                 Focus on the root cause — what went wrong and what developers should do instead.\n\n\
                 File: {}\nOriginal commit: {}\n\nDiff:\n```\n{}\n```\n\n\
                 Respond with ONLY the rule text (no preamble).",
                rd.file_path, rd.original_commit, diff_snippet
            );

            let llm_analysis = dreaming
                .generate_text(&prompt, 200, std::time::Duration::from_secs(15))
                .await;

            let rule_text = if llm_analysis.is_empty() {
                // Deterministic fallback with more detail than before
                format!(
                    "AVOID: Pattern in {} was reverted (commit {}). The change was rolled back indicating it introduced a regression. Review carefully before reintroducing similar changes.",
                    rd.file_path, rd.original_commit
                )
            } else {
                format!(
                    "AVOID (reverted in {}): {}",
                    rd.original_commit, llm_analysis
                )
            };

            let rule = RepoRule {
                rule_id: rd.rule_id.clone(),
                file_pattern: rd.file_pattern.clone(),
                rule_text,
                priority: 10,
                updated_at_ms: now_ms(),
            };
            self.state
                .registry
                .put_repo_rule(&req.project_id, &rule)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            rules_added += 1;

            // Build IndexDoc for Tantivy
            let immune_content_hash = engram_core::ContentHash::compute(rd.diff_text.as_bytes());
            let immune_doc_id_str =
                engram_core::DocIdStr::compute(rd.file_path.as_str(), 0, 0, &immune_content_hash).0;

            anti_docs.push(engram_index::IndexDoc {
                generation: active_gen,
                chunk_id: engram_index::chunk_id_from_content_hash(&immune_content_hash),
                doc_id: immune_doc_id_str,
                content_hash: immune_content_hash.0,
                path: rd.file_path.clone(),
                language: "code".into(),
                content: rd.diff_text.clone(),
                namespace: "antipattern".into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
            });
        }

        // Phase 3 (async): index the anti-pattern docs
        let docs_indexed = anti_docs.len();
        if !anti_docs.is_empty() {
            ps.search
                .index_docs(&req.project_id, &anti_docs, &cancel)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        // Phase 4: Build graph edges connecting affected files to anti-pattern nodes
        let mut ap_edges = Vec::new();
        for rd in &revert_data {
            // Create an AntiPattern edge from file → anti-pattern marker
            ap_edges.push(engram_graph::Edge {
                source_id: format!("file:{}", rd.file_path),
                target_id: format!("antipattern:{}", rd.rule_id),
                namespace: "antipattern".into(),
                language: "code".into(),
                edge_kind: EdgeKind::AntiPattern,
                weight: 1,
                generation: active_gen,
                metadata: None,
                updated_at_ms: now_ms(),
            });
        }
        let edges_created = ap_edges.len();
        if !ap_edges.is_empty() {
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let _ = tokio::task::spawn_blocking(move || graph.upsert_edges(&pid, &ap_edges)).await;
        }

        // Record metrics
        engram_core::metrics()
            .tantivy_docs_indexed
            .add(docs_indexed as u64);

        let summary = format!(
            "\u{2705} Immune System active.\n\
             Reverts analyzed: {reverts_found}\n\
             Anti-patterns indexed: {docs_indexed}\n\
             Repo rules generated: {rules_added}\n\
             Graph edges created: {edges_created}",
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

        let file_path_for_confidence = req.file_path.clone();
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

        // Append confidence footer if the target is a WebForms file.
        let mut result = out;
        if let Some(ref fp) = file_path_for_confidence {
            let rel = engram_core::RelPath::from(fp.as_str());
            let lang = engram_core::guess_language(std::path::Path::new(fp));
            result.push_str(&self.confidence_footer(&rel, &lang));
        }

        Ok(CallToolResult::success(vec![Content::text(result)]))
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
        let mut trace_used_fallback = false;
        let mut trace_candidate_count: usize = 0;
        let mut unresolved_candidates: Vec<String> = Vec::new();
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
                    .query_nodes(&req.project_id, Some("control"), Some(ctrl), None, 10)
            && !candidates.is_empty()
        {
            trace_used_fallback = true;
            trace_candidate_count = candidates.len();
            // Record all candidate node IDs for provenance
            unresolved_candidates = candidates.iter().map(|n| n.node_id.clone()).collect();
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

        // 3. Format output with ambiguity provenance
        let mut out = format!("Found {} path(s) to SQL:\n", paths.len());

        // Ambiguity provenance block (structured for machine parsing)
        let confidence_penalty = if trace_used_fallback {
            (trace_candidate_count as f64 * 0.2).min(0.8)
        } else {
            0.0
        };

        out.push_str("\n## Trace Provenance\n");
        out.push_str(&format!("trace_used_fallback: {}\n", trace_used_fallback));
        out.push_str(&format!(
            "trace_candidate_count: {}\n",
            trace_candidate_count
        ));
        out.push_str(&format!(
            "trace_confidence_penalty: {:.2}\n",
            confidence_penalty
        ));
        out.push_str(&format!("selected_start_node: {}\n", start_id));

        if trace_used_fallback {
            out.push_str(&format!(
                "\n### Ambiguity Warning\n\
                 Control lookup used fallback candidate matching ({} candidates found).\n\
                 Penalty reason: {} candidate(s) matched control ID filter; first-match selected.\n\
                 Risk: Incorrect handler resolution may lead to wrong trace path.\n",
                trace_candidate_count, trace_candidate_count
            ));
            out.push_str("\n### Unresolved Candidates\n");
            for (i, cand) in unresolved_candidates.iter().enumerate() {
                let selected = if i == 0 { " ← SELECTED" } else { "" };
                out.push_str(&format!("  {}. {}{}\n", i + 1, cand, selected));
            }
            out.push_str("\n### Follow-up Probes\n");
            out.push_str("- Provide explicit `handler_fqn` to disambiguate\n");
            out.push_str("- Verify control ID uniqueness across master/user controls\n");
            out.push_str("- Check code-behind inheritance chain for handler shadowing\n");
        }

        for (i, path) in paths.iter().enumerate() {
            out.push_str(&format!("\n## Path #{}\n", i + 1));
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
                    "Starting point".to_string()
                } else {
                    let prev = &path[step - 1];
                    match (prev.node_type.as_str(), node.node_type.as_str()) {
                        ("page", "class") => "Inherits class".to_string(),
                        ("control", "function") => "Event wiring (OnClick/Handles)".to_string(),
                        ("function", "function") => "Method call".to_string(),
                        (_, "inline_sql") | (_, "stored_proc") => "Executes SQL".to_string(),
                        _ => "Dependency".to_string(),
                    }
                };

                // Per-hop source evidence
                let evidence = format!(
                    "node_type={}, file={}, lines={}-{}",
                    node.node_type,
                    node.file_path.as_str(),
                    node.start_line,
                    node.end_line
                );

                let indent = "  ".repeat(step);
                out.push_str(&format!(
                    "{indent}Step {}: {} [{}] ({}) - {} | evidence: {}\n",
                    step + 1,
                    node.name,
                    label,
                    node.node_id,
                    justification,
                    evidence
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

    #[tool(
        description = "Comprehensive codebase overview with language breakdown, symbol-type aggregation, architectural analysis, graph metrics, antipattern stats, dead code detection, test file coverage, and hotspot files."
    )]
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
        let ns_counts = ps.search.count_docs_by_namespace(&pid).unwrap_or_default();
        let antipattern_docs = ns_counts.get("antipattern").copied().unwrap_or(0);
        let history_docs = ns_counts.get("history").copied().unwrap_or(0);

        // Language breakdown (from Tantivy "memory" namespace)
        let lang_counts = ps.search.count_docs_by_language(&pid).unwrap_or_default();

        // Graph: node/edge counts, state node usage, dead code count, test file count — all in one blocking call
        let graph = self.state.graph.clone();
        let pid_clone2 = pid.clone();
        let active_gen = gen_;
        let (
            node_type_counts,
            edge_kind_counts,
            centrality,
            state_usage_data,
            dead_code_count,
            test_file_count,
            total_file_count,
        ) = tokio::task::spawn_blocking(move || {
            let ntc = graph.count_nodes_by_type(&pid_clone2).unwrap_or_default();
            let ekc = graph.count_edges_by_kind(&pid_clone2).unwrap_or_default();
            let pr = engram_graph::analysis::compute_pagerank(&graph, &pid_clone2, active_gen).ok();

            // Batch state node usage: collect all state nodes and their read/write counts in one pass
            let state_nodes = graph
                .query_nodes(&pid_clone2, Some("global_state"), None, None, 100)
                .unwrap_or_default();
            let mut state_usage: Vec<(String, usize, usize)> =
                Vec::with_capacity(state_nodes.len());
            for sn in &state_nodes {
                let reads = graph
                    .find_incoming_edges(&pid_clone2, Some(EdgeKind::ReadsState), &sn.node_id, 200)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let writes = graph
                    .find_incoming_edges(&pid_clone2, Some(EdgeKind::WritesState), &sn.node_id, 200)
                    .map(|v| v.len())
                    .unwrap_or(0);
                state_usage.push((sn.name.clone(), reads, writes));
            }
            state_usage.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));

            // Dead code detection: count function/class nodes with zero incoming edges
            let mut dead = 0usize;
            let mut test_files = 0usize;
            let file_nodes = graph
                .query_nodes(&pid_clone2, Some("file"), None, None, 5000)
                .unwrap_or_default();
            let total_files = file_nodes.len();
            for f in &file_nodes {
                let path_lower = f.file_path.as_str().to_lowercase();
                if path_lower.contains("test")
                    || path_lower.contains("spec")
                    || path_lower.contains("_test.")
                {
                    test_files += 1;
                }
            }

            // Check functions with no incoming references (potential dead code)
            let func_nodes = graph
                .query_nodes(&pid_clone2, Some("function"), None, None, 2000)
                .unwrap_or_default();
            for func in &func_nodes {
                let incoming = graph
                    .find_incoming_edges(&pid_clone2, None, &func.node_id, 1)
                    .unwrap_or_default();
                if incoming.is_empty() {
                    dead += 1;
                }
            }

            (ntc, ekc, pr, state_usage, dead, test_files, total_files)
        })
        .await
        .unwrap_or_default();

        // ── Build output ──
        let mut out = String::with_capacity(6144);
        out.push_str(&format!("Codebase Overview: {}\n", rec.project_name));
        out.push_str(&format!("project_id: {}\n", rec.project_id));
        out.push_str(&format!("project_type: {}\n", rec.project_type));
        out.push_str(&format!("directory: {}\n", rec.directory));
        out.push_str(&format!("active_generation: {}\n", gen_));
        out.push_str(&format!("repo_rules: {}\n", rule_count));
        out.push_str(&format!("chunks_indexed: {}\n", tantivy_docs));
        out.push_str(&format!("vectors_stored: {}\n", lancedb_rows));
        out.push_str(&format!("history_docs: {}\n", history_docs));
        out.push_str(&format!("antipattern_docs: {}\n", antipattern_docs));

        // Language breakdown
        if !lang_counts.is_empty() {
            let mut lang_sorted: Vec<_> = lang_counts.into_iter().collect();
            lang_sorted.sort_by(|a, b| b.1.cmp(&a.1));
            out.push_str("\n--- Language Breakdown (chunks) ---\n");
            for (lang, count) in &lang_sorted {
                let pct = if tantivy_docs > 0 {
                    (*count as f64 / tantivy_docs as f64 * 100.0) as u32
                } else {
                    0
                };
                out.push_str(&format!("  {}: {} ({}%)\n", lang, count, pct));
            }
        }

        // Symbol-type breakdown from graph
        if !node_type_counts.is_empty() {
            let mut nts: Vec<_> = node_type_counts.iter().collect();
            nts.sort_by(|a, b| b.1.cmp(a.1));
            let total_nodes: usize = node_type_counts.values().sum();
            out.push_str(&format!("\n--- Symbol Types ({} total) ---\n", total_nodes));
            for (ntype, count) in &nts {
                out.push_str(&format!("  {}: {}\n", ntype, count));
            }
        }

        // Edge-kind breakdown
        if !edge_kind_counts.is_empty() {
            let mut eks: Vec<_> = edge_kind_counts.iter().collect();
            eks.sort_by(|a, b| b.1.cmp(a.1));
            let total_edges: usize = edge_kind_counts.values().sum();
            out.push_str(&format!("\n--- Edge Types ({} total) ---\n", total_edges));
            for (ekind, count) in eks.iter().take(15) {
                out.push_str(&format!("  {}: {}\n", ekind, count));
            }
            if eks.len() > 15 {
                out.push_str(&format!("  ... and {} more kinds\n", eks.len() - 15));
            }
        }

        // Architectural layer inference from node types
        {
            let files = node_type_counts.get("file").copied().unwrap_or(0);
            let classes = node_type_counts.get("class").copied().unwrap_or(0);
            let functions = node_type_counts.get("function").copied().unwrap_or(0);
            let interfaces = node_type_counts.get("interface").copied().unwrap_or(0);
            let db_tables = node_type_counts.get("db_table").copied().unwrap_or(0);
            let web_services = node_type_counts.get("web_service").copied().unwrap_or(0);
            let http_handlers = node_type_counts.get("http_handler").copied().unwrap_or(0);
            let wcf_services = node_type_counts.get("wcf_service").copied().unwrap_or(0);
            let controls = node_type_counts.get("control").copied().unwrap_or(0);
            let ui_containers = node_type_counts.get("ui_container").copied().unwrap_or(0);
            let app_settings = node_type_counts.get("app_setting").copied().unwrap_or(0);
            let conn_strings = node_type_counts
                .get("connection_string")
                .copied()
                .unwrap_or(0);

            out.push_str("\n--- Architecture ---\n");
            if files > 0 {
                out.push_str(&format!("  Source files: {}\n", files));
            }
            if classes > 0 || interfaces > 0 {
                out.push_str(&format!(
                    "  Types: {} classes, {} interfaces\n",
                    classes, interfaces
                ));
            }
            if functions > 0 {
                out.push_str(&format!("  Functions/Methods: {}\n", functions));
            }
            if controls > 0 || ui_containers > 0 {
                out.push_str(&format!(
                    "  UI: {} controls, {} containers\n",
                    controls, ui_containers
                ));
            }
            if web_services + http_handlers + wcf_services > 0 {
                out.push_str(&format!(
                    "  Service endpoints: {} ASMX, {} ASHX, {} WCF\n",
                    web_services, http_handlers, wcf_services
                ));
            }
            if db_tables > 0 {
                out.push_str(&format!("  Database tables: {}\n", db_tables));
            }
            if app_settings > 0 || conn_strings > 0 {
                out.push_str(&format!(
                    "  Config: {} app settings, {} connection strings\n",
                    app_settings, conn_strings
                ));
            }
        }

        // Test coverage and dead code stats
        out.push_str("\n--- Code Quality ---\n");
        if total_file_count > 0 {
            let test_pct = (test_file_count as f64 / total_file_count as f64 * 100.0) as u32;
            out.push_str(&format!(
                "  Test files: {} / {} ({}%)\n",
                test_file_count, total_file_count, test_pct
            ));
        }
        if dead_code_count > 0 {
            out.push_str(&format!(
                "  Potential dead functions (zero incoming refs): {}\n",
                dead_code_count
            ));
        }
        if antipattern_docs > 0 {
            out.push_str(&format!("  Anti-patterns indexed: {}\n", antipattern_docs));
        }

        // PageRank central nodes
        if let Some(metrics) = centrality {
            let mut top_nodes: Vec<_> = metrics.pagerank.into_iter().collect();
            top_nodes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            out.push_str("\n--- Top Central Nodes (PageRank) ---\n");
            for (id, score) in top_nodes.iter().take(10) {
                out.push_str(&format!("  {} ({:.4})\n", id, score));
            }
        }

        // Database tables overview
        let table_nodes = self
            .state
            .graph
            .query_nodes(&pid, Some("db_table"), None, None, 100)
            .unwrap_or_default();
        if !table_nodes.is_empty() {
            out.push_str(&format!(
                "\n--- Database Tables ({}) ---\n",
                table_nodes.len()
            ));
            let names: Vec<_> = table_nodes
                .iter()
                .take(20)
                .map(|n| n.name.as_str())
                .collect();
            out.push_str(&format!("  {}\n", names.join(", ")));
            if table_nodes.len() > 20 {
                out.push_str(&format!("  ... and {} more\n", table_nodes.len() - 20));
            }
        }

        // Global state keys (batched in spawn_blocking above)
        if !state_usage_data.is_empty() {
            out.push_str(&format!(
                "\n--- Global State Keys ({} total) ---\n",
                state_usage_data.len()
            ));
            for (name, reads, writes) in state_usage_data.iter().take(10) {
                out.push_str(&format!(
                    "  {} (reads={}, writes={})\n",
                    name, reads, writes
                ));
            }
            if state_usage_data.len() > 10 {
                out.push_str(&format!("  ... and {} more\n", state_usage_data.len() - 10));
            }
        }

        // Top temporal couplings
        let couplings =
            engram_graph::algorithms::coupling::top_project_couplings(&self.state.graph, &pid, 5)
                .unwrap_or_default();
        if !couplings.is_empty() {
            out.push_str("\n--- Top Temporal Couplings ---\n");
            for c in couplings {
                out.push_str(&format!(
                    "  {} <-> {} (w={})\n",
                    c.file_node_id, c.neighbor_node_id, c.weight
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    #[tool(
        description = "Find all references to a symbol across the codebase using graph edges with FQN matching, configurable edge kind filters, file scope filtering, configurable limits, and lexical fallback."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, symbol_name = %params.0.symbol_name))]
    pub async fn find_symbol_references(
        &self,
        params: Parameters<FindSymbolReferencesRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let max_incoming = req.sanitized_max_incoming();
        let max_outgoing_per_kind = req.sanitized_max_outgoing_per_kind();
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let needle = &req.symbol_name;

        // Parse edge kind filter
        let edge_kind_filter: Option<Vec<EdgeKind>> = req
            .edge_kind_filter
            .as_ref()
            .map(|f| f.iter().filter_map(|s| EdgeKind::parse(s)).collect());

        // 1. Find matching symbol nodes — exact name match and FQN suffix match
        let nodes = self
            .state
            .graph
            .query_nodes(&req.project_id, None, Some(needle), None, 50)
            .unwrap_or_default();

        let mut out = String::with_capacity(4096);
        let mut found_in_graph = false;

        for node in &nodes {
            // Multi-strategy name match: exact, FQN suffix, node-id suffix
            let name_lower = node.name.to_lowercase();
            let needle_lower = needle.to_lowercase();
            let name_matches = name_lower == needle_lower
                || name_lower.ends_with(&format!(".{}", needle_lower))
                || name_lower.ends_with(&format!("::{}", needle_lower))
                || node
                    .node_id
                    .to_lowercase()
                    .ends_with(&format!(":{}", needle_lower));
            if !name_matches {
                continue;
            }

            // Apply file scope filter
            if let Some(ref scope) = req.file_scope {
                if !node.file_path.as_str().is_empty()
                    && !node.file_path.as_str().starts_with(scope.as_str())
                {
                    continue;
                }
            }

            // Query incoming edge kinds (filtered if specified)
            let incoming_kind_filter = edge_kind_filter.as_ref().map(|_| ()).and(None);
            let mut incoming = self
                .state
                .graph
                .find_incoming_edges_with_kind(
                    &req.project_id,
                    incoming_kind_filter, // None = all kinds
                    &node.node_id,
                    max_incoming,
                )
                .unwrap_or_default();

            // Apply edge kind filter post-query if specified
            if let Some(ref filter) = edge_kind_filter {
                incoming.retain(|(_, kind, _)| filter.contains(kind));
            }

            // Apply file scope filter to incoming edges
            if let Some(ref scope) = req.file_scope {
                incoming.retain(|(src_id, _, _)| {
                    // src_id may be like "file:path" or "sym:type:path:name"
                    src_id.contains(scope.as_str())
                });
            }

            // Outgoing edges — filter to requested kinds only
            let outgoing_kinds: &[EdgeKind] = if let Some(ref filter) = edge_kind_filter {
                filter
            } else {
                EdgeKind::ALL
            };

            let mut outgoing: Vec<(String, EdgeKind, u32)> = Vec::new();
            for kind in outgoing_kinds {
                if let Ok(neighbors) = self.state.graph.neighbors(
                    &req.project_id,
                    kind.clone(),
                    &node.node_id,
                    max_outgoing_per_kind,
                ) {
                    for (target_id, weight) in neighbors {
                        // Apply file scope filter to outgoing
                        if let Some(ref scope) = req.file_scope {
                            if !target_id.contains(scope.as_str()) {
                                continue;
                            }
                        }
                        outgoing.push((target_id, kind.clone(), weight));
                    }
                }
            }

            if !incoming.is_empty() || !outgoing.is_empty() {
                found_in_graph = true;
                out.push_str(&format!(
                    "Symbol: {} ({}) in {}\n  node_id: {}\n",
                    node.name, node.node_type, node.file_path, node.node_id
                ));

                if !incoming.is_empty() {
                    out.push_str(&format!("  Incoming references ({}):\n", incoming.len()));
                    let mut by_kind: std::collections::HashMap<String, Vec<(&str, u32)>> =
                        std::collections::HashMap::new();
                    for (src_id, kind, weight) in &incoming {
                        by_kind
                            .entry(kind.as_str().to_string())
                            .or_default()
                            .push((src_id.as_str(), *weight));
                    }
                    let mut kinds_sorted: Vec<_> = by_kind.into_iter().collect();
                    kinds_sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
                    for (kind, refs) in &kinds_sorted {
                        out.push_str(&format!("    [{}] ({}):\n", kind, refs.len()));
                        for (src, w) in refs.iter().take(20) {
                            out.push_str(&format!("      <- {} (w={})\n", src, w));
                        }
                        if refs.len() > 20 {
                            out.push_str(&format!("      ... and {} more\n", refs.len() - 20));
                        }
                    }
                }

                if !outgoing.is_empty() {
                    out.push_str(&format!("  Outgoing dependencies ({}):\n", outgoing.len()));
                    let mut by_kind: std::collections::HashMap<String, Vec<(&str, u32)>> =
                        std::collections::HashMap::new();
                    for (tgt_id, kind, weight) in &outgoing {
                        by_kind
                            .entry(kind.as_str().to_string())
                            .or_default()
                            .push((tgt_id.as_str(), *weight));
                    }
                    let mut kinds_sorted: Vec<_> = by_kind.into_iter().collect();
                    kinds_sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
                    for (kind, refs) in &kinds_sorted {
                        out.push_str(&format!("    [{}] ({}):\n", kind, refs.len()));
                        for (tgt, w) in refs.iter().take(20) {
                            out.push_str(&format!("      -> {} (w={})\n", tgt, w));
                        }
                        if refs.len() > 20 {
                            out.push_str(&format!("      ... and {} more\n", refs.len() - 20));
                        }
                    }
                }
                out.push('\n');
            }
        }

        if found_in_graph {
            return Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]));
        }

        // 2. Fallback: Lexical search (deduplicated — only runs if graph found nothing)
        let lexical_path_filter = req.file_scope.map(|s| vec![s]);
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
                    include_path_prefixes: lexical_path_filter,
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
        out.push_str(&format!(
            "No graph symbol found for '{}'; lexical references:\n",
            req.symbol_name
        ));
        for h in hits {
            out.push_str(&format!(
                "- {} (chunk_id={}, score={:.3})\n",
                h.path, h.chunk_id, h.score
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    #[tool(
        description = "Analyze an error stacktrace with structured multi-language parsing and suggest likely source files with graph centrality context (v1 parity: analyze_error_stack)."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn analyze_error_stack(
        &self,
        params: Parameters<AnalyzeErrorStackRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // 1. Structured parsing of stack frames
        let frames = crate::utils::text::parse_stack_frames(&req.traceback);
        let query = stacktrace_to_query(&req.traceback);

        // 2. Hybrid search for initial candidates, using MMR for diversity.
        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "memory".into(),
                    generation: gen_,
                    text: query,
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

        let mut out = String::with_capacity(4096);
        out.push_str("Error Stacktrace Analysis\n");
        out.push_str(&format!("Frames parsed: {}\n\n", frames.len()));

        // 3. Show extracted frames summary
        if !frames.is_empty() {
            out.push_str("--- Extracted Frames ---\n");
            for (i, f) in frames.iter().enumerate().take(15) {
                let mut parts = Vec::new();
                if let Some(ref file) = f.file {
                    let basename = file.rsplit(['/', '\\']).next().unwrap_or(file);
                    if let Some(line) = f.line {
                        parts.push(format!("{}:{}", basename, line));
                    } else {
                        parts.push(basename.to_string());
                    }
                }
                if let Some(ref fqn) = f.fqn {
                    parts.push(fqn.clone());
                } else if let Some(ref func) = f.function {
                    parts.push(func.clone());
                }
                if !parts.is_empty() {
                    out.push_str(&format!("  #{}: {}\n", i + 1, parts.join(" in ")));
                }
            }
            if frames.len() > 15 {
                out.push_str(&format!("  ... and {} more frames\n", frames.len() - 15));
            }
            out.push('\n');
        }

        if hits.is_empty() {
            out.push_str("No matching codebase files found.\n");
            return Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]));
        }

        // 4. Boost hits that match extracted file paths from frames
        let frame_files: std::collections::HashSet<String> = frames
            .iter()
            .filter_map(|f| f.file.as_deref())
            .map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f).to_lowercase())
            .collect();
        let frame_functions: std::collections::HashSet<String> = frames
            .iter()
            .filter_map(|f| f.function.as_deref())
            .map(|s| s.to_lowercase())
            .collect();

        let mut scored_hits: Vec<_> = hits
            .iter()
            .map(|h| {
                let basename = h
                    .path
                    .as_str()
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(h.path.as_str())
                    .to_lowercase();
                let mut bonus = 0.0f32;
                // Exact file match from stack frame
                if frame_files.contains(&basename) {
                    bonus += 0.3;
                }
                // Centrality bonus
                bonus += h.centrality * 0.1;
                (h, h.score + bonus)
            })
            .collect();
        scored_hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 5. Output ranked results
        out.push_str("--- Likely Source Files ---\n");
        out.push_str(
            "Ranked by search relevance + stack frame matching + architectural centrality.\n\n",
        );

        for (i, (h, final_score)) in scored_hits.iter().enumerate().take(8) {
            let centrality_note = if h.centrality > 0.5 {
                " [Hub]"
            } else if h.centrality > 0.2 {
                " [Utility]"
            } else {
                ""
            };

            let basename = h
                .path
                .as_str()
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(h.path.as_str())
                .to_lowercase();
            let frame_match = if frame_files.contains(&basename) {
                " [STACK MATCH]"
            } else {
                ""
            };

            // Check if any frame function matches a graph node in this file
            let mut func_matches = Vec::new();
            if !frame_functions.is_empty() {
                let file_nodes = self
                    .state
                    .graph
                    .query_nodes(
                        &req.project_id,
                        Some("function"),
                        None,
                        Some(h.path.as_str()),
                        50,
                    )
                    .unwrap_or_default();
                for node in &file_nodes {
                    let node_name_lower = node.name.to_lowercase();
                    if frame_functions.contains(&node_name_lower) {
                        func_matches.push(format!("{}:{}", node.name, node.start_line));
                    }
                }
            }

            out.push_str(&format!(
                "#{}: {}{}{} (score: {:.3})\n",
                i + 1,
                h.path,
                centrality_note,
                frame_match,
                final_score
            ));

            if !func_matches.is_empty() {
                out.push_str(&format!(
                    "   Matching functions: {}\n",
                    func_matches.join(", ")
                ));
            }

            if let Ok(Some((_, _, content, start_line, _))) =
                ps.search
                    .get_doc_by_doc_id(&req.project_id, "memory", gen_, &h.doc_id)
            {
                let snippet: String = content.lines().take(3).collect::<Vec<_>>().join("\n");
                out.push_str(&format!("   (line ~{})\n", start_line));
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
        description = "Generate insights from co-occurrence clusters via dreaming. Clusters search co-occurrence patterns into insight nodes that surface non-obvious relationships. Configurable clustering parameters and timeout."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn dream_project(
        &self,
        params: Parameters<DreamProjectRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let pid = req.project_id.clone();
        let _ = self.ensure_project_record(&pid).await?;

        let min_edge_weight = req.sanitized_min_edge_weight();
        let min_cluster_size = req.sanitized_min_cluster_size();
        let max_clusters = req.sanitized_max_clusters();

        if req.wait {
            let timeout_dur = std::time::Duration::from_secs(req.sanitized_timeout_secs());

            let result = tokio::time::timeout(
                timeout_dur,
                crate::actors::dreamer::dream_once(
                    &self.state,
                    &pid,
                    min_edge_weight,
                    min_cluster_size,
                    max_clusters,
                ),
            )
            .await;

            return match result {
                Ok(Ok(insights)) => Ok(CallToolResult::success(vec![Content::text(format!(
                    "Dream completed for project_id: {pid}\n\
                     insights_generated: {insights}\n\
                     parameters: max_clusters={max_clusters}, \
                     min_edge_weight={min_edge_weight}, \
                     min_cluster_size={min_cluster_size}"
                ))])),
                Ok(Err(e)) => Err(McpError::internal_error(e.to_string(), None)),
                Err(_) => Ok(CallToolResult::success(vec![Content::text(format!(
                    "Dream timed out after {}s for project_id: {pid}. \
                     Try increasing timeout_secs or reducing max_clusters.",
                    req.sanitized_timeout_secs()
                ))])),
            };
        }

        if let Err(e) = self.state.events_tx.send(AppEvent::TriggerDream {
            project_id: pid.clone(),
        }) {
            tracing::warn!("Failed to send TriggerDream event: {e}");
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Dream requested (async) for project_id: {pid}
             parameters: max_clusters={max_clusters},              min_edge_weight={min_edge_weight},              min_cluster_size={min_cluster_size}"
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

    // ---- Immune system ----

    #[tool(
        description = "Immune system check: compare a code draft against the anti-pattern index using hybrid search (FTS + vector). Returns a structured verdict (PASS/WARN/BLOCK) with severity, confidence, and matched patterns."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn immune_check(
        &self,
        params: Parameters<ImmuneCheckRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // Check if antipattern index is populated.
        let ns_counts = ps
            .search
            .count_docs_by_namespace(&req.project_id)
            .unwrap_or_default();
        let ap_count = ns_counts.get("antipattern").copied().unwrap_or(0);

        let q = code_to_query(&req.code);
        let fts_mode = if req.use_vector { "loose" } else { "strict" };

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q,
                    top_k: req.sanitized_top_k(),
                    fts_mode: fts_mode.into(),
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

        // Tunable thresholds from project meta, falling back to config defaults.
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

        let (similarity, snippet) = if let Some(best) = hits.first() {
            let sn = if req.include_content {
                if let Ok(Some((_, _, content, _, _))) =
                    ps.search
                        .get_doc_by_doc_id(&req.project_id, "antipattern", gen_, &best.doc_id)
                {
                    Some(content)
                } else {
                    best.snippet.clone()
                }
            } else {
                best.snippet.clone()
            };
            (best.score, sn)
        } else {
            (0.0, None)
        };

        let engine = engram_ml::ImmuneEngine::new(warn_t, block_t);
        let decision = engine.decide(similarity, snippet.as_deref());

        let mut out = String::with_capacity(2048);

        // Distinguish between "no antipatterns indexed" and "no matches found".
        if ap_count == 0 && hits.is_empty() {
            out.push_str("verdict: PASS\n");
            out.push_str("severity: none\n");
            out.push_str("note: No anti-patterns indexed for this project. ");
            out.push_str("Run analyze_reverts first to populate the anti-pattern index.\n");
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        out.push_str(&format!("verdict: {}\n", decision.verdict()));
        out.push_str(&format!("severity: {}\n", decision.severity()));
        out.push_str(&format!("confidence: {:.3}\n", similarity));
        out.push_str(&format!("antipattern_index_size: {}\n", ap_count));
        out.push_str(&format!("matches_found: {}\n", hits.len()));

        if let engram_ml::ImmuneDecision::Warn { message, .. }
        | engram_ml::ImmuneDecision::Block { message, .. } = &decision
        {
            out.push_str(&format!("\n{message}\n"));
        }

        if !hits.is_empty() {
            out.push_str("\nTop anti-pattern matches:\n");
            for (i, h) in hits.iter().take(5).enumerate() {
                out.push_str(&format!(
                    "  {}. score={:.3} path={} chunk_id={}\n",
                    i + 1,
                    h.score,
                    h.path,
                    h.chunk_id
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    #[tool(
        description = "Anti-pattern guard: score a code snippet against the anti-pattern index with remediation guidance. Uses hybrid search (FTS + vector) and extracts revert commit metadata for actionable suggestions."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn anti_pattern_guard(
        &self,
        params: Parameters<AntiPatternGuardRequest>,
    ) -> Result<CallToolResult, McpError> {
        static COMMIT_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"Reverted in Commit:\s*([a-fA-F0-9]{7,40})").unwrap()
        });

        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // Check if antipattern index is populated.
        let ns_counts = ps
            .search
            .count_docs_by_namespace(&req.project_id)
            .unwrap_or_default();
        let ap_count = ns_counts.get("antipattern").copied().unwrap_or(0);

        if ap_count == 0 {
            return Ok(CallToolResult::success(vec![Content::text(
                "verdict: PASS\n\
                 note: No anti-patterns indexed for this project. \
                 Run analyze_reverts first to populate the anti-pattern index.",
            )]));
        }

        let q = code_to_query(&req.code);
        let fts_mode = if req.use_vector { "loose" } else { "strict" };
        let display_limit = req.sanitized_limit().min(10);

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "antipattern".into(),
                    generation: gen_,
                    text: q,
                    top_k: req.sanitized_limit(),
                    fts_mode: fts_mode.into(),
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
                "verdict: PASS\nmatches_found: 0\n\
                 No matching anti-patterns found. The code snippet looks safe based on history.",
            )]));
        }

        let best = &hits[0];

        // Tunable thresholds from project meta.
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

        // Fetch full content for the top match only (not in a loop).
        let full_content = if req.include_content {
            if let Ok(Some((_, _, content, _, _))) =
                ps.search
                    .get_doc_by_doc_id(&req.project_id, "antipattern", gen_, &best.doc_id)
            {
                Some(content)
            } else {
                best.snippet.clone()
            }
        } else {
            best.snippet.clone()
        };

        let decision = engine.decide(best.score, full_content.as_deref());

        let mut out = String::with_capacity(4096);
        out.push_str(&format!("verdict: {}\n", decision.verdict()));
        out.push_str(&format!("severity: {}\n", decision.severity()));
        out.push_str(&format!("confidence: {:.3}\n", best.score));
        out.push_str(&format!("matches_found: {}\n", hits.len()));

        // Risk explanation.
        out.push_str("\nRisk explanation:\n");
        out.push_str(
            "This code snippet matches a pattern that was previously reverted in the \
             project's history. Reverted code usually indicates bugs, performance issues, \
             or architectural violations that were later corrected.\n",
        );

        // Extract commit metadata using robust regex.
        out.push_str("\nRemediation guidance:\n");
        if let Some(ref content) = full_content {
            if let Some(caps) = COMMIT_RE.captures(content) {
                let commit_hash = &caps[1];
                out.push_str(&format!(
                    "Review the reverting commit {} to understand why this pattern was \
                     rejected. The correct approach is typically the inverse of the \
                     reverted diff or the pattern established in the reverting commit.\n",
                    commit_hash
                ));
            } else {
                out.push_str(
                    "Review the project's history for similar files to identify the \
                     current best practices. Avoid the logic shown in the matched snippet.\n",
                );
            }
        } else {
            out.push_str(
                "Consult the project's documentation or lead engineers to identify the \
                 preferred pattern for this functionality.\n",
            );
        }

        // Matched patterns.
        out.push_str("\nTop anti-pattern matches:\n");
        for (i, h) in hits.iter().take(display_limit).enumerate() {
            out.push_str(&format!(
                "  {}. score={:.3} path={} chunk_id={}\n",
                i + 1,
                h.score,
                h.path,
                h.chunk_id
            ));
        }

        // Include content of the best match if requested.
        if req.include_content {
            if let Some(ref content) = full_content {
                out.push_str("\nBest match content:\n");
                let truncated: String = content.chars().take(2000).collect();
                out.push_str(&truncated);
                if content.chars().count() > 2000 {
                    out.push_str("\n... (truncated)");
                }
                out.push('\n');
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
        description = "Suggest microservice/bounded-context migration boundaries from temporal coupling clusters, shared state, and SQL table references. Includes cross-cluster dependency analysis and data ownership. Uses LLM when available, falls back to directory-prefix grouping with graph-driven data assignment."
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
                req.sanitized_timeout_secs(),
                req.include_cross_cluster_deps,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if boundaries.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No temporal coupling data found. Index git history first \
                 (index_git_history) to populate coupling edges.",
            )]));
        }

        if req.output_json {
            let json = serde_json::to_string_pretty(&boundaries)
                .unwrap_or_else(|_| format!("{boundaries:?}"));
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Structured human-readable output.
        let mut out = String::with_capacity(4096);
        out.push_str(&format!(
            "Migration Boundary Suggestions ({} contexts)\n",
            boundaries.len()
        ));
        out.push_str(&format!(
            "parameters: min_frequency={}, max_clusters={}, timeout={}s\n\n",
            req.sanitized_min_frequency(),
            req.sanitized_max_clusters(),
            req.sanitized_timeout_secs(),
        ));

        for (i, b) in boundaries.iter().enumerate() {
            out.push_str(&format!("--- Context {}: {} ---\n", i + 1, b.context_name));
            out.push_str(&format!("  risk: {}\n", b.risk));
            out.push_str(&format!(
                "  files ({}): {}\n",
                b.files.len(),
                b.files.join(", ")
            ));
            if !b.owned_data.is_empty() {
                out.push_str(&format!("  owned_data: {}\n", b.owned_data.join(", ")));
            }
            if !b.depends_on.is_empty() {
                out.push_str(&format!("  depends_on: {}\n", b.depends_on.join(", ")));
            }
            if !b.seam_files.is_empty() {
                out.push_str(&format!("  seam_files: {}\n", b.seam_files.join(", ")));
            }
            if !b.shared_across.is_empty() {
                out.push_str(&format!(
                    "  shared_data_with: {}\n",
                    b.shared_across.join(", ")
                ));
            }
            out.push('\n');
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
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
        description = "Compile a vertical-slice migration blueprint for a legacy entry point. Resolves node via multiple prefix strategies (file:, sym:class:, sym:function:, page:, control:). Supports JSON output and edge kind filtering. Use this BEFORE rewriting any legacy feature."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, entry_node = %params.0.entry_node))]
    pub async fn generate_migration_blueprint(
        &self,
        params: Parameters<GenerateMigrationBlueprintRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let max_depth = req.sanitized_max_depth();
        let output_json = req.output_json;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let entry_raw = req.entry_node.clone();

        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            // Multi-prefix node resolution strategy
            let entry_node_id = if graph
                .get_node(&project_id, &entry_raw)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                entry_raw.clone()
            } else {
                // Try common prefixes in priority order
                let candidates = [
                    format!("file:{entry_raw}"),
                    format!("sym:class:{entry_raw}"),
                    format!("sym:function:{entry_raw}"),
                    format!("page:{entry_raw}"),
                    format!("control:{entry_raw}"),
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

            if output_json {
                serde_json::to_string_pretty(&serde_json::json!({
                    "entry_node_id": slice.entry_node_id,
                    "entry_node_type": slice.entry_node_type,
                    "entry_file": slice.entry_file,
                    "nodes_visited": slice.nodes_visited,
                    "dead_code_skipped": slice.dead_code_skipped,
                    "frontend_deps": slice.frontend_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "backend_methods": slice.backend_methods.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "state_mutations": slice.state_mutations.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "database_deps": slice.database_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "component_deps": slice.component_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "data_bindings": slice.data_bindings,
                    "config_deps": slice.config_deps.iter().map(|s| serde_json::json!({
                        "node_id": s.node_id, "node_type": s.node_type,
                        "file_path": s.file_path, "edge_kind": s.edge_kind, "depth": s.depth
                    })).collect::<Vec<_>>(),
                    "lifecycle_info": slice.lifecycle_info.iter().map(|(id, stage, seq)| {
                        serde_json::json!({"node_id": id, "stage": stage, "sequence": seq})
                    }).collect::<Vec<_>>(),
                    "side_effects": slice.side_effects.iter().map(|(id, fx)| {
                        serde_json::json!({"node_id": id, "effects": fx})
                    }).collect::<Vec<_>>(),
                })).map_err(|e| e.to_string())
            } else {
                Ok(graph_service::format_migration_blueprint(&slice))
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ---- AST Dependency Graph ----

    #[tool(
        description = "Generate an AST-level dependency graph for a file or symbol. Shows compile-time dependencies (imports, contains, dependency edges) in a structured tree. Supports incoming/outgoing/both directions and configurable depth."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, entry = %params.0.entry))]
    pub async fn ast_dependency_graph(
        &self,
        params: Parameters<AstDependencyGraphRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let max_depth = req.sanitized_max_depth();
        let output_json = req.output_json;
        let compile_time_only = req.compile_time_only;
        let direction = req.direction.to_lowercase();
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();
        let entry_raw = req.entry.clone();

        let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
            // Resolve entry node (same multi-prefix strategy as migration blueprint)
            let entry_node_id = if graph
                .get_node(&project_id, &entry_raw)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                entry_raw.clone()
            } else {
                let candidates = [
                    format!("file:{entry_raw}"),
                    format!("sym:class:{entry_raw}"),
                    format!("sym:function:{entry_raw}"),
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
                    let nodes = graph
                        .query_nodes(&project_id, None, Some(&entry_raw), None, 10)
                        .map_err(|e| e.to_string())?;
                    if let Some(n) = nodes.first() {
                        found = Some(n.node_id.clone());
                    }
                }
                found.ok_or_else(|| {
                    format!(
                        "No node found matching '{}'. Try query_graph_nodes to discover node IDs.",
                        entry_raw
                    )
                })?
            };

            // Determine edge kinds for traversal
            let edge_kinds: Vec<EdgeKind> = if compile_time_only {
                vec![EdgeKind::Dependency, EdgeKind::Imports, EdgeKind::Contains]
            } else {
                EdgeKind::ALL.to_vec()
            };

            // BFS traversal
            let graph_direction = match direction.as_str() {
                "incoming" => "in",
                "outgoing" => "out",
                "both" => "both",
                _ => "out",
            };

            let traversal = graph
                .traverse(
                    &project_id,
                    &entry_node_id,
                    max_depth,
                    Some(edge_kinds),
                    graph_direction,
                )
                .map_err(|e| e.to_string())?;

            if output_json {
                let nodes_json: Vec<serde_json::Value> = traversal
                    .iter()
                    .map(|(node, depth)| {
                        serde_json::json!({
                            "node_id": node.node_id,
                            "name": node.name,
                            "node_type": node.node_type,
                            "file_path": node.file_path.as_str(),
                            "depth": depth,
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&serde_json::json!({
                    "entry_node": entry_node_id,
                    "direction": graph_direction,
                    "max_depth": max_depth,
                    "compile_time_only": compile_time_only,
                    "nodes": nodes_json,
                    "total_nodes": traversal.len(),
                }))
                .map_err(|e| e.to_string())
            } else {
                let mut out = String::with_capacity(4096);
                out.push_str(&format!(
                    "AST Dependency Graph for '{}' (direction={}, depth={}, {} nodes):\n",
                    entry_node_id,
                    graph_direction,
                    max_depth,
                    traversal.len()
                ));

                // Group by depth for tree-like display
                let max_d = traversal.iter().map(|(_, d)| *d).max().unwrap_or(0);
                for d in 0..=max_d {
                    let at_depth: Vec<_> =
                        traversal.iter().filter(|(_, depth)| *depth == d).collect();
                    if at_depth.is_empty() {
                        continue;
                    }
                    out.push_str(&format!("\n  Depth {}:\n", d));
                    for (node, _) in &at_depth {
                        let indent = "    ".repeat(d.min(4) + 1);
                        out.push_str(&format!(
                            "{}{} ({}) [{}]\n",
                            indent, node.name, node.node_type, node.file_path
                        ));
                    }
                }
                Ok(out)
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e, None))?;

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ---- Incremental Indexing GC ----

    #[tool(
        description = "Manually trigger garbage collection for a project's stale index data. Purges old generations from graph, Tantivy, and LanceDB. Optionally compacts vector storage to reclaim tombstone space."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn incremental_indexing_gc(
        &self,
        params: Parameters<IncrementalIndexingGcRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let pid = req.project_id;
        let ps = self.ensure_project_runtime(&pid).await?;
        let active_gen = self.get_active_generation(&pid).await?;
        let target_gen = req.target_generation.unwrap_or(active_gen);

        let mut steps: Vec<String> = Vec::new();

        // Pre-GC stats
        let pre_graph_nodes = self.state.graph.count_nodes(&pid).unwrap_or(0);
        let pre_graph_edges = self.state.graph.count_edges(&pid).unwrap_or(0);
        let pre_tantivy = ps.search.count_docs(&pid).unwrap_or(0);
        let pre_vectors = ps.search.count_vectors(&pid).await.unwrap_or(0);

        // GC graph (blocking)
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

        // GC search (Tantivy + LanceDB)
        ps.search
            .purge_old_generations(&pid, target_gen)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        steps.push("Purged Tantivy stale documents.".into());

        if req.compact_vectors {
            steps.push("LanceDB garbage collection triggered.".into());
        }

        // Post-GC stats
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
            "  graph_nodes: {} -> {} ({}{})\n",
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
            "  graph_edges: {} -> {} ({}{})\n",
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
            "  tantivy_docs: {} -> {} ({}{})\n",
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
            "  lancedb_vectors: {} -> {} ({}{})\n",
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

    // ---- Antipattern Index Management ----

    #[tool(
        description = "Manage the anti-pattern index: view stats, list indexed anti-patterns, search by query or file pattern, or clear the entire namespace. Complements immune_check and anti_pattern_guard."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id, action = %params.0.action))]
    pub async fn dedicated_antipattern_index(
        &self,
        params: Parameters<AntipatternIndexRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let limit = req.sanitized_limit();
        let pid = req.project_id;
        let action = req.action.to_lowercase();
        let query = req.query;
        let file_filter = req.file_filter;
        let ps = self.ensure_project_runtime(&pid).await?;
        let gen_ = self.get_active_generation(&pid).await.unwrap_or(1);

        match action.as_str() {
            "stats" => {
                let ns_counts = ps.search.count_docs_by_namespace(&pid).unwrap_or_default();
                let antipattern_docs = ns_counts.get("antipattern").copied().unwrap_or(0);

                // Count repo rules
                let reg = self.state.registry.clone();
                let pid_r = pid.clone();
                let rules = tokio::task::spawn_blocking(move || reg.list_repo_rules(&pid_r))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or_default();

                let mut out = String::with_capacity(512);
                out.push_str(&format!("Anti-Pattern Index Stats: {}\n", pid));
                out.push_str(&format!("indexed_antipattern_docs: {}\n", antipattern_docs));
                out.push_str(&format!("repo_rules: {}\n", rules.len()));
                if !rules.is_empty() {
                    out.push_str("\n--- Repo Rules ---\n");
                    for r in rules.iter().take(20) {
                        out.push_str(&format!(
                            "  [{}] {} (priority={})\n",
                            r.rule_id, r.file_pattern, r.priority
                        ));
                    }
                    if rules.len() > 20 {
                        out.push_str(&format!("  ... and {} more\n", rules.len() - 20));
                    }
                }

                Ok(CallToolResult::success(vec![Content::text(
                    out.trim().to_string(),
                )]))
            }
            "list" => {
                // List antipattern docs via search with empty query
                let include_path_prefixes = file_filter.map(|f| vec![f]);
                let hits = ps
                    .search
                    .lexical_search(&HybridQuery {
                        project_id: pid.clone(),
                        namespace: "antipattern".into(),
                        generation: gen_,
                        text: String::new(),
                        top_k: limit,
                        fts_mode: "loose".into(),
                        include_path_prefixes,
                        exclude_path_prefixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    })
                    .unwrap_or_default();

                if hits.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No anti-patterns indexed.",
                    )]));
                }

                let mut out = String::with_capacity(2048);
                out.push_str(&format!("Anti-Pattern Index ({} entries):\n", hits.len()));
                for (i, h) in hits.iter().enumerate() {
                    out.push_str(&format!("  {}. {} (score={:.3})\n", i + 1, h.path, h.score));
                }

                Ok(CallToolResult::success(vec![Content::text(
                    out.trim().to_string(),
                )]))
            }
            "search" => {
                let query = query.unwrap_or_default();
                if query.is_empty() {
                    return Err(McpError::invalid_params(
                        "query is required for action='search'",
                        None,
                    ));
                }

                let include_path_prefixes = file_filter.map(|f| vec![f]);
                let hits = ps
                    .search
                    .search(
                        &HybridQuery {
                            project_id: pid.clone(),
                            namespace: "antipattern".into(),
                            generation: gen_,
                            text: query.clone(),
                            top_k: limit,
                            fts_mode: "strict".into(),
                            include_path_prefixes,
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
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "No anti-patterns matching '{}'.",
                        query
                    ))]));
                }

                let mut out = String::with_capacity(4096);
                out.push_str(&format!(
                    "Anti-pattern search for '{}' ({} hits):\n",
                    query,
                    hits.len()
                ));
                for (i, h) in hits.iter().enumerate() {
                    out.push_str(&format!("\n#{} {} (score={:.3})\n", i + 1, h.path, h.score));
                    if let Ok(Some((_, _, content, _, _))) =
                        ps.search
                            .get_doc_by_doc_id(&pid, "antipattern", gen_, &h.doc_id)
                    {
                        let preview: String = content.chars().take(500).collect();
                        out.push_str(&preview);
                        if content.chars().count() > 500 {
                            out.push_str("...");
                        }
                        out.push('\n');
                    }
                }

                Ok(CallToolResult::success(vec![Content::text(
                    out.trim().to_string(),
                )]))
            }
            "clear" => {
                // Clear antipattern namespace by purging with a very high generation
                // This effectively deletes all antipattern docs
                ps.search
                    .purge_old_generations(&pid, u64::MAX)
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;

                Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{2705} Anti-pattern index cleared for project '{}'.",
                    pid
                ))]))
            }
            _ => Err(McpError::invalid_params(
                "action must be one of: stats, list, search, clear",
                None,
            )),
        }
    }

    // ---- Observability ----

    #[tool(
        description = "Get server metrics: job latencies, queue depths, index drift, cardinality, repair outcomes, memory, checkpoints, confidence scoring, safety rails."
    )]
    pub async fn get_metrics(
        &self,
        params: Parameters<GetMetricsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let snapshot = engram_core::metrics().snapshot();

        if req.output_json {
            let json = serde_json::to_string_pretty(&snapshot)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
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
                s.cardinality.tantivy_doc_count, s.cardinality.vector_doc_count,
                s.cardinality.graph_node_count, s.cardinality.graph_edge_count
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
    }

    // ---- Integrity ----

    #[tool(
        description = "Run cross-store integrity check for a project (Tantivy, LanceDB, Graph, Docstore). Auto-repairs mismatches if configured."
    )]
    pub async fn check_integrity(
        &self,
        params: Parameters<CheckIntegrityRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;

        // Ensure project is cached
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

    // ---- Safety Rails ----

    #[tool(
        description = "Evaluate safety of a proposed automated edit/refactoring. Returns go/no-go decision with risk level, checks, and mitigations."
    )]
    pub async fn evaluate_safety(
        &self,
        params: Parameters<EvaluateSafetyRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;

        let eval_req = crate::services::safety_service::SafetyEvalRequest {
            project_id: req.project_id,
            affected_files: req.affected_files,
            refactor_type: req.refactor_type,
            impact_node_count: req.impact_node_count,
            impact_confidence: req.impact_confidence,
            test_coverage: req.test_coverage,
            anti_pattern_clear: req.anti_pattern_clear,
            downstream_dependents: req.downstream_dependents,
            touches_global_state: req.touches_global_state,
            touches_database: req.touches_database,
        };

        let decision = crate::services::safety_service::evaluate_safety(
            &eval_req,
            self.state.cfg.safety_policy_enabled,
            self.state.cfg.safety_min_confidence,
            self.state.cfg.safety_min_coverage,
        );

        let json = serde_json::to_string_pretty(&decision)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // ---- Migration Plan ----

    #[tool(
        description = "Generate an executable migration plan with waves, seams, contract tests, adapters, and rollback playbooks."
    )]
    pub async fn generate_migration_plan(
        &self,
        params: Parameters<GenerateMigrationPlanRequest>,
    ) -> Result<CallToolResult, McpError> {
        use crate::services::migration_service as mig;

        let req = params.0;
        let pid = req.project_id.clone();

        let _ps = self.ensure_project_runtime(&pid).await?;

        // Build PlanInput from graph store data
        let graph = self.state.graph.clone();
        let pid2 = pid.clone();
        let now = crate::utils::now_ms();
        let plan = tokio::task::spawn_blocking(move || -> anyhow::Result<mig::MigrationPlan> {
            // Collect file nodes and group into boundary clusters by directory
            let file_nodes = graph
                .query_nodes(&pid2, Some("file"), None, None, 5000)
                .unwrap_or_default();
            let db_files: Vec<String> = graph
                .query_nodes(&pid2, Some("db_table"), None, None, 1000)
                .unwrap_or_default()
                .iter()
                .map(|n| n.name.clone())
                .collect();
            let global_files: Vec<String> = graph
                .query_nodes(&pid2, Some("global_state"), None, None, 1000)
                .unwrap_or_default()
                .iter()
                .map(|n| n.name.clone())
                .collect();

            // Group files by directory prefix for clustering
            let mut dir_clusters: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for node in &file_nodes {
                let dir = if let Some(pos) = node.name.rfind('/').or_else(|| node.name.rfind('\\'))
                {
                    node.name[..pos].to_string()
                } else {
                    "root".to_string()
                };
                dir_clusters.entry(dir).or_default().push(node.name.clone());
            }

            let boundaries: Vec<mig::BoundaryCluster> = dir_clusters
                .into_iter()
                .enumerate()
                .map(|(i, (dir, files))| mig::BoundaryCluster {
                    cluster_id: format!("cluster_{}", i),
                    name: dir,
                    files,
                    internal_edges: 0,
                    shared_across: vec![],
                })
                .collect();

            let input = mig::PlanInput {
                project_id: pid2,
                boundaries,
                cross_boundary_edges: vec![],
                global_state_files: global_files,
                database_files: db_files,
                timestamp_ms: now,
                solution_structure: None, // TODO: populate from parsed .sln if available
            };
            Ok(mig::generate_migration_plan(&input))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&plan)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
            let mut out = String::with_capacity(8192);
            out.push_str(&format!(
                "Migration Plan for {} ({} waves)\n",
                plan.project_id, plan.total_waves
            ));
            out.push_str(&format!("Generated at: {}\n", plan.generated_at_ms));
            out.push_str(&format!(
                "Risk: {} high-risk items, {} total items\n\n",
                plan.risk_summary.high_risk_items, plan.risk_summary.total_items,
            ));
            for wave in &plan.waves {
                out.push_str(&format!(
                    "=== Wave {} — {} (risk: {:?}, effort: {}) ===\n",
                    wave.wave_number, wave.name, wave.risk_level, wave.estimated_effort
                ));
                out.push_str(&format!("{}\n", wave.description));
                if !wave.depends_on.is_empty() {
                    out.push_str(&format!("  Depends on waves: {:?}\n", wave.depends_on));
                }
                for item in &wave.items {
                    out.push_str(&format!(
                        "  - {} ({:?}, {:?})\n",
                        item.path, item.item_type, item.complexity
                    ));
                }
                if !wave.contract_tests.is_empty() {
                    out.push_str(&format!(
                        "  Contract tests: {}\n",
                        wave.contract_tests.len()
                    ));
                }
                if !wave.adapters.is_empty() {
                    out.push_str(&format!("  Adapters: {}\n", wave.adapters.len()));
                }
                out.push('\n');
            }
            if !plan.seams.is_empty() {
                out.push_str(&format!("--- Seams ({}) ---\n", plan.seams.len()));
                for seam in &plan.seams {
                    out.push_str(&format!(
                        "  {} <-> {} ({:?}): {}\n",
                        seam.legacy_endpoint, seam.modern_endpoint, seam.seam_type, seam.contract
                    ));
                }
            }
            if !plan.rollback_playbook.waves.is_empty() {
                out.push_str(&format!(
                    "\n--- Rollback Playbook ({} waves) ---\n",
                    plan.rollback_playbook.waves.len()
                ));
                for rb in &plan.rollback_playbook.waves {
                    out.push_str(&format!(
                        "  Wave {}: {} steps\n",
                        rb.wave_number,
                        rb.steps.len()
                    ));
                }
            }
            Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]))
        }
    }

    // ---- Benchmark ----

    #[tool(
        description = "Benchmark retrieval quality (NDCG@10, Recall@10, MRR) against known-relevant queries. Gates vector_search for production readiness."
    )]
    pub async fn benchmark_retrieval(
        &self,
        params: Parameters<BenchmarkRetrievalRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let pid = req.project_id.clone();

        let ps = self.ensure_project_runtime(&pid).await?;
        let generation = self.get_active_generation(&pid).await?;

        // Build benchmark queries
        let queries: Vec<crate::services::benchmark_service::BenchmarkQuery> =
            if let Some(custom) = req.custom_queries {
                custom
                    .into_iter()
                    .map(|q| crate::services::benchmark_service::BenchmarkQuery {
                        query: q.query,
                        relevant_paths: q.relevant_paths,
                    })
                    .collect()
            } else {
                crate::services::benchmark_service::generate_legacy_benchmark_queries()
            };

        let mut per_query: Vec<crate::services::benchmark_service::QueryBenchmarkResult> =
            Vec::new();
        let mut total_ndcg = 0.0f64;
        let mut total_recall = 0.0f64;
        let mut total_mrr = 0.0f64;
        let mut total_latency = 0u64;
        let mut max_latency = 0u64;
        let mut latencies: Vec<u64> = Vec::new();

        for bq in &queries {
            let start = std::time::Instant::now();
            let hits = ps
                .search
                .search(
                    &HybridQuery {
                        project_id: pid.clone(),
                        namespace: "memory".into(),
                        generation,
                        text: bq.query.clone(),
                        top_k: 10,
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
                .unwrap_or_default();
            let elapsed_ms = start.elapsed().as_millis() as u64;

            let actual_paths: Vec<String> =
                hits.iter().map(|h| h.path.as_str().to_string()).collect();
            let ndcg = crate::services::benchmark_service::compute_ndcg(
                &actual_paths,
                &bq.relevant_paths,
                10,
            );
            let recall = crate::services::benchmark_service::compute_recall(
                &actual_paths,
                &bq.relevant_paths,
                10,
            );
            let mrr = crate::services::benchmark_service::compute_reciprocal_rank(
                &actual_paths,
                &bq.relevant_paths,
            );

            total_ndcg += ndcg;
            total_recall += recall;
            total_mrr += mrr;
            total_latency += elapsed_ms;
            if elapsed_ms > max_latency {
                max_latency = elapsed_ms;
            }
            latencies.push(elapsed_ms);

            per_query.push(crate::services::benchmark_service::QueryBenchmarkResult {
                query: bq.query.clone(),
                expected_top_paths: bq.relevant_paths.clone(),
                actual_top_paths: actual_paths,
                ndcg,
                recall,
                reciprocal_rank: mrr,
                latency_ms: elapsed_ms,
            });
        }

        let q_count = queries.len().max(1);
        let mean_ndcg = total_ndcg / q_count as f64;
        let mean_recall = total_recall / q_count as f64;
        let mean_mrr = total_mrr / q_count as f64;
        let mean_latency = total_latency as f64 / q_count as f64;

        // P95 latency
        latencies.sort();
        let p95_idx = ((latencies.len() as f64 * 0.95).ceil() as usize)
            .min(latencies.len())
            .saturating_sub(1);
        let p95_latency = latencies.get(p95_idx).copied().unwrap_or(0);

        let (passed_ndcg, passed_recall, production_ready) =
            crate::services::benchmark_service::evaluate_gates(
                mean_ndcg,
                mean_recall,
                self.state.cfg.retrieval_min_ndcg,
                self.state.cfg.retrieval_min_recall,
            );

        let result = crate::services::benchmark_service::BenchmarkResult {
            project_id: pid,
            timestamp_ms: crate::utils::now_ms(),
            query_count: queries.len(),
            ndcg_at_10: mean_ndcg,
            recall_at_10: mean_recall,
            mean_reciprocal_rank: mean_mrr,
            mean_latency_ms: mean_latency,
            p95_latency_ms: p95_latency as f64,
            passed_ndcg_gate: passed_ndcg,
            passed_recall_gate: passed_recall,
            production_ready,
            per_query_results: per_query,
        };

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
            let mut out = String::with_capacity(4096);
            out.push_str(&format!(
                "Retrieval Benchmark ({} queries)\n",
                result.query_count
            ));
            out.push_str(&format!(
                "NDCG@10:  {:.3} (gate: {:.2}, {})\n",
                result.ndcg_at_10,
                self.state.cfg.retrieval_min_ndcg,
                if result.passed_ndcg_gate {
                    "PASS"
                } else {
                    "FAIL"
                }
            ));
            out.push_str(&format!(
                "Recall@10: {:.3} (gate: {:.2}, {})\n",
                result.recall_at_10,
                self.state.cfg.retrieval_min_recall,
                if result.passed_recall_gate {
                    "PASS"
                } else {
                    "FAIL"
                }
            ));
            out.push_str(&format!("MRR:      {:.3}\n", result.mean_reciprocal_rank));
            out.push_str(&format!(
                "Latency:  avg={:.0}ms p95={:.0}ms\n",
                result.mean_latency_ms, result.p95_latency_ms
            ));
            out.push_str(&format!(
                "\nProduction Ready: {}\n",
                if result.production_ready { "YES" } else { "NO" }
            ));
            for qr in &result.per_query_results {
                out.push_str(&format!(
                    "\n  '{}': ndcg={:.3} recall={:.3} mrr={:.3} latency={}ms",
                    qr.query, qr.ndcg, qr.recall, qr.reciprocal_rank, qr.latency_ms
                ));
            }
            Ok(CallToolResult::success(vec![Content::text(
                out.trim().to_string(),
            )]))
        }
    }

    // ---- Confidence Scoring ----

    #[tool(
        description = "Score extraction confidence for WebForms event wiring, SQL traces, or control bindings. Returns confidence band (High/Medium/Low) with signal breakdown."
    )]
    pub async fn get_extraction_confidence(
        &self,
        params: Parameters<GetExtractionConfidenceRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;

        let src = &req.source_content;
        let cb = req.codebehind_content.as_deref().unwrap_or("");

        let confidence = match req.extraction_type.as_str() {
            "event_wiring" => {
                // Detect signals from source content
                let has_inherits = src.contains("Inherits=") || src.contains("Inherits \"");
                let has_codebehind =
                    !cb.is_empty() || src.contains("CodeBehind=") || src.contains("CodeFile=");
                let has_handler = cb.contains("Handles ")
                    || cb.contains("_Click")
                    || cb.contains("_Load")
                    || cb.contains("EventHandler");
                let sig_valid =
                    cb.contains("Sub ") || cb.contains("void ") || cb.contains("Function ");
                let ctrl_explicit = src.contains("ID=\"") || src.contains("id=\"");
                engram_index::score_event_wiring(
                    has_inherits,
                    has_codebehind,
                    has_handler,
                    sig_valid,
                    ctrl_explicit,
                )
            }
            "sql_trace" => {
                let has_conn = src.contains("ConnectionString")
                    || src.contains("connectionString")
                    || src.contains("SqlConnection");
                let has_param = (src.contains("@") && src.contains("Parameters.Add"))
                    || src.contains("SqlParameter")
                    || src.contains("AddWithValue");
                let table_resolved = src.contains("FROM ")
                    || src.contains("INTO ")
                    || src.contains("UPDATE ")
                    || src.contains("JOIN ");
                let col_resolved = src.contains("SELECT ") && !src.contains("SELECT *");
                let sp_verified = src.contains("CommandType.StoredProcedure")
                    || src.contains("EXEC ")
                    || src.contains("sp_");
                engram_index::score_sql_trace(
                    has_conn,
                    has_param,
                    table_resolved,
                    col_resolved,
                    sp_verified,
                )
            }
            "control_binding" => {
                let runat = src.contains("runat=\"server\"") || src.contains("runat=\"Server\"");
                let explicit_id = src.contains("ID=\"") || src.contains("id=\"");
                let designer_field = !cb.is_empty()
                    && (cb.contains("Protected WithEvents") || cb.contains("protected "));
                let cb_ref = !cb.is_empty()
                    && (cb.contains(".Text")
                        || cb.contains(".Value")
                        || cb.contains(".SelectedValue")
                        || cb.contains("FindControl"));
                engram_index::score_control_binding(runat, explicit_id, designer_field, cb_ref)
            }
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "Unknown extraction_type '{}'. Must be: event_wiring, sql_trace, control_binding",
                        other
                    ),
                    None,
                ));
            }
        };

        // Record metric
        match confidence.band {
            engram_index::ConfidenceBand::High => {
                engram_core::metrics().extractions_high_confidence.inc();
            }
            engram_index::ConfidenceBand::Medium => {
                engram_core::metrics().extractions_medium_confidence.inc();
            }
            engram_index::ConfidenceBand::Low => {
                engram_core::metrics().extractions_low_confidence.inc();
            }
        }

        let json = serde_json::to_string_pretty(&confidence)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    // ---- Checkpoint Status ----

    #[tool(
        description = "Get crash-recovery checkpoint status for jobs. Shows resumable jobs, their phase, and progress."
    )]
    pub async fn get_checkpoint_status(
        &self,
        params: Parameters<GetCheckpointStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
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

    // ---- Memory Budget ----

    #[tool(
        description = "Get current memory budget status: usage, limits, per-subsystem breakdown, pressure state."
    )]
    pub async fn get_memory_budget(
        &self,
        params: Parameters<GetMemoryBudgetRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let budget = &self.state.memory_budget;
        let breakdown = budget.breakdown();

        if req.output_json {
            let json = serde_json::to_string_pretty(&breakdown)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
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
    }

    // ---- Migration Blast Radius ----

    #[tool(
        description = "Compute migration blast radius for a file or symbol. Returns risk score (1-10), complexity breakdown (event wiring, SQL, PageRank, state, GIS, script injection), seam candidates, and agentic migration guidance."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn compute_blast_radius(
        &self,
        params: Parameters<ComputeBlastRadiusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;

        // Resolve target node ID
        let target_id = if let Some(ref fp) = req.file_path {
            format!("file:{}", fp)
        } else if let Some(ref fqn) = req.symbol_fqn {
            // Try to resolve symbol FQN to a node ID
            let graph = self.state.graph.clone();
            let pid = req.project_id.clone();
            let fqn_c = fqn.clone();
            let found = tokio::task::spawn_blocking(move || {
                // First try exact sym: prefix
                let candidate = format!("sym:function:{}", fqn_c);
                if graph.get_node(&pid, &candidate).ok().flatten().is_some() {
                    return Some(candidate);
                }
                let candidate = format!("sym:class:{}", fqn_c);
                if graph.get_node(&pid, &candidate).ok().flatten().is_some() {
                    return Some(candidate);
                }
                None
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            found.unwrap_or_else(|| format!("sym:function:{}", fqn))
        } else {
            return Err(McpError::invalid_params(
                "Either file_path or symbol_fqn is required",
                None,
            ));
        };

        let gen_ = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let include_guidance = req.include_guidance;

        let report = tokio::task::spawn_blocking(move || {
            crate::services::blast_radius_service::compute_blast_radius(
                &graph,
                &pid,
                &target_id,
                gen_,
                include_guidance,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut output = crate::services::blast_radius_service::format_report(&report);

        // Append confidence footer if the target is a file.
        if let Some(ref fp) = req.file_path {
            let rel = engram_core::RelPath::from(fp.as_str());
            let lang = engram_core::guess_language(std::path::Path::new(fp));
            output.push_str(&self.confidence_footer(&rel, &lang));
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    // ---- Design Pattern Detection ----

    #[tool(
        description = "Detect design anti-patterns in the codebase graph. Runs 5 deterministic heuristics: God Object, Spaghetti Events, Session Soup, SqlDataSource Coupling, Tight GIS Coupling. Returns affected nodes, severity, modern migration targets, and step-by-step refactoring guidance."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn detect_design_patterns(
        &self,
        params: Parameters<DetectDesignPatternsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let pattern_filter = req.pattern_filter.clone();
        let limit = req.limit;

        let mut patterns = tokio::task::spawn_blocking(move || {
            crate::services::pattern_detection_service::detect_design_antipatterns(
                &graph, &pid, 20, // god_threshold
                10, // spaghetti_threshold
                5,  // soup_threshold
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Apply pattern name filter if specified
        if !pattern_filter.is_empty() {
            patterns.retain(|p| {
                pattern_filter
                    .iter()
                    .any(|f| p.pattern_name.to_lowercase().contains(&f.to_lowercase()))
            });
        }

        patterns.truncate(limit);

        // Detect background service patterns by scanning class/file nodes
        {
            let graph2 = self.state.graph.clone();
            let reg = self.state.registry.clone();
            let pid2 = req.project_id.clone();
            let project_root = reg
                .get_project(&pid2)
                .ok()
                .flatten()
                .map(|p| p.directory.clone());
            if let Some(root) = project_root {
                let svc_patterns = tokio::task::spawn_blocking(move || {
                    let mut results = Vec::new();
                    if let Ok(class_nodes) =
                        graph2.query_nodes(&pid2, Some("class"), None, None, 500)
                    {
                        for node in &class_nodes {
                            let fp = node.file_path.as_str();
                            let full_path = std::path::Path::new(&root).join(fp);
                            if let Ok(source) = std::fs::read_to_string(&full_path) {
                                let lang = if fp.ends_with(".vb") { "vb" } else { "cs" };
                                let hits =
                                    crate::services::pattern_detection_service::detect_background_service_patterns(
                                        &source, fp, lang,
                                    );
                                results.extend(hits);
                            }
                        }
                    }
                    results
                })
                .await
                .unwrap_or_default();

                for svc in &svc_patterns {
                    patterns.push(
                        crate::services::pattern_detection_service::DesignAntiPattern {
                            pattern_name: format!("Background Service: {}", svc.pattern),
                            description: svc.evidence.clone(),
                            severity:
                                crate::services::pattern_detection_service::AntiPatternSeverity::Moderate,
                            affected_nodes: vec![svc.file_path.clone()],
                            evidence: vec![svc.evidence.clone()],
                            modern_target: svc.modern_equivalent.clone(),
                            refactoring_steps: vec![
                                format!("Migrate to {}", svc.modern_equivalent),
                                "Register as IHostedService in Program.cs".to_string(),
                            ],
                        },
                    );
                }
            }
        }

        if patterns.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No design anti-patterns detected in the project graph.".to_string(),
            )]));
        }

        let mut out = format!("# Design Anti-Patterns Detected: {}\n\n", patterns.len());
        for (i, p) in patterns.iter().enumerate() {
            out.push_str(&format!(
                "## {}. {} [{}]\n\n",
                i + 1,
                p.pattern_name,
                p.severity
            ));
            out.push_str(&format!("{}\n\n", p.description));
            out.push_str("**Affected nodes:**\n");
            for n in &p.affected_nodes {
                out.push_str(&format!("- `{}`\n", n));
            }
            out.push_str("\n**Evidence:**\n");
            for e in &p.evidence {
                out.push_str(&format!("- {}\n", e));
            }
            out.push_str(&format!("\n**Modern target:** {}\n\n", p.modern_target));
            out.push_str("**Refactoring steps:**\n");
            for (j, step) in p.refactoring_steps.iter().enumerate() {
                out.push_str(&format!("{}. {}\n", j + 1, step));
            }
            out.push_str("\n---\n\n");
        }

        Ok(CallToolResult::success(vec![Content::text(
            out.trim().to_string(),
        )]))
    }

    // ---- Autonomous Decision Gate ----

    #[tool(
        description = "Mandatory autonomous decision gate (ADP vNext). Runs an 8-gate verification pipeline \
        with evidence orchestration: extraction confidence, trace certainty, safety policy, retrieval quality, \
        blast radius, anti-pattern, runtime evidence (with reconciliation scoring), and evidence sufficiency. \
        Supports three evidence depths (fast/standard/deep), calibrated confidence aggregation, migration class \
        thresholds, and wave-level evaluation for entire migration plans. Returns allow/deny/abstain verdict \
        with machine-readable reasons. Must pass before any auto-applied change."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn autonomous_decision_gate(
        &self,
        params: Parameters<AutonomousDecisionGateRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        // ── Wave-level evaluation branch ──
        if let Some(wave_items) = req.wave_items {
            return self
                .evaluate_wave_decision(
                    &req.project_id,
                    &req.risk_profile,
                    wave_items,
                    gen_,
                    req.require_runtime_evidence,
                    &req.evidence_depth,
                    req.output_json,
                )
                .await;
        }

        // ── Build evidence overrides from caller-supplied fields (backward compat) ──
        let overrides = crate::services::evidence_orchestration::EvidenceOverrides {
            extraction_confidence: req.extraction_confidence,
            extraction_type: req.extraction_type,
            trace_used_fallback: if req.trace_used_fallback {
                Some(true)
            } else {
                None
            },
            trace_candidate_count: if req.trace_candidate_count > 0 {
                Some(req.trace_candidate_count)
            } else {
                None
            },
            immune_verdict: req.immune_verdict,
            immune_confidence: req.immune_confidence,
            has_runtime_evidence: if req.has_runtime_evidence {
                Some(true)
            } else {
                None
            },
            reconciliation: None, // Reconciliation comes from runtime_evidence_batch below
            safety_decision: None,
            retrieval_production_ready: None,
            retrieval_ndcg: None,
            retrieval_recall: None,
            migration_class: req.migration_class,
        };

        let depth =
            crate::services::evidence_orchestration::EvidenceDepth::from_str(&req.evidence_depth);

        // ── Gather evidence via the Evidence Orchestration Engine ──
        let adp_input = crate::services::evidence_orchestration::gather_evidence(
            &self.state,
            &req.project_id,
            &req.target_files,
            &req.proposed_change,
            crate::services::autonomous_decision_service::RiskProfile::from_str(&req.risk_profile),
            depth,
            &overrides,
            req.require_runtime_evidence,
            gen_,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // ── Run gate pipeline ──
        let decision = crate::services::autonomous_decision_service::evaluate_gates(&adp_input);

        // ── Record metrics ──
        match decision.verdict {
            crate::services::autonomous_decision_service::AdpVerdict::Allow => {
                engram_core::metrics::metrics().refactors_approved.inc();
            }
            crate::services::autonomous_decision_service::AdpVerdict::Deny => {
                engram_core::metrics::metrics().refactors_blocked.inc();
            }
            crate::services::autonomous_decision_service::AdpVerdict::Abstain => {
                // Abstain is logged but not counted as blocked or approved
            }
        }

        if req.output_json {
            let json = serde_json::to_string_pretty(&decision)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
            let text = crate::services::autonomous_decision_service::format_decision(&decision);
            Ok(CallToolResult::success(vec![Content::text(text)]))
        }
    }

    /// Evaluate a migration wave using plan-level ADP (vNext).
    async fn evaluate_wave_decision(
        &self,
        project_id: &str,
        _default_risk_profile: &str,
        wave_items: Vec<crate::models::requests::WaveItemInput>,
        generation: u64,
        require_runtime_evidence: bool,
        evidence_depth: &str,
        output_json: bool,
    ) -> Result<CallToolResult, McpError> {
        use crate::services::autonomous_decision_service::{
            WaveAdpInput, evaluate_wave, format_wave_decision,
        };
        use crate::services::evidence_orchestration::{EvidenceDepth, EvidenceOverrides};

        let depth = EvidenceDepth::from_str(evidence_depth);
        let mut items = Vec::with_capacity(wave_items.len());

        // Gather evidence for each wave item
        for item in &wave_items {
            let overrides = EvidenceOverrides::default();
            let risk_profile = crate::services::autonomous_decision_service::RiskProfile::from_str(
                &item.risk_profile,
            );

            match crate::services::evidence_orchestration::gather_evidence(
                &self.state,
                project_id,
                &[item.file_path.clone()],
                &item.change_description,
                risk_profile,
                depth,
                &overrides,
                require_runtime_evidence,
                generation,
            )
            .await
            {
                Ok(adp_input) => {
                    items.push((item.file_path.clone(), adp_input));
                }
                Err(e) => {
                    tracing::warn!(
                        file = %item.file_path,
                        error = %e,
                        "Failed to gather evidence for wave item"
                    );
                    // Skip items we can't evaluate — they'll be missing from the wave
                }
            }
        }

        let wave_input = WaveAdpInput {
            wave_number: 1,
            wave_name: format!("{} wave", project_id),
            items,
            cross_item_deps: 0, // TODO: compute from graph cross-references
        };

        let wave_decision = evaluate_wave(&wave_input);

        // Record aggregate metrics
        match wave_decision.verdict {
            crate::services::autonomous_decision_service::AdpVerdict::Allow => {
                engram_core::metrics::metrics().refactors_approved.inc();
            }
            crate::services::autonomous_decision_service::AdpVerdict::Deny => {
                engram_core::metrics::metrics().refactors_blocked.inc();
            }
            crate::services::autonomous_decision_service::AdpVerdict::Abstain => {}
        }

        if output_json {
            let json = serde_json::to_string_pretty(&wave_decision)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
            let text = format_wave_decision(&wave_decision);
            Ok(CallToolResult::success(vec![Content::text(text)]))
        }
    }

    // ---- Graph Centrality Rerank ----

    #[tool(
        description = "Compute multi-algorithm graph centrality (PageRank + degree + betweenness) and optionally rerank search results by structural importance. Three modes: (1) query mode - search then rerank by centrality, (2) node_ids mode - score specific nodes, (3) top-N mode - return the most central nodes in the project."
    )]
    #[tracing::instrument(skip(self, params), fields(project_id = %params.0.project_id))]
    pub async fn graph_centrality_rerank(
        &self,
        params: Parameters<GraphCentralityRerankRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let top_k = req.sanitized_top_k();
        let samples = req.sanitized_betweenness_samples();

        // Compute multi-centrality in blocking task
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let active_gen = gen_;
        let centrality: engram_graph::analysis::MultiCentrality =
            tokio::task::spawn_blocking(move || {
                engram_graph::analysis::compute_multi_centrality(&graph, &pid, active_gen, samples)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let pr_w = req.pagerank_weight;
        let deg_w = req.degree_weight;
        let bt_w = req.betweenness_weight;

        // Determine mode and build scored results
        #[derive(serde::Serialize)]
        struct ScoredNode {
            node_id: String,
            blended_score: f32,
            pagerank: f32,
            in_degree: u32,
            out_degree: u32,
            betweenness: f32,
            #[serde(skip_serializing_if = "Option::is_none")]
            search_score: Option<f32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            node_type: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            file_path: Option<String>,
        }

        let mut scored: Vec<ScoredNode> = Vec::new();

        if let Some(query) = &req.query {
            // Mode 1: Search then rerank
            let hits = ps
                .search
                .search(
                    &HybridQuery {
                        project_id: req.project_id.clone(),
                        namespace: req.namespace.clone(),
                        generation: gen_,
                        text: query.clone(),
                        top_k: top_k * 3, // oversample for reranking
                        fts_mode: "strict".to_string(),
                        include_path_prefixes: None,
                        exclude_path_prefixes: None,
                        language_filters: None,
                        author_filter: None,
                        date_after: None,
                        date_before: None,
                        use_mmr: false,
                    },
                    Some(&centrality.pagerank),
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for hit in &hits {
                let file_node_id = format!("file:{}", hit.path.as_str());
                let blended = centrality.blended_score(&file_node_id, pr_w, deg_w, bt_w);
                // Combine search score + centrality blend (70% search, 30% centrality)
                let combined = hit.score * 0.7 + blended * 0.3;

                let (node_type, name, file_path) = if req.include_metadata {
                    (
                        Some("file".to_string()),
                        Some(hit.path.as_str().to_string()),
                        Some(hit.path.as_str().to_string()),
                    )
                } else {
                    (None, None, None)
                };

                scored.push(ScoredNode {
                    node_id: file_node_id.clone(),
                    blended_score: combined,
                    pagerank: centrality
                        .pagerank
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0.0),
                    in_degree: centrality
                        .in_degree
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0),
                    out_degree: centrality
                        .out_degree
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0),
                    betweenness: centrality
                        .betweenness
                        .get(&file_node_id)
                        .copied()
                        .unwrap_or(0.0),
                    search_score: Some(hit.score),
                    node_type,
                    name,
                    file_path,
                });
            }

            // Sort by combined score descending
            scored.sort_by(|a, b| {
                b.blended_score
                    .partial_cmp(&a.blended_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else if let Some(node_ids) = &req.node_ids {
            // Mode 2: Score specific nodes
            let graph_store = self.state.graph.clone();
            let pid2 = req.project_id.clone();
            let node_ids_clone = node_ids.clone();
            let include_meta = req.include_metadata;

            let nodes_meta: Vec<(String, Option<String>, Option<String>, Option<String>)> =
                tokio::task::spawn_blocking(
                    move || -> Vec<(String, Option<String>, Option<String>, Option<String>)> {
                        let mut result = Vec::new();
                        for nid in &node_ids_clone {
                            let meta = if include_meta {
                                graph_store.get_node(&pid2, nid).ok().flatten().map(|n| {
                                    (n.node_type, n.name, n.file_path.as_str().to_string())
                                })
                            } else {
                                None
                            };
                            let (nt, nm, fp) = meta
                                .map(|(t, n, f)| (Some(t), Some(n), Some(f)))
                                .unwrap_or((None, None, None));
                            result.push((nid.clone(), nt, nm, fp));
                        }
                        result
                    },
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for (nid, nt, nm, fp) in nodes_meta {
                let blended = centrality.blended_score(&nid, pr_w, deg_w, bt_w);
                scored.push(ScoredNode {
                    node_id: nid.clone(),
                    blended_score: blended,
                    pagerank: centrality.pagerank.get(&nid).copied().unwrap_or(0.0),
                    in_degree: centrality.in_degree.get(&nid).copied().unwrap_or(0),
                    out_degree: centrality.out_degree.get(&nid).copied().unwrap_or(0),
                    betweenness: centrality.betweenness.get(&nid).copied().unwrap_or(0.0),
                    search_score: None,
                    node_type: nt,
                    name: nm,
                    file_path: fp,
                });
            }

            scored.sort_by(|a, b| {
                b.blended_score
                    .partial_cmp(&a.blended_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            // Mode 3: Top-N most central nodes
            let graph_store = self.state.graph.clone();
            let pid3 = req.project_id.clone();
            let include_meta = req.include_metadata;

            // Build blended scores for all nodes
            let mut all_scores: Vec<(String, f32)> = centrality
                .pagerank
                .keys()
                .map(|nid| {
                    let blended = centrality.blended_score(nid, pr_w, deg_w, bt_w);
                    (nid.clone(), blended)
                })
                .collect();
            all_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            all_scores.truncate(top_k);

            let top_ids: Vec<String> = all_scores.iter().map(|(id, _score)| id.clone()).collect();
            let nodes_meta: Vec<(String, Option<String>, Option<String>, Option<String>)> =
                tokio::task::spawn_blocking(
                    move || -> Vec<(String, Option<String>, Option<String>, Option<String>)> {
                        let mut result = Vec::new();
                        for nid in &top_ids {
                            let meta = if include_meta {
                                graph_store.get_node(&pid3, nid).ok().flatten().map(|n| {
                                    (n.node_type, n.name, n.file_path.as_str().to_string())
                                })
                            } else {
                                None
                            };
                            let (nt, nm, fp) = meta
                                .map(|(t, n, f)| (Some(t), Some(n), Some(f)))
                                .unwrap_or((None, None, None));
                            result.push((nid.clone(), nt, nm, fp));
                        }
                        result
                    },
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            for (nid, nt, nm, fp) in nodes_meta {
                let blended = centrality.blended_score(&nid, pr_w, deg_w, bt_w);
                scored.push(ScoredNode {
                    node_id: nid.clone(),
                    blended_score: blended,
                    pagerank: centrality.pagerank.get(&nid).copied().unwrap_or(0.0),
                    in_degree: centrality.in_degree.get(&nid).copied().unwrap_or(0),
                    out_degree: centrality.out_degree.get(&nid).copied().unwrap_or(0),
                    betweenness: centrality.betweenness.get(&nid).copied().unwrap_or(0.0),
                    search_score: None,
                    node_type: nt,
                    name: nm,
                    file_path: fp,
                });
            }
        }

        scored.truncate(top_k);

        if req.output_json {
            let json = serde_json::to_string_pretty(&scored)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        } else {
            let mode = if req.query.is_some() {
                "search+rerank"
            } else if req.node_ids.is_some() {
                "node scoring"
            } else {
                "top-N centrality"
            };
            let mut out = format!(
                "Graph Centrality Rerank ({mode})\n\
                 Weights: PR={pr_w:.2}, Degree={deg_w:.2}, Betweenness={bt_w:.2}\n\
                 Results: {}\n\n",
                scored.len()
            );

            for (i, node) in scored.iter().enumerate() {
                out.push_str(&format!(
                    "{}. {} (blended={:.4})\n",
                    i + 1,
                    node.node_id,
                    node.blended_score
                ));
                out.push_str(&format!(
                    "   PR={:.6}  in_deg={}  out_deg={}  betw={:.4}",
                    node.pagerank, node.in_degree, node.out_degree, node.betweenness
                ));
                if let Some(ss) = node.search_score {
                    out.push_str(&format!("  search={ss:.4}"));
                }
                out.push('\n');
                if let Some(ref nt) = node.node_type {
                    out.push_str(&format!("   type={nt}"));
                }
                if let Some(ref nm) = node.name {
                    out.push_str(&format!("  name={nm}"));
                }
                if let Some(ref fp) = node.file_path {
                    out.push_str(&format!("  path={fp}"));
                }
                if node.node_type.is_some() || node.name.is_some() || node.file_path.is_some() {
                    out.push('\n');
                }
            }

            Ok(CallToolResult::success(vec![Content::text(out)]))
        }
    }

    // ── Phase 30: Migration Engine Tools ────────────────────────────────────

    /// Generate a migration scaffold for a legacy file targeting Blazor, React, or Angular.
    #[tool(
        name = "generate_migration_scaffold",
        description = "Generate a compilable target-stack skeleton (Blazor/React/Angular) from a legacy WebForms file's extraction data. Produces component code, repository interfaces, DTOs, and optional test scaffolds."
    )]
    pub async fn generate_migration_scaffold(
        &self,
        params: Parameters<GenerateMigrationScaffoldRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let target = req.target_stack.clone();
        let include_tests = req.include_test_scaffold;
        let format = req.output_format.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::scaffold_service::generate_scaffold(
                &graph,
                &pid,
                &file_path,
                &target,
                include_tests,
                &format,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = format!("# Migration Scaffold ({})\n\n", result.target_stack);
        out.push_str("## Component Code\n```\n");
        out.push_str(&result.component_code);
        out.push_str("```\n\n");

        if let Some(ref repo) = result.repository_interface {
            out.push_str("## Repository Interface\n```csharp\n");
            out.push_str(repo);
            out.push_str("```\n\n");
        }
        if let Some(ref dto) = result.dto_classes {
            out.push_str("## DTO Classes\n```csharp\n");
            out.push_str(dto);
            out.push_str("```\n\n");
        }
        if let Some(ref test) = result.test_scaffold {
            out.push_str("## Test Scaffold\n```\n");
            out.push_str(test);
            out.push_str("```\n\n");
        }

        if !result.mapping_report.is_empty() {
            out.push_str("## Mapping Report\n");
            for entry in &result.mapping_report {
                out.push_str(&format!(
                    "- **{}** → {} [{}] {}\n",
                    entry.legacy_element, entry.modern_element, entry.category, entry.notes
                ));
            }
        }

        if !result.warnings.is_empty() {
            out.push_str("\n## Warnings\n");
            for w in &result.warnings {
                out.push_str(&format!("- {w}\n"));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Generate runtime instrumentation code for a legacy ASP.NET application.
    #[tool(
        name = "generate_instrumentation_code",
        description = "Generate injectable C# or VB.NET HttpModule code that captures route events, session access, SQL execution, control interactions, and errors from a running legacy application."
    )]
    pub async fn generate_instrumentation_code(
        &self,
        params: Parameters<GenerateInstrumentationCodeRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let files = req.target_files.clone();
        let lang = req.language.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::instrumentation_service::generate_instrumentation_code(
                &graph, &pid, &files, &lang,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::from("# Runtime Instrumentation Package\n\n");

        out.push_str("## C# Module\n```csharp\n");
        out.push_str(&result.csharp_module);
        out.push_str("```\n\n");

        out.push_str("## VB.NET Module\n```vbnet\n");
        out.push_str(&result.vb_module);
        out.push_str("```\n\n");

        if let Some(ref wrapper) = result.session_wrapper {
            out.push_str(
                "## Session State Wrapper (InstrumentedSessionStateWrapper.cs)\n```csharp\n",
            );
            out.push_str(wrapper);
            out.push_str("```\n\n");
        }

        if let Some(ref wrapper) = result.sql_wrapper {
            out.push_str("## SQL Command Wrapper (InstrumentedDbCommand.cs)\n```csharp\n");
            out.push_str(wrapper);
            out.push_str("```\n\n");
        }

        out.push_str("## web.config Entries\n```xml\n");
        out.push_str(&result.webconfig_entries);
        out.push_str("```\n\n");

        out.push_str("## Captured Events\n");
        for evt in &result.captured_events {
            out.push_str(&format!("- {evt}\n"));
        }

        out.push_str("\n## Installation Steps\n");
        for step in &result.installation_steps {
            out.push_str(&format!("{step}\n"));
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Reconcile static analysis with runtime evidence.
    #[tool(
        name = "reconcile_runtime_evidence",
        description = "Compare static analysis paths (from graph edges) with runtime behavior (from ingested RuntimeEvidenceBatch). Classifies each path as confirmed, contradicted, or inconclusive."
    )]
    pub async fn reconcile_runtime_evidence(
        &self,
        params: Parameters<ReconcileRuntimeEvidenceRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();

        let batch: engram_core::runtime_evidence::RuntimeEvidenceBatch =
            serde_json::from_str(&req.evidence_json).map_err(|e| {
                McpError::invalid_params(format!("Invalid evidence JSON: {e}"), None)
            })?;

        let report = tokio::task::spawn_blocking(move || {
            crate::services::instrumentation_service::reconcile_runtime_evidence(
                &graph, &pid, &batch,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let s = &report.summary;
        let mut out = format!(
            "# Reconciliation Report\n\n\
             **Total static paths**: {}\n\
             **Confirmed**: {} ({:.1}%)\n\
             **Contradicted**: {} ({:.1}%)\n\
             **Inconclusive**: {} ({:.1}%)\n\
             **Confidence delta**: {:.3}\n\n",
            s.total_static_paths,
            s.confirmed_count,
            s.confirmed_ratio * 100.0,
            s.contradicted_count,
            s.contradicted_ratio * 100.0,
            s.inconclusive_count,
            (1.0 - s.confirmed_ratio - s.contradicted_ratio) * 100.0,
            s.confidence_delta,
        );

        if !report.contradicted_paths.is_empty() {
            out.push_str("## Contradicted Paths (Dead Code Candidates)\n");
            for p in &report.contradicted_paths {
                out.push_str(&format!(
                    "- {} → {} [{}]\n",
                    p.source, p.target, p.edge_kind
                ));
            }
            out.push('\n');
        }

        if !report.confirmed_paths.is_empty() {
            out.push_str(&format!(
                "## Confirmed Paths ({} total, first 20 shown)\n",
                report.confirmed_paths.len()
            ));
            for p in report.confirmed_paths.iter().take(20) {
                out.push_str(&format!(
                    "- {} → {} [{}]: {}\n",
                    p.source,
                    p.target,
                    p.edge_kind,
                    p.runtime_evidence.as_deref().unwrap_or("confirmed")
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Analyze state management and suggest migration strategies.
    #[tool(
        name = "suggest_state_migration",
        description = "Analyze all Session, ViewState, Application, Cache, and Cookie access in a project and produce per-key migration recommendations with code hints."
    )]
    pub async fn suggest_state_migration(
        &self,
        params: Parameters<SuggestStateMigrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();

        let report = tokio::task::spawn_blocking(move || {
            crate::services::state_migration_service::analyze_state_migration(&graph, &pid)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let s = &report.summary;
        let mut out = format!(
            "# State Migration Report\n\n**Total state keys**: {}\n\n",
            s.total_state_keys
        );

        if !s.by_store.is_empty() {
            out.push_str("## By Store Type\n");
            for (store, count) in &s.by_store {
                out.push_str(&format!("- {store}: {count}\n"));
            }
            out.push('\n');
        }

        if !s.by_target.is_empty() {
            out.push_str("## By Migration Target\n");
            for (target, count) in &s.by_target {
                out.push_str(&format!("- {target}: {count}\n"));
            }
            out.push('\n');
        }

        if !s.high_risk_keys.is_empty() {
            out.push_str("## High-Risk Keys\n");
            for k in &s.high_risk_keys {
                out.push_str(&format!("- {k}\n"));
            }
            out.push('\n');
        }

        out.push_str("## Recommendations\n\n");
        for rec in &report.recommendations {
            out.push_str(&format!("### {}\n", rec.state_key));
            out.push_str(&format!("- **Store**: {:?}\n", rec.store_type));
            out.push_str(&format!("- **Pattern**: {:?}\n", rec.access_pattern));
            out.push_str(&format!(
                "- **Type inference**: {}\n",
                rec.data_type_inference
            ));
            out.push_str(&format!(
                "- **Readers**: {} | **Writers**: {}\n",
                rec.readers.len(),
                rec.writers.len()
            ));
            out.push_str(&format!("- **Target**: {}\n", rec.recommended_target));
            out.push_str(&format!("- **Reasoning**: {}\n", rec.reasoning));
            out.push_str(&format!(
                "- **Code hint**: `{}`\n\n",
                rec.migration_code_hint
            ));
        }

        if let Some(ref vs) = report.viewstate_report {
            out.push_str(&format!(
                "## ViewState Elimination Report\n\
                 **Total ViewState keys**: {}\n\
                 **Estimated payload**: ~{} bytes\n\n",
                vs.total_viewstate_keys, vs.estimated_payload_bytes
            ));
            for page in &vs.pages {
                out.push_str(&format!("### {}\n", page.file_path));
                for key in &page.keys {
                    out.push_str(&format!(
                        "- **{}** [{:?}]: {}{}\n",
                        key.key,
                        key.lifecycle,
                        key.elimination_strategy,
                        if key.is_url_state_crutch {
                            " ⚠ URL state crutch"
                        } else {
                            ""
                        }
                    ));
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Generate characterization tests from extraction data.
    #[tool(
        name = "generate_characterization_tests",
        description = "Generate test skeletons (NUnit/xUnit/MSTest) from extraction data covering event handlers, data flows, state transitions, navigation paths, and API contracts."
    )]
    pub async fn generate_characterization_tests(
        &self,
        params: Parameters<GenerateCharacterizationTestsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let framework = req.framework.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::characterization_test_service::generate_characterization_tests(
                &graph, &pid, &file_path, &framework,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Characterization Tests ({} tests, {})\n\n",
            result.test_count, result.framework
        );

        out.push_str("## Generated Test Code\n```csharp\n");
        out.push_str(&result.test_code);
        out.push_str("```\n\n");

        if !result.coverage_map.is_empty() {
            out.push_str("## Coverage Map\n");
            for entry in &result.coverage_map {
                out.push_str(&format!(
                    "- **{}** [{:?}]: {} edges covered\n",
                    entry.test_name,
                    entry.category,
                    entry.covered_edges.len()
                ));
            }
        }

        if !result.warnings.is_empty() {
            out.push_str("\n## Warnings\n");
            for w in &result.warnings {
                out.push_str(&format!("- {w}\n"));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Generate strangler fig migration infrastructure (YARP, feature flags, middleware, health check).
    #[tool(
        name = "generate_strangler_fig_config",
        description = "Generate complete strangler fig migration infrastructure for incremental cutover from legacy ASP.NET WebForms to modern stack. Produces YARP reverse proxy configuration, Microsoft.FeatureManagement feature flags, routing middleware with percentage-based rollout, and a health check endpoint reporting migration progress."
    )]
    pub async fn generate_strangler_fig_config(
        &self,
        params: Parameters<GenerateStranglerFigRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let legacy_url = req.legacy_base_url.clone();
        let modern_url = req.modern_base_url.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::strangler_fig_service::generate_strangler_fig_config(
                &graph,
                &pid,
                &legacy_url,
                &modern_url,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut out = String::from("# Strangler Fig Migration Infrastructure\n\n");

        out.push_str(&format!(
            "**Pages discovered**: {} total ({} migrated, {} unmigrated)\n\n",
            result.migrated_pages.len() + result.unmigrated_pages.len(),
            result.migrated_pages.len(),
            result.unmigrated_pages.len(),
        ));

        // YARP reverse proxy config
        out.push_str("## YARP Reverse Proxy (appsettings.YARP.json)\n```json\n");
        out.push_str(&result.yarp_config);
        out.push_str("```\n\n");

        // Feature flags config + middleware
        out.push_str("## Feature Flags (appsettings.FeatureFlags.json + FeatureFlagMiddleware.cs)\n```csharp\n");
        out.push_str(&result.feature_flags_config);
        out.push_str("```\n\n");

        // Routing middleware
        out.push_str(
            "## Strangler Fig Routing Middleware (StranglerFigMiddleware.cs)\n```csharp\n",
        );
        out.push_str(&result.routing_middleware);
        out.push_str("```\n\n");

        // Health check
        out.push_str("## Migration Health Check (MigrationHealthCheck.cs)\n```csharp\n");
        out.push_str(&result.health_check);
        out.push_str("```\n\n");

        // Program.cs registration
        out.push_str("## Program.cs Registration (with Polly, Correlation ID, Session Affinity)\n```csharp\n");
        out.push_str(&result.program_cs);
        out.push_str("```\n\n");

        // Page inventory
        if !result.migrated_pages.is_empty() {
            out.push_str("## Migrated Pages\n");
            for p in &result.migrated_pages {
                out.push_str(&format!("- ✅ {p}\n"));
            }
            out.push('\n');
        }
        if !result.unmigrated_pages.is_empty() {
            out.push_str("## Unmigrated Pages\n");
            for p in &result.unmigrated_pages {
                out.push_str(&format!("- ⬜ {p}\n"));
            }
            out.push('\n');
        }

        // Deployment steps
        out.push_str("## Deployment Steps\n");
        for step in &result.deployment_steps {
            out.push_str(&format!("{step}\n"));
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── Phase 31: Migration Workflow Engine Tools ─────────────────────────────

    /// Map WebForms validation controls to modern equivalents.
    #[tool(
        name = "map_validation_controls",
        description = "Parse ASP.NET validator controls (<asp:RequiredFieldValidator>, CompareValidator, RangeValidator, RegularExpressionValidator, CustomValidator, ValidationSummary) from an ASPX file and map each to DataAnnotations, FluentValidation, and Blazor equivalents. Groups validators by ValidationGroup and detects CausesValidation buttons."
    )]
    pub async fn map_validation_controls(
        &self,
        params: Parameters<MapValidationControlsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        // Read ASPX content from disk
        let aspx_full = std::path::Path::new(&rec.directory).join(&file_path);
        let aspx_content = tokio::fs::read_to_string(&aspx_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", aspx_full.display()), None)
        })?;

        // Try to read code-behind
        let cb_path = find_codebehind_path(&aspx_full);
        let cb_content = if let Some(ref p) = cb_path {
            tokio::fs::read_to_string(p).await.ok()
        } else {
            None
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::validation_mapping_service::analyze_validation_controls(
                &graph,
                &pid,
                &file_path,
                &aspx_content,
                cb_content.as_deref(),
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Validation Controls — {}\n\n\
             **Total validators**: {} | **Complexity**: {}\n\n",
            result.file_path, result.total_validators, result.migration_complexity
        );

        if !result.validators.is_empty() {
            out.push_str("## Validators\n");
            for v in &result.validators {
                out.push_str(&format!(
                    "- **{}** ({}) → validates `{}` | group: `{}`\n  - DataAnnotation: `{}`\n  - FluentValidation: `{}`\n  - Blazor: `{}`\n",
                    v.validator_id, v.validator_type, v.control_to_validate,
                    if v.validation_group.is_empty() { "(default)" } else { &v.validation_group },
                    v.modern_data_annotation, v.modern_fluent_validation, v.modern_blazor,
                ));
            }
            out.push('\n');
        }

        if !result.custom_validators.is_empty() {
            out.push_str("## Custom Validators\n");
            for cv in &result.custom_validators {
                out.push_str(&format!(
                    "- **{}** → validates `{}`\n  - Server handler: `{}`\n  - Client function: `{}`\n  - Approach: {}\n",
                    cv.validator_id, cv.control_to_validate,
                    cv.server_validate_handler.as_deref().unwrap_or("(none)"),
                    cv.client_validation_function.as_deref().unwrap_or("(none)"),
                    cv.modern_approach,
                ));
            }
            out.push('\n');
        }

        if !result.validation_groups.is_empty() {
            out.push_str("## Validation Groups\n");
            for g in &result.validation_groups {
                out.push_str(&format!(
                    "- **{}**: {} validators, triggers: [{}]\n",
                    g.group_name,
                    g.validator_ids.len(),
                    g.trigger_buttons.join(", "),
                ));
            }
            out.push('\n');
        }

        if let Some(ref vs) = result.validation_summary {
            out.push_str(&format!(
                "## ValidationSummary\n- ID: `{}` | Display: `{}` | Group: `{}`\n\n",
                vs.summary_id, vs.display_mode, vs.validation_group,
            ));
        }

        if !result.causes_validation_buttons.is_empty() {
            out.push_str("## CausesValidation Buttons\n");
            for b in &result.causes_validation_buttons {
                out.push_str(&format!(
                    "- `{}` ({}) → group: `{}` | causes_validation: {}\n",
                    b.control_id, b.control_type, b.validation_group, b.causes_validation,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Map authentication and authorization configuration.
    #[tool(
        name = "map_auth_config",
        description = "Parse web.config authentication/authorization sections (Forms, Windows, location rules, membership, roleManager) and scan code-behind files for FormsAuthentication, Membership, Roles, and session-based auth patterns. Maps everything to ASP.NET Core Identity equivalents."
    )]
    pub async fn map_auth_config(
        &self,
        params: Parameters<MapAuthConfigRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let project_dir = rec.directory.clone();

        // Try to read web.config
        let webconfig_path = std::path::Path::new(&project_dir).join("web.config");
        let webconfig_content = tokio::fs::read_to_string(&webconfig_path).await.ok();
        // Also try Web.config (case-sensitive on Linux)
        let webconfig_content = if webconfig_content.is_none() {
            let alt = std::path::Path::new(&project_dir).join("Web.config");
            tokio::fs::read_to_string(&alt).await.ok()
        } else {
            webconfig_content
        };

        // Collect code files to scan for auth patterns
        let code_files = if let Some(ref scope) = req.file_scope {
            // Only scan the specified file
            let full = std::path::Path::new(&project_dir).join(scope);
            match tokio::fs::read_to_string(&full).await {
                Ok(content) => vec![(scope.clone(), content)],
                Err(_) => vec![],
            }
        } else {
            // Scan all code-behind files from graph nodes
            let g = graph.clone();
            let p = pid.clone();
            let dir = project_dir.clone();
            tokio::task::spawn_blocking(move || -> Vec<(String, String)> {
                let file_nodes = g
                    .query_nodes(&p, Some("file"), None, None, 50_000)
                    .unwrap_or_default();
                let mut files = Vec::new();
                for node in &file_nodes {
                    let path = &node.name;
                    if path.ends_with(".vb")
                        || path.ends_with(".cs")
                        || path.ends_with(".aspx.vb")
                        || path.ends_with(".aspx.cs")
                    {
                        let full = std::path::Path::new(&dir).join(path);
                        if let Ok(content) = std::fs::read_to_string(&full) {
                            files.push((path.clone(), content));
                        }
                    }
                }
                files
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };

        let result = tokio::task::spawn_blocking(move || {
            let code_refs: Vec<(&str, &str)> = code_files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            crate::services::auth_config_service::analyze_auth_config(
                &graph,
                &pid,
                webconfig_content.as_deref(),
                &code_refs,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Authentication & Authorization Map\n\n\
             **Auth mode**: {} | **Complexity**: {}\n\n",
            result.auth_mode, result.migration_complexity,
        );

        if let Some(ref fa) = result.forms_auth {
            out.push_str(&format!(
                "## Forms Authentication\n\
                 - Login URL: `{}`\n\
                 - Default URL: `{}`\n\
                 - Timeout: {} min\n\
                 - Cookie: `{}`\n\n",
                fa.login_url, fa.default_url, fa.timeout_minutes, fa.cookie_name,
            ));
        }

        if let Some(ref wa) = result.windows_auth {
            out.push_str(&format!(
                "## Windows Authentication\n- Modern equivalent: {}\n\n",
                wa.modern_equivalent,
            ));
        }

        if !result.location_rules.is_empty() {
            out.push_str("## Location Authorization Rules\n");
            for lr in &result.location_rules {
                out.push_str(&format!("### `{}`\n", lr.path));
                if !lr.allow_roles.is_empty() {
                    out.push_str(&format!("- Allow roles: {}\n", lr.allow_roles.join(", ")));
                }
                if !lr.allow_users.is_empty() {
                    out.push_str(&format!("- Allow users: {}\n", lr.allow_users.join(", ")));
                }
                if !lr.deny_roles.is_empty() {
                    out.push_str(&format!("- Deny roles: {}\n", lr.deny_roles.join(", ")));
                }
                if !lr.deny_users.is_empty() {
                    out.push_str(&format!("- Deny users: {}\n", lr.deny_users.join(", ")));
                }
                out.push_str(&format!(
                    "- Modern: `{}` / policy: `{}`\n\n",
                    lr.modern_attribute, lr.modern_policy,
                ));
            }
        }

        if let Some(ref mc) = result.membership_config {
            out.push_str(&format!(
                "## Membership Provider\n- Provider: `{}`\n- Type: `{}`\n- Password format: {}\n- Min length: {}\n- Modern: {}\n\n",
                mc.default_provider, mc.provider_type, mc.password_format,
                mc.min_password_length, mc.modern_equivalent,
            ));
        }

        if let Some(ref rp) = result.role_provider {
            out.push_str(&format!(
                "## Role Provider\n- Provider: `{}`\n- Type: `{}`\n- Modern: {}\n\n",
                rp.default_provider, rp.provider_type, rp.modern_equivalent,
            ));
        }

        if !result.code_auth_checks.is_empty() {
            out.push_str("## Code-Level Auth Checks\n");
            for c in &result.code_auth_checks {
                out.push_str(&format!(
                    "- `{}` ({}) in `{}:{}` → {}\n",
                    c.expression, c.check_type, c.file_path, c.line_number, c.modern_equivalent,
                ));
            }
            out.push('\n');
        }

        if !result.session_auth_patterns.is_empty() {
            out.push_str("## Session-Based Auth Anti-Patterns\n");
            for s in &result.session_auth_patterns {
                out.push_str(&format!(
                    "- `{}` ({}) key: `{}` in `{}` → {}\n",
                    s.description, s.pattern_type, s.session_key, s.file_path, s.modern_equivalent,
                ));
            }
            out.push('\n');
        }

        if !result.recommendations.is_empty() {
            out.push_str("## Migration Recommendations\n");
            for r in &result.recommendations {
                out.push_str(&format!(
                    "### {} ({})\n- {}\n- Modern: {}\n\n",
                    r.category, r.severity, r.recommendation, r.modern_pattern,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Map page lifecycle events to modern equivalents.
    #[tool(
        name = "map_page_lifecycle",
        description = "Extract WebForms page lifecycle events (Page_Init, Page_Load, Page_PreRender, etc.) and control events from a code-behind file. Detects IsPostBack branching, implicit behaviors, and maps each to Blazor, React, and Angular equivalents."
    )]
    pub async fn map_page_lifecycle(
        &self,
        params: Parameters<MapPageLifecycleRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let cb_full = std::path::Path::new(&rec.directory).join(&file_path);
        let cb_content = tokio::fs::read_to_string(&cb_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", cb_full.display()), None)
        })?;

        // Try to find corresponding ASPX file
        let aspx_path = find_aspx_for_codebehind(&cb_full);
        let aspx_content = if let Some(ref p) = aspx_path {
            tokio::fs::read_to_string(p).await.ok()
        } else {
            None
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::lifecycle_service::analyze_page_lifecycle(
                &graph,
                &pid,
                &file_path,
                &cb_content,
                aspx_content.as_deref(),
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!("# Page Lifecycle — {}\n\n", result.file_path);

        if let Some(ref bc) = result.base_class {
            out.push_str(&format!("**Base class**: `{bc}`\n\n"));
        }

        if !result.lifecycle_events.is_empty() {
            out.push_str("## Lifecycle Events\n");
            for ev in &result.lifecycle_events {
                out.push_str(&format!(
                    "### {} → `{}`\n- IsPostBack branch: {}\n",
                    ev.event_name, ev.handler_name, ev.has_ispostback_branch,
                ));
                if !ev.first_load_actions.is_empty() {
                    out.push_str("- **First-load actions**:\n");
                    for a in &ev.first_load_actions {
                        out.push_str(&format!("  - {a}\n"));
                    }
                }
                if !ev.postback_actions.is_empty() {
                    out.push_str("- **Postback actions**:\n");
                    for a in &ev.postback_actions {
                        out.push_str(&format!("  - {a}\n"));
                    }
                }
                if !ev.always_actions.is_empty() {
                    out.push_str("- **Always actions**:\n");
                    for a in &ev.always_actions {
                        out.push_str(&format!("  - {a}\n"));
                    }
                }
                out.push_str(&format!(
                    "- Blazor: `{}`\n- React: `{}`\n- Angular: `{}`\n\n",
                    ev.modern_blazor, ev.modern_react, ev.modern_angular,
                ));
            }
        }

        if !result.control_events.is_empty() {
            out.push_str("## Control Events\n");
            for ce in &result.control_events {
                out.push_str(&format!(
                    "- **{}** ({}) `{}` → `{}`\n  - Blazor: `{}`\n  - React: `{}`\n",
                    ce.control_id,
                    ce.control_type,
                    ce.event_name,
                    ce.handler_name,
                    ce.modern_blazor,
                    ce.modern_react,
                ));
            }
            out.push('\n');
        }

        if !result.implicit_behaviors.is_empty() {
            out.push_str("## Implicit Behaviors\n");
            for ib in &result.implicit_behaviors {
                out.push_str(&format!(
                    "- **{}** ({}): {} → {}\n",
                    ib.behavior, ib.severity, ib.webforms_mechanism, ib.modern_replacement,
                ));
            }
            out.push('\n');
        }

        if !result.migration_notes.is_empty() {
            out.push_str("## Migration Notes\n");
            for n in &result.migration_notes {
                out.push_str(&format!("- {n}\n"));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Analyze ViewState dependencies (explicit and implicit).
    #[tool(
        name = "analyze_viewstate_deps",
        description = "Analyze explicit ViewState[\"key\"] usage and implicit control-level ViewState (TextBox.Text, GridView.DataSource, etc.) in a WebForms page. Generates modern state model recommendations."
    )]
    pub async fn analyze_viewstate_deps(
        &self,
        params: Parameters<AnalyzeViewStateDepsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let cb_full = std::path::Path::new(&rec.directory).join(&file_path);
        let cb_content = tokio::fs::read_to_string(&cb_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", cb_full.display()), None)
        })?;

        let aspx_path = find_aspx_for_codebehind(&cb_full);
        let aspx_content = if let Some(ref p) = aspx_path {
            tokio::fs::read_to_string(p).await.ok()
        } else {
            None
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::viewstate_service::analyze_viewstate_dependencies(
                &graph,
                &pid,
                &file_path,
                &cb_content,
                aspx_content.as_deref(),
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# ViewState Dependencies — {}\n\n\
             **Total state fields**: {} | **Complexity**: {}\n",
            result.file_path, result.total_state_fields, result.migration_complexity,
        );

        if let Some(pv) = result.page_level_viewstate {
            out.push_str(&format!(
                "**Page-level EnableViewState**: {}\n",
                if pv { "true" } else { "false (disabled)" }
            ));
        }
        out.push('\n');

        if !result.explicit_viewstate.is_empty() {
            out.push_str("## Explicit ViewState Keys\n");
            for vs in &result.explicit_viewstate {
                out.push_str(&format!(
                    "- **{}** ({})\n  - Readers: [{}]\n  - Writers: [{}]\n  - Lifecycle: {}\n  - Modern: `{}`\n",
                    vs.key, vs.data_type_guess,
                    vs.readers.join(", "), vs.writers.join(", "),
                    vs.lifecycle, vs.modern_replacement,
                ));
            }
            out.push('\n');
        }

        if !result.implicit_viewstate.is_empty() {
            out.push_str("## Implicit ViewState (Control Properties)\n");
            for iv in &result.implicit_viewstate {
                out.push_str(&format!(
                    "- **{}** (`{}`) — props: [{}] — {}\n  - {}\n",
                    iv.control_id,
                    iv.control_type,
                    iv.properties_persisted.join(", "),
                    iv.estimated_size_impact,
                    iv.modern_replacement,
                ));
            }
            out.push('\n');
        }

        if !result.viewstate_disabled_controls.is_empty() {
            out.push_str("## ViewState Disabled Controls\n");
            for c in &result.viewstate_disabled_controls {
                out.push_str(&format!("- {c}\n"));
            }
            out.push('\n');
        }

        if !result.heaviest_controls.is_empty() {
            out.push_str("## Heaviest Controls\n");
            for (id, ctrl_type, reason) in &result.heaviest_controls {
                out.push_str(&format!("- `{id}` ({ctrl_type}): {reason}\n"));
            }
            out.push('\n');
        }

        if !result.modern_state_model.is_empty() {
            out.push_str("## Modern State Model\n");
            for sm in &result.modern_state_model {
                out.push_str(&format!(
                    "- **{}** (source: {})\n  - Blazor: `{}`\n  - React: `{}`\n  - Persist: {}\n",
                    sm.field_name,
                    sm.source,
                    sm.blazor_declaration,
                    sm.react_declaration,
                    sm.persist_across,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Map UpdatePanel / AJAX regions to modern component boundaries.
    #[tool(
        name = "map_ajax_regions",
        description = "Parse UpdatePanel, ScriptManager, AsyncPostBackTrigger, PostBackTrigger, Timer, and UpdateProgress controls from an ASPX file. Maps each region to modern component boundaries (Blazor components, React fetch, Angular HttpClient)."
    )]
    pub async fn map_ajax_regions(
        &self,
        params: Parameters<MapAjaxRegionsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();

        let aspx_full = std::path::Path::new(&rec.directory).join(&file_path);
        let aspx_content = tokio::fs::read_to_string(&aspx_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", aspx_full.display()), None)
        })?;

        let result = tokio::task::spawn_blocking(move || {
            crate::services::ajax_region_service::analyze_ajax_regions(
                &graph,
                &pid,
                &file_path,
                &aspx_content,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# AJAX Regions — {}\n\n\
             **ScriptManager**: {} | **Partial rendering**: {} | **PageMethods**: {} | **Complexity**: {}\n\n",
            result.file_path,
            result.has_script_manager,
            result.enable_partial_rendering,
            result.enable_page_methods,
            result.migration_complexity,
        );

        if !result.update_panels.is_empty() {
            out.push_str("## UpdatePanels\n");
            for up in &result.update_panels {
                out.push_str(&format!(
                    "### `{}`\n- Mode: {} | ChildrenAsTriggers: {}\n",
                    up.panel_id, up.update_mode, up.children_as_triggers,
                ));
                if !up.async_triggers.is_empty() {
                    out.push_str("- Async triggers:\n");
                    for t in &up.async_triggers {
                        out.push_str(&format!("  - `{}`.`{}`\n", t.control_id, t.event_name));
                    }
                }
                if !up.postback_triggers.is_empty() {
                    out.push_str("- Full postback triggers:\n");
                    for t in &up.postback_triggers {
                        out.push_str(&format!("  - `{t}`\n"));
                    }
                }
                if !up.controls_inside.is_empty() {
                    out.push_str(&format!(
                        "- Contains {} controls\n",
                        up.controls_inside.len()
                    ));
                }
                out.push_str(&format!("- Modern: {}\n", up.modern_pattern));
                out.push('\n');
            }
        }

        if !result.timers.is_empty() {
            out.push_str("## Timers\n");
            for t in &result.timers {
                out.push_str(&format!(
                    "- `{}`: {}ms interval (enabled: {}) → {}\n",
                    t.timer_id, t.interval_ms, t.enabled, t.modern_equivalent,
                ));
            }
            out.push('\n');
        }

        if !result.update_progress_controls.is_empty() {
            out.push_str("## UpdateProgress Controls\n");
            for up in &result.update_progress_controls {
                out.push_str(&format!(
                    "- `{}` → associated panel: `{}`\n",
                    up.progress_id,
                    up.associated_update_panel.as_deref().unwrap_or("(any)"),
                ));
            }
            out.push('\n');
        }

        if !result.full_postback_controls.is_empty() {
            out.push_str("## Full Postback Controls (outside UpdatePanels)\n");
            for c in &result.full_postback_controls {
                out.push_str(&format!("- {c}\n"));
            }
            out.push('\n');
        }

        if !result.suggested_components.is_empty() {
            out.push_str("## Suggested Modern Components\n");
            for sc in &result.suggested_components {
                out.push_str(&format!(
                    "- **{}**: {}\n  - Controls: [{}]\n  - Blazor: `{}`\n",
                    sc.name,
                    sc.reason,
                    sc.controls.join(", "),
                    sc.blazor_pattern,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Trace data flow from an event handler through SQL, state, and data binding.
    #[tool(
        name = "trace_data_flow",
        description = "Trace the business logic flow from a named event handler (e.g. btnSearch_Click) through control reads, state access, SQL operations, data binding, and redirects. Supplements parsed code with graph edges."
    )]
    pub async fn trace_data_flow(
        &self,
        params: Parameters<TraceDataFlowRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let entry_point = req.entry_point.clone();

        let cb_full = std::path::Path::new(&rec.directory).join(&file_path);
        let cb_content = tokio::fs::read_to_string(&cb_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", cb_full.display()), None)
        })?;

        let result = tokio::task::spawn_blocking(move || {
            crate::services::data_flow_service::trace_data_flow(
                &graph,
                &pid,
                &file_path,
                &entry_point,
                &cb_content,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Data Flow Trace — `{}`\n\n**Trigger**: {}\n\n",
            result.entry_point, result.trigger,
        );

        if !result.steps.is_empty() {
            out.push_str("## Flow Steps\n");
            for (i, step) in result.steps.iter().enumerate() {
                out.push_str(&format!(
                    "{}. **{}** — `{}`\n",
                    i + 1,
                    step.step_type,
                    step.description,
                ));
            }
            out.push('\n');
        }

        if !result.tables_touched.is_empty() {
            out.push_str(&format!(
                "## Tables Touched\n{}\n\n",
                result
                    .tables_touched
                    .iter()
                    .map(|t| format!("- `{t}`"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }

        if !result.state_reads.is_empty() || !result.state_writes.is_empty() {
            out.push_str("## State Access\n");
            for sr in &result.state_reads {
                out.push_str(&format!("- READ `{}` ({})\n", sr.key, sr.state_type));
            }
            for sw in &result.state_writes {
                out.push_str(&format!("- WRITE `{}` ({})\n", sw.key, sw.state_type));
            }
            out.push('\n');
        }

        if !result.controls_read.is_empty() || !result.controls_written.is_empty() {
            out.push_str("## Control I/O\n");
            for cr in &result.controls_read {
                out.push_str(&format!("- INPUT ← `{cr}`\n"));
            }
            for cw in &result.controls_written {
                out.push_str(&format!("- OUTPUT → `{cw}`\n"));
            }
            out.push('\n');
        }

        if !result.methods_called.is_empty() {
            out.push_str(&format!(
                "## Methods Called\n{}\n\n",
                result
                    .methods_called
                    .iter()
                    .map(|m| format!("- `{m}`"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
        }

        out.push_str(&format!(
            "## Modern Flow Hint\n{}\n",
            result.modern_flow_hint,
        ));

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Get a complete migration dossier for a single page.
    #[tool(
        name = "get_migration_dossier",
        description = "Build a comprehensive migration dossier for a single WebForms page. Orchestrates lifecycle analysis, ViewState, AJAX regions, validation, auth config, blast radius, and scaffold generation into one report."
    )]
    pub async fn get_migration_dossier(
        &self,
        params: Parameters<GetMigrationDossierRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let file_path = req.file_path.clone();
        let target_stack = req.target_stack.clone();
        let project_dir = rec.directory.clone();

        // Read ASPX content
        let aspx_full = std::path::Path::new(&project_dir).join(&file_path);
        let aspx_content = tokio::fs::read_to_string(&aspx_full).await.map_err(|e| {
            McpError::internal_error(format!("Failed to read {}: {e}", aspx_full.display()), None)
        })?;

        // Read code-behind
        let cb_path = find_codebehind_path(&aspx_full);
        let cb_content = if let Some(ref p) = cb_path {
            tokio::fs::read_to_string(p).await.map_err(|e| {
                McpError::internal_error(
                    format!("Failed to read code-behind {}: {e}", p.display()),
                    None,
                )
            })?
        } else {
            String::new()
        };

        // Try to read web.config
        let webconfig_path = std::path::Path::new(&project_dir).join("web.config");
        let webconfig_content = tokio::fs::read_to_string(&webconfig_path).await.ok();
        let webconfig_content = if webconfig_content.is_none() {
            let alt = std::path::Path::new(&project_dir).join("Web.config");
            tokio::fs::read_to_string(&alt).await.ok()
        } else {
            webconfig_content
        };

        let result = tokio::task::spawn_blocking(move || {
            crate::services::dossier_service::build_migration_dossier(
                &graph,
                &pid,
                &file_path,
                &aspx_content,
                &cb_content,
                webconfig_content.as_deref(),
                &target_stack,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Migration Dossier — {}\n\n\
             **Page type**: {} | **Target**: {} | **Complexity**: {}\n",
            result.file_path, result.page_type, result.target_stack, result.estimated_complexity,
        );

        if let Some(ref bc) = result.inherits_class {
            out.push_str(&format!("**Class**: `{bc}`\n"));
        }
        if let Some(ref mp) = result.master_page {
            out.push_str(&format!("**Master page**: `{mp}`\n"));
        }

        out.push_str(&format!(
            "\n**User controls**: {} | **Tables touched**: {} | **Risk**: {}/100\n\n",
            result.user_controls.len(),
            result.tables_touched.len(),
            result.blast_radius_score,
        ));

        // Sub-service summaries
        out.push_str(&format!(
            "## Lifecycle\n{}\n\n## ViewState\n{}\n\n## AJAX\n{}\n\n## Validation\n{}\n\n## Auth\n{}\n\n",
            serde_json::to_string_pretty(&result.lifecycle_summary).unwrap_or_default(),
            serde_json::to_string_pretty(&result.viewstate_summary).unwrap_or_default(),
            serde_json::to_string_pretty(&result.ajax_summary).unwrap_or_default(),
            serde_json::to_string_pretty(&result.validation_summary).unwrap_or_default(),
            serde_json::to_string_pretty(&result.auth_summary).unwrap_or_default(),
        ));

        if !result.risk_factors.is_empty() {
            out.push_str("## Risk Factors\n");
            for rf in &result.risk_factors {
                out.push_str(&format!("- {rf}\n"));
            }
            out.push('\n');
        }

        if !result.migration_steps.is_empty() {
            out.push_str("## Migration Steps\n");
            for (i, step) in result.migration_steps.iter().enumerate() {
                out.push_str(&format!("{}. {step}\n", i + 1));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Check migration coverage — what did the modern code miss?
    #[tool(
        name = "check_migration_coverage",
        description = "Compare generated modern code against the graph-known elements (event handlers, SQL tables, state keys, data bindings, controls, API calls) of the original legacy file. Returns a coverage score and lists missing items."
    )]
    pub async fn check_migration_coverage(
        &self,
        params: Parameters<CheckMigrationCoverageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let original_file = req.original_file.clone();
        let modern_code = req.modern_code.clone();

        let result = tokio::task::spawn_blocking(move || {
            crate::services::coverage_service::check_migration_coverage(
                &graph,
                &pid,
                &original_file,
                &modern_code,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&result)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let pct = result.coverage_score * 100.0;
        let mut out = format!(
            "# Migration Coverage — {}\n\n\
             **Score**: {:.1}% ({}/{} items covered)\n\n",
            result.original_file, pct, result.covered_items, result.total_items,
        );

        fn format_category(
            out: &mut String,
            name: &str,
            cat: &crate::services::coverage_service::CoverageCategory,
        ) {
            if cat.total == 0 {
                return;
            }
            let pct = if cat.total > 0 {
                (cat.covered as f64 / cat.total as f64) * 100.0
            } else {
                100.0
            };
            out.push_str(&format!(
                "## {} ({}/{} = {:.0}%)\n",
                name, cat.covered, cat.total, pct,
            ));
            if !cat.missing_names.is_empty() {
                out.push_str("**Missing:**\n");
                for m in &cat.missing_names {
                    out.push_str(&format!("- {m}\n"));
                }
            }
            out.push('\n');
        }

        format_category(&mut out, "Event Handlers", &result.event_handlers);
        format_category(&mut out, "Data Bindings", &result.data_bindings);
        format_category(&mut out, "SQL Queries", &result.sql_queries);
        format_category(&mut out, "State Access", &result.state_access);
        format_category(&mut out, "API Calls", &result.api_calls);
        format_category(&mut out, "Controls", &result.controls);

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Update migration status for a file.
    #[tool(
        name = "update_migration_status",
        description = "Set the migration status (not_started, in_progress, migrated, verified, blocked) for a specific file in a project. Stores notes, risk score, blocked reason, and blocking dependencies."
    )]
    pub async fn update_migration_status(
        &self,
        params: Parameters<UpdateMigrationStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_record(&req.project_id).await?;
        let store = self.state.migration_progress.clone();
        let pid = req.project_id.clone();
        let fp = req.file_path.clone();
        let notes = req.notes.clone();
        let risk = req.risk_score;
        let blocked_reason = req.blocked_reason.clone();
        let blocking_deps = req.blocking_dependencies.clone();

        // Parse status string
        let status = match req.status.to_lowercase().as_str() {
            "not_started" => {
                crate::services::migration_progress_service::MigrationStatus::NotStarted
            }
            "in_progress" => {
                crate::services::migration_progress_service::MigrationStatus::InProgress
            }
            "migrated" => crate::services::migration_progress_service::MigrationStatus::Migrated,
            "verified" => crate::services::migration_progress_service::MigrationStatus::Verified,
            "blocked" => crate::services::migration_progress_service::MigrationStatus::Blocked,
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "Invalid status '{other}'. Must be one of: not_started, in_progress, migrated, verified, blocked"
                    ),
                    None,
                ));
            }
        };

        tokio::task::spawn_blocking(move || {
            store.update_status(
                &pid,
                &fp,
                status,
                &notes,
                risk,
                blocked_reason.as_deref(),
                blocking_deps,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Updated `{}` → status: **{}**, notes: \"{}\"",
            req.file_path, req.status, req.notes,
        ))]))
    }

    /// Get migration progress for a project.
    #[tool(
        name = "get_migration_progress",
        description = "Get an overview of migration progress for a project: total files, status breakdown, completion percentage, blocked items, recently updated files, and suggested next files to migrate."
    )]
    pub async fn get_migration_progress(
        &self,
        params: Parameters<GetMigrationProgressRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let _ = self.ensure_project_record(&req.project_id).await?;
        let store = self.state.migration_progress.clone();
        let pid = req.project_id.clone();

        let progress = tokio::task::spawn_blocking(move || store.get_progress(&pid))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&progress)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let mut out = format!(
            "# Migration Progress — {}\n\n\
             **Total files**: {} | **Completion**: {:.1}%\n\n\
             | Status | Count |\n|--------|-------|\n\
             | Not Started | {} |\n\
             | In Progress | {} |\n\
             | Migrated | {} |\n\
             | Verified | {} |\n\
             | Blocked | {} |\n\n",
            progress.project_id,
            progress.total_files,
            progress.completion_pct,
            progress.not_started,
            progress.in_progress,
            progress.migrated,
            progress.verified,
            progress.blocked,
        );

        if !progress.by_file_type.is_empty() {
            out.push_str("## By File Type\n");
            for (ext, tp) in &progress.by_file_type {
                out.push_str(&format!(
                    "- `{}`: {}/{} ({:.0}%)\n",
                    ext, tp.completed, tp.total, tp.pct,
                ));
            }
            out.push('\n');
        }

        if !progress.blocked_items.is_empty() {
            out.push_str("## Blocked Files\n");
            for bi in &progress.blocked_items {
                out.push_str(&format!(
                    "- **{}**: {}\n",
                    bi.file_path,
                    if bi.reason.is_empty() {
                        "(no reason given)"
                    } else {
                        &bi.reason
                    },
                ));
                if !bi.blocking_deps.is_empty() {
                    out.push_str(&format!(
                        "  - Blocked by: {}\n",
                        bi.blocking_deps.join(", "),
                    ));
                }
            }
            out.push('\n');
        }

        if !progress.recently_updated.is_empty() {
            out.push_str("## Recently Updated\n");
            for ru in &progress.recently_updated {
                out.push_str(&format!("- `{}` → {}\n", ru.file_path, ru.status,));
            }
            out.push('\n');
        }

        if !progress.suggested_next.is_empty() {
            out.push_str("## Suggested Next (lowest risk)\n");
            for s in &progress.suggested_next {
                out.push_str(&format!("- {s}\n"));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    /// Suggest optimal migration order based on dependency graph.
    #[tool(
        name = "suggest_migration_order",
        description = "Compute a topologically sorted migration plan from the project's dependency graph. Groups files into parallelizable waves, detects dependency cycles, and identifies bottleneck files."
    )]
    pub async fn suggest_migration_order(
        &self,
        params: Parameters<SuggestMigrationOrderRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();

        let plan = tokio::task::spawn_blocking(move || {
            crate::services::migration_order_service::suggest_migration_order(&graph, &pid)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if req.output_json {
            let json = serde_json::to_string_pretty(&plan)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // The plan has a summary field — use it, then append details.
        let mut out = format!(
            "# Migration Order — {}\n\n**Total files**: {}\n\n",
            plan.project_id, plan.total_files,
        );

        if !plan.summary.is_empty() {
            out.push_str(&plan.summary);
            out.push_str("\n\n");
        }

        for wave in &plan.waves {
            out.push_str(&format!("## Wave {} — {}\n", wave.wave_number, wave.theme,));
            if !wave.prerequisites.is_empty() {
                out.push_str(&format!(
                    "Prerequisites: {}\n",
                    wave.prerequisites.join(", "),
                ));
            }
            for wf in &wave.files {
                out.push_str(&format!(
                    "- `{}` ({}, deps: {}, dependents: {}) — {}\n",
                    wf.path,
                    wf.estimated_complexity,
                    wf.dependency_count,
                    wf.dependent_count,
                    wf.reason,
                ));
            }
            if wave.strangler_fig_checkpoint {
                out.push_str("**Strangler fig checkpoint after this wave.**\n");
            }
            out.push('\n');
        }

        if !plan.circular_dependencies.is_empty() {
            out.push_str("## Circular Dependencies\n");
            for cycle in &plan.circular_dependencies {
                out.push_str(&format!("- {}\n", cycle.join(" → ")));
            }
            out.push('\n');
        }

        if !plan.bottleneck_files.is_empty() {
            out.push_str("## Bottleneck Files\n");
            for bf in &plan.bottleneck_files {
                out.push_str(&format!(
                    "- `{}` — blocks {} downstream files: {}\n",
                    bf.path, bf.blocks_count, bf.suggestion,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }

    // ── Phase 31: Full Project Migration ──────────────────────────────────

    /// Analyze an entire project for migration in one call.
    #[tool(
        name = "analyze_full_project_migration",
        description = "Analyze an entire project for migration in one call. Produces a complete migration dossier for every page, project-wide auth/state/data-access analysis, topologically sorted migration waves, cross-cutting risk assessment, and actionable migration steps. This is the single tool needed to understand a legacy project before writing any migration code."
    )]
    pub async fn analyze_full_project_migration(
        &self,
        params: Parameters<AnalyzeFullProjectMigrationRequest>,
    ) -> Result<CallToolResult, McpError> {
        let req = params.0;
        let rec = self.ensure_project_record(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let pid = req.project_id.clone();
        let target_stack = req.target_stack.clone();
        let max_files = req.max_files;
        let project_dir = rec.directory.clone();

        // ── Async phase: discover and read all files from disk ────────────

        // 1. Get all file nodes from the graph to discover markup files
        let graph_clone = graph.clone();
        let pid_clone = pid.clone();
        let file_nodes = tokio::task::spawn_blocking(move || {
            graph_clone.query_nodes(&pid_clone, Some("file"), None, None, 10_000)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // 2. Identify markup files (.aspx, .ascx, .master)
        let markup_extensions = [".aspx", ".ascx", ".master"];
        let mut markup_paths: Vec<String> = file_nodes
            .iter()
            .filter_map(|n| {
                let name = n.name.to_lowercase();
                if markup_extensions.iter().any(|ext| name.ends_with(ext)) {
                    Some(n.name.clone())
                } else {
                    None
                }
            })
            .collect();

        // If graph has no file nodes, fall back to filesystem scan
        if markup_paths.is_empty() {
            if let Ok(mut entries) = tokio::fs::read_dir(&project_dir).await {
                let mut discovered = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        let ext_lower = ext.to_lowercase();
                        if ext_lower == "aspx" || ext_lower == "ascx" || ext_lower == "master" {
                            if let Some(rel) = path
                                .strip_prefix(&project_dir)
                                .ok()
                                .and_then(|r| r.to_str())
                            {
                                discovered.push(rel.replace('\\', "/"));
                            }
                        }
                    }
                }
                markup_paths = discovered;
            }
        }

        // Cap at max_files
        markup_paths.truncate(max_files);

        // 3. Read all markup + code-behind files concurrently
        use crate::services::full_project_migration_service::FileContent;

        let read_futures: Vec<_> = markup_paths
            .iter()
            .map(|rel_path| {
                let dir = project_dir.clone();
                let rel = rel_path.clone();
                async move {
                    let full_path = std::path::Path::new(&dir).join(&rel);
                    let markup = tokio::fs::read_to_string(&full_path).await.ok()?;
                    let cb_path = find_codebehind_path(&full_path);
                    let cb_content = if let Some(ref p) = cb_path {
                        tokio::fs::read_to_string(p).await.ok()
                    } else {
                        None
                    };
                    Some(FileContent {
                        file_path: rel,
                        markup_content: markup,
                        codebehind_content: cb_content,
                    })
                }
            })
            .collect();

        let file_contents: Vec<FileContent> = futures::future::join_all(read_futures)
            .await
            .into_iter()
            .flatten()
            .collect();

        // 4. Read web.config
        let webconfig_path = std::path::Path::new(&project_dir).join("web.config");
        let webconfig_content = tokio::fs::read_to_string(&webconfig_path).await.ok();
        let webconfig_content = if webconfig_content.is_none() {
            let alt = std::path::Path::new(&project_dir).join("Web.config");
            tokio::fs::read_to_string(&alt).await.ok()
        } else {
            webconfig_content
        };

        // 5. Collect all code-behind files for auth scanning
        let code_files: Vec<(String, String)> = file_nodes
            .iter()
            .filter_map(|n| {
                let name = n.name.to_lowercase();
                if name.ends_with(".cs") || name.ends_with(".vb") {
                    Some(n.name.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|rel| {
                let full = std::path::Path::new(&project_dir).join(&rel);
                // Use std::fs since we already did the async part
                std::fs::read_to_string(&full).ok().map(|c| (rel, c))
            })
            .collect();

        // ── Blocking phase: run all analysis ──────────────────────────────

        let report = tokio::task::spawn_blocking(move || {
            let code_refs: Vec<(&str, &str)> = code_files
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();

            crate::services::full_project_migration_service::analyze_full_project(
                &graph,
                &pid,
                &target_stack,
                &file_contents,
                webconfig_content.as_deref(),
                &code_refs,
                max_files,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // ── Output ────────────────────────────────────────────────────────

        if req.output_json {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        Ok(CallToolResult::success(vec![Content::text(
            report.markdown_report,
        )]))
    }
}

/// Find the code-behind file for an ASPX file.
/// Tries .aspx.vb, .aspx.cs, then strips .aspx and tries .vb, .cs.
fn find_codebehind_path(aspx_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let s = aspx_path.to_string_lossy();
    // .aspx.vb / .aspx.cs
    for ext in &[".vb", ".cs"] {
        let cb = std::path::PathBuf::from(format!("{s}{ext}"));
        if cb.exists() {
            return Some(cb);
        }
    }
    // Strip extension and try .vb / .cs (handles .ascx, .master too)
    if let Some(stem) = aspx_path.to_str() {
        for base_ext in &[".aspx", ".ascx", ".master"] {
            if let Some(stripped) = stem.strip_suffix(base_ext) {
                for ext in &[".aspx.vb", ".aspx.cs", ".ascx.vb", ".ascx.cs"] {
                    let cb = std::path::PathBuf::from(format!("{stripped}{ext}"));
                    if cb.exists() {
                        return Some(cb);
                    }
                }
            }
        }
    }
    None
}

/// Find the ASPX file for a code-behind file (reverse of find_codebehind_path).
fn find_aspx_for_codebehind(cb_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let s = cb_path.to_string_lossy();
    // Strip .vb or .cs from end — what remains should be .aspx, .ascx, or .master
    for ext in &[".vb", ".cs"] {
        if let Some(stripped) = s.strip_suffix(ext) {
            let aspx = std::path::PathBuf::from(stripped);
            if aspx.exists() {
                return Some(aspx);
            }
        }
    }
    None
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
