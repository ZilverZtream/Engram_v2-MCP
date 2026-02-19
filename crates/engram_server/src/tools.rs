use crate::state::{AppEvent, AppState, ProjectInfo, ProjectState, SearchHitLite};
use engram_core::{JobRecord, MemorySection, ProjectRecord, RepoRule, WatchRecord};
use engram_git::GitWalker;
use engram_graph::EdgeKind;
use engram_index::{HybridQuery, IndexDoc};
use git2::Oid;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::Parameters, tool::ToolRouter},
    model::*,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone)]
pub struct Engram {
    pub state: AppState,
    pub tool_router: ToolRouter<Engram>,
}

// -------------------- Request structs --------------------

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IndexProjectRequest {
    pub directory: String,
    pub project_name: String,
    pub project_type: String,
    #[serde(default = "default_true")]
    pub wait: bool,
    #[serde(default = "default_true")]
    pub dedupe_by_directory: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateProjectRequest {
    pub project_id: String,
    #[serde(default = "default_true")]
    pub wait: bool,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
    #[serde(default)]
    pub index_antipatterns: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ProjectIdRequest {
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
pub struct SearchMemoryRequest {
    pub query: String,
    pub project_id: String,
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    #[serde(default = "default_top_k")]
    pub max_results: usize,
    #[serde(default = "default_true")]
    pub use_mmr: bool,
    #[serde(default = "default_fts_strict")]
    pub fts_mode: String,
    #[serde(default = "default_true")]
    pub include_content: bool,
    #[serde(default = "default_max_content_chars")]
    pub max_content_chars_per_result: usize,
    #[serde(default)]
    pub include_path_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_path_prefixes: Option<Vec<String>>,
    #[serde(default)]
    pub language_filters: Option<Vec<String>>,
    #[serde(default)]
    pub metadata_filter: Option<serde_json::Value>,
}

fn default_fts_strict() -> String {
    "strict".to_string()
}
fn default_max_content_chars() -> usize {
    1200
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetChunkRequest {
    pub project_id: String,
    /// Per-instance document identity (required).
    pub doc_id: String,
    #[serde(default = "default_namespace_memory")]
    pub namespace: String,
    #[serde(default)]
    pub inject_rules: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateMemoryBankRequest {
    pub project_id: String,
    #[serde(default)]
    pub section_id: Option<String>,
    pub section: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MemorySectionRequest {
    pub project_id: String,
    pub section: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AddRepoRuleRequest {
    pub project_id: String,
    pub file_pattern: String,
    pub rule_text: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default)]
    pub rule_id: Option<String>,
}

fn default_priority() -> i32 {
    5
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeleteRepoRuleRequest {
    pub project_id: String,
    pub rule_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WatchProjectRequest {
    pub project_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct QueryGraphNodesRequest {
    pub project_id: String,
    #[serde(default)]
    pub node_type: String,
    #[serde(default)]
    pub name_pattern: String,
    #[serde(default)]
    pub file_path: String,
    #[serde(default = "default_limit_100")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindReferencesRequest {
    pub project_id: String,
    pub node_id: String,
    #[serde(default)]
    pub edge_kind: Option<String>,
    #[serde(default = "default_direction_in")]
    pub direction: String, // "in", "out", "both"
}

fn default_direction_in() -> String {
    "in".to_string()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GraphSearchRequest {
    pub project_id: String,
    pub query: String,
    #[serde(default = "default_top_k")]
    pub max_results: usize,
    #[serde(default = "default_symbol_boost")]
    pub symbol_boost: f32,
}

fn default_limit_100() -> usize {
    100
}
fn default_symbol_boost() -> f32 {
    0.03
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TraverseGraphRequest {
    pub project_id: String,
    pub node_id: String,
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
    #[serde(default)]
    pub edge_kinds: Option<Vec<String>>,
    #[serde(default = "default_direction_both")]
    pub direction: String, // "in", "out", "both"
}

fn default_max_hops() -> usize {
    2
}
fn default_direction_both() -> String {
    "both".to_string()
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IndexGitHistoryRequest {
    pub project_id: String,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
    #[serde(default)]
    pub index_antipatterns: bool,
    #[serde(default = "default_true")]
    pub wait: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchHistoryRequest {
    pub query: String,
    pub project_id: String,
    #[serde(default)]
    pub file_filter: Option<String>,
    #[serde(default)]
    pub author_filter: Option<String>,
    #[serde(default)]
    pub date_after: Option<u64>,
    #[serde(default)]
    pub date_before: Option<u64>,
    #[serde(default = "default_limit_5")]
    pub limit: usize,
}

fn default_limit_5() -> usize {
    5
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AnalyzeTemporalCouplingsRequest {
    pub project_id: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default = "default_min_freq")]
    pub min_frequency: usize,
    #[serde(default = "default_limit_50")]
    pub limit: usize,
    #[serde(default = "default_true")]
    pub inject_edges: bool,
}

fn default_min_freq() -> usize {
    5
}
fn default_limit_50() -> usize {
    50
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AnalyzeRevertsRequest {
    pub project_id: String,
    #[serde(default = "default_max_commits")]
    pub max_commits: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FindSymbolReferencesRequest {
    pub symbol_name: String,
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AnalyzeErrorStackRequest {
    pub traceback: String,
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DreamProjectRequest {
    pub project_id: String,
    #[serde(default)]
    pub wait: bool,
    #[serde(default = "default_max_pairs")]
    pub max_pairs: usize,
}

fn default_max_pairs() -> usize {
    10
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AnalyzeFileCodingStyleRequest {
    pub project_id: String,
    pub file_path: String,
    #[serde(default = "default_diff_limit")]
    pub diff_limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ExportCapturePackRequest {
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TraceUiActionRequest {
    pub project_id: String,
    pub query: String,
    #[serde(default = "default_max_depth_3")]
    pub max_depth: u8,
    #[serde(default = "default_limit_5")]
    pub max_paths: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TraceUiEventRequest {
    pub project_id: String,
    pub page_path: String,
    pub control_id: Option<String>,
    pub handler_fqn: Option<String>,
    #[serde(default = "default_max_depth_10")]
    pub max_hops: u8,
    #[serde(default = "default_limit_5")]
    pub max_paths: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetInstrumentationPackRequest {
    pub language: String, // "csharp" or "vb"
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IngestInstrumentationLogsRequest {
    pub project_id: String,
    pub log_content: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetInstrumentationPackResult {
    pub snippet: String,
    pub instructions: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ImpactAnalysisRequest {
    pub project_id: String,
    pub file_path: Option<String>,
    pub symbol_fqn: Option<String>,
    #[serde(default = "default_limit_50")]
    pub limit: usize,
}

fn default_max_depth_10() -> u8 {
    10
}

fn default_max_depth_3() -> u8 {
    3
}

fn default_diff_limit() -> usize {
    10
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IngestZipHistoryRequest {
    pub project_id: String,
    pub directory: String,
    #[serde(default = "default_true")]
    pub wait: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListJobsRequest {
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CancelJobRequest {
    pub job_id: String,
}

// Extra v2 tools (aliases)
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ImmuneCheckRequest {
    pub project_id: String,
    pub code: String,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AntiPatternGuardRequest {
    pub project_id: String,
    pub code: String,
    #[serde(default = "default_top_k")]
    pub limit: usize,
}

fn default_true() -> bool {
    true
}
fn default_top_k() -> usize {
    10
}
fn default_max_commits() -> usize {
    200
}
fn default_namespace_memory() -> String {
    "memory".to_string()
}

fn default_exts() -> Vec<&'static str> {
    vec![
        "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "cs", "vb", "c", "cpp", "cc", "cxx",
        "h", "hpp", "md", "toml", "yaml", "yml", "json", "aspx", "ascx", "master", "config", "xml",
    ]
}

/// Return the file extensions to index for a given project_type.
/// WebForms presets add .aspx/.ascx/.master/.vb/.config/.xml/.csproj/.vbproj/.sln/.sql/.rdlc.
fn exts_for_project_type(project_type: &str) -> Vec<&'static str> {
    match project_type.to_lowercase().as_str() {
        "dotnetwebformscs" | "dotnet_webforms_cs" | "webforms_cs" | "webformscs" => vec![
            "cs", "aspx", "ascx", "master", "config", "xml", "sln", "csproj", "sql", "rdlc", "md",
            "json",
        ],
        "dotnetwebformsvb" | "dotnet_webforms_vb" | "webforms_vb" | "webformsvb" => vec![
            "vb", "aspx", "ascx", "master", "config", "xml", "sln", "vbproj", "sql", "rdlc", "md",
            "json",
        ],
        _ => default_exts(),
    }
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

        // Dedupe: if a project already exists for this directory, return it.
        if req.dedupe_by_directory {
            let dir_str = req.directory.clone();
            let reg = self.state.registry.clone();
            if let Ok(existing) = tokio::task::spawn_blocking(move || reg.list_projects()).await
                && let Ok(list) = existing
                && let Some(p) = list.into_iter().find(|p| p.directory == dir_str)
            {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{2705} Already indexed.\nproject_id: {}\nproject_name: {}\ndirectory: {}",
                    p.project_id, p.project_name, p.directory
                ))]));
            }
        }

        let dir = match self.state.paths.resolve_path(&req.directory) {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{274C} {e}"
                ))]));
            }
        };

        let project_id = Uuid::new_v4().to_string();
        let project_root = self.state.cfg.data_dir.join("projects").join(&project_id);
        let tantivy_dir = project_root.join("tantivy");
        let lancedb_dir = project_root.join("lancedb");
        tokio::fs::create_dir_all(&tantivy_dir).await.ok();
        tokio::fs::create_dir_all(&lancedb_dir).await.ok();

        let search = engram_index::HybridSearchEngine::new(
            tantivy_dir.clone(),
            lancedb_dir.clone(),
            self.state.cfg.embedding_backend.clone(),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let search = std::sync::Arc::new(search);

        // Persist registry now so a background job can resume.
        let now = now_ms();
        let rec = ProjectRecord {
            project_id: project_id.clone(),
            project_name: req.project_name.clone(),
            project_type: req.project_type.clone(),
            directory: dir.to_string_lossy().to_string(),
            created_at_ms: now,
            updated_at_ms: now,
        };
        {
            let reg = self.state.registry.clone();
            let pid_clone = project_id.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                reg.put_project(&rec)?;
                reg.set_meta(&pid_clone, "active_generation", "1")?;
                Ok(())
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        // Runtime cache
        let info = ProjectInfo {
            project_id: project_id.clone(),
            project_name: req.project_name,
            project_type: req.project_type,
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
            if let Some(limit) = self.state.cfg.max_project_files
                && files.len() as u64 > limit
            {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "\u{274C} Too many files: {} > limit {}",
                    files.len(),
                    limit
                ))]));
            }

            let stats = search
                .index_files(
                    &project_id,
                    "memory",
                    1,
                    &dir,
                    files,
                    2000,
                    &cancel,
                    |_, _| {},
                )
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            self.process_ingest_stats(&project_id, 1, &stats)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

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
                    req.max_commits,
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
                req.max_commits,
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
        let _parse_permit = self.state.parse_semaphore.acquire().await.ok();
        let stats = ps
            .search
            .index_files(
                &pid,
                "memory",
                new_gen,
                &dir,
                changed,
                2000,
                cancel,
                |_, _| {},
            )
            .await?;
        drop(_parse_permit);

        self.process_ingest_stats(project_id, new_gen, &stats)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

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

        // Commit generation
        {
            let reg = self.state.registry.clone();
            let pid_clone = project_id.to_string();
            tokio::task::spawn_blocking(move || {
                reg.set_meta(&pid_clone, "active_generation", &new_gen.to_string())
            })
            .await
            .ok();
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

        // Remove cache entry
        {
            let mut map = self.state.projects.write().await;
            map.remove(&pid);
        }

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

        // Delete on-disk project dir
        let proj_dir = self.state.cfg.data_dir.join("projects").join(&pid);
        let _ = std::fs::remove_dir_all(&proj_dir);

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
        let pid = req.project_id;
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
            .ok();

        // Notify watcher actor
        let _ = self.state.events_tx.send(AppEvent::WatchUpdate {
            project_id: pid.clone(),
            directory: rec.directory.clone(),
            enabled: req.enabled,
        });

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
            .ok();

        // Notify watcher actor
        let _ = self.state.events_tx.send(AppEvent::WatchUpdate {
            project_id: pid.clone(),
            directory: "".into(),
            enabled: false,
        });

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
                    top_k: req.max_results,
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
                    let limit = req.max_content_chars_per_result;
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
            .ok();

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
        let nodes = self
            .state
            .graph
            .query_nodes(
                &req.project_id,
                Some(&req.node_type),
                Some(&req.name_pattern),
                Some(&req.file_path),
                req.limit,
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if nodes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matching nodes.",
            )]));
        }

        let mut out = String::new();
        for n in nodes {
            out.push_str(&format!(
                "- {} | {} | {} (lines {}-{} | gen {})\n",
                n.node_id, n.node_type, n.file_path, n.start_line, n.end_line, n.generation
            ));
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

        let mut out = String::new();

        if req.direction == "in" || req.direction == "both" {
            let incoming = self
                .state
                .graph
                .find_incoming_edges(&req.project_id, kind.clone(), &req.node_id, 100)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if !incoming.is_empty() {
                let header = match req.edge_kind.as_deref() {
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
            let outgoing = self
                .state
                .graph
                .neighbors(&req.project_id, search_kind, &req.node_id, 100)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            if !outgoing.is_empty() {
                let header = match req.edge_kind.as_deref() {
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
                    top_k: req.max_results,
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

            // Fetch neighbors
            if let Ok(neighbors) =
                self.state
                    .graph
                    .neighbors(&req.project_id, EdgeKind::Dependency, &node_id, 5)
            {
                for (neigh_id, weight) in neighbors {
                    let neigh_score = score * (1.0 + (weight as f32 * req.symbol_boost));
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
        for (id, score) in sorted.iter().take(req.max_results) {
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
                .filter_map(|s| match s.as_str() {
                    "co_occurrence" => Some(EdgeKind::CoOccurrence),
                    "temporal_coupling" => Some(EdgeKind::TemporalCoupling),
                    "insight" => Some(EdgeKind::Insight),
                    "dependency" => Some(EdgeKind::Dependency),
                    "anti_pattern" => Some(EdgeKind::AntiPattern),
                    "contains" => Some(EdgeKind::Contains),
                    "imports" => Some(EdgeKind::Imports),
                    _ => None,
                })
                .collect::<Vec<_>>()
        });

        let results = self
            .state
            .graph
            .traverse(
                &req.project_id,
                &req.node_id,
                req.max_hops,
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
                    req.max_commits,
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
            .spawn_job_git_history(pid_clone, req.max_commits, req.index_antipatterns)
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

                // Sort numerically — extract leading digits for proper chronological order.
                // e.g. "2.zip" before "10.zip" even though "10" < "2" lexicographically.
                zip_files.sort_by(|a, b| {
                    let name_a = a.file_name().to_string_lossy().to_string();
                    let name_b = b.file_name().to_string_lossy().to_string();
                    let num_a: u64 = name_a
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .unwrap_or(u64::MAX);
                    let num_b: u64 = name_b
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .unwrap_or(u64::MAX);
                    num_a.cmp(&num_b).then_with(|| name_a.cmp(&name_b))
                });

                if zip_files.len() < 2 {
                    return Ok("Need at least 2 zip files to compute pseudo-history.".to_string());
                }

                let mut temporal_edges = 0;
                let mut prev_fingerprints: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();

                for (i, entry) in zip_files.iter().enumerate() {
                    let path = entry.path();
                    let file = std::fs::File::open(&path)?;
                    let mut archive = zip::ZipArchive::new(file)?;

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
                        for (a, b) in pairs {
                            graph.increment_undirected_edge(
                                &project_id,
                                engram_core::namespaces::NAMESPACE_HISTORY,
                                "text",
                                engram_graph::EdgeKind::TemporalCoupling,
                                &format!("file:{}", a),
                                &format!("file:{}", b),
                                1,
                                active_gen,
                            )?;
                            temporal_edges += 1;
                        }
                    }

                    prev_fingerprints = current_fingerprints;
                }

                Ok(format!(
                    "\u{2705} Ingested {} snapshots, added {} temporal edges.",
                    zip_files.len(),
                    temporal_edges
                ))
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
        let include_path_prefixes = req.file_filter.map(|f| vec![f]);

        let hits = ps
            .search
            .search(
                &HybridQuery {
                    project_id: req.project_id.clone(),
                    namespace: "history".into(),
                    generation: gen_,
                    text: req.query,
                    top_k: req.limit,
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
                req.min_frequency as u32,
                req.limit,
            )
        } else {
            // Global search
            engram_graph::algorithms::coupling::top_project_couplings(
                &self.state.graph,
                &req.project_id,
                req.limit,
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

        let (summary, _rules_added) = tokio::task::spawn_blocking({
            let directory = ps.info.directory.clone();
            let project_id = req.project_id.clone();
            let registry = self.state.registry.clone();
            let search = ps.search.clone();
            let max_commits = req.max_commits;
            let cancel = tokio_util::sync::CancellationToken::new();

            move || -> anyhow::Result<(String, usize)> {
                let repo = GitWalker::open_repo(Path::new(&directory))?;

                let mut reverts_found = 0;
                let mut rules_added = 0;
                let mut anti_docs = Vec::new();

                GitWalker::walk_commits_streaming(
                    &repo,
                    None,
                    max_commits,
                    engram_git::history::MergeCommitPolicy::AllParents,
                    &cancel,
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
                            // Wait, RepoRule doesn't have progress fields. I updated JobRecord.
                            // I'll fix this in a moment.
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

                if !anti_docs.is_empty() {
                    // We need to wait for index_docs which is async.
                    // Blocking inside spawn_blocking is okay if we use a runtime block_on.
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(search.index_docs(&project_id, &anti_docs, &cancel))?;
                }

                Ok((format!("\u{2705} Immune System active.\nReverts analyzed: {}\nAnti-patterns indexed: {}\nRepo rules generated: {}", reverts_found, anti_docs.len(), rules_added), rules_added))
            }
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

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

        // 1. Resolve target node
        let target_id = if let Some(ref fqn) = req.symbol_fqn {
            if fqn.starts_with("sql:") {
                fqn.clone()
            } else {
                // If it looks like an FQN, try to find the node.
                // We don't have rel_path here, so we might need to search.
                if let Ok(candidates) =
                    self.state
                        .graph
                        .query_nodes(&req.project_id, None, None, None, 100)
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
                    // Fallback to searching by short name, but verify FQN match
                    let short = fqn.split('.').next_back().unwrap_or(fqn);
                    if let Ok(candidates) =
                        self.state
                            .graph
                            .query_nodes(&req.project_id, None, Some(short), None, 20)
                        && !candidates.is_empty()
                    {
                        // Prefer exact FQN match to avoid pollution from other symbols
                        // sharing the same short name.
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
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "Symbol '{fqn}' not found in graph."
                        ))]));
                    }
                }
            }
        } else if let Some(ref path) = req.file_path {
            engram_core::ids::NodeId::file(path).0
        } else {
            return Err(McpError::invalid_params(
                "Either file_path or symbol_fqn must be provided.",
                None,
            ));
        };

        // 2. Find incoming edges (who depends on this?)
        let incoming = self
            .state
            .graph
            .find_incoming_edges_with_kind(&req.project_id, None, &target_id, req.limit)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        if incoming.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No dependent nodes found for {target_id}."
            ))]));
        }

        // 3. Format results
        let mut out = format!("Impact Analysis for {target_id}:\n\n");
        out.push_str("Nodes that depend on or are related to this:\n");

        // Group by source_id to combine reasons
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
            let Some(src_node) = self
                .state
                .graph
                .get_node(&req.project_id, &src_id)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
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
        let pid = req.project_id;
        let _ps = self.ensure_project_runtime(&pid).await?;
        let _active_gen = self.get_active_generation(&pid).await.unwrap_or(1);

        // Prepare zip in memory first
        let mut buffer = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);

            // 1. overview.md
            let overview = self
                .get_codebase_overview(Parameters(ProjectIdRequest {
                    project_id: pid.clone(),
                }))
                .await?;
            let overview_text = match &overview.content[0].raw {
                RawContent::Text(t) => &t.text,
                _ => "",
            };
            zip.start_file("overview.md", options)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            std::io::Write::write_all(&mut zip, overview_text.as_bytes())
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            // 2. graph_topology.json — only load the 1000 we actually use
            let all_nodes = self
                .state
                .graph
                .query_nodes(&pid, None, None, None, 1000)
                .unwrap_or_default();
            // Get total count separately (cheap: no deserialization of 100k nodes)
            let total_node_count = self
                .state
                .graph
                .count_nodes(&pid)
                .unwrap_or(all_nodes.len());
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
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            std::io::Write::write_all(
                &mut zip,
                serde_json::to_string_pretty(&topo)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .as_bytes(),
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            // 3. ui_wiring.json
            let ui_nodes = self
                .state
                .graph
                .query_nodes(&pid, Some("control"), None, None, 5000)
                .unwrap_or_default();
            let mut wiring = Vec::new();
            for ctrl in ui_nodes {
                let deps = self
                    .state
                    .graph
                    .neighbors(&pid, engram_graph::EdgeKind::Dependency, &ctrl.node_id, 10)
                    .unwrap_or_default();
                wiring.push(serde_json::json!({
                    "control": ctrl.node_id,
                    "handlers": deps.iter().map(|(id, _)| id).collect::<Vec<_>>()
                }));
            }
            zip.start_file("ui_wiring.json", options)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            std::io::Write::write_all(
                &mut zip,
                serde_json::to_string_pretty(&wiring)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .as_bytes(),
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            // 4. sql_map.json
            let sql_edges = self
                .state
                .graph
                .list_edges_by_kind(&pid, engram_graph::EdgeKind::SqlCalls, 5000)
                .unwrap_or_default();
            zip.start_file("sql_map.json", options)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            std::io::Write::write_all(
                &mut zip,
                serde_json::to_string_pretty(&sql_edges)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
                    .as_bytes(),
            )
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            zip.finish()
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }

        // Write to disk — use tokio::fs to avoid blocking the async executor
        let timestamp = now_ms();
        let exports_dir = self.state.cfg.data_dir.join("exports").join(&pid);
        tokio::fs::create_dir_all(&exports_dir)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let zip_path = exports_dir.join(format!("{}.zip", timestamp));
        tokio::fs::write(&zip_path, buffer)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{2705} Capture pack exported to: {}",
            zip_path.to_string_lossy()
        ))]))
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
        let pid = req.project_id;
        let _ = self.ensure_project_record(&pid).await?;

        if req.wait {
            let insights = crate::actors::dreamer::dream_once(
                &self.state,
                &pid,
                2, // min_edge_weight
                3, // min_cluster_size
                req.max_pairs,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            return Ok(CallToolResult::success(vec![Content::text(format!(
                "ðŸ§  Dream completed for project_id: {pid}\ninsights_generated: {insights}"
            ))]));
        }

        let _ = self.state.events_tx.send(AppEvent::TriggerDream {
            project_id: pid.clone(),
        });

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

        let cache_key = format!("style_guide:{}:{}", req.file_path, latest_oid);
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
            let limit = req.diff_limit;
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
                if !is_dir {
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
                        for path in file_paths {
                            if let Ok(content) = std::fs::read_to_string(&path) {
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
                    top_k: req.top_k,
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
                    top_k: req.limit,
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
            for h in hits.iter().take(req.limit) {
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
            let mut edges_added = 0;
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

                // Normalize path (strip ~/)
                let rel_path = path.trim_start_matches("~/").trim_start_matches('/');

                let source_id = if !control_id.is_empty() {
                    engram_core::ids::NodeId::control(rel_path, control_id).0
                } else {
                    engram_core::ids::NodeId::page(rel_path).0
                };

                if !sql_hash.is_empty() {
                    let target_id = format!("sql:inline:{}", sql_hash);
                    graph.increment_edge(
                        &project_id,
                        engram_core::namespaces::NAMESPACE_HISTORY,
                        "text",
                        engram_graph::EdgeKind::SqlCalls,
                        &source_id,
                        &target_id,
                        1,
                        active_gen,
                    )?;
                    edges_added += 1;
                }
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

// -------------------- Helper methods --------------------

impl Engram {
    async fn ensure_project_record(&self, project_id: &str) -> Result<ProjectRecord, McpError> {
        // Security: reject NUL bytes in project_id to prevent key-prefix injection
        if project_id.contains('\0') {
            return Err(McpError::invalid_params(
                "project_id must not contain NUL bytes",
                None,
            ));
        }
        let reg = self.state.registry.clone();
        let pid = project_id.to_string();
        let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        rec.ok_or_else(|| {
            McpError::invalid_params(format!("Unknown project_id: {project_id}"), None)
        })
    }

    fn generate_indexing_report(&self, stats: &engram_index::IngestStats) -> String {
        let mut report = String::new();
        report.push_str("# Indexing Report\n\n");

        report.push_str("## Summary\n");
        report.push_str(&format!("- Total files found: {}\n", stats.files));
        report.push_str(&format!("- Files indexed: {}\n", stats.all_files.len()));
        report.push_str(&format!("- Files skipped: {}\n", stats.skipped_files.len()));
        report.push_str(&format!("- Total chunks created: {}\n", stats.chunks));
        report.push_str(&format!(
            "- Total bytes processed: {} ({:.2} MB)\n",
            stats.bytes,
            stats.bytes as f64 / 1024.0 / 1024.0
        ));

        if !stats.languages.is_empty() {
            report.push_str("\n## Languages Detected\n");
            let mut langs: Vec<_> = stats.languages.iter().collect();
            langs.sort_by(|a, b| b.1.cmp(a.1));
            for (lang, count) in langs {
                report.push_str(&format!("- {}: {}\n", lang, count));
            }
        }

        report.push_str("\n## Graph Stats\n");
        let mut node_kinds = std::collections::HashMap::new();
        for (_, sym) in &stats.symbols {
            *node_kinds.entry(sym.kind.clone()).or_insert(0) += 1;
        }
        report.push_str("- Nodes by kind:\n");
        let mut kinds: Vec<_> = node_kinds.iter().collect();
        kinds.sort_by(|a, b| b.1.cmp(a.1));
        for (kind, count) in kinds {
            report.push_str(&format!("  - {}: {}\n", kind, count));
        }

        let mut edge_kinds = std::collections::HashMap::new();
        for (_, edge) in &stats.edges {
            *edge_kinds.entry(edge.kind.clone()).or_insert(0) += 1;
        }
        report.push_str("- Edges by kind:\n");
        let mut ekinds: Vec<_> = edge_kinds.iter().collect();
        ekinds.sort_by(|a, b| b.1.cmp(a.1));
        for (kind, count) in ekinds {
            report.push_str(&format!("  - {}: {}\n", kind, count));
        }

        if !stats.skipped_files.is_empty() {
            report.push_str("\n## Skipped Files\n");
            for (path, reason) in stats.skipped_files.iter().take(50) {
                report.push_str(&format!("- {}: {}\n", path, reason));
            }
            if stats.skipped_files.len() > 50 {
                report.push_str(&format!(
                    "... and {} more\n",
                    stats.skipped_files.len() - 50
                ));
            }
        }

        if !stats.warnings.is_empty() {
            report.push_str("\n## Warnings\n");
            for warn in &stats.warnings {
                report.push_str(&format!("- {}\n", warn));
            }
        }

        report
    }

    async fn ensure_project_runtime(&self, project_id: &str) -> Result<ProjectState, McpError> {
        if let Some(p) = self.state.get_project_cached(project_id).await {
            return Ok(p);
        }

        let rec = self.ensure_project_record(project_id).await?;
        let project_root = self.state.cfg.data_dir.join("projects").join(project_id);
        let tantivy_dir = project_root.join("tantivy");
        let lancedb_dir = project_root.join("lancedb");
        tokio::fs::create_dir_all(&tantivy_dir).await.ok();
        tokio::fs::create_dir_all(&lancedb_dir).await.ok();

        let search = engram_index::HybridSearchEngine::new(
            tantivy_dir.clone(),
            lancedb_dir.clone(),
            self.state.cfg.embedding_backend.clone(),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let ps = ProjectState {
            info: ProjectInfo {
                project_id: project_id.to_string(),
                project_name: rec.project_name,
                project_type: rec.project_type,
                directory: rec.directory,
                tantivy_dir,
                lancedb_dir,
            },
            search: std::sync::Arc::new(search),
        };
        self.state.put_project_cached(ps.clone()).await;
        Ok(ps)
    }

    async fn get_active_generation(&self, project_id: &str) -> Result<u64, McpError> {
        let reg = self.state.registry.clone();
        let pid = project_id.to_string();
        let s = tokio::task::spawn_blocking(move || reg.get_meta(&pid, "active_generation"))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(s.and_then(|x| x.parse::<u64>().ok()).unwrap_or(1))
    }

    async fn process_ingest_stats(
        &self,
        project_id: &str,
        generation: u64,
        stats: &engram_index::IngestStats,
    ) -> anyhow::Result<()> {
        let mut nodes = Vec::with_capacity(stats.symbols.len() + stats.all_files.len());

        let fp_map: std::collections::HashMap<_, _> = stats
            .fingerprints
            .iter()
            .map(|fp| (&fp.rel_path, fp))
            .collect();

        for rel_path in &stats.all_files {
            if std::path::Path::new(rel_path.as_str()).is_absolute() {
                anyhow::bail!(
                    "process_ingest_stats: absolute path in all_files: {}",
                    rel_path.as_str()
                );
            }
            let language = engram_core::guess_language(std::path::Path::new(rel_path.as_str()));

            let mut metadata = None;

            if let Some(fp) = fp_map.get(&rel_path.as_str().to_string()) {
                metadata = Some(serde_json::json!({
                    "mtime": fp.mtime_ms / 1000,
                    "size": fp.size,
                    "file_hash": fp.file_hash,
                }));
            }

            nodes.push(engram_graph::Node {
                node_id: engram_core::ids::NodeId::file(rel_path.as_str()).0,
                node_type: "file".into(),
                name: rel_path
                    .file_name()
                    .unwrap_or_else(|| rel_path.as_str())
                    .to_string(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: language.into(),
                file_path: rel_path.clone(),
                start_line: 0,
                end_line: 0,
                generation,
                metadata,
            });
        }

        for (rel_path, sym) in &stats.symbols {
            let language = engram_core::guess_language(std::path::Path::new(rel_path.as_str()));

            let mut metadata = None;
            if let Some(m) = &sym.metadata {
                let mut map = std::collections::HashMap::new();
                for (k, v) in m {
                    map.insert(k.clone(), serde_json::Value::String(v.clone()));
                }
                metadata = Some(serde_json::to_value(map).unwrap_or(serde_json::Value::Null));
            }

            let fqn = sym
                .metadata
                .as_ref()
                .and_then(|m| m.get("fqn"))
                .map(|v| v.as_str());

            let (node_id, final_kind) = if sym.kind == "page" {
                (
                    engram_core::ids::NodeId::page(rel_path.as_str()).0,
                    sym.kind.clone(),
                )
            } else if sym.kind == "control" {
                let control_id = sym
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("control_id"))
                    .map(|s| s.as_str())
                    .unwrap_or(sym.name.as_str());
                (
                    engram_core::ids::NodeId::control(rel_path.as_str(), control_id).0,
                    "control".to_string(),
                )
            } else if sym.kind == "control_ref" {
                // Derive page path: Foo.aspx.designer.cs -> Foo.aspx
                let path_str = rel_path.as_str();
                let page_path = if let Some(idx) = path_str.find(".designer.") {
                    &path_str[..idx]
                } else if let Some(idx) = path_str.find(".aspx.") {
                    &path_str[..idx + 5]
                } else if let Some(idx) = path_str.find(".ascx.") {
                    &path_str[..idx + 5]
                } else {
                    path_str
                };
                (
                    engram_core::ids::NodeId::control(page_path, &sym.name).0,
                    "control".to_string(),
                )
            } else {
                (
                    engram_core::ids::NodeId::symbol(
                        &sym.kind,
                        fqn,
                        rel_path.as_str(),
                        &sym.name,
                        sym.start_line,
                    )
                    .0,
                    sym.kind.clone(),
                )
            };

            nodes.push(engram_graph::Node {
                node_id,
                node_type: final_kind,
                name: sym.name.clone(),
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: language.into(),
                file_path: rel_path.clone(),
                start_line: sym.start_line,
                end_line: sym.end_line,
                generation,
                metadata,
            });
        }

        let mut edges = Vec::with_capacity(stats.edges.len());
        for (rel_path, edge) in &stats.edges {
            let language = engram_core::guess_language(std::path::Path::new(&format!(
                "dummy.{}",
                edge.source_language
            )));

            let source_id = if edge.source_name == "file" || edge.source_kind == "file" {
                let path = if edge.source_name == "file" {
                    rel_path.as_str()
                } else {
                    &edge.source_name
                };
                if std::path::Path::new(path).is_absolute() {
                    anyhow::bail!(
                        "process_ingest_stats: absolute path in edge source: {} (file: {})",
                        path,
                        rel_path.as_str()
                    );
                }
                if edge.source_kind == "page" {
                    engram_core::ids::NodeId::page(path).0
                } else {
                    engram_core::ids::NodeId::file(path).0
                }
            } else if edge.source_kind == "page" {
                engram_core::ids::NodeId::page(rel_path.as_str()).0
            } else if edge.source_kind == "control" {
                let control_id = edge
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("control_id"))
                    .map(|s| s.as_str())
                    .unwrap_or(edge.source_name.as_str());

                // FIX: if we are in a code-behind file, the control belongs to the page
                let path_str = rel_path.as_str();
                let page_path = if path_str.ends_with(".cs") || path_str.ends_with(".vb") {
                    if let Some(idx) = path_str.find(".aspx.") {
                        &path_str[..idx + 5]
                    } else if let Some(idx) = path_str.find(".ascx.") {
                        &path_str[..idx + 5]
                    } else {
                        path_str
                    }
                } else {
                    path_str
                };
                engram_core::ids::NodeId::control(page_path, control_id).0
            } else {
                let fqn = if edge.source_name.contains('.') {
                    Some(edge.source_name.as_str())
                } else {
                    edge.metadata
                        .as_ref()
                        .and_then(|m| m.get("source_fqn"))
                        .map(|s| s.as_str())
                };
                engram_core::ids::NodeId::symbol(
                    &edge.source_kind,
                    fqn,
                    rel_path.as_str(),
                    &edge.source_name,
                    edge.source_start_line,
                )
                .0
            };

            let target_id =
                if edge.target_name == "file" || edge.target_kind.as_deref() == Some("file") {
                    let path = if edge.target_name == "file" {
                        rel_path.as_str()
                    } else {
                        &edge.target_name
                    };
                    if std::path::Path::new(path).is_absolute() {
                        anyhow::bail!(
                            "process_ingest_stats: absolute path in edge target: {} (file: {})",
                            path,
                            rel_path.as_str()
                        );
                    }
                    if edge.target_kind.as_deref() == Some("page") {
                        engram_core::ids::NodeId::page(path).0
                    } else {
                        engram_core::ids::NodeId::file(path).0
                    }
                } else if edge.target_kind.as_deref() == Some("page") {
                    engram_core::ids::NodeId::page(rel_path.as_str()).0
                } else if edge.target_kind.as_deref() == Some("control") {
                    let control_id = edge
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("control_id"))
                        .map(|s| s.as_str())
                        .unwrap_or(edge.target_name.as_str());
                    engram_core::ids::NodeId::control(rel_path.as_str(), control_id).0
                } else if edge.target_kind.as_deref() == Some("control_ref") {
                    let path_str = rel_path.as_str();
                    let page_path = if let Some(idx) = path_str.find(".designer.") {
                        &path_str[..idx]
                    } else {
                        path_str
                    };
                    // Use simple name for control ID matching
                    let simple_name = edge
                        .target_name
                        .split('.')
                        .next_back()
                        .unwrap_or(&edge.target_name);
                    engram_core::ids::NodeId::control(page_path, simple_name).0
                } else if edge.target_name.starts_with("sql:") {
                    edge.target_name.clone()
                } else if let Some(kind) = &edge.target_kind {
                    let fqn = if edge.target_name.contains('.') {
                        Some(edge.target_name.as_str())
                    } else {
                        edge.metadata
                            .as_ref()
                            .and_then(|m| m.get("fqn"))
                            .map(|s| s.as_str())
                    };
                    engram_core::ids::NodeId::symbol(
                        kind,
                        fqn,
                        rel_path.as_str(),
                        &edge.target_name,
                        edge.target_start_line.unwrap_or(0),
                    )
                    .0
                } else {
                    format!("::{}", edge.target_name)
                };

            // Virtual nodes for SQL targets
            if target_id.starts_with("sql:") {
                let sql_name = target_id.split(':').next_back().unwrap_or(&target_id);
                let sql_kind = if target_id.contains(":stored_proc:") {
                    "stored_proc"
                } else {
                    "inline_sql"
                };
                nodes.push(engram_graph::Node {
                    node_id: target_id.clone(),
                    node_type: sql_kind.into(),
                    name: sql_name.to_string(),
                    namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                    language: "sql".into(),
                    file_path: rel_path.clone(),
                    start_line: edge.target_start_line.unwrap_or(0),
                    end_line: edge.target_start_line.unwrap_or(0),
                    generation,
                    metadata: edge.metadata.as_ref().map(|m| {
                        let mut map = std::collections::HashMap::new();
                        for (k, v) in m {
                            map.insert(k.clone(), serde_json::Value::String(v.clone()));
                        }
                        serde_json::to_value(map).unwrap_or(serde_json::Value::Null)
                    }),
                });
            }

            let edge_kind = match edge.kind.as_str() {
                "contains" | "cb_defines" | "inherits" | "codebehind_file" | "codebehind_class" => {
                    engram_graph::EdgeKind::Contains
                }
                "imports" => engram_graph::EdgeKind::Imports,
                "sql_calls" => engram_graph::EdgeKind::SqlCalls,
                _ => engram_graph::EdgeKind::Dependency,
            };

            edges.push(engram_graph::Edge {
                source_id,
                target_id,
                namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
                language: language.into(),
                edge_kind,
                weight: 1,
                generation,
                metadata: edge.metadata.as_ref().map(|m| {
                    let mut map = std::collections::HashMap::new();
                    for (k, v) in m {
                        map.insert(k.clone(), serde_json::Value::String(v.clone()));
                    }
                    serde_json::to_value(map).unwrap_or(serde_json::Value::Null)
                }),
                updated_at_ms: now_ms(),
            });
        }

        if !nodes.is_empty() {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            match tokio::task::spawn_blocking(move || graph.upsert_nodes(&pid, &nodes)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!("graph upsert_nodes failed for {project_id}: {e}");
                    return Err(e);
                }
                Err(e) => {
                    tracing::error!("graph upsert_nodes task panicked for {project_id}: {e}");
                    anyhow::bail!("graph upsert_nodes task panicked: {e}");
                }
            }
        }

        if !edges.is_empty() {
            let graph = self.state.graph.clone();
            let pid = project_id.to_string();
            match tokio::task::spawn_blocking(move || graph.upsert_edges(&pid, &edges)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!("graph upsert_edges failed for {project_id}: {e}");
                    return Err(e);
                }
                Err(e) => {
                    tracing::error!("graph upsert_edges task panicked for {project_id}: {e}");
                    anyhow::bail!("graph upsert_edges task panicked: {e}");
                }
            }
        }

        Ok(())
    }

    async fn inject_repo_rules(
        &self,
        project_id: &str,
        file_path: &engram_core::RelPath,
        content: &str,
    ) -> String {
        let reg = self.state.registry.clone();
        let pid = project_id.to_string();
        let fp = file_path.as_str();
        let rules = tokio::task::spawn_blocking(move || reg.list_repo_rules(&pid))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        let applicable: Vec<RepoRule> = rules
            .into_iter()
            .filter(|r| pattern_match(fp, &r.file_pattern))
            .collect();
        if applicable.is_empty() {
            return content.to_string();
        }

        let mut header = String::new();
        for r in applicable {
            header.push_str(&format!("[Repo Constraint]: {}\n", r.rule_text));
        }
        header.push('\n');
        header + content
    }

    async fn get_incremental_changes(
        &self,
        project_id: &str,
        root: &Path,
        exts: &[&str],
    ) -> anyhow::Result<(Vec<PathBuf>, Vec<engram_core::RelPath>)> {
        // 1. Scan disk (already in spawn_blocking)
        let root_clone = root.to_path_buf();
        let exts_owned: Vec<String> = exts.iter().map(|s| s.to_string()).collect();
        let disk_files = tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = exts_owned.iter().map(|s| s.as_str()).collect();
            engram_index::ingest::iter_files(&root_clone, &refs)
        })
        .await?;

        // 2. Scan DB — only load file_path + metadata, limit to streaming batches
        let graph = self.state.graph.clone();
        let pid = project_id.to_string();
        let db_nodes = tokio::task::spawn_blocking(move || {
            graph.query_nodes(&pid, Some("file"), None, None, 1_000_000)
        })
        .await??;

        // 3. Compare — do the expensive I/O (metadata + hash reads) in spawn_blocking
        let root_owned = root.to_path_buf();
        let (changed, deleted) = tokio::task::spawn_blocking(move || {
            let mut changed = Vec::new();
            let mut deleted = Vec::new();
            let mut db_map = std::collections::HashMap::new();

            for n in db_nodes {
                db_map.insert(n.file_path.clone(), n);
            }

            for p in disk_files {
                let rel = engram_core::RelPath::from_relative(&root_owned, &p)
                    .unwrap_or_else(|| engram_core::RelPath::new(&p.to_string_lossy()));

                let metadata = std::fs::metadata(&p).ok();
                let mtime = metadata
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);

                if let Some(node) = db_map.remove(&rel) {
                    let mut is_changed = true;

                    if let Some(meta) = node.metadata {
                        let stored_mtime = meta.get("mtime").and_then(|v| v.as_u64()).unwrap_or(0);
                        let stored_size = meta.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                        let stored_hash = meta
                            .get("file_hash")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        if stored_mtime == mtime && stored_size == size {
                            // mtime + size match => trust it unless hash disagrees
                            if let Some(ref sh) = stored_hash {
                                if let Ok(bytes) = std::fs::read(&p) {
                                    let current_hash = blake3::hash(&bytes).to_hex().to_string();
                                    if current_hash == *sh {
                                        is_changed = false;
                                    }
                                }
                            } else {
                                is_changed = false;
                            }
                        } else if stored_size == size {
                            // Bug fix: mtime differs but size is same — check hash
                            // (handles git checkout of same-content branch)
                            if let Some(ref sh) = stored_hash {
                                if let Ok(bytes) = std::fs::read(&p) {
                                    let current_hash = blake3::hash(&bytes).to_hex().to_string();
                                    if current_hash == *sh {
                                        is_changed = false;
                                    }
                                }
                            }
                        }
                    }

                    if is_changed {
                        changed.push(p);
                    }
                } else {
                    // New file
                    changed.push(p);
                }
            }

            // Remaining in db_map are deleted
            for (rel, _) in db_map {
                deleted.push(rel);
            }

            (changed, deleted)
        })
        .await?;

        Ok((changed, deleted))
    }

    async fn spawn_job_index_directory(
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
        .ok();

        let reg2 = self.state.registry.clone();
        let projects_cache = self.state.projects.clone();
        let active_jobs = self.state.active_jobs.clone();
        let cancellation_tokens = self.state.cancellation_tokens.clone();
        let state_for_spawn = self.state.clone();
        let project_id_for_job = project_id.clone();
        let job_id_for_job = job_id.clone();

        let active_count = state_for_spawn
            .active_indexing_count
            .load(std::sync::atomic::Ordering::SeqCst);
        if active_count >= state_for_spawn.cfg.max_concurrent_jobs {
            return Err(McpError::internal_error(
                format!(
                    "Too many concurrent jobs running (limit: {})",
                    state_for_spawn.cfg.max_concurrent_jobs
                ),
                None,
            ));
        }

        state_for_spawn
            .active_indexing_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let token = tokio_util::sync::CancellationToken::new();
        {
            let mut m = cancellation_tokens.write().await;
            m.insert(job_id.clone(), token.clone());
        }

        let handle = tokio::spawn(async move {
            let search_init = engram_index::HybridSearchEngine::new(
                tantivy_dir,
                lancedb_dir,
                state_for_spawn.cfg.embedding_backend.clone(),
            )
            .await;

            let res = match search_init {
                Ok(search) => {
                    let exts = exts_for_project_type(&project_type);
                    let job_id_for_cb = job_id_for_job.clone();
                    let reg_for_cb = reg2.clone();
                    let files = engram_index::ingest::iter_files(&directory, &exts);

                    if let Some(limit) = state_for_spawn.cfg.max_project_files {
                        if files.len() as u64 > limit {
                            Err(anyhow::anyhow!(
                                "Too many files: {} > limit {}",
                                files.len(),
                                limit
                            ))
                        } else {
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
                                        let pct = ((curr as f32 / total as f32) * 100.0) as u8;
                                        if let Ok(Some(mut job)) =
                                            reg_for_cb.get_job(&job_id_for_cb)
                                            && (job.progress_pct != pct || curr % 10 == 0)
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
                                    let pct = ((curr as f32 / total as f32) * 100.0) as u8;
                                    if let Ok(Some(mut job)) = reg_for_cb.get_job(&job_id_for_cb)
                                        && (job.progress_pct != pct || curr % 10 == 0)
                                    {
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

            let mut status = "done";

            let mut msg = "completed".to_string();
            let mut progress = 100;

            if token.is_cancelled() {
                status = "cancelled";
                msg = "cancelled by user".to_string();
                progress = 0;
            } else if let Ok(stats) = &res {
                // Real implementation: process AST symbols and link edges
                let engram = Engram::new(state_for_spawn.clone());
                let pid = project_id_for_job.clone();

                if let Err(e) = engram.process_ingest_stats(&pid, 1, stats).await {
                    status = "failed";
                    msg = format!("Graph processing failed: {}", e);
                    progress = 0;
                } else {
                    // Link unresolved edges
                    let _ = engram.state.graph.resolve_symbol_edges(&pid);

                    // Generate and save report
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
                created_at_ms: now,
                updated_at_ms: now,
            };
            let _ = reg2.put_job(&jr);

            // Drop handle and token from active maps
            {
                let mut m = active_jobs.write().await;
                m.remove(&job_id_for_job);
            }
            {
                let mut m = cancellation_tokens.write().await;
                m.remove(&job_id_for_job);
            }

            // Keep cache map untouched; opened engines are independent.
            let _ = projects_cache;
        });

        {
            let mut m = self.state.active_jobs.write().await;
            m.insert(job_id.clone(), handle);
        }

        Ok(job_id)
    }

    async fn spawn_job_update_project(
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
        .ok();

        let state = self.state.clone();
        let job_id_for_job = job_id.clone();
        let project_id_for_job = project_id.clone();

        let active_count = state
            .active_indexing_count
            .load(std::sync::atomic::Ordering::SeqCst);
        if active_count >= state.cfg.max_concurrent_jobs {
            return Err(McpError::internal_error(
                format!(
                    "Too many concurrent jobs running (limit: {})",
                    state.cfg.max_concurrent_jobs
                ),
                None,
            ));
        }

        state
            .active_indexing_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

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
                    // open runtime
                    let project_root = state
                        .cfg
                        .data_dir
                        .join("projects")
                        .join(&project_id_for_job);
                    let tantivy_dir = project_root.join("tantivy");
                    let lancedb_dir = project_root.join("lancedb");
                    let search = engram_index::HybridSearchEngine::new(
                        tantivy_dir,
                        lancedb_dir,
                        state.cfg.embedding_backend.clone(),
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
                            tantivy_dir: PathBuf::new(),
                            lancedb_dir: PathBuf::new(),
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

                if !deleted.is_empty() {
                    ps.search
                        .delete_files(&project_id_for_job, "memory", &deleted)
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;
                }

                let reg_for_progress = reg_for_cb.clone();
                let job_id_for_progress = job_id_for_cb.clone();
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
                            let pct = ((curr as f32 / total as f32) * 100.0) as u8;
                            if let Ok(Some(mut job)) =
                                reg_for_progress.get_job(&job_id_for_progress)
                                && (job.progress_pct != pct || curr % 10 == 0)
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
                    if let Ok(Some(mut job)) = reg_for_cb.get_job(&job_id_for_cb) {
                        job.status = "failed".into();
                        job.message = format!("Graph processing failed: {}", e);
                        job.updated_at_ms = now_ms();
                        let _ = reg_for_cb.put_job(&job);
                    }
                    return Ok(());
                }

                // Link unresolved edges
                let _ = engram.state.graph.resolve_symbol_edges(&project_id_for_job);

                // Git stream
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
                                && (curr % 10 == 0 || curr == total)
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
                // Commit gen best-effort
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
                created_at_ms: now,
                updated_at_ms: now,
            };
            let _ = state.registry.put_job(&jr2);

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

    async fn spawn_job_git_history(
        &self,
        project_id: String,
        max_commits: usize,
        index_antipatterns: bool,
    ) -> Result<String, McpError> {
        let job_id = Uuid::new_v4().to_string();
        let now = now_ms();
        let jr = JobRecord {
            job_id: job_id.clone(),
            kind: "index_git_history".into(),
            project_id: Some(project_id.clone()),
            status: "running".into(),
            message: "walking commits".into(),
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
        .ok();

        let state = self.state.clone();
        let job_id_for_job = job_id.clone();
        let project_id_for_job = project_id.clone();

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
                // open project record
                let rec = state
                    .registry
                    .get_project(&project_id_for_job)?
                    .ok_or_else(|| anyhow::anyhow!("missing project"))?;

                if token.is_cancelled() {
                    return Ok::<(), anyhow::Error>(());
                }

                // stream update
                let engram = Engram::new(state.clone());
                let active_gen = engram
                    .get_active_generation(&project_id_for_job)
                    .await
                    .unwrap_or(1);

                let reg_for_git = state.registry.clone();
                let jid_for_git = job_id_for_job.clone();
                let _ = engram
                    .git_update_stream(
                        &project_id_for_job,
                        &rec.directory,
                        active_gen,
                        max_commits,
                        index_antipatterns,
                        engram_git::history::MergeCommitPolicy::AllParents,
                        &token,
                        Box::new(move |curr, total| {
                            if let Ok(Some(mut job)) = reg_for_git.get_job(&jid_for_git) {
                                let pct = ((curr as f32 / total as f32) * 100.0) as u8;
                                if job.progress_pct != pct || curr % 10 == 0 {
                                    job.progress_pct = pct;
                                    job.message =
                                        format!("Analyzing history: {}/{} commits", curr, total);
                                    job.updated_at_ms = now_ms();
                                    let _ = reg_for_git.put_job(&job);
                                }
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
            }

            let now = now_ms();
            let jr2 = JobRecord {
                job_id: job_id_for_job.clone(),
                kind: "index_git_history".into(),
                project_id: Some(project_id_for_job.clone()),
                status: status.into(),
                message: msg,
                progress_pct: progress,
                estimated_time_remaining_ms: None,
                created_at_ms: now,
                updated_at_ms: now,
            };
            let _ = state.registry.put_job(&jr2);

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

    async fn cancel_job_internal(&self, job_id: &str) -> bool {
        let mut tokens = self.state.cancellation_tokens.write().await;
        if let Some(token) = tokens.remove(job_id) {
            token.cancel();

            // Also abort handle as fallback if not responsive to cooperative cancellation
            let mut handles = self.state.active_jobs.write().await;
            if let Some(h) = handles.remove(job_id) {
                h.abort();
            }

            let now = now_ms();
            let jr = JobRecord {
                job_id: job_id.to_string(),
                kind: "unknown".into(),
                project_id: None,
                status: "cancelled".into(),
                message: "cancelled by user".into(),
                progress_pct: 0,
                estimated_time_remaining_ms: None,
                created_at_ms: now,
                updated_at_ms: now,
            };
            let _ = self.state.registry.put_job(&jr);
            true
        } else {
            false
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn git_update_stream(
        &self,
        project_id: &str,
        directory: &str,
        generation: u64,
        max_commits: usize,
        index_antipatterns: bool,
        policy: engram_git::history::MergeCommitPolicy,
        cancel: &tokio_util::sync::CancellationToken,
        mut progress_cb: Box<dyn FnMut(usize, usize) + Send>,
    ) -> Result<String, McpError> {
        let project_root = PathBuf::from(directory);
        let pid = project_id.to_string();

        let ps = self
            .ensure_project_runtime(project_id)
            .await
            .map_err(|e| McpError::internal_error(e.message, None))?;
        let search = ps.search.clone();

        let reg = self.state.registry.clone();
        let last = tokio::task::spawn_blocking({
            let pid = pid.clone();
            move || reg.get_meta(&pid, "last_git_oid")
        })
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten();

        let cancel_clone = cancel.clone();
        let pid_clone = pid.clone();
        let graph = self.state.graph.clone();
        let active_gen = self.get_active_generation(project_id).await.unwrap_or(1);

        let summary = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let repo = GitWalker::open_repo(&project_root)?;
            let stop = last.as_deref().and_then(|s| git2::Oid::from_str(s).ok());

            let mut temporal_edges: u64 = 0;
            let mut reverts: usize = 0;
            let mut anti_docs: Vec<engram_index::IndexDoc> = Vec::new();
            let mut history_docs: Vec<engram_index::IndexDoc> = Vec::new();
            let mut last_processed_oid: Option<Oid> = None;

            let mut commit_history: Vec<Oid> = Vec::new();

            let commits_processed = GitWalker::walk_commits_streaming(&repo, stop, max_commits, policy, &cancel_clone, |oid, curr, total| {
                progress_cb(curr, total);
                last_processed_oid = Some(oid);
                commit_history.push(oid);
                let changes = GitWalker::files_changed_in_commit(&repo, oid)?;

                // 1. Handle renames first (transfer edges)
                for change in &changes {
                    if let engram_git::history::FileChange::Renamed { old, new } = change {
                        let old_node_id = format!("file:{}", old);
                        let new_node_id = format!("file:{}", new);

                        // We "transfer" edges by finding all neighbors of old and adding them to new.
                        // This is a simplified BFS of 1 hop.
                        if let Ok(neighbors) = graph.neighbors(&pid_clone, EdgeKind::TemporalCoupling, &old_node_id, 1000) {
                            for (neigh_id, weight) in neighbors {
                                let _ = graph.increment_undirected_edge(
                                    &pid_clone,
                                    engram_core::namespaces::NAMESPACE_HISTORY,
                                    "text",
                                    EdgeKind::TemporalCoupling,
                                    &new_node_id,
                                    &neigh_id,
                                    weight,
                                    active_gen,
                                );
                            }
                        }
                    }
                }

                // 2. Normal temporal coupling for all changed files in this commit
                let files: Vec<engram_core::RelPath> = changes.iter().map(|c| c.path().clone()).collect();
                let pairs = engram_git::temporal::file_pairs(&files, 80);

                for (a, b) in &pairs {
                    let na = format!("file:{}", a);
                    let nb = format!("file:{}", b);
                    let _ = graph.increment_undirected_edge(
                        &pid_clone,
                        engram_core::namespaces::NAMESPACE_HISTORY,
                        "text",
                        EdgeKind::TemporalCoupling,
                        &na,
                        &nb,
                        1,
                        active_gen,
                    );
                }
                temporal_edges += pairs.len() as u64;

                let commit = repo.find_commit(oid)?;
                let msg = commit.message().unwrap_or("").to_string();
                let author = commit.author().name().unwrap_or("unknown").to_string();
                let timestamp = commit.time().seconds();

                // Index commit message
                let msg_content = format!("Author: {}\nDate: {}\n\n{}", author, timestamp, msg);
                let msg_content_hash = engram_core::ContentHash::compute(msg_content.as_bytes());
                let msg_doc_id_str = engram_core::DocIdStr::compute(
                    &format!("commit:{}", oid), 0, 0,
                    &msg_content_hash,
                ).0;
                history_docs.push(engram_index::IndexDoc {
                    generation,
                    chunk_id: engram_index::chunk_id_from_content_hash(&msg_content_hash),
                    doc_id: msg_doc_id_str,
                    content_hash: msg_content_hash.0,
                    path: format!("commit:{}", oid).into(),
                    language: "text".into(),
                    content: msg_content,
                    namespace: "history".into(),
                    author: Some(author.clone()),
                    timestamp: Some(timestamp as u64),
                    start_line: 0,
                    end_line: 0,
                });

                // Index diffs (limited)
                let diffs = GitWalker::diff_text_for_commit(&repo, oid, 50_000)?;
                for (path, text) in diffs {
                    let diff_content_hash = engram_core::ContentHash::compute(text.as_bytes());
                    let diff_path_str = format!("diff:{}:{}", oid, path);
                    let diff_doc_id_str = engram_core::DocIdStr::compute(
                        &diff_path_str, 0, 0,
                        &diff_content_hash,
                    ).0;
                    history_docs.push(engram_index::IndexDoc {
                        generation,
                        chunk_id: engram_index::chunk_id_from_content_hash(&diff_content_hash),
                        doc_id: diff_doc_id_str,
                        content_hash: diff_content_hash.0,
                        path: diff_path_str.into(),
                        language: "diff".into(),
                        content: text,
                        namespace: "history".into(),
                        author: Some(author.clone()),
                        timestamp: Some(timestamp as u64),
                        start_line: 0,
                        end_line: 0,
                    });
                }

                // Revert detection
                let mut rev_oid = GitWalker::reverted_oid_from_message(&msg);

                // If not found by string, try structural scan of recent history
                if rev_oid.is_none() && index_antipatterns {
                    // Look back up to 10 commits for a structural revert
                    for old_oid in commit_history.iter().rev().skip(1).take(10) {
                        if let Ok(true) = GitWalker::is_structural_revert(&repo, *old_oid, oid) {
                            rev_oid = Some(*old_oid);
                            break;
                        }
                    }
                }

                if let Some(ro) = rev_oid {
                    reverts += 1;
                    if index_antipatterns {
                        let diffs = GitWalker::diff_text_for_commit(&repo, ro, 200_000)?;
                        for (p, d) in diffs {
                            // Include metadata in the content for better visibility and retrieval
                            let augmented_content = format!(
                                "ANTI-PATTERN\nOriginal Commit: {}\nReverted in Commit: {}\nPath: {}\n\n{}",
                                ro, oid, p, d
                            );
                            let anti_content_hash = engram_core::ContentHash::compute(augmented_content.as_bytes());
                            let anti_doc_id_str = engram_core::DocIdStr::compute(
                                p.as_str(), 0, 0,
                                &anti_content_hash,
                            ).0;

                            anti_docs.push(engram_index::IndexDoc {
                                generation,
                                chunk_id: engram_index::chunk_id_from_content_hash(&anti_content_hash),
                                doc_id: anti_doc_id_str,
                                content_hash: anti_content_hash.0,
                                path: p,
                                language: "code".into(),
                                content: augmented_content,
                                namespace: "antipattern".into(),
                                author: Some(author.clone()),
                                timestamp: Some(timestamp as u64),
                                start_line: 0,
                                end_line: 0,
                            });
                        }
                    }
                }

                // Batch index to avoid huge memory usage
                if history_docs.len() >= 100 {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(search.index_docs(&pid_clone, &history_docs, &cancel_clone))?;
                    history_docs.clear();
                }
                if anti_docs.len() >= 100 {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(search.index_docs(&pid_clone, &anti_docs, &cancel_clone))?;
                    anti_docs.clear();
                }

                Ok(())
            })?;

            // Final flush
            if !history_docs.is_empty() {
                let rt = tokio::runtime::Handle::current();
                rt.block_on(search.index_docs(&pid_clone, &history_docs, &cancel_clone))?;
            }
            if !anti_docs.is_empty() {
                let rt = tokio::runtime::Handle::current();
                rt.block_on(search.index_docs(&pid_clone, &anti_docs, &cancel_clone))?;
            }

            Ok(format!(
                "git_update:\ncommits_processed: {}\ntemporal_edges_added: {}\nreverted_commits: {}\nantipattern_docs: {}\nlast_oid: {}",
                commits_processed,
                temporal_edges,
                reverts,
                0,
                last_processed_oid.map(|o: Oid| o.to_string()).unwrap_or_else(|| "<none>".into())
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Update last_git_oid meta best-effort
        if let Some(last_line) = summary.lines().find(|l| l.starts_with("last_oid: ")) {
            let oid = last_line.trim_start_matches("last_oid: ").trim();
            if oid != "<none>" {
                let reg2 = self.state.registry.clone();
                let pid2 = project_id.to_string();
                let oid2 = oid.to_string();
                tokio::task::spawn_blocking(move || reg2.set_meta(&pid2, "last_git_oid", &oid2))
                    .await
                    .ok();
            }
        }

        Ok(summary)
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn pattern_match(file_path: &str, pattern: &str) -> bool {
    // Very small glob-like matcher:
    // - if pattern contains '*' we treat it like a suffix/prefix/contains check
    // - if pattern starts with '.' treat as suffix
    if pattern.trim().is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with('.') {
        return file_path.ends_with(pattern);
    }
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('*').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return true;
        }
        let mut idx = 0usize;
        for p in parts {
            if let Some(pos) = file_path[idx..].find(p) {
                idx += pos + p.len();
            } else {
                return false;
            }
        }
        return true;
    }
    file_path.contains(pattern)
}

fn stacktrace_to_query(stack: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}|[A-Za-z0-9_\-/\\]+\.(rs|py|js|ts|go|java|cs)")
            .expect("Invalid regex")
    });
    let mut terms: Vec<String> = Vec::new();
    for m in re.find_iter(stack).take(60) {
        let t = m.as_str();
        if t.len() > 80 {
            continue;
        }
        terms.push(t.to_string());
    }
    terms.join(" ")
}

fn code_to_query(code: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").expect("Invalid regex"));
    let mut terms: Vec<String> = Vec::new();
    for m in re.find_iter(code).take(30) {
        let t = m.as_str();
        if t.len() > 30 {
            continue;
        }
        if matches!(t, "self" | "this" | "that" | "Some" | "None" | "Result") {
            continue;
        }
        terms.push(t.to_string());
    }
    terms.join(" ")
}
