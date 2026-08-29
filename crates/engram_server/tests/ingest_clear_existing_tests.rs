#![allow(clippy::unwrap_used)]
//! Row-3 audit A7 (live finding 2026-08-29): `ingest_quality_gates` documents
//! `clear_existing` as "reserved … currently a no-op", so a corpus ingested
//! with a broken parser could never be replaced — the quiet-failure class
//! "schema param no handler reads". `clear_existing=true` must purge the
//! project's `quality_gate` namespace before ingesting, and say so.

use engram_core::config::Config;
use engram_server::models::{IngestQualityGatesRequest, PrePushAuditRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

async fn build() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site")).unwrap();
    std::fs::write(root.join("Site/a.vb"), "Public Class a\nEnd Class\n").unwrap();
    std::fs::write(
        root.join("rules_v1.txt"),
        "- Every endpoint that reads pr_id must call check_pr_id before touching project data\n- Never build SQL by string concatenation; always parameterize\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rules_v2.txt"),
        "- Always return IQueryable from data-access helpers and call ToList only at the boundary\n",
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
            project_name: "IngestFixture".into(),
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

async fn ingest(engram: &Engram, pid: &str, path: &str, clear: bool) -> String {
    let req: IngestQualityGatesRequest = serde_json::from_value(json!({
        "project_id": pid, "source_path": path, "source_type": "text", "clear_existing": clear
    }))
    .unwrap();
    let res = engram.handle_ingest_quality_gates(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

fn qg_count(state: &AppState, pid: &str) -> usize {
    let engine = state.get_project_cached(pid).unwrap().search;
    engine
        .count_docs_by_namespace(pid)
        .unwrap()
        .get("quality_gate")
        .copied()
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_existing_replaces_the_namespace_instead_of_adding_to_it() {
    let (_tmp, state, engram, pid) = build().await;
    let first = ingest(&engram, &pid, "rules_v1.txt", false).await;
    assert!(first.contains("Ingested 2"), "{first}");
    assert_eq!(qg_count(&state, &pid), 2);

    let second = ingest(&engram, &pid, "rules_v2.txt", true).await;

    assert_eq!(
        qg_count(&state, &pid),
        1,
        "clear_existing=true must purge the old rules before ingesting: {second}"
    );
    assert!(
        second.contains("purged") || second.contains("cleared"),
        "the purge must be stated in the output: {second}"
    );
    let req: PrePushAuditRequest = serde_json::from_value(json!({
        "project_id": pid,
        "code": "Dim sql = \"SELECT * FROM projekt WHERE id = \" & pr_id",
        "file_path": "Site/a.vb",
        "top_k": 10
    }))
    .unwrap();
    let audit = engram.handle_pre_push_audit(req).await.unwrap();
    let out = audit.content[0].as_text().unwrap().text.clone();
    assert!(
        !out.contains("string concatenation"),
        "a purged rule must not be retrievable any more:\n{out}"
    );
    // The tally reflects the post-purge count in either header form:
    // "retrieved of 1 in the namespace" (a hit) or "1 rule(s) exist in the
    // … namespace and were searched" (no lexical hit for this probe).
    assert!(
        out.contains("of 1 in the namespace") || out.contains("1 rule(s) exist in the"),
        "the tally must reflect the purged namespace (1 rule):\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_clear_existing_ingest_still_accumulates() {
    let (_tmp, state, engram, pid) = build().await;
    ingest(&engram, &pid, "rules_v1.txt", false).await;
    ingest(&engram, &pid, "rules_v2.txt", false).await;
    assert_eq!(qg_count(&state, &pid), 3);
}
