#![allow(clippy::unwrap_used)]
//! End-to-end tests for `ingest_code_review_history` using a fixture
//! JSONL (so we exercise the full parse → cluster → store pipeline
//! against real-shaped data without hitting Azure DevOps).

use std::io::Write;
use std::path::PathBuf;

use engram_core::config::Config;
use engram_server::services::code_review_ingest_service::{
    ingest_code_review_history, IngestConfig, IngestSource,
};
use engram_server::services::project_service::ensure_project_record;
use engram_server::state::AppState;

// ─── helpers ────────────────────────────────────────────────────────────────

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir],
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
    (tmp, state)
}

async fn register_project(state: &AppState, tmp: &tempfile::TempDir) -> String {
    // Build a minimal ProjectRecord directly — ensure_project_record
    // expects one to exist in the registry. We bypass the full
    // index_project path because this test doesn't need FTS/vector.
    let project_dir = tmp.path().join("project");
    let rec = engram_core::ProjectRecord {
        project_id: "cr-test".into(),
        project_name: "cr-test".into(),
        directory: project_dir.to_string_lossy().into_owned(),
        project_type: "general".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        reindex_required_since_ms: None,
    };
    state.registry.put_project(&rec).unwrap();
    let _ = ensure_project_record(state, "cr-test").await.unwrap();
    // Seed active_generation so ensure_project_runtime + index_docs
    // have a valid generation key to read.
    state
        .registry
        .set_meta("cr-test", "active_generation", "1")
        .unwrap();
    "cr-test".into()
}

fn write_fixture_jsonl(tmp: &tempfile::TempDir, records: &[serde_json::Value]) -> PathBuf {
    let path = tmp.path().join("reviews.jsonl");
    let mut f = std::fs::File::create(&path).unwrap();
    for r in records {
        writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
    }
    path
}

