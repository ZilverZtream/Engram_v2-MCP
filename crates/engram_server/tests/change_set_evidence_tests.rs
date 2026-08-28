#![allow(clippy::unwrap_used)]
//! Row-1 audit (docs/audits/03-story-to-change-scope.md), slice 1: the
//! change set is a typed evidence set — every candidate carries WHY it is
//! there, every retrieval arm reports what it delivered, caps become
//! reported omissions, and paths are the indexed (canonical) forms.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};

const PID: &str = "change-set-test";

fn build_state() -> (tempfile::TempDir, AppState) {
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
    (tmp, state)
}

fn file_node(path: &str) -> Node {
    Node {
        node_id: format!("file:{path}"),
        node_type: "file".into(),
        name: path.rsplit('/').next().unwrap().into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 0,
        end_line: 0,
        generation: 1,
        metadata: None,
    }
}

fn func_node(path: &str, class: &str, name: &str, line: u32) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{class}.{name}:{line}"),
        node_type: "function".into(),
        name: name.into(),
        namespace: class.into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line + 5,
        generation: 1,
        metadata: None,
    }
}

/// Two indexed files whose symbols carry the story's concept ("invoice"),
/// with MIXED-CASE, `Site/`-prefixed canonical paths.
fn seed(state: &AppState) {
    let a = "Site/App_Code/invoice/InvoiceCategory.vb";
    let b = "Site/modules/admin/InvoiceCategoryEdit.aspx.vb";
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                file_node(a),
                file_node(b),
                func_node(a, "_rv.InvoiceCategory", "GetInvoiceCategories", 10),
                func_node(a, "_rv.InvoiceCategory", "SaveInvoiceCategory", 40),
                func_node(b, "InvoiceCategoryEdit", "btnSaveInvoice_Click", 20),
            ],
        )
        .unwrap();
}

async fn change_set(engram: &Engram, body: Value) -> Value {
    let req: GetChangeSetRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

async fn change_set_md(engram: &Engram, body: Value) -> String {
    let req: GetChangeSetRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

const STORY: &str =
    "As an admin I want to set the invoice category on a project so invoices roll up correctly";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_output_carries_per_file_evidence_and_arm_coverage() {
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let v = change_set(
        &engram,
        json!({"project_id": PID, "story": STORY, "output_json": true}),
    )
    .await;

    let concepts = v["concepts"].as_array().expect("concepts array");
    assert!(
        concepts.iter().any(|c| c.as_str() == Some("invoice")),
        "concepts: {concepts:?}"
    );
    let files = v["files"].as_array().expect("files array");
    assert!(!files.is_empty(), "{v}");
    for f in files {
        assert!(f["path"].is_string(), "{f}");
        assert!(f["layer"].is_string(), "{f}");
        assert!(
            f["signals"].as_array().is_some_and(|s| !s.is_empty()),
            "{f}"
        );
        assert!(
            f["why"].as_array().is_some_and(|w| !w.is_empty()),
            "every candidate carries a rationale: {f}"
        );
    }
    let coverage = v["coverage"].as_object().expect("coverage object");
    for arm in ["concept", "history", "cochange", "vector"] {
        assert!(
            coverage[arm]["status"].is_string(),
            "arm {arm} must report a status: {coverage:?}"
        );
    }
    assert!(v["omissions"].is_array(), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_reports_every_arm_status_including_a_dead_vector_arm() {
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let md = change_set_md(&engram, json!({"project_id": PID, "story": STORY})).await;

    assert!(md.contains("## Coverage"), "no coverage section:\n{md}");
    for arm in ["concept", "history", "co-change", "vector"] {
        assert!(
            md.lines().any(|l| l.starts_with(&format!("- {arm}:"))),
            "arm '{arm}' has no status line:\n{md}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candidate_paths_are_the_indexed_canonical_forms() {
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let v = change_set(
        &engram,
        json!({"project_id": PID, "story": STORY, "output_json": true}),
    )
    .await;

    let paths: Vec<String> = v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    assert!(
        paths
            .iter()
            .any(|p| p == "Site/App_Code/invoice/InvoiceCategory.vb"),
        "the indexed path (case + Site/ prefix) must be used, got {paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.starts_with("site/") || p.starts_with("app_code/")),
        "lowercased / prefix-stripped variants leaked: {paths:?}"
    );
    for f in v["files"].as_array().unwrap() {
        assert_eq!(
            f["historical"], false,
            "indexed files are not historical: {f}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_flag_is_rejected_nowhere_and_markdown_stays_default() {
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let md = change_set_md(&engram, json!({"project_id": PID, "story": STORY})).await;
    assert!(
        md.starts_with("# ") || md.contains("Candidate files"),
        "{md}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_call_performs_one_full_node_scan() {
    // Slice 2 (audit D7): the 2-3 detect_incomplete_changes passes inside a
    // single get_change_set call share ONE node snapshot instead of each
    // re-scanning the whole project (200k-node cap, twice, every call).
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let v = change_set(
        &engram,
        json!({"project_id": PID, "story": STORY, "output_json": true}),
    )
    .await;

    assert_eq!(
        v["coverage"]["node_scans"], 1,
        "full node scans per call must be reported and be exactly one: {}",
        v["coverage"]
    );
}
