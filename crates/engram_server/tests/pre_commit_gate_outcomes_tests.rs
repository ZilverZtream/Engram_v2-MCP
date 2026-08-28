#![allow(clippy::unwrap_used)]
//! Row-3 audit (docs/audits/05-pre-commit-gates.md) A1/A2: a gate that
//! errors or panics is a gate that did NOT run — the orchestrator records
//! it, the verdict cannot be green, and both renderers say so. Before this,
//! the errors were `tracing::warn!`ed and the diff "passed all gates
//! cleanly".

use engram_core::config::Config;
use engram_server::services::pre_commit_review_service::{
    Gate, GateContext, GateStatus, ReviewConfig, ReviewFinding, Verdict, render_json,
    render_markdown, run_pre_commit_review_with,
};
use engram_server::state::AppState;

const PID: &str = "gate-outcomes-test";

fn build_state() -> (tempfile::TempDir, AppState, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state, project_dir)
}

const DIFF: &str = "diff --git a/Site/App_Code/x.vb b/Site/App_Code/x.vb\n\
--- a/Site/App_Code/x.vb\n\
+++ b/Site/App_Code/x.vb\n\
@@ -1,2 +1,3 @@\n\
 Public Class X\n\
+    Public Sub Added()\n\
 End Class\n";

struct QuietGate;
impl Gate for QuietGate {
    fn name(&self) -> &'static str {
        "quiet"
    }
    fn run(&self, _ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        Ok(Vec::new())
    }
}

struct BoomGate;
impl Gate for BoomGate {
    fn name(&self) -> &'static str {
        "boom"
    }
    fn run(&self, _ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        anyhow::bail!("provider exploded")
    }
}

struct PanicGate;
impl Gate for PanicGate {
    fn name(&self) -> &'static str {
        "kaboom"
    }
    fn run(&self, _ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        panic!("kaboom")
    }
}

fn gates(failing: bool) -> Vec<Box<dyn Gate>> {
    if failing {
        vec![Box::new(QuietGate), Box::new(BoomGate), Box::new(PanicGate)]
    } else {
        vec![Box::new(QuietGate)]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_or_panicked_gate_is_recorded_and_keeps_the_verdict_off_green() {
    let (_tmp, state, dir) = build_state();
    let config = ReviewConfig::default();

    let (findings, gates_run, files, outcomes) =
        run_pre_commit_review_with(&state, PID, &dir, 1, DIFF, &config, gates(true))
            .await
            .unwrap();

    assert_eq!(files, 1);
    assert_eq!(gates_run, 3, "dispatch count is still reported");
    assert_eq!(outcomes.len(), 3, "{outcomes:?}");
    let status = |n: &str| {
        outcomes
            .iter()
            .find(|o| o.name == n)
            .map(|o| o.status.clone())
    };
    assert_eq!(status("quiet"), Some(GateStatus::Passed));
    assert!(
        matches!(status("boom"), Some(GateStatus::Failed(ref r)) if r.contains("provider exploded")),
        "{outcomes:?}"
    );
    assert!(
        matches!(status("kaboom"), Some(GateStatus::Panicked(_))),
        "{outcomes:?}"
    );

    let verdict = Verdict::with_outcomes(&findings, &outcomes);
    assert_ne!(
        verdict,
        Verdict::Green,
        "a gate that did not run is missing evidence"
    );

    let md = render_markdown(&findings, files, gates_run, 5, &outcomes);
    assert!(
        !md.contains("passed all gates cleanly"),
        "clean-bill line printed although two gates never ran:\n{md}"
    );
    assert!(md.contains("did not run"), "{md}");
    assert!(md.contains("boom") && md.contains("kaboom"), "{md}");

    let json = render_json(findings, files, gates_run, 5, &outcomes);
    let v = serde_json::to_value(&json).unwrap();
    assert_eq!(v["summary"]["gates_failed"], 1, "{v}");
    assert_eq!(v["summary"]["gates_panicked"], 1, "{v}");
    assert_eq!(v["gate_status"].as_array().unwrap().len(), 3, "{v}");
    assert_ne!(v["verdict"], "green", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_gates_passing_without_findings_is_still_green_and_clean() {
    let (_tmp, state, dir) = build_state();
    let config = ReviewConfig::default();

    let (findings, gates_run, files, outcomes) =
        run_pre_commit_review_with(&state, PID, &dir, 1, DIFF, &config, gates(false))
            .await
            .unwrap();

    assert_eq!(Verdict::with_outcomes(&findings, &outcomes), Verdict::Green);
    let md = render_markdown(&findings, files, gates_run, 5, &outcomes);
    assert!(md.contains("passed all gates cleanly"), "{md}");
    assert!(!md.contains("did not run"), "{md}");
}
