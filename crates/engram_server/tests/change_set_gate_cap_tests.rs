#![allow(clippy::unwrap_used)]
//! Row-1 audit (docs/audits/03-story-to-change-scope.md) D10: the
//! "Permission gates in the candidate set" section listed the ten most
//! frequent gate types and dropped the rest silently — a planner reading
//! the brief could not know a gate was cut. The cut is a fact: the
//! markdown states "… and N more gate type(s)" and the JSON carries the
//! omitted names.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::models::GetChangeSetRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};

const PID: &str = "change-set-gate-cap-test";
const STORY: &str =
    "As an admin I want to set the invoice category on a project so invoices roll up correctly";
const GATE_TYPES: usize = 13;

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

fn gated_func(path: &str, name: &str, line: u32, gate: &str) -> Node {
    Node {
        node_id: format!("sym:function:{path}:_rv.InvoiceCategory.{name}:{line}"),
        node_type: "function".into(),
        name: name.into(),
        namespace: "_rv.InvoiceCategory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line + 5,
        generation: 1,
        metadata: Some(json!({"permission_checks": gate})),
    }
}

/// One candidate file whose gated symbols carry MORE distinct gate types
/// than the section's cap (10): gate01 … gate13, each on one symbol except
/// gate01 (two symbols) so the order is deterministic (count desc, name asc).
fn seed(state: &AppState) {
    let a = "Site/App_Code/invoice/InvoiceCategory.vb";
    let mut nodes = vec![file_node(a)];
    for i in 1..=GATE_TYPES {
        nodes.push(gated_func(
            a,
            &format!("SaveInvoiceCategory{i:02}"),
            (i * 10) as u32,
            &format!("gate{i:02}"),
        ));
    }
    nodes.push(gated_func(a, "GetInvoiceCategories", 900, "gate01"));
    state.graph.upsert_nodes(PID, &nodes).unwrap();
}

async fn change_set_md(engram: &Engram) -> String {
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": PID, "story": STORY})).unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

async fn change_set_json(engram: &Engram) -> Value {
    let req: GetChangeSetRequest =
        serde_json::from_value(json!({"project_id": PID, "story": STORY, "output_json": true}))
            .unwrap();
    let res = engram.handle_get_change_set(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gate_type_cap_is_stated_in_the_markdown() {
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let md = change_set_md(&engram).await;

    let section = md
        .split("## Permission gates in the candidate set")
        .nth(1)
        .unwrap_or_else(|| panic!("no permission-gates section:\n{md}"));
    let listed = section.lines().filter(|l| l.starts_with("- gate")).count();
    assert_eq!(listed, 10, "the cap still lists ten gate types:\n{section}");
    assert!(
        section.contains(&format!("and {} more gate type(s)", GATE_TYPES - 10)),
        "the cut must be stated (… and 3 more gate type(s)):\n{section}"
    );
    assert!(
        !section.contains("- gate13"),
        "gate13 is the least frequent and is cut:\n{section}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_omitted_gate_types_are_listed_in_the_json() {
    let (_tmp, state) = build_state();
    seed(&state);
    let engram = Engram::new(state);

    let v = change_set_json(&engram).await;

    let gates = &v["permission_gates"];
    assert!(
        gates.is_object(),
        "output_json.permission_gates must exist: {}",
        serde_json::to_string_pretty(&v).unwrap()
    );
    assert_eq!(gates["shown"], 10, "{gates}");
    assert_eq!(gates["total"], GATE_TYPES, "{gates}");
    let omitted: Vec<String> = gates["omitted"]
        .as_array()
        .unwrap_or_else(|| panic!("omitted must be an array: {gates}"))
        .iter()
        .map(|g| g.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        omitted,
        vec![
            "gate11".to_string(),
            "gate12".to_string(),
            "gate13".to_string()
        ],
        "the three cut gate types, least frequent last: {gates}"
    );
}