fn mk_record(
    pr: u64,
    status: &str,
    file: &str,
    severity: &str,
    body: &str,
) -> serde_json::Value {
    serde_json::json!({
        "pr_id": pr,
        "pr_title": format!("PR {pr}"),
        "pr_author": "tester",
        "pr_date": "2026-01-01",
        "pr_branch": "main",
        "pr_url": format!("https://example/{pr}"),
        "thread_id": pr * 1000,
        "thread_status": status,
        "file_path": file,
        "line_start": 10,
        "line_end": 20,
        "severity": severity,
        "coderabbit_comment": body,
    })
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_ingest_indexes_clusters_and_separates_wontfix() {
    let (tmp, state) = build_state();
    let project_id = register_project(&state, &tmp).await;

    // Three PRs all flag the same pattern — a static-cache-on-
    // document-scoped-object issue — then resolved with a ✅ marker.
    let body_fix = "_⚠️ Potential issue_ | _🟠 Major_\n\n\
        **Move the PdfExtGState cache to instance scope.**\n\n\
        The static `_fillOpacityStates` cache is shared across all \
        `GisPdfExport` instances, and reusing `PdfExtGState` objects \
        across a new `PdfDocument` in `WriteToDisk()` causes \
        \"indirect object belongs to other PDF document\" errors.\n\n\
        ✅ Addressed in commits 8133c13 to 1331879";

    // One wontFix — the classic window.gQtyManager null-check
    // suppression we want scoped to qtyManager files specifically.
    let body_wontfix = "_⚠️ Potential issue_ | _🟡 Minor_\n\n\
        **Consider guarding `gQtyManager.validate()` against missing globals.**\n\n\
        The call site assumes `window.gQtyManager` is always defined. A null \
        check on `gQtyManager` would harden the `validate` path.";

    let fixture = vec![
        mk_record(1, "fixed", "/Site/Export/GisPdfExport.vb", "major", body_fix),
        mk_record(
            2,
            "fixed",
            "/Site/Export/GisPdfExport.vb",
            "major",
            body_fix,
        ),
        mk_record(
            3,
            "fixed",
            "/Site/Export/GisPdfExport.vb",
            "major",
            body_fix,
        ),
        mk_record(
            4,
            "wontFix",
            "/Site/ts/qty/qtyManager.ts",
            "minor",
            body_wontfix,
        ),
    ];
    let path = write_fixture_jsonl(&tmp, &fixture);

    let config = IngestConfig {
        source: IngestSource::JsonlFile { path },
        min_fix_rate: 0.5,
        ..Default::default()
    };
    let stats = ingest_code_review_history(&state, &project_id, config)
        .await
        .expect("ingest must succeed");

    assert!(stats.total_raw >= 4, "got {stats:#?}");
    assert!(stats.parsed_success >= 4);
    assert_eq!(stats.clusters_produced, 1, "expected one positive cluster");
    assert_eq!(
        stats.suppression_clusters, 1,
        "expected one wontFix suppression cluster"
    );
    assert!(stats.antipattern_docs_indexed >= 1);
    assert!(stats.suppression_docs_indexed >= 1);
    assert!(stats.graph_nodes_created >= 2, "two review_pattern nodes");
    assert!(stats.graph_edges_created >= 2, "edges to each file");
    assert!(
        stats.repo_rules_promoted >= 1,
        "3-PR 100% fix-rate cluster must auto-promote"
    );
    assert_eq!(stats.newest_pr_id, Some(4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_ingest_respects_incremental_marker() {
    let (tmp, state) = build_state();
    let project_id = register_project(&state, &tmp).await;

    let body = "_⚠️ Potential issue_ | _🟠 Major_\n\n\
        **Move the PdfExtGState cache to instance scope.**\n\n\
        `_fillOpacityStates` collides across `GisPdfExport` / `PdfDocument` / `WriteToDisk`.\n\n\
        ✅ Addressed in commits 8133c13";
    // First run: index PR 1.
    let p1 = write_fixture_jsonl(
        &tmp,
        &[mk_record(1, "fixed", "/x/GisPdfExport.vb", "major", body)],
    );
    let config1 = IngestConfig {
        source: IngestSource::JsonlFile { path: p1 },
        ..Default::default()
    };
    let s1 = ingest_code_review_history(&state, &project_id, config1)
        .await
        .unwrap();
    assert_eq!(s1.parsed_success, 1);
    assert_eq!(s1.newest_pr_id, Some(1));

    // Second run — same path but add PR 2. PR 1 should be skipped via
    // the registry's last_pr_id marker.
    let p2 = write_fixture_jsonl(
        &tmp,
        &[
            mk_record(1, "fixed", "/x/GisPdfExport.vb", "major", body),
            mk_record(2, "fixed", "/x/GisPdfExport.vb", "major", body),
        ],
    );
    let config2 = IngestConfig {
        source: IngestSource::JsonlFile { path: p2 },
        ..Default::default()
    };
    let s2 = ingest_code_review_history(&state, &project_id, config2)
        .await
        .unwrap();
    assert!(s2.incremental_skipped_prs >= 1, "PR 1 must be skipped");
    assert_eq!(s2.newest_pr_id, Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_ingest_force_full_rescan_ignores_marker() {
    let (tmp, state) = build_state();
    let project_id = register_project(&state, &tmp).await;

    let body = "_⚠️ Potential issue_ | _🟠 Major_\n\n\
        **Move the PdfExtGState cache to instance scope.**\n\n\
        `_fillOpacityStates` collides across `GisPdfExport` / `PdfDocument` / `WriteToDisk`.\n\n\
        ✅ Addressed in commits 8133c13";
    let path = write_fixture_jsonl(
        &tmp,
        &[mk_record(1, "fixed", "/x/GisPdfExport.vb", "major", body)],
    );

    // Prime last_pr_id.
    let first = IngestConfig {
        source: IngestSource::JsonlFile { path: path.clone() },
        ..Default::default()
    };
    ingest_code_review_history(&state, &project_id, first)
        .await
        .unwrap();

    // Rerun with force_full_rescan — should NOT skip PR 1.
    let rescan = IngestConfig {
        source: IngestSource::JsonlFile { path },
        force_full_rescan: true,
        ..Default::default()
    };
    let s = ingest_code_review_history(&state, &project_id, rescan)
        .await
        .unwrap();
    assert_eq!(s.incremental_skipped_prs, 0);
    assert_eq!(s.parsed_success, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suppression_is_scoped_to_wontfix_file_family_not_language() {
    // Cluster has 3 members total:
    //   - 2 fixed (in /Site/export/*.vb)      → positive cluster lives at that dir
    //   - 1 wontFix (in /Site/ts/qty/*.ts)    → suppression cluster MUST be scoped
    //                                             to /site/ts/qty/, not */**.ts
    //
    // The wontFix is in a different language from the fixed members,
    // so they won't cluster together at all — but the invariant still
    // holds: the suppression cluster inherits its file patterns from
    // the wontFix member only, never from the positive partition.
    let (tmp, state) = build_state();
    let project_id = register_project(&state, &tmp).await;

    let body_fix = "_⚠️ Potential issue_ | _🟠 Major_\n\n\
        **Avoid calling `SubmitChanges()` without audit log.**\n\n\
        `SubmitChanges()` on `DataContext` must be preceded by `handelselogg.Create()`.\n\n\
        ✅ Addressed in commits abc1234";
    let body_wontfix = "_⚠️ Potential issue_ | _🟡 Minor_\n\n\
        **Consider null-checking `gQtyManager.validate()`.**\n\n\
        The `gQtyManager.validate()` call on `window` may throw if globals aren't ready.";

    let fixture = vec![
        mk_record(10, "fixed", "/Site/export/Orders.vb", "major", body_fix),
        mk_record(11, "fixed", "/Site/export/Orders.vb", "major", body_fix),
        mk_record(
            12,
            "wontFix",
            "/Site/ts/qty/qtyManager.ts",
            "minor",
            body_wontfix,
        ),
    ];
    let path = write_fixture_jsonl(&tmp, &fixture);

    let stats = ingest_code_review_history(
        &state,
        &project_id,
        IngestConfig {
            source: IngestSource::JsonlFile { path },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // Two clusters (different languages), one doc each.
    assert_eq!(stats.clusters_produced, 1, "positive VB cluster");
    assert_eq!(stats.suppression_clusters, 1, "suppression TS cluster");

    // The suppression cluster's graph node must have a file_pattern
    // that targets the qtyManager.ts family — not every *.ts in the
    // repo. We read the review_pattern nodes back from the graph and
    // inspect their metadata.
    let graph = state.graph.clone();
    let pid = project_id.clone();
    let supp_nodes = tokio::task::spawn_blocking(move || {
        graph.query_nodes(&pid, Some("review_pattern"), None, None, 100)
    })
    .await
    .unwrap()
    .unwrap();
    // Find the suppression one (node_id starts with review_suppression:).
    let supp = supp_nodes
        .iter()
        .find(|n| n.node_id.starts_with("review_suppression:"))
        .expect("expected one suppression node");
    let meta = supp
        .metadata
        .as_ref()
        .expect("metadata present")
        .to_string();
    assert!(
        meta.contains("/site/ts/qty"),
        "suppression file pattern must be scoped to qtyManager family, got: {meta}"
    );
    // And critically, it must NOT contain the fixed-cluster's file
    // path (/Site/export/Orders.vb) — that would mean the suppression
    // bled across into the positive partition's files.
    assert!(
        !meta.contains("orders.vb"),
        "suppression must NOT inherit file patterns from positive members: {meta}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jsonl_ingest_drops_clusters_below_min_fix_rate() {
    let (tmp, state) = build_state();
    let project_id = register_project(&state, &tmp).await;

    // Two findings — one fixed, two wontFix → fix_rate = 1/3 ≈ 33%.
    // With default min_fix_rate=0.5 the positive cluster should be
    // filtered out; the wontFix members go to suppression regardless.
    let fix = "_⚠️ Potential issue_ | _🟠 Major_\n\n\
        **Consider adding a null check on `foo()` before invoking `doThing()`.**\n\n\
        `foo()` may return null when `SharedState` is mid-rotation.\n\n\
        ✅ Addressed in commits 0000111";
    let wontfix = "_⚠️ Potential issue_ | _🟠 Major_\n\n\
        **Consider adding a null check on `foo()` before invoking `doThing()`.**\n\n\
        `foo()` may return null when `SharedState` is mid-rotation.";
    let fixture = vec![
        mk_record(10, "fixed", "/a/A.ts", "major", fix),
        mk_record(11, "wontFix", "/a/B.ts", "major", wontfix),
        mk_record(12, "wontFix", "/a/C.ts", "major", wontfix),
    ];
    let path = write_fixture_jsonl(&tmp, &fixture);

    let config = IngestConfig {
        source: IngestSource::JsonlFile { path },
        min_fix_rate: 0.5,
        ..Default::default()
    };
    let s = ingest_code_review_history(&state, &project_id, config)
        .await
        .unwrap();
    // Positive cluster filtered: 33% < 50%.
    assert_eq!(
        s.antipattern_docs_indexed, 0,
        "positive cluster should be dropped below min_fix_rate"
    );
    // Suppression still indexed.
    assert!(s.suppression_docs_indexed >= 1);
}
