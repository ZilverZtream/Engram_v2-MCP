//! Handler for the flagship `pre_commit_review` tool.
//!
//! The handler orchestrates the 10 gates in `pre_commit_review_service`
//! and formats the result as either markdown (human-facing) or JSON
//! (CI-facing). All the heavy lifting lives in the service module — this
//! is deliberately thin.

use std::path::PathBuf;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::handlers::validate_project_id;
use crate::models::requests::PreCommitReviewRequest;
use crate::services::pre_commit_review_service::{
    ReviewConfig, Severity, render_compact_markdown, render_json, render_markdown, resolve_diff_source,
    run_pre_commit_review_with_source_view,
};
use crate::services::project_service::ensure_project_record;
use crate::tools::Engram;
mod prior_context;
mod response_delivery;

impl Engram {
    pub async fn handle_pre_commit_review(
        &self,
        req: PreCommitReviewRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let budget = response_delivery::budget(req.max_tokens)?;
        if let Some(id) = req.response_id.as_deref() {
            let body = super::planning_tools::change_set_delivery::recover_review(
                &self.state.cfg.data_dir, &req.project_id, id, req.response_offset, budget)?;
            return Ok(CallToolResult::success(vec![Content::text(body)]));
        }
        if req.response_offset != 0 {
            return Err(McpError::invalid_params("response_offset requires response_id from a review recovery action", None));
        }
        let min_severity = Severity::from_str(&req.min_severity).ok_or_else(|| {
            McpError::invalid_params(
                "min_severity must be critical, warning, info, or style",
                None,
            )
        })?;
        if !matches!(req.detail_level.to_ascii_lowercase().as_str(), "compact" | "full") {
            return Err(McpError::invalid_params(
                "detail_level must be compact or full",
                None,
            ));
        }
        let gates = crate::services::pre_commit_review_service::all_gates();
        for name in &req.skip_gates {
            if !gates.iter().any(|gate| gate.name() == name) {
                return Err(McpError::invalid_params(
                    format!("unknown skip_gates entry: {name}"),
                    None,
                ));
            }
        }
        let rec = ensure_project_record(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let indexed_project_dir = PathBuf::from(rec.directory.clone());
        let (project_dir, review_source) =
            resolve_review_directory(&indexed_project_dir, req.working_directory.as_deref())
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let start = std::time::Instant::now();

        // Resolve the diff input into raw unified-diff text.
        let diff_text = resolve_diff_source(&project_dir, &req.diff)
            .map_err(|e| McpError::invalid_params(format!("diff resolution failed: {e}"), None))?;
        let parsed = crate::services::pre_commit_review_service::parse_unified_diff(&diff_text);
        let head_before = head_commit(&project_dir);
        let source_before = source_snapshot(&project_dir, &parsed);

        if diff_text.trim().is_empty() {
            let unexamined = if matches!(req.diff.trim(), "staged" | "unstaged" | "head") {
                crate::services::pre_commit_review_service::working_tree_changes(&project_dir).unwrap_or_default()
            } else { Vec::new() };
            let note = if unexamined.is_empty() {
                "No changes in the requested diff; review gates were not run. This does not certify the working tree as unchanged or reviewed.".to_string()
            } else {
                format!("The requested diff ({}) is empty, but the working tree has {} changed file(s) that were not examined: {}. Rerun with diff=\"unstaged\" to review them.",
                    req.diff.trim(), unexamined.len(), unexamined.iter().take(20).cloned().collect::<Vec<_>>().join(", "))
            };
            let payload = serde_json::json!({
                "verdict":if unexamined.is_empty() {"NO_CHANGES"} else {"NOT_REVIEWED"},
                "findings":[], "gate_status":[],
                "summary":{"files_analysed":0,"gates_run":0,"total_findings":0},
                "coverage":{"submitted_files":[],"textual_diff_files":[],"unexamined_files":unexamined,
                    "static_analysis":"not_run","compilation":"not_run","test_execution":"not_run"},
                "note":note
            });
            let body = if req.output_json { payload.to_string() }
                else if unexamined.is_empty() { "No changes detected in the requested diff. Nothing to review.".into() }
                else { format!("Not reviewed. {note}") };
            return response_delivery::finish(&req, &self.state.cfg.data_dir, body, &payload);
        }

        if crate::services::pre_commit_review_service::parse_unified_diff(&diff_text).is_empty() {
            return Err(McpError::invalid_params(
                "diff contains no parseable file changes; review was not run",
                None,
            ));
        }
        let source_runtime = self.ensure_project_runtime(&req.project_id).await?;
        let (generation, source_view) = self.pin_active_source_generation(
            &req.project_id, &source_runtime.search).await?;
        let source_view = std::sync::Arc::new(source_view);
        let config = ReviewConfig {
            max_findings: req.max_findings.clamp(1, 200),
            min_severity,
            skip_gates: req.skip_gates.iter().cloned().collect(),
            output_json: req.output_json,
        };

        let (findings, gates_run, files_analysed, outcomes) = run_pre_commit_review_with_source_view(
            &self.state,
            &req.project_id,
            &project_dir,
            generation,
            &diff_text,
            &config,
            source_view.clone(),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let policy_root = project_dir.clone();
        let policy_paths = parsed.iter().map(|file| file.path.clone()).collect::<Vec<_>>();
        let project_policy_sources = tokio::task::spawn_blocking(move || {
            crate::services::project_policy::resolve(&policy_root, &policy_paths, &[])
        }).await.map_err(|error| McpError::internal_error(error.to_string(), None))?;

        // Evidence is collected after gate findings have been produced. It
        // cannot suppress, downgrade or silently reconcile those findings.
        let prior_input = prior_context::Input::from_diff(&parsed, &source_before);
        let prior_state = self.state.clone();
        let prior_search = source_runtime.search.clone();
        let prior_pid = req.project_id.clone();
        let prior_head = head_before.clone();
        let prior_root = project_dir.clone();
        let prior_review_context = tokio::task::spawn_blocking(move || {
            prior_context::collect(&prior_state, &prior_search, &prior_pid,
                prior_head.as_deref(), &prior_root, prior_input)
        }).await.unwrap_or_else(|error| prior_context::unavailable(&format!("Evidence worker unavailable: {error}")));

        let elapsed_ms = start.elapsed().as_millis();
        let source_after = source_snapshot(&project_dir, &parsed);
        let head_after = head_commit(&project_dir);
        let changed_during_review = head_before != head_after || source_before != source_after;
        let snapshot_unavailable = source_after
            .iter()
            .any(|(_, value)| value.starts_with("unavailable:"));
        let unexamined: Vec<_> = parsed.iter().filter(|f| f.is_binary || f.hunks.is_empty()).map(|f|
            serde_json::json!({"path":f.path,"reason":if f.is_binary {"binary_content_not_inspected"} else {"no_text_hunks; metadata_only"}})).collect();
        let unindexed =
            unindexed_changed_files(&self.state, &req.project_id, &rec.project_type, &parsed).await;
        let unindexed_count = unindexed
            .as_array()
            .map_or_else(|| "unknown".to_string(), |files| files.len().to_string());
        let coverage = serde_json::json!({
            "submitted_files":parsed.iter().map(|f| &f.path).collect::<Vec<_>>(),
            "repository_policy":"retrieved for agent review; prose compliance is not mechanically verified",
            "unindexed_files":unindexed,
            "textual_diff_files":parsed.iter().filter(|f| !f.is_binary && !f.hunks.is_empty()).map(|f| &f.path).collect::<Vec<_>>(),
            "unexamined_files":unexamined,
            "static_analysis":if gates_run == 0 {"not_run"} else {"gate_scoped; not a complete code audit"},
            "compilation":"not_run","test_execution":"not_run",
            "head_before":head_before,"head_after":head_after,
            "changed_during_review":changed_during_review,
            "source_snapshot_complete":!snapshot_unavailable,
            "diff_blake3":blake3::hash(diff_text.as_bytes()).to_hex().to_string(),
            "source_before":source_before,"source_after":source_after,
            "source_scope":"bounded current-file snapshots; supplied diff is not certified to match current files",
            "provider_coverage":"see gate_status and source_scope; not_applicable means that provider does not support the submitted source. Failed, skipped and degraded gates are not passing evidence",
            "review_decisions":prior_review_context["status"],
            "review_source":review_source,
            "indexed_project_directory":indexed_project_dir,
            "review_working_directory":project_dir
        });

        tracing::info!(
            project_id = %req.project_id,
            files = files_analysed,
            findings = findings.len(),
            gates_run,
            elapsed_ms,
            "pre_commit_review complete"
        );

        let mut payload = serde_json::to_value(render_json(
            findings.clone(), files_analysed, gates_run, elapsed_ms, &outcomes,
        )).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        payload["coverage"] = coverage.clone();
        payload["project_policy_sources"] = serde_json::to_value(&project_policy_sources)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        payload["prior_review_context"] = prior_review_context.clone();
        if (changed_during_review || snapshot_unavailable) && payload["verdict"] == "green" {
            payload["verdict"] = "yellow".into();
        }
        let body = if config.output_json {
            serde_json::to_string_pretty(&payload)
                .map_err(|e| McpError::internal_error(format!("json render: {e}"), None))?
        } else {
            let compact = req.detail_level.eq_ignore_ascii_case("compact");
            let mut report = if compact {
                render_compact_markdown(
                    &findings,
                    files_analysed,
                    gates_run,
                    elapsed_ms,
                    &outcomes,
                )
            } else {
                render_markdown(&findings, files_analysed, gates_run, elapsed_ms, &outcomes)
            };
            if changed_during_review || snapshot_unavailable {
                report = report.replace(
                    "GREEN — no concerns within reported static gate coverage",
                    "YELLOW — source snapshot changed or unavailable",
                );
                report = report.replace(
                    "# Pre-Commit Review - GREEN",
                    "# Pre-Commit Review - YELLOW",
                );
            }
            if changed_during_review {
                report.insert_str(
                    0,
                    "SOURCE CHANGED DURING REVIEW: results are not a current-source clearance.\n\n",
                );
            }
            if compact {
                report.push_str(&format!(
                    "\nCoverage: submitted_files={}, textual_diff_files={}, unexamined_files={}, unindexed_files={}, changed_during_review={}, compilation=not_run, tests=not_run. Use detail_level=\"full\" or output_json=true for complete coverage evidence.\n",
                    parsed.len(),
                    parsed.iter().filter(|f| !f.is_binary && !f.hunks.is_empty()).count(),
                    unexamined.len(),
                    unindexed_count,
                    changed_during_review,
                ));
            } else {
                report.push_str(&format!(
                    "\n## Coverage evidence\n```json\n{}\n```\n",
                    serde_json::to_string_pretty(&coverage).unwrap_or_default()
                ));
            }
            report.push_str(&project_policy_sources.render_markdown());
            report.push_str("\n## Prior review context — imported evidence; findings remain visible\n\n");
            match serde_json::to_string_pretty(&prior_review_context) {
                Ok(context) => report.push_str(&format!("```json\n{context}\n```\n")),
                Err(error) => report.push_str(&format!("Context rendering unavailable: {error}. No absence or clearance is established.\n")),
            }
            report
        };

        response_delivery::finish(&req, &self.state.cfg.data_dir, body, &payload)
    }
}

/// Changed files the index has never seen, typically files this diff adds.
/// Graph-backed gates have no symbols for them, so their silence is not a
/// clean result. Deleted and binary files, and extensions the indexer never
/// takes, are not listed. An unreadable file index is reported as unknown.
async fn unindexed_changed_files(
    state: &crate::state::AppState,
    project_id: &str,
    project_type: &str,
    parsed: &[crate::services::pre_commit_review_service::DiffFile],
) -> serde_json::Value {
    use crate::services::pre_commit_review_service::ChangeType;
    let exts = crate::utils::files::exts_for_project_type(project_type);
    let candidates: Vec<String> = parsed
        .iter()
        .filter(|f| !f.is_binary && !matches!(f.change_type, ChangeType::Deleted))
        .filter(|f| {
            std::path::Path::new(&f.path)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
        })
        .map(|f| f.path.clone())
        .collect();
    if candidates.is_empty() {
        return serde_json::json!([]);
    }
    let graph = state.graph.clone();
    let pid = project_id.to_string();
    match tokio::task::spawn_blocking(move || graph.list_file_node_metadata(&pid)).await {
        Ok(Ok(rows)) => {
            let known: std::collections::HashSet<String> = rows
                .into_iter()
                .map(|(path, _)| path.as_str().replace('\\', "/").to_ascii_lowercase())
                .collect();
            serde_json::json!(
                candidates
                    .into_iter()
                    .filter(|p| !known.contains(&p.replace('\\', "/").to_ascii_lowercase()))
                    .collect::<Vec<_>>()
            )
        }
        Ok(Err(e)) => serde_json::json!(format!("unknown: file index unreadable ({e})")),
        Err(e) => serde_json::json!(format!("unknown: file index lookup failed ({e})")),
    }
}

/// Bind Git shortcuts and current-file snapshots to the checkout the agent is
/// editing. Linked worktrees share a common Git directory; unrelated
/// repositories are rejected so they cannot borrow another project's rules.
fn resolve_review_directory(
    indexed_project_dir: &std::path::Path,
    requested: Option<&str>,
) -> anyhow::Result<(PathBuf, &'static str)> {
    let Some(requested) = requested else {
        return Ok((indexed_project_dir.to_path_buf(), "indexed_project"));
    };
    anyhow::ensure!(
        !requested.trim().is_empty(),
        "working_directory cannot be empty"
    );

    let indexed_repo = git2::Repository::discover(indexed_project_dir)
        .map_err(|e| anyhow::anyhow!("indexed project is not a readable Git repository: {e}"))?;
    let requested_repo = git2::Repository::discover(std::path::Path::new(requested))
        .map_err(|e| anyhow::anyhow!("working_directory is not a readable Git repository: {e}"))?;

    let indexed_common = indexed_repo
        .commondir()
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve indexed repository identity: {e}"))?;
    let requested_common = requested_repo.commondir().canonicalize().map_err(|e| {
        anyhow::anyhow!("cannot resolve working-directory repository identity: {e}")
    })?;
    anyhow::ensure!(
        indexed_common == requested_common,
        "working_directory belongs to a different Git repository than the indexed project"
    );

    let worktree = requested_repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("working_directory resolves to a bare Git repository"))?
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot resolve working-directory root: {e}"))?;
    Ok((worktree, "explicit_same_repository_worktree"))
}

fn head_commit(root: &std::path::Path) -> Option<String> {
    git2::Repository::open(root)
        .ok()
        .and_then(|r| r.head().ok().and_then(|h| h.target()))
        .map(|id| id.to_string())
}

fn source_snapshot(
    root: &std::path::Path,
    files: &[crate::services::pre_commit_review_service::DiffFile],
) -> std::collections::BTreeMap<String, String> {
    use std::io::Read;
    let mut budget = 64 * 1024 * 1024usize;
    files
        .iter()
        .map(|file| {
            if matches!(
                file.change_type,
                crate::services::pre_commit_review_service::ChangeType::Deleted
            ) {
                return (
                    file.path.clone(),
                    "deleted_in_supplied_diff; previous_content_not_verified".into(),
                );
            }
            let read = || -> Result<Vec<u8>, String> {
                let path = engram_core::safe_join(root, &file.path)
                    .map_err(|e| e.to_string())?
                    .canonicalize()
                    .map_err(|e| e.to_string())?;
                if !path.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
                    return Err("path outside project".into());
                }
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .map_err(|e| e.to_string())?
                    .take((budget.min(8 * 1024 * 1024) + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() > budget.min(8 * 1024 * 1024) {
                    return Err("snapshot budget exceeded".into());
                }
                Ok(bytes)
            };
            let value = match read() {
                Ok(bytes) => {
                    budget -= bytes.len();
                    format!("blake3:{}", blake3::hash(&bytes).to_hex())
                }
                Err(error) => format!("unavailable:{error}"),
            };
            (file.path.clone(), value)
        })
        .collect()
}

#[cfg(test)]
mod worktree_binding_tests {
    use super::resolve_review_directory;
    use crate::services::pre_commit_review_service::resolve_diff_source;
    use std::path::Path;

    fn repository_with_commit(path: &Path) -> git2::Repository {
        std::fs::create_dir_all(path).unwrap();
        let repo = git2::Repository::init(path).unwrap();
        std::fs::write(path.join("tracked.txt"), "base\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_id).unwrap();
            let sig = git2::Signature::now("Engram test", "engram@example.invalid").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
                .unwrap();
        }
        repo
    }

    #[test]
    fn explicit_linked_worktree_binds_shortcut_diff_to_edited_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let indexed = temp.path().join("indexed");
        let linked = temp.path().join("linked");
        let repo = repository_with_commit(&indexed);
        let worktree = repo.worktree("linked", &linked, None).unwrap();
        drop(worktree);
        std::fs::write(linked.join("tracked.txt"), "edited in linked worktree\n").unwrap();

        let (root, source) =
            resolve_review_directory(&indexed, Some(linked.to_str().unwrap())).unwrap();
        assert_eq!(root, linked.canonicalize().unwrap());
        assert_eq!(source, "explicit_same_repository_worktree");
        let diff = resolve_diff_source(&root, "unstaged").unwrap();
        assert!(diff.contains("+edited in linked worktree"), "{diff}");
    }

    #[test]
    fn unrelated_repository_cannot_borrow_indexed_project_review_context() {
        let temp = tempfile::tempdir().unwrap();
        let indexed = temp.path().join("indexed");
        let unrelated = temp.path().join("unrelated");
        let _indexed_repo = repository_with_commit(&indexed);
        let _unrelated_repo = repository_with_commit(&unrelated);

        let error = resolve_review_directory(&indexed, Some(unrelated.to_str().unwrap()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("different Git repository"), "{error}");
    }
}
