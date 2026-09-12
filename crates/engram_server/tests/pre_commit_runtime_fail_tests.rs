#![allow(clippy::unwrap_used)]
//! Doc-11 P1b (round-2 audit item 5 residue): the two runtime-constructing
//! gates (`product_intent` gates.rs:2832, `co_added_family` gates.rs:3050)
//! FAIL OPEN — when `ensure_project_runtime` errors they return zero
//! findings and record NOTHING, so the outcome renders `passed`. At the
//! runner level the completeness pre-check usually degrades such reviews
//! first, but the sites stay silent for any failure that arrives after the
//! pre-check (transient registry/store errors) — this test pins the GATE
//! contract itself: a runtime-construction failure must degrade the gate.

use engram_server::services::pre_commit_review_service::{
    Gate, GateContext, all_gates, parse_unified_diff,
};
use engram_server::state::AppState;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const PID: &str = "runtime-fail-test";

/// An ADDED, non-binary, non-test .vb file: `co_added_family` fires on the
/// Added change type; `product_intent` gets a non-empty query from the stem.
const DIFF: &str = "diff --git a/Site/modules/orders/panel/orderexport.vb b/Site/modules/orders/panel/orderexport.vb\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/Site/modules/orders/panel/orderexport.vb\n\
@@ -0,0 +1,2 @@\n\
+Public Class OrderExport\n\
+End Class\n";

/// A state whose registry KNOWS the project but whose runtime can never be
/// built: `data_dir/projects` is a regular FILE, so the tantivy/lancedb
/// directory creation inside `ensure_project_runtime` fails deterministically.
fn build_broken_state() -> (tempfile::TempDir, AppState, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = engram_core::config::Config {
        data_dir: data_dir.clone(),
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
    // The lever: the runtime's project root can never be created under a file.
    std::fs::write(data_dir.join("projects"), b"not a directory").unwrap();
    (tmp, state, project_dir)
}

/// Run ONE gate against a context whose completeness pre-check said nothing
/// (`search_index_note: None`) while the runtime cannot be built. The gate
/// itself must record the provider failure.
async fn degraded_notes_of(name: &'static str) -> Vec<String> {
    let (_tmp, state, dir) = build_broken_state();
    let diffs = parse_unified_diff(DIFF);
    assert!(!diffs.is_empty(), "the diff must parse");
    let changed: HashSet<String> = diffs.iter().map(|d| d.path.clone()).collect();
    let ctx = GateContext {
        state: &state,
        graph: state.graph.clone(),
        registry: state.registry.clone(),
        project_id: PID,
        project_dir: &dir,
        generation: 1,
        diff_files: &diffs,
        changed_paths: &changed,
        total_commits: 0,
        repo_rules: Arc::new(Vec::new()),
        files_by_parent: Arc::new(HashMap::new()),
        audit_function: None,
        search_index_note: None,
        degraded: Mutex::new(Vec::new()),
        caps: Mutex::new(Vec::new()),
    };
    let gate = all_gates()
        .into_iter()
        .find(|g| g.name() == name)
        .unwrap_or_else(|| panic!("gate {name} must exist"));
    let findings = gate.run_async(&ctx).await.expect("the gate must not error");
    assert!(
        findings.is_empty(),
        "no findings can exist without a runtime: {findings:?}"
    );
    let notes = ctx.degraded.lock().unwrap().clone();
    notes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn product_intent_degrades_when_the_runtime_cannot_be_built() {
    let notes = degraded_notes_of("product_intent").await;
    assert!(
        notes.iter().any(|n| n.contains("runtime")),
        "a runtime-construction failure must DEGRADE the gate, not pass it \
         silently; degraded notes: {notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_added_family_degrades_when_the_runtime_cannot_be_built() {
    let notes = degraded_notes_of("co_added_family").await;
    assert!(
        notes.iter().any(|n| n.contains("runtime")),
        "a runtime-construction failure must DEGRADE the gate, not pass it \
         silently; degraded notes: {notes:?}"
    );
}
