#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 10 (change exposure / edit risk): the tools
//! must be ONE authority for "how many distinct callers" — find_symbol_references
//! recounted a capped incoming list (so above the cap it printed a lower bound)
//! while check_edit_safety / get_method_edit_context printed the cap-exact
//! figure. On a node with 60 callers both must print 60.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::{FindSymbolReferencesRequest, GetMethodEditContextRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "caller-parity-test";
const FILE: &str = "Site/App_Code/us/accessctrl.vb";
const CALLERS: usize = 60;

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

fn calls(src: &str, tgt: &str) -> Edge {
    Edge {
        source_id: src.into(),
        target_id: tgt.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

fn seed(state: &AppState, dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("Site/App_Code/us")).unwrap();
    std::fs::write(
        dir.join(FILE),
        "Public Class accessctrl\n    Public Function Check_pr_id(id As Integer) As Boolean\n        Return True\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let target = func(FILE, "accessctrl", "Check_pr_id", 2);
    let mut nodes = vec![target.clone()];
    let mut edges = Vec::new();
    for i in 0..CALLERS {
        let c = func(
            &format!("Site/App_Code/api/api{i:02}.vb"),
            "api",
            &format!("Caller{i:02}"),
            1,
        );
        edges.push(calls(&c.node_id, &target.node_id));
        nodes.push(c);
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_tools_print_the_same_distinct_caller_figure_above_the_display_cap() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);

    let req: FindSymbolReferencesRequest =
        serde_json::from_value(json!({"project_id": PID, "symbol_name": "Check_pr_id"})).unwrap();
    let refs = engram.handle_find_symbol_references(req).await.unwrap();
    let refs_out = refs.content[0].as_text().unwrap().text.clone();

    let req: GetMethodEditContextRequest = serde_json::from_value(
        json!({"project_id": PID, "file_path": FILE, "method_name": "Check_pr_id"}),
    )
    .unwrap();
    let ctx = engram.handle_get_method_edit_context(req).await.unwrap();
    let ctx_out = ctx.content[0].as_text().unwrap().text.clone();

    let expected = format!("{CALLERS} distinct caller");
    assert!(
        ctx_out.contains(&expected),
        "edit context must print the cap-exact figure {CALLERS}:\n{ctx_out}"
    );
    assert!(
        refs_out.contains(&expected),
        "find_symbol_references must print the SAME distinct-caller figure ({CALLERS}) — not a lower bound recounted from a capped list:\n{refs_out}"
    );
}
