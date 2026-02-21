//! Immune system actor.
//!
//! Periodically scans all registered projects for new git revert commits.
//! When a revert is detected, the diff of the *original* (reverted) commit is
//! indexed into a dedicated "antipattern" namespace. The dreamer actor then
//! checks new insight clusters against that namespace using the ImmuneEngine to
//! decide whether to warn or block the insight.
//!
//! Design mirrors v1's immune system:
//! - Detects git revert commits (message "This reverts commit <hash>")
//! - Extracts the original commit's diff as anti-pattern evidence
//! - Indexes each reverted diff as a searchable document in the "antipattern" namespace
//! - Persists a watermark per project so each commit is only processed once

use crate::state::AppState;
use engram_core::{ContentHash, DocIdStr, RelPath};
use engram_git::{AntiPatternDoc, GitWalker};
use engram_index::IndexDoc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How often the immune actor wakes to scan for new reverts.
const SCAN_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes

/// Maximum number of commits to walk per project per scan cycle.
const MAX_COMMITS_PER_SCAN: usize = 200;

/// Maximum bytes of diff text extracted per file per revert commit.
const MAX_DIFF_BYTES: usize = 16_384; // 16 KB

pub async fn run_immune_actor(state: AppState) {
    let mut interval = tokio::time::interval(SCAN_INTERVAL);
    // Don't run immediately on start — let indexing settle first.
    interval.tick().await;

    loop {
        interval.tick().await;

        // Skip while indexing is active (immune scan is low priority).
        if state
            .active_indexing_count
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
        {
            continue;
        }

        let project_ids: Vec<String> = {
            let reg = state.registry.clone();
            tokio::task::spawn_blocking(move || {
                reg.list_projects()
                    .map(|v| v.into_iter().map(|p| p.project_id).collect())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default()
        };

        for pid in project_ids {
            if let Err(e) = scan_project_reverts(&state, &pid).await {
                tracing::debug!(project = %pid, "immune scan error: {e:#}");
            }
        }
    }
}

/// Scan a single project for new revert commits and index anti-patterns.
async fn scan_project_reverts(state: &AppState, project_id: &str) -> anyhow::Result<()> {
    // Resolve the project directory from the registry.
    let pid = project_id.to_string();
    let reg = state.registry.clone();
    let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid))
        .await
        .unwrap_or_else(|_| Ok(None))?;
    let Some(rec) = rec else {
        return Ok(());
    };

    let directory = std::path::PathBuf::from(&rec.directory);

    // Load the watermark — last commit OID we processed for immune scanning.
    let watermark_key = "immune_watermark";
    let pid2 = project_id.to_string();
    let reg2 = state.registry.clone();
    let watermark_str: Option<String> =
        tokio::task::spawn_blocking(move || reg2.get_meta(&pid2, watermark_key).ok().flatten())
            .await
            .unwrap_or(None);

    let stop_oid: Option<git2::Oid> = watermark_str
        .as_deref()
        .and_then(|s| git2::Oid::from_str(s).ok());

    // Get the project's search engine (needed to index anti-pattern docs).
    let project = {
        // Try cache first, then open lazily.
        if let Some(p) = state.get_project_cached(project_id) {
            p
        } else {
            // Open search engine for this project.
            let tantivy_dir = state
                .cfg
                .data_dir
                .join("projects")
                .join(project_id)
                .join("tantivy");
            let lancedb_dir = state
                .cfg
                .data_dir
                .join("projects")
                .join(project_id)
                .join("lancedb");
            std::fs::create_dir_all(&tantivy_dir).ok();
            std::fs::create_dir_all(&lancedb_dir).ok();

            let search = engram_index::HybridSearchEngine::new_with_budget(
                tantivy_dir.clone(),
                lancedb_dir,
                &state.cfg,
                Some(state.memory_budget.clone()),
            )
            .await?;

            let ps = crate::state::ProjectState {
                info: crate::state::ProjectInfo {
                    project_id: project_id.to_string(),
                    project_name: rec.project_name.clone(),
                    project_type: rec.project_type.clone(),
                    directory: rec.directory.clone(),
                    tantivy_dir,
                    lancedb_dir: state
                        .cfg
                        .data_dir
                        .join("projects")
                        .join(project_id)
                        .join("lancedb"),
                },
                search: std::sync::Arc::new(search),
            };
            state.put_project_cached(ps.clone()).await;
            ps
        }
    };

    // Everything from here is CPU-bound git I/O — run in spawn_blocking.
    let pid3 = project_id.to_string();
    let anti_patterns: Vec<AntiPatternDoc> = tokio::task::spawn_blocking(move || {
        scan_reverts_blocking(&directory, stop_oid, MAX_COMMITS_PER_SCAN, MAX_DIFF_BYTES)
    })
    .await
    .unwrap_or_else(|_| Ok(Vec::new()))?;

    if anti_patterns.is_empty() {
        return Ok(());
    }

    tracing::info!(
        project = %project_id,
        count = anti_patterns.len(),
        "ImmuneActor: indexing anti-pattern documents from reverts"
    );

    // Determine current generation.
    let active_gen: u64 = {
        let reg3 = state.registry.clone();
        let pid4 = project_id.to_string();
        tokio::task::spawn_blocking(move || {
            reg3.get_meta(&pid4, "active_generation")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1)
        })
        .await
        .unwrap_or(1)
    };

    let namespace = engram_core::namespaces::NAMESPACE_ANTIPATTERN;
    let effective_gen = if let Ok(policy) = engram_core::get_policy(namespace) {
        if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
            0
        } else {
            active_gen
        }
    } else {
        active_gen
    };

    let cancel = CancellationToken::new();
    let mut docs: Vec<IndexDoc> = Vec::new();

    for ap in &anti_patterns {
        let content = format!(
            "# Anti-pattern from reverted commit {}\n\nFile: {}\n\n```diff\n{}\n```",
            ap.original_commit,
            ap.file_path.as_str(),
            ap.diff_text,
        );

        let content_hash = ContentHash::compute(content.as_bytes());
        let synthetic_path = format!(
            "__antipatterns/{}/{}.diff",
            ap.original_commit,
            ap.file_path.as_str().replace('/', "_")
        );
        let doc_id = DocIdStr::compute(&synthetic_path, 0, 0, &content_hash);
        let chunk_id = {
            let h = blake3::hash(content_hash.0.as_bytes());
            let mut b = [0u8; 8];
            b.copy_from_slice(&h.as_bytes()[..8]);
            u64::from_le_bytes(b)
        };

        docs.push(IndexDoc {
            generation: effective_gen,
            chunk_id,
            path: RelPath::new(&synthetic_path),
            language: "diff".into(),
            content,
            namespace: namespace.into(),
            author: None,
            timestamp: None,
            start_line: 0,
            end_line: 0,
            doc_id: doc_id.0,
            content_hash: content_hash.0,
        });
    }

    // Index all anti-pattern docs in one batch.
    project.search.index_docs(&pid3, &docs, &cancel).await?;

    // Advance the watermark to the most recent commit OID.
    // (The most-recently-processed commit's OID is the last one the walker returned,
    // which is the newest commit seen. We persist it so next scan starts there.)
    // We store the first anti-pattern's commit hash as an approximation; a more
    // precise approach would track the actual last-scanned HEAD OID.
    if let Some(first) = anti_patterns.first() {
        let pid5 = project_id.to_string();
        let reg5 = state.registry.clone();
        let new_watermark = first.original_commit.clone();
        tokio::task::spawn_blocking(move || {
            reg5.set_meta(&pid5, watermark_key, &new_watermark).ok();
        })
        .await
        .ok();
    }

    Ok(())
}

/// CPU-bound: open repo, walk commits, find reverts, extract anti-patterns.
fn scan_reverts_blocking(
    directory: &std::path::Path,
    stop_oid: Option<git2::Oid>,
    max_commits: usize,
    max_diff_bytes: usize,
) -> anyhow::Result<Vec<AntiPatternDoc>> {
    let repo = GitWalker::open_repo(directory)?;
    let cancel = CancellationToken::new(); // never cancelled inside blocking scope

    let oids = GitWalker::walk_new_commits(
        &repo,
        stop_oid,
        max_commits,
        engram_git::history::MergeCommitPolicy::FirstParentOnly,
        &cancel,
    )?;

    let mut out: Vec<AntiPatternDoc> = Vec::new();
    for oid in oids {
        let commit = repo.find_commit(oid)?;
        let msg = commit.message().unwrap_or("");
        if msg.contains("This reverts commit") {
            match GitWalker::extract_antipatterns_from_reverts(&repo, oid, max_diff_bytes) {
                Ok(mut aps) => out.append(&mut aps),
                Err(e) => {
                    tracing::debug!("extract_antipatterns error for {oid}: {e:#}");
                }
            }
        }
    }

    Ok(out)
}
