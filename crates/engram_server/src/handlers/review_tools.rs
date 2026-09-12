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
    ReviewConfig, Severity, render_json, render_markdown, resolve_diff_source,
    run_pre_commit_review,
};
use crate::services::project_service::{ensure_project_record, get_active_generation};
use crate::tools::Engram;

impl Engram {
    pub async fn handle_pre_commit_review(
        &self,
        req: PreCommitReviewRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        let min_severity = Severity::from_str(&req.min_severity).ok_or_else(|| {
            McpError::invalid_params(
                "min_severity must be critical, warning, info, or style",
                None,
            )
        })?;
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
        let generation = get_active_generation(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
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
            if req.output_json {
                return Ok(CallToolResult::success(vec![Content::text(
                    serde_json::json!({
                        "verdict": "NO_CHANGES", "findings": [], "gate_status": [],
                        "summary": { "files_analysed": 0, "gates_run": 0, "total_findings": 0 },
                        "coverage": {"submitted_files": [], "textual_diff_files": [], "unexamined_files": [], "static_analysis": "not_run", "compilation": "not_run", "test_execution": "not_run"},
                        "note": "No changes detected; review gates were not run."
                    })
                    .to_string(),
                )]));
            }
            let body = "No changes detected in the requested diff. Nothing to review.";
            return Ok(CallToolResult::success(vec![Content::text(
                body.to_string(),
            )]));
        }

        if crate::services::pre_commit_review_service::parse_unified_diff(&diff_text).is_empty() {
            return Err(McpError::invalid_params(
                "diff contains no parseable file changes; review was not run",
                None,
            ));
        }
        let config = ReviewConfig {
            max_findings: req.max_findings.clamp(1, 200),
            min_severity,
            skip_gates: req.skip_gates.iter().cloned().collect(),
            output_json: req.output_json,
        };

        let (findings, gates_run, files_analysed, outcomes) = run_pre_commit_review(
            &self.state,
            &req.project_id,
            &project_dir,
            generation,
            &diff_text,
            &config,
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let elapsed_ms = start.elapsed().as_millis();
        let source_after = source_snapshot(&project_dir, &parsed);
        let head_after = head_commit(&project_dir);
        let changed_during_review = head_before != head_after || source_before != source_after;
        let snapshot_unavailable = source_after
            .iter()
            .any(|(_, value)| value.starts_with("unavailable:"));
        let unexamined: Vec<_> = parsed.iter().filter(|f| f.is_binary || f.hunks.is_empty()).map(|f|
            serde_json::json!({"path":f.path,"reason":if f.is_binary {"binary_content_not_inspected"} else {"no_text_hunks; metadata_only"}})).collect();
        let coverage = serde_json::json!({
            "submitted_files":parsed.iter().map(|f| &f.path).collect::<Vec<_>>(),
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
            "provider_coverage":"see gate_status; failed, skipped and degraded gates are not passing evidence",
            "review_decisions":"not_consulted; retrieve get_review_decisions for the relevant review before reconciling findings",
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

        let body = if config.output_json {
            let mut payload = serde_json::to_value(render_json(
                findings,
                files_analysed,
                gates_run,
                elapsed_ms,
                &outcomes,
            ))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            payload["coverage"] = coverage;
            if (changed_during_review || snapshot_unavailable) && payload["verdict"] == "green" {
                payload["verdict"] = "yellow".into();
            }
            serde_json::to_string_pretty(&payload)
                .map_err(|e| McpError::internal_error(format!("json render: {e}"), None))?
        } else {
            let mut report =
                render_markdown(&findings, files_analysed, gates_run, elapsed_ms, &outcomes);
            if changed_during_review || snapshot_unavailable {
                report = report.replace(
                    "GREEN — no concerns within reported static gate coverage",
                    "YELLOW — source snapshot changed or unavailable",
                );
            }
            if changed_during_review {
                report.insert_str(
                    0,
                    "SOURCE CHANGED DURING REVIEW: results are not a current-source clearance.\n\n",
                );
            }
            report.push_str(&format!(
                "\n## Coverage evidence\n```json\n{}\n```\n",
                serde_json::to_string_pretty(&coverage).unwrap_or_default()
            ));
            report
        };

        Ok(CallToolResult::success(vec![Content::text(body)]))
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
