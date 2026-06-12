use crate::handlers::validate_project_id;
use crate::models::{
    AnalyzeRevertsRequest, AnalyzeTemporalCouplingsRequest, GitHistoryMode, IndexGitHistoryRequest,
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
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
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

/// Core blocking logic for zip-snapshot history ingestion.
/// Shared between the synchronous (wait=true) and background (wait=false) paths.
///
/// `cancel` is checked once per zip file; when cancelled the function returns
/// `Err("job cancelled")` immediately, allowing the blocking thread to exit
/// cooperatively without processing all remaining archives.
fn zip_history_core(
    dir: std::path::PathBuf,
    graph: std::sync::Arc<engram_graph::GraphStore>,
    project_id: String,
    active_gen: u64,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> anyhow::Result<String> {
    fn extract_first_number(s: &str) -> u64 {
        let digits: String = s
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().unwrap_or(u64::MAX)
    }

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

    zip_files.sort_by_cached_key(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        let num = extract_first_number(&name);
        (num, name)
    });

    if zip_files.len() < 2 {
        return Ok("Need at least 2 zip files to compute pseudo-history.".to_string());
    }

    let all_non_numeric = zip_files
        .iter()
        .all(|e| extract_first_number(&e.file_name().to_string_lossy()) == u64::MAX);
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
        if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            anyhow::bail!("job cancelled");
        }
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

                archive_uncompressed_bytes =
                    archive_uncompressed_bytes.saturating_add(entry_uncompressed);
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

                let mut hasher = blake3::Hasher::new();
                let mut limited =
                    std::io::Read::take(&mut f, MAX_ENTRY_UNCOMPRESSED_BYTES.saturating_add(1));
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
        summary.push_str(&format!(
            "\n\u{26a0}\u{fe0f} {} zip files were skipped (corrupt or unreadable).",
            skipped_zips
        ));
    }
    if skipped_entries > 0 {
        summary.push_str(&format!(
            "\n\u{26a0}\u{fe0f} {} zip entries were skipped (unsafe path or size limit).",
            skipped_entries
        ));
    }
    Ok(summary)
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
        mode: GitHistoryMode,
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
        let oldest = tokio::task::spawn_blocking({
            let pid = pid.clone();
            let reg = self.state.registry.clone();
            move || reg.get_meta(&pid, "oldest_indexed_git_oid")
        })
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten();

        let cancel_clone = cancel.clone();
        let pid_clone = pid.clone();
        let graph = self.state.graph.clone();
        let active_gen = self.get_active_generation(project_id).await.unwrap_or(1);

        // ── Channel pipeline ─────────────────────────────────────────────
        // Bounded channel (64 slots) provides backpressure: if the consumer
        // falls behind, the producer blocks on send — no unbounded growth.
        let (doc_tx, mut doc_rx) = tokio::sync::mpsc::channel::<Vec<engram_index::IndexDoc>>(64);

        // ── Async consumer: Tantivy bulk writer + vector embedding ───────
        let search_consumer = search.clone();
        let cancel_consumer = cancel.clone();
        let pid_consumer = pid.clone();
        let consumer_handle = tokio::spawn(async move {
            // BulkWriterGuard: commits + waits on Drop (cancel-safe).
            // The single per-process tantivy writer slot can be held
            // transiently by the GC actor or setup steps — retry briefly
            // instead of failing the whole history run on a race.
            let mut guard = {
                let mut attempt = 0u32;
                loop {
                    match search_consumer.create_bulk_writer() {
                        Ok(g) => break g,
                        Err(e) if attempt < 5 => {
                            attempt += 1;
                            tracing::warn!(
                                "bulk writer busy (attempt {attempt}/5): {e:#}; retrying"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
            };
            let fields = search_consumer.fields();
            // Separate vector queues per namespace to avoid heterogeneous batch errors.
            let mut vector_queues: std::collections::HashMap<String, Vec<engram_index::IndexDoc>> =
                std::collections::HashMap::new();
            const TANTIVY_COMMIT_EVERY: usize = 1000;
            const VECTOR_FLUSH_EVERY: usize = 500;
            let mut unembedded_docs: usize = 0;

            while let Some(batch) = doc_rx.recv().await {
                if cancel_consumer.is_cancelled() {
                    break;
                }

                engram_index::HybridSearchEngine::write_docs_to_writer(
                    &fields,
                    &mut guard,
                    &pid_consumer,
                    &batch,
                )?;
                guard.maybe_commit(TANTIVY_COMMIT_EVERY)?;

                // Partition docs by namespace before queuing for vector upsert.
                for doc in batch {
                    vector_queues
                        .entry(doc.namespace.clone())
                        .or_default()
                        .push(doc);
                }

                // Flush any namespace queue that hit the threshold.
                // 16c: vector failures degrade (docs stay searchable via
                // tantivy) — they must not kill a multi-thousand-commit
                // walk. Count and report instead.
                for (_ns, queue) in vector_queues.iter_mut() {
                    if queue.len() >= VECTOR_FLUSH_EVERY {
                        let vq = std::mem::take(queue);
                        if let Err(e) = search_consumer
                            .embed_and_upsert_vectors(&pid_consumer, &vq, &cancel_consumer)
                            .await
                        {
                            unembedded_docs += vq.len();
                            tracing::warn!(
                                "history vector batch failed ({} docs, total unembedded {}): {e:#}",
                                vq.len(),
                                unembedded_docs
                            );
                        }
                    }
                }
            }

            // Final vector flush — each namespace separately; same
            // degrade-not-die policy.
            if !cancel_consumer.is_cancelled() {
                for (_ns, queue) in vector_queues.drain() {
                    if !queue.is_empty() {
                        if let Err(e) = search_consumer
                            .embed_and_upsert_vectors(&pid_consumer, &queue, &cancel_consumer)
                            .await
                        {
                            unembedded_docs += queue.len();
                            tracing::warn!(
                                "history final vector flush failed ({} docs): {e:#}",
                                queue.len()
                            );
                        }
                    }
                }
            }
            if unembedded_docs > 0 {
                tracing::warn!(
                    "history indexing completed with {unembedded_docs} doc(s) unembedded — \
                     lexical search covers them; rerun index_git_history to backfill vectors"
                );
            }

            // finish() commits + waits for merge threads (the one expensive call).
            // If this is reached via cancel, Drop will do best-effort instead.
            guard.finish()?;
            Ok::<(), anyhow::Error>(())
        });

        // ── Blocking producer: git walk + graph writes ───────────────────
        let summary = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            use engram_graph::EdgeKind;

            let repo = GitWalker::open_repo(&project_root)?;
            let stop = last.as_deref().and_then(|s| git2::Oid::from_str(s).ok());
            let start_backfill = oldest.as_deref().and_then(|s| git2::Oid::from_str(s).ok());

            let mut temporal_edges: u64 = 0;
            let mut reverts: usize = 0;
            let mut history_docs: Vec<engram_index::IndexDoc> = Vec::new();
            let mut history_batch_bytes: usize = 0;
            let mut anti_docs: Vec<engram_index::IndexDoc> = Vec::new();
            let mut anti_batch_bytes: usize = 0;
            let newest_processed_oid: Cell<Option<Oid>> = Cell::new(None);
            let oldest_processed_oid: Cell<Option<Oid>> = Cell::new(None);
            // Phase flag: watermark bookkeeping differs between the forward
            // walk (oldest→newest) and the backfill walk (newest→oldest).
            let in_backfill: Cell<bool> = Cell::new(false);
            let backfill_oldest_oid: Cell<Option<Oid>> = Cell::new(None);
            // Only need last ~10 commits for revert detection — cap at 12.
            let mut commit_history: VecDeque<Oid> = VecDeque::with_capacity(12);
            let mut processed_total = 0usize;

            // ── Batched graph edge accumulator ───────────────────────────
            // Merge edge weights in-memory, flush every 50 commits.
            let mut edge_accum: HashMap<(EdgeKind, String, String), u32> = HashMap::new();
            let mut commits_since_edge_flush = 0u32;
            let mut rename_nodes: Vec<engram_graph::Node> = Vec::new();
            const EDGE_FLUSH_EVERY: u32 = 50;

            const MAX_BATCH_DOCS: usize = 200;
            const MAX_BATCH_BYTES: usize = 10_000_000;

            let doc_tx_ref = &doc_tx;
            let rt = tokio::runtime::Handle::current();

            let mut process_commit = |oid: Oid, curr: usize, total: usize| -> anyhow::Result<()> {
                progress_cb(curr, total);
                if in_backfill.get() {
                    // Backfill walks newest→oldest: the LAST processed oid is
                    // the new oldest watermark. Never touch the forward
                    // (newest) watermark here — overwriting it with an old
                    // commit would make the next incremental run re-process
                    // (and double-count) everything newer.
                    backfill_oldest_oid.set(Some(oid));
                } else {
                    newest_processed_oid.set(Some(oid));
                    if oldest_processed_oid.get().is_none() {
                        oldest_processed_oid.set(Some(oid));
                    }
                }
                commit_history.push_back(oid);
                if commit_history.len() > 12 {
                    commit_history.pop_front();
                }
                let changes = GitWalker::files_changed_in_commit(&repo, oid)?;

                // ── Handle renames (batch node upserts) ──────────────
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
                                    *edge_accum
                                        .entry((
                                            EdgeKind::TemporalCoupling,
                                            new_node_id.clone(),
                                            neigh_id,
                                        ))
                                        .or_default() += weight;
                                }
                            }
                        }

                        if let Ok(Some(mut old_node)) =
                            graph.get_node(&pid_clone, &old_node_id)
                        {
                            old_node.generation = 0;
                            rename_nodes.push(old_node);
                        }
                    }
                }

                // Flush batched rename nodes
                if !rename_nodes.is_empty() {
                    let _ = graph.upsert_nodes(&pid_clone, &rename_nodes);
                    rename_nodes.clear();
                }

                // ── Temporal coupling ────────────────────────────────
                let files: Vec<engram_core::RelPath> =
                    changes.iter().map(|c| c.path().clone()).collect();
                let pairs = engram_git::temporal::file_pairs(&files, 80);

                for (a, b) in &pairs {
                    let na = format!("file:{}", a);
                    let nb = format!("file:{}", b);
                    *edge_accum
                        .entry((EdgeKind::TemporalCoupling, na, nb))
                        .or_default() += 1;
                }
                temporal_edges += pairs.len() as u64;

                // ── Flush graph edges every N commits ────────────────
                commits_since_edge_flush += 1;
                if commits_since_edge_flush >= EDGE_FLUSH_EVERY {
                    if !edge_accum.is_empty() {
                        let batch: Vec<_> = edge_accum
                            .drain()
                            .map(|((k, s, t), w)| (k, s, t, w))
                            .collect();
                        graph.batch_increment_undirected_edges(
                            &pid_clone,
                            engram_core::namespaces::NAMESPACE_HISTORY,
                            "text",
                            active_gen,
                            &batch,
                        )?;
                    }
                    commits_since_edge_flush = 0;
                }

                // ── Index commit message ─────────────────────────────
                let commit = repo.find_commit(oid)?;
                let msg = commit.message().unwrap_or("").to_string();
                let author = commit.author().name().unwrap_or("unknown").to_string();
                let timestamp = commit.time().seconds();

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
                history_batch_bytes += msg_content.len();
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

                // ── Index diffs ──────────────────────────────────────
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
                    history_batch_bytes += text.len();
                    history_docs.push(engram_index::IndexDoc {
                        generation,
                        chunk_id: engram_index::chunk_id_from_content_hash(
                            &diff_content_hash,
                        ),
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

                // ── Revert detection ─────────────────────────────────
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

                            anti_batch_bytes += augmented_content.len();
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

                // ── Send history doc batch through channel ───────────
                if history_docs.len() >= MAX_BATCH_DOCS
                    || history_batch_bytes >= MAX_BATCH_BYTES
                {
                    let batch = std::mem::take(&mut history_docs);
                    history_batch_bytes = 0;
                    rt.block_on(doc_tx_ref.send(batch))
                        .map_err(|_| anyhow::anyhow!("index consumer dropped"))?;
                }

                // ── Send anti-pattern doc batch through channel ──────
                if anti_docs.len() >= MAX_BATCH_DOCS
                    || anti_batch_bytes >= MAX_BATCH_BYTES
                {
                    let batch = std::mem::take(&mut anti_docs);
                    anti_batch_bytes = 0;
                    rt.block_on(doc_tx_ref.send(batch))
                        .map_err(|_| anyhow::anyhow!("index consumer dropped"))?;
                }

                Ok(())
            };

            let forward_processed =
                if matches!(mode, GitHistoryMode::Forward | GitHistoryMode::Both) {
                    GitWalker::walk_commits_streaming(
                        &repo,
                        stop,
                        max_commits,
                        policy,
                        &cancel_clone,
                        &mut process_commit,
                    )?
                } else {
                    0
                };
            processed_total += forward_processed;

            let remaining = max_commits.saturating_sub(processed_total);
            let backfill_processed = if remaining > 0
                && matches!(mode, GitHistoryMode::Backfill | GitHistoryMode::Both)
            {
                // The PERSISTED oldest watermark takes precedence: on an
                // incremental run the forward walk only covered new commits,
                // and starting backfill from this run's oldest would re-walk
                // (and double-count temporal couplings for) every commit
                // already indexed in previous runs.
                let backfill_start = start_backfill.or(oldest_processed_oid.get());
                in_backfill.set(true);
                GitWalker::walk_older_commits_streaming(
                    &repo,
                    backfill_start,
                    remaining,
                    policy,
                    &cancel_clone,
                    &mut process_commit,
                )?
            } else {
                0
            };

            let commits_processed = processed_total + backfill_processed;
            let effective_last_oid = newest_processed_oid.get().or(stop);
            let effective_oldest_oid = backfill_oldest_oid
                .get()
                .or(start_backfill)
                .or(oldest_processed_oid.get());

            // ── Final edge flush ─────────────────────────────────────
            if !edge_accum.is_empty() {
                let batch: Vec<_> = edge_accum
                    .drain()
                    .map(|((k, s, t), w)| (k, s, t, w))
                    .collect();
                graph.batch_increment_undirected_edges(
                    &pid_clone,
                    engram_core::namespaces::NAMESPACE_HISTORY,
                    "text",
                    active_gen,
                    &batch,
                )?;
            }

            // ── Final doc flushes through channel ────────────────────
            if !history_docs.is_empty() {
                rt.block_on(doc_tx_ref.send(history_docs))
                    .map_err(|_| anyhow::anyhow!("index consumer dropped"))?;
            }
            if !anti_docs.is_empty() {
                rt.block_on(doc_tx_ref.send(anti_docs))
                    .map_err(|_| anyhow::anyhow!("index consumer dropped"))?;
            }

            // Drop sender to signal consumer that no more batches are coming.
            drop(doc_tx);

            let diagnostic = if commits_processed == 0 {
                match mode {
                    GitHistoryMode::Forward => {
                        "No new commits at HEAD past last_oid. To backfill older history, set mode='backfill' or mode='both'."
                    }
                    GitHistoryMode::Backfill => {
                        "No older commits were found beyond oldest_indexed_oid. History backfill may already be complete."
                    }
                    GitHistoryMode::Both => {
                        "No new HEAD commits and no older commits found; repository history appears fully indexed."
                    }
                }
            } else if commits_processed >= max_commits {
                "max_commits cap reached; re-run with mode='both' to continue indexing remaining history."
            } else {
                "ok"
            };

            Ok(format!(
                "git_update:\ncommits_processed: {}\ntemporal_edges_added: {}\nreverted_commits: {}\nantipattern_docs: {}\nlast_oid: {}\noldest_indexed_oid: {}\ndiagnostic: {}",
                commits_processed,
                temporal_edges,
                reverts,
                0,
                effective_last_oid.map(|o: Oid| o.to_string()).unwrap_or_else(|| "<none>".into()),
                effective_oldest_oid
                    .map(|o: Oid| o.to_string())
                    .unwrap_or_else(|| "<none>".into()),
                diagnostic
            ))
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // ── Wait for consumer BEFORE judging the producer ─────────────────
        // When the consumer errors, the channel closes and the producer
        // fails with the opaque "index consumer dropped" — awaiting the
        // consumer first surfaces the ROOT error (embed failure, writer
        // lock, ...) instead of masking it.
        let consumer_result = consumer_handle
            .await
            .map_err(|e| McpError::internal_error(format!("index consumer panicked: {e}"), None))?;
        if let Err(consumer_err) = consumer_result {
            return Err(McpError::internal_error(
                format!("index consumer failed: {consumer_err:#}"),
                None,
            ));
        }
        let summary = summary.map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Update git checkpoints meta best-effort.
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
        if let Some(oldest_line) = summary
            .lines()
            .find(|l| l.starts_with("oldest_indexed_oid: "))
        {
            let oid = oldest_line
                .trim_start_matches("oldest_indexed_oid: ")
                .trim();
            if oid != "<none>" {
                let reg2 = self.state.registry.clone();
                let pid2 = project_id.to_string();
                let oid2 = oid.to_string();
                tokio::task::spawn_blocking(move || {
                    reg2.set_meta(&pid2, "oldest_indexed_git_oid", &oid2)
                })
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
        mode: GitHistoryMode,
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

        // X5: increment active_indexing_count so GC skips purge ticks while
        // git history indexing is in flight — mirrors the project_tools pattern.
        state
            .active_indexing_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let state_for_decrement = state.clone();

        let handle = tokio::spawn(async move {
            // RAII decrement: runs on task exit for any reason (success/fail/cancel).
            struct ActiveGuard(crate::state::AppState);
            impl Drop for ActiveGuard {
                fn drop(&mut self) {
                    self.0
                        .active_indexing_count
                        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _active_guard = ActiveGuard(state_for_decrement);

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
                        mode,
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
            // M-2 fix: log errors instead of silently discarding them.
            // A failure here leaves the job perpetually in "running" state.
            match tokio::task::spawn_blocking(move || reg_final.put_job(&jr2)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    job_id = %job_id_for_job,
                    "failed to persist final job state: {e:#}"
                ),
                Err(e) => tracing::warn!(
                    job_id = %job_id_for_job,
                    "spawn_blocking panicked writing final job state: {e}"
                ),
            }

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
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;

        if req.wait {
            let active_gen = self.get_active_generation(&req.project_id).await?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let mode = req.sanitized_mode();
            let summary = self
                .git_update_stream(
                    &req.project_id,
                    &ps.info.directory,
                    active_gen,
                    req.sanitized_max_commits(),
                    mode,
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
                req.sanitized_mode(),
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
        validate_project_id(&req.project_id)?;
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
            let summary = tokio::task::spawn_blocking(move || {
                zip_history_core(dir, graph, project_id, active_gen, None)
            })
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            return Ok(CallToolResult::success(vec![Content::text(summary)]));
        }

        // Background (wait=false) path: persist a "running" job record, wire a
        // cancellation token, spawn the ingestion in the background, and return
        // the job_id immediately.
        let job_id = Uuid::new_v4().to_string();
        let now_ts = now_ms();
        let jr_running = JobRecord {
            job_id: job_id.clone(),
            kind: "ingest_zip_history".into(),
            project_id: Some(project_id.clone()),
            status: "running".into(),
            message: "zip history ingestion running in background".into(),
            progress_pct: 0,
            estimated_time_remaining_ms: None,
            created_at_ms: now_ts,
            updated_at_ms: now_ts,
        };
        let reg_bg = self.state.registry.clone();
        tokio::task::spawn_blocking({
            let jr = jr_running.clone();
            move || reg_bg.put_job(&jr)
        })
        .await
        .map_err(|e| McpError::internal_error(format!("failed to persist job: {e}"), None))?
        .map_err(|e| {
            McpError::internal_error(format!("failed to persist job record: {e}"), None)
        })?;

        // Register a cancellation token so cancel_job_internal can cooperatively
        // stop the blocking zip workload before it processes all archives.
        let cancel_token = tokio_util::sync::CancellationToken::new();
        {
            let mut tokens = self.state.cancellation_tokens.write().await;
            tokens.insert(job_id.clone(), cancel_token.clone());
        }

        let state_bg = self.state.clone();
        let jid = job_id.clone();
        let pid_bg = project_id.clone();
        let handle = tokio::spawn(async move {
            let pid_for_core = pid_bg.clone();
            let cancel_for_core = cancel_token.clone();
            let result = tokio::task::spawn_blocking(move || {
                zip_history_core(dir, graph, pid_for_core, active_gen, Some(cancel_for_core))
            })
            .await;

            let (status, message, pct) = match result {
                Ok(Ok(s)) => ("completed".to_string(), s, 100u8),
                Ok(Err(e)) => ("failed".to_string(), e.to_string(), 0u8),
                Err(e) => ("failed".to_string(), format!("task panicked: {e}"), 0u8),
            };
            let reg2 = state_bg.registry.clone();
            let jid2 = jid.clone();
            let now2 = now_ms();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = reg2.put_job(&JobRecord {
                    job_id: jid2,
                    kind: "ingest_zip_history".into(),
                    project_id: Some(pid_bg),
                    status,
                    message,
                    progress_pct: pct,
                    estimated_time_remaining_ms: None,
                    created_at_ms: now_ts,
                    updated_at_ms: now2,
                });
            })
            .await;
            // Clean up both tracking maps regardless of completion path.
            state_bg.active_jobs.write().await.remove(&jid);
            state_bg.cancellation_tokens.write().await.remove(&jid);
        });

        {
            self.state
                .active_jobs
                .write()
                .await
                .insert(job_id.clone(), handle);
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "\u{1F7E1} Zip history ingestion started.\njob_id: {job_id}\nproject_id: {project_id}"
        ))]))
    }

    pub async fn handle_search_history(
        &self,
        req: SearchHistoryRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;
        let content_limit = req.max_content_chars;
        let limit = req.sanitized_limit();

        // fts_mode is now a validated enum — invalid values are rejected by serde
        // at the request boundary before this handler runs.
        let fts_mode = req.fts_mode.as_str().to_owned();

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
                &tokio_util::sync::CancellationToken::new(),
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
        validate_project_id(&req.project_id)?;
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
        validate_project_id(&req.project_id)?;
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
            // H-1 fix: propagate errors so the caller knows the graph is out
            // of sync rather than silently returning success with stale state.
            tokio::task::spawn_blocking(move || graph.upsert_edges(&pid, &ap_edges))
                .await
                .map_err(|e| {
                    McpError::internal_error(
                        format!("spawn_blocking join error persisting anti-pattern edges: {e}"),
                        None,
                    )
                })?
                .map_err(|e| {
                    McpError::internal_error(
                        format!("failed to persist anti-pattern edges to graph: {e:#}"),
                        None,
                    )
                })?;
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
