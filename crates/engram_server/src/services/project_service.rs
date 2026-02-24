use crate::error::EngramError;
use crate::state::{AppState, ProjectInfo, ProjectState};
use engram_core::ProjectRecord;
use std::path::{Path, PathBuf};

/// Ensure a project record exists in the registry. Returns `EngramError::ProjectNotFound` if missing.
pub async fn ensure_project_record(
    state: &AppState,
    project_id: &str,
) -> Result<ProjectRecord, EngramError> {
    validate_project_id(project_id)?;
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let rec = tokio::task::spawn_blocking(move || reg.get_project(&pid)).await??;
    rec.ok_or_else(|| EngramError::ProjectNotFound(project_id.to_string()))
}

/// Validate that `project_id` contains only safe characters to prevent directory traversal.
/// Allowed: ASCII alphanumerics, hyphens, underscores.
pub fn validate_project_id(project_id: &str) -> Result<(), EngramError> {
    // Hard upper bound prevents path/accounting amplification attacks and accidental
    // oversized IDs that can exceed filesystem or DB index limits.
    if project_id.len() > 128 {
        return Err(EngramError::InvalidParams(
            "project_id must be at most 128 characters".into(),
        ));
    }

    if project_id.is_empty()
        || !project_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(EngramError::InvalidParams(
            "project_id must contain only alphanumeric characters, hyphens, or underscores".into(),
        ));
    }
    Ok(())
}

/// Open (or cache-hit) a project's search engine runtime.
pub async fn ensure_project_runtime(
    state: &AppState,
    project_id: &str,
) -> Result<ProjectState, EngramError> {
    validate_project_id(project_id)?;

    if let Some(p) = state.get_project_cached(project_id) {
        return Ok(p);
    }

    let rec = ensure_project_record(state, project_id).await?;
    let project_root = state.cfg.data_dir.join("projects").join(project_id);
    let tantivy_dir = project_root.join("tantivy");
    let lancedb_dir = project_root.join("lancedb");
    tokio::fs::create_dir_all(&tantivy_dir)
        .await
        .map_err(|e| EngramError::Internal(format!("failed to create tantivy dir: {e}")))?;
    tokio::fs::create_dir_all(&lancedb_dir)
        .await
        .map_err(|e| EngramError::Internal(format!("failed to create lancedb dir: {e}")))?;

    let search = engram_index::HybridSearchEngine::new_with_budget(
        tantivy_dir.clone(),
        lancedb_dir.clone(),
        &state.cfg,
        Some(state.memory_budget.clone()),
    )
    .await?;

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
    state.put_project_cached(ps.clone()).await;
    Ok(ps)
}

