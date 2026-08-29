#![allow(clippy::unwrap_used)]
//! Row-4 audit (docs/audits/04-concept-and-consumer-discovery.md) A9:
//! three tools gave three incoming counts for the same node (`Check_pr_id`:
//! 78 / 98 / 50) because each counts a different thing — every edge of
//! every kind, causal kinds only, or distinct callers over calls+dependency
//! capped at 50 — and none said which. A count without its rule is a number
//! an agent cannot reconcile. Every rendered incoming count names its kind
//! set and dedup rule, and `find_symbol_references` prints BOTH the edge
//! count and the distinct-caller count that the edit tools use.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::{FindSymbolReferencesRequest, GetMethodEditContextRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "incoming-parity-test";
const FILE: &str = "Site/App_Code/us/accessctrl.vb";

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

fn func(path: &str, class: &str, name: &str, start: u32) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{class}.{name}:{start}"),
        node_type: "function".into(),
        name: name.into(),
        namespace: class.into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: start,
        end_line: start + 3,
        generation: 1,
        metadata: None,
    }
}

fn edge(src: &str, tgt: &str, kind: EdgeKind) -> Edge {
    Edge {
        source_id: src.into(),
        target_id: tgt.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        edge_kind: kind,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

/// `Check_pr_id` has 3 distinct callers; one of them carries BOTH a Calls
/// and a Dependency edge, and a fourth node reads state from it — 5
/// incoming edges across all kinds, 3 distinct callers.
fn seed(state: &AppState, dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("Site/App_Code/us")).unwrap();
    std::fs::write(
        dir.join(FILE),
        "Public Class accessctrl\n    Public Function Check_pr_id(id As Integer) As Boolean\n        Return True\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let target = func(FILE, "accessctrl", "Check_pr_id", 2);
    let a = func("Site/App_Code/api/a.vb", "api", "A", 1);
    let b = func("Site/App_Code/api/b.vb", "api", "B", 1);
    let c = func("Site/App_Code/api/c.vb", "api", "C", 1);
    let d = func("Site/App_Code/api/d.vb", "api", "D", 1);
    let tid = target.node_id.clone();
    let edges = vec![
        edge(&a.node_id, &tid, EdgeKind::Calls),
        edge(&a.node_id, &tid, EdgeKind::Dependency),
        edge(&b.node_id, &tid, EdgeKind::Calls),
        edge(&c.node_id, &tid, EdgeKind::Dependency),
        edge(&d.node_id, &tid, EdgeKind::ReadsState),
    ];
    state
        .graph
        .upsert_nodes(PID, &[target, a, b, c, d])
        .unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn symbol_references_names_both_counts_and_their_rules() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);
    let req: FindSymbolReferencesRequest =
        serde_json::from_value(json!({"project_id": PID, "symbol_name": "Check_pr_id"})).unwrap();
    let out = engram
        .handle_find_symbol_references(req)
        .await
        .unwrap()
        .content[0]
        .as_text()
        .unwrap()
        .text
        .clone();
    assert!(
        out.contains("5 edge") && out.contains("all kinds"),
        "the edge count must name its kind set (all kinds):\n{out}"
    );
    assert!(
        out.contains("3 distinct caller"),
        "the distinct-caller count (calls+dependency, dedup by caller) must be printed beside it:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_context_names_its_caller_rule() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);
    let req: GetMethodEditContextRequest = serde_json::from_value(
        json!({"project_id": PID, "file_path": FILE, "method_name": "Check_pr_id"}),
    )
    .unwrap();
    let out = engram
        .handle_get_method_edit_context(req)
        .await
        .unwrap()
        .content[0]
        .as_text()
        .unwrap()
        .text
        .clone();
    assert!(
        out.contains("3") && out.to_lowercase().contains("distinct caller"),
        "the callers line must say it counts distinct callers over calls+dependency:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("calls+dependency")
            || out.to_lowercase().contains("calls + dependency"),
        "the kind set must be named:\n{out}"
    );
}
