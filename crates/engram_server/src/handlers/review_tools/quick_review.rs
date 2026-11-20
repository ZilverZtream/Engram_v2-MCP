use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{
    models::requests::PreCommitReviewRequest,
    services::pre_commit_review_service::{GateObserver, ReviewFinding, Severity},
    tools::Engram,
};

const MAX_REVIEW_RUNS: usize = 8;
const MAX_PREVIEW_FINDINGS: usize = 8;
const FIRST_REPLY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(crate) struct QuickReviews(Mutex<VecDeque<Run>>);

struct Run {
    id: String,
    project: String,
    budget: usize,
    progress: watch::Receiver<Progress>,
}

#[derive(Default)]
struct Progress {
    digest: Option<String>,
    patch: Option<Value>,
    pending: BTreeSet<&'static str>,
    findings: Vec<ReviewFinding>,
    findings_seen: usize,
    result: Option<Result<CallToolResult, McpError>>,
}

#[derive(Clone)]
pub(super) struct Reporter(watch::Sender<Progress>);

impl Reporter {
    pub(super) fn captured(
        &self,
        digest: &Option<String>,
        patch: &Option<Value>,
        gates: impl Iterator<Item = &'static str>,
    ) {
        self.0.send_modify(|progress| {
            progress.digest.clone_from(digest);
            progress.patch.clone_from(patch);
            progress.pending = gates.collect();
        });
    }

    pub(super) fn observer(&self, min_severity: Severity, max_findings: usize) -> GateObserver {
        let sender = self.0.clone();
        let preview_limit = max_findings.min(MAX_PREVIEW_FINDINGS);
        Arc::new(move |gate, findings| {
            sender.send_modify(|progress| {
                progress.pending.remove(gate);
                let selected = findings
                    .iter()
                    .filter(|finding| finding.severity <= min_severity);
                for finding in selected {
                    progress.findings_seen += 1;
                    progress.findings.push(finding.clone());
                    progress.findings.sort_by_key(|entry| entry.severity);
                    progress.findings.truncate(preview_limit);
                }
            });
        })
    }
}

impl QuickReviews {
    fn register(
        &self,
        project: &str,
        budget: usize,
        progress: watch::Receiver<Progress>,
    ) -> Result<String, McpError> {
        let mut runs = self
            .0
            .lock()
            .map_err(|_| McpError::internal_error("Review run registry unavailable", None))?;
        if runs.len() >= MAX_REVIEW_RUNS {
            let finished = runs.iter().position(|run| {
                run.progress.borrow().result.is_some() || run.progress.has_changed().is_err()
            });
            let Some(index) = finished else {
                return Err(McpError::invalid_params(
                    "Quick reviews are busy; poll an existing run before starting another",
                    None,
                ));
            };
            runs.remove(index);
        }
        let id = uuid::Uuid::new_v4().to_string();
        runs.push_back(Run {
            id: id.clone(),
            project: project.to_owned(),
            budget,
            progress,
        });
        Ok(id)
    }

    pub(crate) fn poll(&self, project: &str, id: &str) -> Result<CallToolResult, McpError> {
        let (receiver, budget) = {
            let runs = self
                .0
                .lock()
                .map_err(|_| McpError::internal_error("Review run registry unavailable", None))?;
            let run = runs
                .iter()
                .find(|run| run.project == project && run.id == id)
                .ok_or_else(|| {
                    McpError::invalid_params("Unknown or no longer retained review run", None)
                })?;
            (run.progress.clone(), run.budget)
        };
        let progress = receiver.borrow();
        if let Some(result) = &progress.result {
            return result.clone();
        }
        if receiver.has_changed().is_err() {
            return Err(McpError::internal_error(
                "Review ended before a final report was available",
                None,
            ));
        }
        let mut findings: Vec<_> = progress
            .findings
            .iter()
            .map(|finding| {
                json!({
                    "gate": finding.gate,
                    "finding_id": finding.finding_id,
                    "severity": finding.severity,
                    "file_path": finding.file_path,
                    "lines": finding.lines,
                    "title": finding.title,
                    "suggestion": finding.suggestion,
                })
            })
            .collect();
        let body = loop {
            let body = json!({
                "status": "review_in_progress",
                "phase": if progress.digest.is_none() { "capturing_patch" }
                    else if progress.pending.is_empty() { "assembling_final_report" } else { "running_gates" },
                "complete": false,
                "diff_blake3": progress.digest,
                "reviewed_patch": progress.patch,
                "pending_gates": progress.pending,
                "findings": findings,
                "findings_seen": progress.findings_seen,
                "note": "These are preliminary findings. Wait for the complete report before using a verdict.",
                "next_call": {"tool": "pre_commit_review", "arguments": {"project_id": project, "review_run_id": id}},
            }).to_string();
            if body.len() <= budget {
                break body;
            }
            if findings.pop().is_none() {
                return Err(McpError::invalid_params(
                    "Reply budget cannot fit quick-review progress",
                    Some(json!({
                        "next_call": {"tool":"pre_commit_review", "arguments":{"project_id":project,"review_run_id":id}}
                    })),
                ));
            }
        };
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

pub(super) async fn start(
    server: &Engram,
    mut request: PreCommitReviewRequest,
) -> Result<CallToolResult, McpError> {
    let budget = super::response_delivery::budget(request.max_tokens)?;
    let (sender, mut receiver) = watch::channel(Progress::default());
    let project = request.project_id.clone();
    let id = server
        .state
        .quick_reviews
        .register(&project, budget, receiver.clone())?;
    request.output_json = true;
    request.retain_reviewed_patch = true;
    let worker = server.clone();
    tokio::spawn(async move {
        let result = worker
            .run_review(request, Some(Reporter(sender.clone())))
            .await;
        sender.send_modify(|progress| progress.result = Some(result));
    });
    let _ = tokio::time::timeout(
        FIRST_REPLY,
        receiver.wait_for(|progress| progress.result.is_some() || !progress.findings.is_empty()),
    )
    .await;
    server.state.quick_reviews.poll(&project, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polling_returns_completed_findings_before_the_same_run_finishes() {
        let reviews = QuickReviews::default();
        let (sender, receiver) = watch::channel(Progress::default());
        let id = reviews.register("example", 4096, receiver).unwrap();
        let reporter = Reporter(sender.clone());
        reporter.captured(
            &Some("patch-hash".into()),
            &None,
            ["style", "audit"].into_iter(),
        );
        reporter.observer(Severity::Warning, 10)(
            "style",
            &[ReviewFinding {
                severity: Severity::Warning,
                gate: "style",
                file_path: "example.ts".into(),
                lines: vec![2],
                title: "Correct the returned value".into(),
                detail: String::new(),
                suggestion: "Return the selected value".into(),
                evidence: vec![],
                diff_snippet: None,
                next_tool: None,
                finding_id: "example-finding".into(),
            }],
        );
        let reply = serde_json::to_value(reviews.poll("example", &id).unwrap()).unwrap();
        let view: Value =
            serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(view["pending_gates"], json!(["audit"]));
        assert_eq!(view["findings"][0]["finding_id"], "example-finding");
        assert_eq!(view["diff_blake3"], "patch-hash");
        assert!(view.get("verdict").is_none());
        let final_report = CallToolResult::success(vec![Content::text(r#"{"verdict":"yellow"}"#)]);
        sender.send_modify(|progress| progress.result = Some(Ok(final_report.clone())));
        assert_eq!(reviews.poll("example", &id).unwrap(), final_report);
    }
}