/// Get the current active generation for a project.
pub async fn get_active_generation(state: &AppState, project_id: &str) -> Result<u64, EngramError> {
    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let s = tokio::task::spawn_blocking(move || reg.get_meta(&pid, "active_generation")).await??;
    let generation = s
        .and_then(|x| x.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(1);
    Ok(generation)
}

/// Generate a human-readable indexing report from ingest stats.
pub fn generate_indexing_report(stats: &engram_index::IngestStats) -> String {
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
        *node_kinds.entry(sym.kind).or_insert(0) += 1;
    }
    report.push_str("- Nodes by kind:\n");
    let mut kinds: Vec<_> = node_kinds.iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, count) in kinds {
        report.push_str(&format!("  - {}: {}\n", kind, count));
    }

    let mut edge_kinds = std::collections::HashMap::new();
    for (_, edge) in &stats.edges {
        *edge_kinds.entry(edge.kind).or_insert(0) += 1;
    }
    report.push_str("- Edges by kind:\n");
    let mut ekinds: Vec<_> = edge_kinds.iter().collect();
    ekinds.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, count) in ekinds {
        report.push_str(&format!("  - {}: {}\n", kind, count));
    }

    let observed_runtime_total: usize = edge_kinds
        .iter()
        .filter(|(kind, _)| {
            let k = kind.to_string();
            k.contains("observed_runtime")
        })
        .map(|(_, count)| *count)
        .sum();
    if observed_runtime_total > 0 {
        report.push_str(&format!(
            "- Runtime-observed edges: {} 🟢 observed at runtime\n",
            observed_runtime_total
        ));
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

/// Compute incremental changes between on-disk files and graph-stored file metadata.
pub async fn get_incremental_changes(
    state: &AppState,
    project_id: &str,
    root: &Path,
    exts: &[&str],
) -> anyhow::Result<(Vec<PathBuf>, Vec<engram_core::RelPath>)> {
    // 1. Scan disk
    let root_clone = root.to_path_buf();
    let exts_owned: Vec<String> = exts.iter().map(|s| s.to_string()).collect();
    let disk_files = tokio::task::spawn_blocking(move || {
        let refs: Vec<&str> = exts_owned.iter().map(|s| s.as_str()).collect();
        engram_index::ingest::iter_files(&root_clone, &refs)
    })
    .await?;

    // 2. Scan DB
    let graph = state.graph.clone();
    let pid = project_id.to_string();
    let db_file_meta =
        tokio::task::spawn_blocking(move || graph.list_file_node_metadata(&pid)).await??;

    // 3. Compare
    let root_owned = root.to_path_buf();
    let (changed, deleted) = tokio::task::spawn_blocking(move || {
        let mut changed = Vec::new();
        let mut deleted = Vec::new();
        let mut db_map = std::collections::HashMap::new();

        for (file_path, metadata) in db_file_meta {
            db_map.insert(file_path, metadata);
        }

        // Hash a file if it is ≤100 MB; returns None for oversized files.
        let stream_hash = |path: &std::path::Path| -> Option<String> {
            let meta = std::fs::metadata(path).ok()?;
            if meta.len() > 100_000_000 {
                return None; // Too large — caller handles None specially (fix 2.3)
            }
            let file = std::fs::File::open(path).ok()?;
            let mut hasher = blake3::Hasher::new();
            let mut reader = std::io::BufReader::new(file);
            std::io::copy(&mut reader, &mut hasher).ok()?;
            Some(hasher.finalize().to_hex().to_string())
        };

        for p in disk_files {
            // Fix 2.4: only keep files that can be expressed as a relative path;
            // skip those that cannot to avoid storing absolute paths as DB keys.
            let rel = match engram_core::RelPath::from_relative(&root_owned, &p) {
                Some(r) => r,
                None => continue,
            };

            // Fix 3.3: if the file vanished between the directory scan and now,
            // treat it as deleted rather than logging it as an empty file.
            let metadata = match std::fs::metadata(&p) {
                Ok(m) => m,
                Err(_) => {
                    db_map.remove(&rel);
                    deleted.push(rel);
                    continue;
                }
            };

            let mtime = metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = metadata.len();

            if let Some(db_meta) = db_map.remove(&rel) {
                let mut is_changed = true;

                if let Some(meta) = db_meta {
                    let stored_mtime = meta.get("mtime").and_then(|v| v.as_u64()).unwrap_or(0);
                    let stored_size = meta.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                    let stored_hash = meta
                        .get("file_hash")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    if stored_mtime == mtime && stored_size == size {
                        if let Some(ref sh) = stored_hash {
                            match stream_hash(&p) {
                                Some(ref current_hash) if current_hash == sh => {
                                    is_changed = false;
                                }
                                // Fix 2.3: file >100 MB, mtime+size unchanged →
                                // treat as unchanged to prevent infinite re-index.
                                None => {
                                    is_changed = false;
                                }
                                _ => {}
                            }
                        } else {
                            is_changed = false;
                        }
                    } else if stored_size == size
                        && let Some(ref sh) = stored_hash
                            && let Some(ref current_hash) = stream_hash(&p)
                                && current_hash == sh {
                                    is_changed = false;
                                }
                            // If >100 MB and size same but mtime different, treat as
                            // changed so it gets re-indexed once.
                }

                if is_changed {
                    changed.push(p);
                }
            } else {
                changed.push(p);
            }
        }

        for (rel, _metadata) in db_map {
            deleted.push(rel);
        }

        (changed, deleted)
    })
    .await?;

    Ok((changed, deleted))
}

/// Scoped repair: repair only a specific subsystem (tantivy_only, vector_only, graph_only).
/// Used by the integrity service for targeted auto-repair.
pub async fn repair_project_scoped(
    state: &AppState,
    project_id: &str,
    scope: &str,
) -> anyhow::Result<String> {
    let _lock = state.acquire_project_update_lock(project_id).await;
    let _ps = ensure_project_runtime(state, project_id).await?;
    let generation = get_active_generation(state, project_id).await?;

    match scope {
        "tantivy_only" => {
            tracing::info!(
                project_id,
                generation,
                "Scoped repair: flagging Tantivy for re-index"
            );
            let reg = state.registry.clone();
            let pid = project_id.to_string();
            tokio::task::spawn_blocking(move || reg.set_meta(&pid, "tantivy_needs_repair", "true"))
                .await??;
            Ok(format!(
                "Tantivy flagged for re-index at generation {generation}"
            ))
        }
        "vector_only" => {
            tracing::info!(
                project_id,
                generation,
                "Scoped repair: flagging vectors for rebuild"
            );
            let reg = state.registry.clone();
            let pid = project_id.to_string();
            tokio::task::spawn_blocking(move || reg.set_meta(&pid, "vector_needs_repair", "true"))
                .await??;
            Ok(format!(
                "Vector index flagged for rebuild at generation {generation}"
            ))
        }
        "graph_only" => {
            tracing::info!(project_id, generation, "Scoped repair: purging graph data");
            let graph = state.graph.clone();
            let pid = project_id.to_string();
            tokio::task::spawn_blocking(move || graph.delete_project_data(&pid)).await??;
            Ok(format!(
                "Graph purged for re-indexing at generation {generation}"
            ))
        }
        _ => {
            anyhow::bail!(
                "Unknown repair scope: {scope}. Valid: tantivy_only, vector_only, graph_only"
            )
        }
    }
}

/// Inject repo rules for a file path into the content header.
pub async fn inject_repo_rules(
    state: &AppState,
    project_id: &str,
    file_path: &engram_core::RelPath,
    content: &str,
) -> String {
    use crate::utils::files::pattern_match;

    let reg = state.registry.clone();
    let pid = project_id.to_string();
    let fp = file_path.as_str();
    let rules = tokio::task::spawn_blocking(move || reg.list_repo_rules(&pid))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();

    let applicable: Vec<engram_core::RepoRule> = rules
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
