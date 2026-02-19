use crate::tools::Engram;
use crate::utils::now_ms;
use engram_core::JobRecord;
use engram_git::history::GitWalker;
use git2::Oid;
use rmcp::ErrorData as McpError;
use std::path::PathBuf;
use uuid::Uuid;

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
}
