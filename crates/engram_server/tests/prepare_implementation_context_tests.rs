#![allow(clippy::unwrap_used)]
//! Round-6: prepare_implementation_context must resolve the target method with
//! the SAME exact-name-preferring, ambiguity-refusing resolver the rest of the
//! access layer uses (select_method_node), not a hand-rolled substring scan.
//!
//! The round-5 hand-rolled block matched method names by SUBSTRING (via
//! query_nodes) and then either returned candidates[0] or, after the round-5
//! patch, flagged >1 candidate as AMBIGUOUS. That produced a FALSE ambiguity:
//! asking for `GetAll` also substring-matched `GetAllHistory` in the same
//! class, so a perfectly unambiguous request was refused. These tests drive
//! the REAL handler to prove the exact-name preference now resolves it.

use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

async fn fixture() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site")).unwrap();
    // `GetAll` is a proper substring of `GetAllHistory`: a substring resolver
    // sees two candidates in one class and cannot tell them apart.
    std::fs::write(
        root.join("Site/orders.vb"),
        "Public Class orders\n    Public Function GetAll() As String\n        Return \"all-rows\"\n    End Function\n    Public Function GetAllHistory() As String\n        Return \"history-rows\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(512 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "PrepareFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

/// Only the cheap, deterministic pieces — no git style profile, no caller
/// pattern mining — so the test exercises resolution, not the providers.
fn prepare_req(pid: &str, method: &str) -> serde_json::Value {
    json!({
        "project_id": pid,
        "file_path": "Site/orders.vb",
        "method_name": method,
        "include_style_profile": false,
        "include_pattern_examples": false,
        "include_db_schema": false,
        "include_sp_signatures": false,
        "include_state_context": false,
        "include_control_mappings": false,
        "output_json": true,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn substring_sibling_does_not_create_false_ambiguity() {
    let (_t, _s, engram, pid) = fixture().await;
    let req = serde_json::from_value(prepare_req(&pid, "GetAll")).unwrap();
    let res = engram
        .handle_prepare_implementation_context(req)
        .await
        .expect("GetAll is unambiguous; the resolver must not refuse it");
    let out = res.content[0].as_text().unwrap().text.clone();
    // Resolved to the EXACT method, not its substring sibling, and never the
    // hand-rolled false-ambiguity error. method_name is class-qualified, so the
    // closing quote discriminates `orders.GetAll` from `orders.GetAllHistory`.
    assert!(
        out.contains("\"method_name\": \"orders.GetAll\""),
        "must resolve to exact orders.GetAll:\n{out}"
    );
    assert!(!out.contains("AMBIGUOUS"), "no false ambiguity:\n{out}");
    assert!(
        out.contains("all-rows") && !out.contains("history-rows"),
        "must read GetAll's body, not GetAllHistory's:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_method_surfaces_an_error_not_a_wrong_method() {
    let (_t, _s, engram, pid) = fixture().await;
    let req = serde_json::from_value(prepare_req(&pid, "NoSuchMethod")).unwrap();
    let res = engram.handle_prepare_implementation_context(req).await;
    // The hand-rolled path could silently fall through to candidates[0]; the
    // resolver must instead surface a lookup failure.
    assert!(
        res.is_err(),
        "a nonexistent method must error, not resolve to some other method"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn longer_sibling_still_resolves_exactly() {
    // The reverse direction: asking for the LONGER name must not be dragged to
    // the shorter substring match either.
    let (_t, _s, engram, pid) = fixture().await;
    let req = serde_json::from_value(prepare_req(&pid, "GetAllHistory")).unwrap();
    let res = engram
        .handle_prepare_implementation_context(req)
        .await
        .expect("GetAllHistory is unambiguous");
    let out = res.content[0].as_text().unwrap().text.clone();
    assert!(
        out.contains("\"method_name\": \"orders.GetAllHistory\""),
        "must resolve to exact orders.GetAllHistory:\n{out}"
    );
    assert!(
        out.contains("history-rows"),
        "must read GetAllHistory's body:\n{out}"
    );
}
