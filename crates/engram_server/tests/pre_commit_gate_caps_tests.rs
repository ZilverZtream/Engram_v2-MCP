#![allow(clippy::unwrap_used)]
//! Row-3 audit (docs/audits/05-pre-commit-gates.md) A4: the 17 gates
//! carry internal caps (blast_radius looks at 20 files, temporal at 20
//! neighbours, unwired at 25 candidates, antipattern at 5 hits, …) that
//! never reached the output — a clean gate could simply have stopped
//! looking. Every cap a gate hits is a fact on its outcome, in JSON and
//! in the markdown.

use engram_core::config::Config;
use engram_server::services::pre_commit_review_service::{
    Gate, GateContext, GateStatus, ReviewConfig, ReviewFinding, all_gates, render_json,
    render_markdown, run_pre_commit_review_with,
};
use engram_server::state::AppState;

const PID: &str = "gate-caps-test";

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

/// A diff touching 25 files — more than blast_radius's FILE_CAP (20).
fn wide_diff() -> String {
    let mut d = String::new();
    for i in 0..25 {
        d.push_str(&format!(
            "diff --git a/Site/App_Code/f{i:02}.vb b/Site/App_Code/f{i:02}.vb\n\
             --- a/Site/App_Code/f{i:02}.vb\n\
             +++ b/Site/App_Code/f{i:02}.vb\n\
             @@ -1,2 +1,3 @@\n\
             \x20Public Class F{i:02}\n\
             +    Public Sub Added()\n\
             \x20End Class\n"
        ));
    }
    d
}

struct CappedGate;
impl Gate for CappedGate {
    fn name(&self) -> &'static str {
        "capped"
    }
    fn run(&self, ctx: &GateContext<'_>) -> anyhow::Result<Vec<ReviewFinding>> {
        ctx.note_cap("looked at 20 of 25 files (FILE_CAP 20)");
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cap_a_gate_hit_is_a_fact_on_its_outcome() {
    let (_tmp, state, dir) = build_state();
    let config = ReviewConfig::default();
    let gates: Vec<Box<dyn Gate>> = vec![Box::new(CappedGate)];
    let (findings, gates_run, files, outcomes) =
        run_pre_commit_review_with(&state, PID, &dir, 1, &wide_diff(), &config, gates)
            .await
            .unwrap();
    let o = outcomes.iter().find(|o| o.name == "capped").unwrap();
    assert_eq!(
        o.status,
        GateStatus::Passed,
        "a cap is not a failure: {o:?}"
    );
    assert!(
        o.caps.iter().any(|c| c.contains("20 of 25")),
        "the cap must be recorded on the outcome: {o:?}"
    );
    let md = render_markdown(&findings, files, gates_run, 5, &outcomes);
    assert!(
        md.contains("20 of 25"),
        "the markdown must state the cap:\n{md}"
    );
    let json = render_json(findings, files, gates_run, 5, &outcomes);
    let v: serde_json::Value = serde_json::to_value(&json).unwrap();
    assert!(
        v["gate_status"][0]["caps"].to_string().contains("20 of 25"),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blast_radius_states_its_file_cap_on_a_wide_diff() {
    let (_tmp, state, dir) = build_state();
    let config = ReviewConfig::default();
    let gates: Vec<Box<dyn Gate>> = all_gates()
        .into_iter()
        .filter(|g| g.name() == "blast_radius")
        .collect();
    assert_eq!(gates.len(), 1);
    let (_findings, _gates_run, _files, outcomes) =
        run_pre_commit_review_with(&state, PID, &dir, 1, &wide_diff(), &config, gates)
            .await
            .unwrap();
    let o = outcomes.iter().find(|o| o.name == "blast_radius").unwrap();
    assert!(
        o.caps.iter().any(|c| c.contains("20") && c.contains("25")),
        "25 files in the diff, 20 looked at — the cap must be stated: {o:?}"
    );
}
