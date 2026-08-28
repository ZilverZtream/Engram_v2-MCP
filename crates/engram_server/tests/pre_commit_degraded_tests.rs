#![allow(clippy::unwrap_used)]
//! Row-3 audit (docs/audits/05-pre-commit-gates.md) slice 2 — A3:
//! a gate whose PROVIDER failed inside (file unreadable, graph lookup
//! error, search error, runtime missing ⇒ regex-only fallback) used to
//! swallow the failure and render `passed`. Such a gate ran DEGRADED: its
//! evidence is partial, the verdict cannot be green on its account, the
//! clean-bill line must not print, and both renderers say which provider
//! failed.

use engram_core::config::Config;
use engram_server::services::pre_commit_review_service::{
    Gate, GateContext, GateStatus, ReviewConfig, ReviewFinding, Verdict, all_gates, render_json,
    render_markdown, run_pre_commit_review_with,
};
use engram_server::state::AppState;

const PID: &str = "degraded-test";

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
            project_type: "dotnet_webforms_vb".into(),
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

/// A .vb file that is NOT on disk — every gate that reads the working
/// tree hits a provider failure here.
const DIFF: &str = "diff --git a/Site/App_Code/missing.vb b/Site/App_Code/missing.vb\n\
--- a/Site/App_Code/missing.vb\n\
+++ b/Site/App_Code/missing.vb\n\
@@ -1,2 +1,3 @@\n\
 Public Class X\n\
+    Public Sub Added()\n\
 End Class\n";

struct HalfBlindGate;
impl Gate for HalfBlindGate {
    fn name(&self) -> &'static str {
        "half_blind"
    }
    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        ctx.degrade("history provider unavailable: simulated outage");
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gate_that_reports_a_provider_failure_is_degraded_not_passed() {
    let (_tmp, state, dir) = build_state();
    let config = ReviewConfig::default();
    let gates: Vec<Box<dyn Gate>> = vec![Box::new(HalfBlindGate)];
    let (findings, gates_run, files, outcomes) =
        run_pre_commit_review_with(&state, PID, &dir, 1, DIFF, &config, gates)
            .await
            .unwrap();
    assert_eq!((gates_run, files), (1, 1));
    let o = outcomes.iter().find(|o| o.name == "half_blind").unwrap();
    match &o.status {
        GateStatus::Degraded { findings, notes } => {
            assert_eq!(*findings, 0);
            assert!(
                notes.iter().any(|n| n.contains("simulated outage")),
                "{notes:?}"
            );
        }
        other => panic!("expected Degraded, got {other:?}"),
    }

    assert_ne!(
        Verdict::with_outcomes(&findings, &outcomes),
        Verdict::Green,
        "partial evidence cannot be a green light"
    );

    let md = render_markdown(&findings, files, gates_run, 5, &outcomes);
    assert!(md.contains("DEGRADED"), "{md}");
    assert!(md.contains("simulated outage"), "{md}");
    assert!(
        !md.contains("passed all gates cleanly"),
        "clean bill printed on partial evidence:\n{md}"
    );

    let json = render_json(findings, files, gates_run, 5, &outcomes);
    let v: serde_json::Value = serde_json::to_value(&json).unwrap();
    assert_eq!(v["summary"]["gates_degraded"], 1, "{v}");
    assert_eq!(v["gate_status"][0]["status"]["kind"], "degraded", "{v}");
    assert_ne!(v["verdict"], "green", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_gate_that_cannot_read_the_file_says_so() {
    let (_tmp, state, dir) = build_state();
    let config = ReviewConfig::default();
    let gates: Vec<Box<dyn Gate>> = all_gates()
        .into_iter()
        .filter(|g| g.name() == "complexity_budget")
        .collect();
    assert_eq!(gates.len(), 1);
    let (_findings, _gates_run, _files, outcomes) =
        run_pre_commit_review_with(&state, PID, &dir, 1, DIFF, &config, gates)
            .await
            .unwrap();
    let o = outcomes
        .iter()
        .find(|o| o.name == "complexity_budget")
        .unwrap();
    match &o.status {
        GateStatus::Degraded { notes, .. } => assert!(
            notes
                .iter()
                .any(|n| n.contains("missing.vb") && n.to_lowercase().contains("read")),
            "{notes:?}"
        ),
        other => panic!("an unreadable working-tree file was silently treated as empty: {other:?}"),
    }
}
