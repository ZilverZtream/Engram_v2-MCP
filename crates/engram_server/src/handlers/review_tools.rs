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
    ReviewConfig, Severity, render_compact_markdown, render_json, render_markdown, resolve_review_diff,
    run_parsed_review_with_source_view,
};
use crate::services::project_service::ensure_project_record;
use crate::tools::Engram;
use std::collections::BTreeMap;
mod prior_context;
mod pre_push_context;
mod response_delivery;
mod finding_context;
mod maintenance_context;

fn alternate_review_calls(
    request: &PreCommitReviewRequest,
    changes: &crate::services::pre_commit_review_service::WorkingTreeChanges,
) -> Vec<serde_json::Value> {
    let mode = request.diff.trim();
    let (staged, unstaged) = if mode == "working_tree" {
        (!changes.staged_with_unstaged.is_empty(), false)
    } else {
        (changes.staged, changes.unstaged)
    };
    [("staged", staged), ("unstaged", unstaged), ("working_tree", staged && unstaged)]
        .into_iter()
        .filter(|(candidate, available)| *available && *candidate != mode)
        .map(|(mode, _)| {
            let mut args = request.clone();
            args.diff = mode.to_owned();
            let reason = if mode == "working_tree" {
                "Review the net change from HEAD to the final working tree in one pass, including non-ignored untracked files in scope."
            } else {
                "Inspect this observed Git diff scope if it has not already been reviewed."
            };
            serde_json::json!({"tool":"pre_commit_review","arguments":args,"reason":reason})
        })
        .collect()
}

impl Engram {
    pub async fn handle_pre_commit_review(
        &self,
        mut req: PreCommitReviewRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;
        if req.review_decisions_before.as_deref()
            .is_some_and(|date| !super::review_decisions::valid_iso_date(date))
        {
            return Err(McpError::invalid_params(
                "review_decisions_before must be an exclusive YYYY-MM-DD source-date cutoff", None));
        }
        let budget = response_delivery::budget(req.max_tokens)?;
        if let Some(id) = req.response_id.as_deref() {
            let body = super::planning_tools::change_set_delivery::recover_review(
                &self.state.cfg.data_dir, &req.project_id, id, req.response_offset, budget)?;
            return Ok(CallToolResult::success(vec![Content::text(body)]));
        }
        if req.response_offset != 0 {
            return Err(McpError::invalid_params("response_offset requires response_id from a review recovery action", None));
        }
        if req.path_prefixes.len() > 32 || req.path_prefixes.iter_mut().any(|scope| {
            *scope = scope.replace('\\', "/").trim_end_matches('/').to_owned();
            scope.len() > 4096 || scope.contains(':') || scope.chars().any(char::is_control)
                || scope.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        }) {
            return Err(McpError::invalid_params(
                "path_prefixes requires at most 32 literal paths relative to the Git worktree root", None));
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
        let start = std::time::Instant::now();
        let root = indexed_project_dir.clone();
        let (req, project_dir, review_source, diff_blake3, parsed, head_before, source_before, index_before) =
            tokio::task::spawn_blocking(move || {
                let (project_dir, review_source) =
                    resolve_review_directory(&root, req.working_directory.as_deref())
                        .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
                let diff_text = resolve_review_diff(&project_dir, &req.diff, &req.path_prefixes).map_err(|error| {
                    McpError::invalid_params(format!("diff resolution failed: {error}"), None)
                })?;
                let parsed = std::sync::Arc::new(
                    crate::services::pre_commit_review_service::parse_unified_diff(&diff_text),
                );
                let diff_blake3 = (!diff_text.trim().is_empty())
                    .then(|| blake3::hash(diff_text.as_bytes()).to_hex().to_string());
                drop(diff_text);
                let head = head_commit(&project_dir);
                let source = source_snapshot(&project_dir, &parsed);
                let index = if req.diff.trim() == "staged" {
                    staged_index_snapshot(&project_dir, &parsed).ok()
                } else {
                    None
                };
                Ok::<_, McpError>((req, project_dir, review_source, diff_blake3, parsed, head, source, index))
            })
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))??;
        let mode = req.diff.trim();
        let native_diff = matches!(mode, "staged" | "unstaged" | "working_tree" | "head");
        let git_diff_source = native_diff.then_some(mode);
        let mut scope_note = if req.path_prefixes.is_empty() {
            String::new()
        } else {
            format!("\n\nRequested Git path scope: {}. Other paths are outside this review.",
                serde_json::json!(req.path_prefixes))
        };
        if mode == "working_tree" {
            scope_note.push_str("\n\nDiff source: HEAD to the final working tree, including non-ignored untracked files.");
        }

