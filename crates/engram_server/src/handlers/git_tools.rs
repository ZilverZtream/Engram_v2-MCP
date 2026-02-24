use crate::models::{
    AnalyzeRevertsRequest, AnalyzeTemporalCouplingsRequest, IndexGitHistoryRequest,
    IngestZipHistoryRequest, SearchHistoryRequest,
};
use crate::tools::Engram;
use crate::utils::now_ms;
use engram_core::{JobRecord, RepoRule};
use engram_git::history::GitWalker;
use engram_graph::EdgeKind;
use engram_index::HybridQuery;
use engram_ml::llm_provider::LlmError;
use git2::Oid;
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn is_safe_zip_member_path(name: &str) -> bool {
    let p = std::path::Path::new(name);
    if name.is_empty() || name.contains('\0') || p.is_absolute() {
        return false;
    }
    !p.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    })
}

/// Git-related helper methods on Engram.
impl Engram {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn git_update_stream(
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
            use engram_graph::EdgeKind;

            let repo = GitWalker::open_repo(&project_root)?;
            let stop = last.as_deref().and_then(|s| git2::Oid::from_str(s).ok());

            let mut temporal_edges: u64 = 0;
            let mut reverts: usize = 0;
            let mut anti_docs: Vec<engram_index::IndexDoc> = Vec::new();
            let mut history_docs: Vec<engram_index::IndexDoc> = Vec::new();
            let mut history_batch_bytes: usize = 0;
            let mut anti_batch_bytes: usize = 0;
            let mut last_processed_oid: Option<Oid> = None;
            let mut commit_history: Vec<Oid> = Vec::new();

            let commits_processed = GitWalker::walk_commits_streaming(
                &repo,
                stop,
                max_commits,
                policy,
                &cancel_clone,
                |oid, curr, total| {
                    progress_cb(curr, total);
                    last_processed_oid = Some(oid);
                    commit_history.push(oid);
                    let changes = GitWalker::files_changed_in_commit(&repo, oid)?;

                    let mut commit_edge_batch: Vec<(EdgeKind, String, String, u32)> = Vec::new();

                    // Handle renames
                    for change in &changes {
                        if let engram_git::history::FileChange::Renamed { old, new } = change {
                            let old_node_id = format!("file:{}", old);
                            let new_node_id = format!("file:{}", new);

                            if let Ok(neighbors) = graph.neighbors(
                                &pid_clone,
                                EdgeKind::TemporalCoupling,
                                &old_node_id,
                                1000,
                            ) {
                                for (neigh_id, weight) in neighbors {
                                    if new_node_id != neigh_id {
                                        commit_edge_batch.push((
                                            EdgeKind::TemporalCoupling,
                                            new_node_id.clone(),
                                            neigh_id,
                                            weight,
                                        ));
                                    }
                                }
                            }

                            if let Ok(Some(mut old_node)) = graph.get_node(&pid_clone, &old_node_id)
                            {
                                old_node.generation = 0;
                                let _ = graph.upsert_nodes(&pid_clone, &[old_node]);
                            }
                        }
                    }

                    // Temporal coupling
                    let files: Vec<engram_core::RelPath> =
                        changes.iter().map(|c| c.path().clone()).collect();
                    let pairs = engram_git::temporal::file_pairs(&files, 80);

                    for (a, b) in &pairs {
                        let na = format!("file:{}", a);
                        let nb = format!("file:{}", b);
                        commit_edge_batch.push((EdgeKind::TemporalCoupling, na, nb, 1));
                    }
                    temporal_edges += pairs.len() as u64;

                    if !commit_edge_batch.is_empty() {
                        graph.batch_increment_undirected_edges(
                            &pid_clone,
                            engram_core::namespaces::NAMESPACE_HISTORY,
                            "text",
                            active_gen,
                            &commit_edge_batch,
                        )?;
                    }

                    let commit = repo.find_commit(oid)?;
                    let msg = commit.message().unwrap_or("").to_string();
                    let author = commit.author().name().unwrap_or("unknown").to_string();
                    let timestamp = commit.time().seconds();

                    // Index commit message
                    let msg_content =
                        format!("Author: {}\nDate: {}\n\n{}", author, timestamp, msg);
                    let msg_content_hash =
                        engram_core::ContentHash::compute(msg_content.as_bytes());
                    let msg_doc_id_str = engram_core::DocIdStr::compute(
                        &format!("commit:{}", oid),
                        0,
                        0,
                        &msg_content_hash,
                    )
                    .0;
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

                    // Index diffs
                    let diffs = GitWalker::diff_text_for_commit(&repo, oid, 50_000)?;
                    for (path, text) in diffs {
                        let diff_content_hash =
                            engram_core::ContentHash::compute(text.as_bytes());
                        let diff_path_str = format!("diff:{}:{}", oid, path);
                        let diff_doc_id_str = engram_core::DocIdStr::compute(
                            &diff_path_str,
                            0,
                            0,
                            &diff_content_hash,
                        )
                        .0;
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

                    if rev_oid.is_none() && index_antipatterns {
                        for old_oid in commit_history.iter().rev().skip(1).take(10) {
                            if let Ok(true) =
                                GitWalker::is_structural_revert(&repo, *old_oid, oid)
                            {
                                rev_oid = Some(*old_oid);
                                break;
                            }
                        }
                    }

                    if let Some(ro) = rev_oid {
                        reverts += 1;
                        if index_antipatterns {
                            let diffs =
                                GitWalker::diff_text_for_commit(&repo, ro, 200_000)?;
                            for (p, d) in diffs {
                                let augmented_content = format!(
                                "ANTI-PATTERN\nOriginal Commit: {}\nReverted in Commit: {}\nPath: {}\n\n{}",
                                ro, oid, p, d
                            );
                                let anti_content_hash = engram_core::ContentHash::compute(
                                    augmented_content.as_bytes(),
                                );
                                let anti_doc_id_str = engram_core::DocIdStr::compute(
                                    p.as_str(),
                                    0,
                                    0,
                                    &anti_content_hash,
                                )
                                .0;

                                anti_docs.push(engram_index::IndexDoc {
                                    generation,
                                    chunk_id: engram_index::chunk_id_from_content_hash(
                                        &anti_content_hash,
                                    ),
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

                    history_batch_bytes = history_docs.iter().map(|d| d.content.len()).sum();
                    anti_batch_bytes = anti_docs.iter().map(|d| d.content.len()).sum();

                    const MAX_BATCH_BYTES: usize = 10_000_000;
                    if history_docs.len() >= 100 || history_batch_bytes >= MAX_BATCH_BYTES {
                        let rt = tokio::runtime::Handle::current();
                        rt.block_on(search.index_docs(
                            &pid_clone,
                            &history_docs,
                            &cancel_clone,
                        ))?;
                        history_docs.clear();
                        history_batch_bytes = 0;
                    }
                    if anti_docs.len() >= 100 || anti_batch_bytes >= MAX_BATCH_BYTES {
                        let rt = tokio::runtime::Handle::current();
                        rt.block_on(search.index_docs(&pid_clone, &anti_docs, &cancel_clone))?;
                        anti_docs.clear();
                        anti_batch_bytes = 0;
                    }

                    Ok(())
                },
            )?;

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

    pub(crate) async fn spawn_job_git_history(
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
                let rec = state
                    .registry
                    .get_project(&project_id_for_job)?
                    .ok_or_else(|| anyhow::anyhow!("missing project"))?;

                if token.is_cancelled() {
                    return Ok::<(), anyhow::Error>(());
                }

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
                                if job.progress_pct != pct && (curr % 20 == 0 || curr == total) {
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

    pub async fn handle_index_git_history(
        &self,
        req: IndexGitHistoryRequest,
    ) -> Result<CallToolResult, McpError> {
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

    pub async fn handle_ingest_zip_history(
        &self,
        req: IngestZipHistoryRequest,
    ) -> Result<CallToolResult, McpError> {
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

                const MAX_ARCHIVE_FILES: usize = 250_000;
                const MAX_ENTRY_UNCOMPRESSED_BYTES: u64 = 32 * 1024 * 1024; // 32 MiB
                const MAX_ARCHIVE_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
                const MAX_CHANGED_FILES_PER_SNAPSHOT: usize = 50_000;

                let mut skipped_zips = 0usize;
                let mut skipped_entries = 0usize;
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
                    let mut archive_uncompressed_bytes: u64 = 0;

                    if archive.len() > MAX_ARCHIVE_FILES {
                        tracing::warn!(
                            path = %path.display(),
                            entries = archive.len(),
                            max_entries = MAX_ARCHIVE_FILES,
                            "Skipping oversized zip archive"
                        );
                        skipped_zips += 1;
                        continue;
                    }

                    for j in 0..archive.len() {
                        let mut f = archive.by_index(j)?;
                        if f.is_file() {
                            let name = f.name().to_string();
                            if !is_safe_zip_member_path(&name) {
                                skipped_entries += 1;
                                continue;
                            }

                            let entry_uncompressed = f.size();
                            if entry_uncompressed > MAX_ENTRY_UNCOMPRESSED_BYTES {
                                skipped_entries += 1;
                                tracing::warn!(
                                    path = %path.display(),
                                    entry = %name,
                                    entry_uncompressed,
                                    max = MAX_ENTRY_UNCOMPRESSED_BYTES,
                                    "Skipping oversized zip member"
                                );
                                continue;
                            }

                            archive_uncompressed_bytes = archive_uncompressed_bytes
                                .saturating_add(entry_uncompressed);
                            if archive_uncompressed_bytes > MAX_ARCHIVE_UNCOMPRESSED_BYTES {
                                skipped_entries += 1;
                                tracing::warn!(
                                    path = %path.display(),
                                    accumulated = archive_uncompressed_bytes,
                                    max = MAX_ARCHIVE_UNCOMPRESSED_BYTES,
                                    "Skipping remaining zip members: archive uncompressed budget exceeded"
                                );
                                break;
                            }

                            // Compute a hash with a hard read cap to prevent
                            // decompression-bomb style stream expansion even if
                            // metadata reports an unexpectedly small size.
                            let mut hasher = blake3::Hasher::new();
                            let mut limited = std::io::Read::take(
                                &mut f,
                                MAX_ENTRY_UNCOMPRESSED_BYTES.saturating_add(1),
                            );
                            let copied = std::io::copy(&mut limited, &mut hasher)?;
                            if copied > MAX_ENTRY_UNCOMPRESSED_BYTES {
                                skipped_entries += 1;
                                tracing::warn!(
                                    path = %path.display(),
                                    entry = %name,
                                    copied,
                                    max = MAX_ENTRY_UNCOMPRESSED_BYTES,
                                    "Skipping zip member that exceeded hard streaming cap"
                                );
                                continue;
                            }
                            let hash = hasher.finalize().to_hex().to_string();

                            if current_fingerprints.contains_key(&name) {
                                skipped_entries += 1;
                                tracing::warn!(
                                    path = %path.display(),
                                    entry = %name,
                                    "Skipping duplicate zip member path in same archive"
                                );
                                continue;
                            }

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

                                if changed_files.len() >= MAX_CHANGED_FILES_PER_SNAPSHOT {
                                    tracing::warn!(
                                        path = %path.display(),
                                        max = MAX_CHANGED_FILES_PER_SNAPSHOT,
                                        "Reached changed-file cap for snapshot; truncating remaining diff tracking"
                                    );
                                    break;
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
                if skipped_entries > 0 {
                    summary.push_str(&format!(
                        "\n\u{26a0}\u{fe0f} {} zip entries were skipped (unsafe path or size limit).",
                        skipped_entries
                    ));
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

    pub async fn handle_search_history(
        &self,
        req: SearchHistoryRequest,
    ) -> Result<CallToolResult, McpError> {
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

    pub async fn handle_analyze_temporal_couplings(
        &self,
        req: AnalyzeTemporalCouplingsRequest,
    ) -> Result<CallToolResult, McpError> {
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

    pub async fn handle_analyze_reverts(
        &self,
        req: AnalyzeRevertsRequest,
    ) -> Result<CallToolResult, McpError> {
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
            let max_commits = req.sanitized_max_commits();
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

            let llm_analysis = match dreaming
                .generate_text(&prompt, 200, std::time::Duration::from_secs(15))
                .await
            {
                Ok(text) => text,
                Err(err) => {
                    log_llm_failure("git_tools.revert_antipattern", &rd.rule_id, &err);
                    String::new()
                }
            };

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
}

fn log_llm_failure(operation: &str, target: &str, err: &LlmError) {
    tracing::warn!(
        operation = operation,
        target = target,
        provider = err.provider().unwrap_or("unknown"),
        status_code = err.status_code(),
        retry_exhausted = err.retry_exhausted(),
        error = %err,
        "LLM generation failed; using fallback"
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::is_safe_zip_member_path;

    #[test]
    fn zip_member_path_rejects_traversal_and_absolute() {
        assert!(!is_safe_zip_member_path("../etc/passwd"));
        assert!(!is_safe_zip_member_path("nested/../../escape.txt"));
        assert!(!is_safe_zip_member_path("/abs/path.txt"));
        assert!(!is_safe_zip_member_path("C:/windows/system32"));
    }

    #[test]
    fn zip_member_path_accepts_normal_project_relative_paths() {
        assert!(is_safe_zip_member_path("src/main.rs"));
        assert!(is_safe_zip_member_path("App_Code/Service.vb"));
    }
}