        if diff_blake3.is_none() {
            let status = if native_diff {
                let scopes = req.path_prefixes.clone();
                tokio::task::spawn_blocking(move || {
                    crate::services::pre_commit_review_service::working_tree_changes(&project_dir, true, &scopes)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|status| status)
            } else {
                Ok(Default::default())
            };
            let status_available = status.is_ok();
            let changes = status.unwrap_or_default();
            let review_next_calls = alternate_review_calls(&req, &changes);
            let unexamined = changes.paths;
            let note = if !status_available {
                "The requested diff is empty and Git status is unavailable. No gates ran. Check the intended worktree and diff before treating it as reviewed.".to_owned()
            } else if unexamined.is_empty() {
                "No changes in the requested diff; review gates were not run. This does not certify the working tree as unchanged or reviewed.".to_string()
            } else {
                let advice = if review_next_calls.is_empty() {
                    "Inspect the unexamined changes before relying on this review."
                } else {
                    "Use the supplied review calls to inspect the observed alternate diff scopes."
                };
                format!("The requested diff ({mode}) is empty, but the working tree has {} changed file(s) that were not examined: {}. {advice}",
                    unexamined.len(), unexamined.iter().take(20).cloned().collect::<Vec<_>>().join(", "))
            };
            let nothing_detected = status_available && unexamined.is_empty();
            let payload = serde_json::json!({
                "verdict":if nothing_detected {"NO_CHANGES"} else {"NOT_REVIEWED"},
                "findings":[], "gate_status":[],
                "summary":{"files_analysed":0,"gates_run":0,"total_findings":0},
                "coverage":{"submitted_files":[],"textual_diff_files":[],"unexamined_files":unexamined,
                    "path_prefixes":req.path_prefixes,
                    "git_diff_source":git_diff_source,
                    "static_analysis":"not_run","compilation":"not_run","test_execution":"not_run"},
                "note":note,"review_next_calls":review_next_calls
            });
            let mut body = if req.output_json { payload.to_string() }
                else if nothing_detected { "No changes detected in the requested diff. Nothing to review.".into() }
                else if review_next_calls.is_empty() { format!("Not reviewed. {note}") }
                else { format!("Not reviewed. {note}\n\nreview_next_calls: {}", payload["review_next_calls"]) };
            if !req.output_json {
                body.push_str(&scope_note);
            }
            return response_delivery::finish(&req, &self.state.cfg.data_dir, body, &payload);
        }

        if parsed.is_empty() {
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

        let (findings, gates_run, files_analysed, outcomes) = run_parsed_review_with_source_view(
            &self.state,
            &req.project_id,
            &project_dir,
            generation,
            parsed.clone(),
            &config,
            source_view.clone(),
        )
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let policy_root = project_dir.clone();
        let policy_paths = parsed.iter().map(|file| file.path.clone()).collect::<Vec<_>>();
        let project_policy_sources = tokio::task::spawn_blocking(move || {
            crate::services::project_policy::resolve(&policy_root, &policy_paths, &[])
        }).await.map_err(|error| McpError::internal_error(error.to_string(), None))?.with_source_recovery(&req.project_id);

        // Evidence is collected after gate findings have been produced. It
        // cannot suppress, downgrade or silently reconcile those findings.
        let prior_input = prior_context::Input::from_diff(&parsed, &source_before)
            .with_findings(&findings)
            .with_recorded_before(req.review_decisions_before.clone());
        let prior_state = self.state.clone();
        let prior_search = source_runtime.search.clone();
        let prior_pid = req.project_id.clone();
        let prior_head = head_before.clone();
        let prior_root = project_dir.clone();
        let prior_review_context = tokio::task::spawn_blocking(move || {
            prior_context::collect(&prior_state, &prior_search, &prior_pid,
                prior_head.as_deref(), &prior_root, prior_input)
        }).await.unwrap_or_else(|error| prior_context::unavailable(&format!("Evidence worker unavailable: {error}")));

        // Evidence only: ordinary review does not silently become a policy
        // drafting or write operation. Use this review checkout and file scope.
        let maintenance_root = project_dir.clone();
        let maintenance_project = req.project_id.clone();
        let maintenance_mode = req.diff.clone();
        let maintenance_head = head_before.clone();
        let maintenance_paths = parsed.iter().map(|file| file.path.clone()).collect();
        let instruction_maintenance_context = tokio::task::spawn_blocking(move || {
            maintenance_context::collect(&maintenance_root, &maintenance_project,
                &maintenance_mode, maintenance_head.as_deref(), maintenance_paths)
        }).await.unwrap_or_else(|error| serde_json::json!({
            "status":"maintenance_evidence_unavailable","reason":format!("Evidence worker unavailable: {error}"),
            "evidence_collection_performed":false,"maintenance_semantic_review_performed":false,
            "authored_files_written":0}));

        let elapsed_ms = start.elapsed().as_millis();
        let root = project_dir.clone();
        let reviewed_files = parsed.clone();
        let scopes = req.path_prefixes.clone();
        let staged = mode == "staged";
        let (source_after, head_after, alternate_changes, index_after) = tokio::task::spawn_blocking(move || {
            let source = source_snapshot(&root, &reviewed_files);
            let head = head_commit(&root);
            let alternate = if native_diff {
                crate::services::pre_commit_review_service::working_tree_changes(&root, false, &scopes).ok()
            } else {
                None
            };
            let index = if staged { staged_index_snapshot(&root, &reviewed_files).ok() } else { None };
            (source, head, alternate, index)
        })
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        let review_next_calls = alternate_changes.as_ref()
            .map(|changes| alternate_review_calls(&req, changes))
            .unwrap_or_default();
        let alternate_scope_status = match &alternate_changes {
            None if native_diff => "git_status_unavailable",
            None => "not_assessed_for_supplied_diff",
            Some(_) if review_next_calls.is_empty() => "no_alternate_changes_observed",
            Some(_) => "alternate_changes_observed_after_review",
        };
        let staged_worktree_drift = if staged {
            alternate_changes.as_ref().map(|changes| {
                parsed.iter()
                    .filter(|file| changes.staged_with_unstaged.contains(&file.path))
                    .map(|file| &file.path)
                    .collect::<Vec<_>>()
            })
        } else {
            None
        };
        let index_changed = index_before.as_ref().zip(index_after.as_ref())
            .is_some_and(|(before, after)| before != after);
        let staged_evidence_incomplete = staged
            && (index_before.is_none() || index_after.is_none() || staged_worktree_drift.is_none());
        let staged_context_diverged = staged_worktree_drift.as_ref().is_some_and(|paths| !paths.is_empty());
        let changed_during_review = head_before != head_after || source_before != source_after || index_changed;
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
            "path_prefixes":req.path_prefixes,
            "git_diff_source":git_diff_source,
            "repository_policy":"retrieved for agent review; prose compliance is not mechanically verified",
            "unindexed_files":unindexed,
            "textual_diff_files":parsed.iter().filter(|f| !f.is_binary && !f.hunks.is_empty()).map(|f| &f.path).collect::<Vec<_>>(),
            "unexamined_files":unexamined,
            "static_analysis":if gates_run == 0 {"not_run"} else {"gate_scoped; not a complete code audit"},
            "compilation":"not_run","test_execution":"not_run",
            "head_before":head_before,"head_after":head_after,
            "changed_during_review":changed_during_review,
            "alternate_diff_scope_status":alternate_scope_status,
            "staged_files_with_unstaged_edits":staged_worktree_drift,
            "staged_index_before":index_before,
            "staged_index_after":index_after,
            "staged_index_changed":staged.then_some(index_changed),
            "staged_evidence_incomplete":staged.then_some(staged_evidence_incomplete),
            "source_snapshot_complete":!snapshot_unavailable,
            "diff_blake3":diff_blake3,
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
        if !review_next_calls.is_empty() {
            payload["review_next_calls"] = serde_json::json!(review_next_calls);
        }
        payload["project_policy_sources"] = serde_json::to_value(&project_policy_sources)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        payload["prior_review_context"] = prior_review_context.clone();
        payload["instruction_maintenance_context"] = instruction_maintenance_context.clone();
        finding_context::annotate(&mut payload);
        let source_qualification = if changed_during_review || snapshot_unavailable {
            Some("source or staging snapshot changed or unavailable")
        } else if staged_context_diverged {
            Some("staged diff and current-file context diverge")
        } else if staged_evidence_incomplete {
            Some("staged source evidence is incomplete")
        } else {
            None
        };
        if let Some(reason) = source_qualification {
            payload["verdict_source_qualification"] = reason.into();
        }
        if source_qualification.is_some() && payload["verdict"] == "green" {
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
            report.push_str(&scope_note);
            if let Some(reason) = source_qualification {
                report = report.replacen(
                    "GREEN — no concerns within reported static gate coverage",
                    &format!("YELLOW — {reason}"),
                    1,
                );
                report = report.replacen(
                    "# Pre-Commit Review - GREEN",
                    "# Pre-Commit Review - YELLOW",
                    1,
                );
            }
            if changed_during_review {
                report.insert_str(
                    0,
                    "SOURCE CHANGED DURING REVIEW: results are not a current-source clearance.\n\n",
                );
            }
            if !review_next_calls.is_empty() {
                let reason = if mode == "working_tree" {
                    "The staging area has a different version of at least one changed file. Inspect the staged diff for the snapshot that would be committed."
                } else {
                    "Git status observed additional diff scopes after this review. Inspect them separately before relying on coverage for the whole worktree."
                };
                report.push_str(&format!("\n{reason}\n\nreview_next_calls: {}\n", payload["review_next_calls"]));
            }
            if let Some(paths) = &staged_worktree_drift && !paths.is_empty() {
                report.push_str(&format!("\nStaged review includes files with additional unstaged edits: {}. Current-file context for these paths can differ from the submitted staged diff.\n", coverage["staged_files_with_unstaged_edits"]));
            }
            if index_changed {
                report.push_str("\nThe staging area changed during review. Review the staged diff again before committing.\n");
            } else if staged_evidence_incomplete {
                report.push_str("\nStaging or worktree evidence was unavailable. Verify the selected commit contents before relying on the verdict.\n");
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
            report.push_str(&maintenance_context::markdown(&instruction_maintenance_context));
            report.push_str(&finding_context::markdown(&payload));
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

#[derive(PartialEq, Eq, serde::Serialize)]
struct StagedEntry {
    object_id: String,
    mode: u32,
}

fn staged_index_snapshot(
    root: &std::path::Path,
    files: &[crate::services::pre_commit_review_service::DiffFile],
) -> anyhow::Result<BTreeMap<String, Option<StagedEntry>>> {
    let repository = git2::Repository::discover(root)?;
    let index = repository.index()?;
    Ok(files.iter().map(|file| {
        let entry = index.get_path(std::path::Path::new(&file.path), 0).map(|entry| StagedEntry {
            object_id: entry.id.to_string(),
            mode: entry.mode,
        });
        (file.path.clone(), entry)
    }).collect())
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
        let diff = resolve_review_diff(&root, "unstaged", &[]).unwrap();
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
