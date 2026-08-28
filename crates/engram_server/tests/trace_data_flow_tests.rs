#![allow(clippy::unwrap_used)]
//! Row-7 audit (docs/audits/06-causal-trace-engine.md) slice 1:
//! graph steps belong to the traced METHOD, not to any method in the same
//! file (the live false step); dotted / argument-bearing calls are steps
//! (resolved or explicitly unresolved) instead of invisible; `output_json`
//! returns the full trace; hop counts in messages equal the searched depth.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::{TraceDataFlowRequest, TraceUiEventRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};

const PID: &str = "trace-test";
const FILE: &str = "Site/App_Code/api/api-x.vb";

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

fn func_node(path: &str, class: &str, name: &str, start: u32, end: u32) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{class}.{name}:{start}"),
        node_type: "function".into(),
        name: name.into(),
        namespace: class.into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: start,
        end_line: end,
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

/// Two methods in one file. `Exporter` writes a Session key (graph edge);
/// `Filter` does not — but calls a domain helper with arguments.
const SRC: &str = "Public Class api\n\
    Public Function Exporter(qry As Query) As String\n\
        Session(\"map_iomarker_export_ids\") = ids\n\
        Return \"ok\"\n\
    End Function\n\
\n\
    Public Function Filter(qry As Query) As String\n\
        If Not _us.UserAccess.CheckRead(_us.UserAccessObject.vs_karta_io_objekt) Then Return s\n\
        Dim pr_id = GetDictionaryIntegerValue(qry.params, \"pr_id\")\n\
        Dim rows = _io.installationsobjektprojekt.GetAllByCheckingTotalProject(pr_id, db)\n\
        Return \"ok\"\n\
    End Function\n\
End Class\n";

fn seed(state: &AppState, dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("Site/App_Code/api")).unwrap();
    std::fs::write(dir.join(FILE), SRC).unwrap();
    let exporter = func_node(FILE, "api", "Exporter", 2, 5);
    let filter = func_node(FILE, "api", "Filter", 7, 12);
    let eid = exporter.node_id.clone();
    state.graph.upsert_nodes(PID, &[exporter, filter]).unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[edge(
                &eid,
                "state:Session:map_iomarker_export_ids",
                EdgeKind::WritesState,
            )],
        )
        .unwrap();
}

async fn trace(engram: &Engram, body: Value) -> String {
    let req: TraceDataFlowRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_trace_data_flow(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_steps_are_attributed_to_the_traced_method_only() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);

    let filter = trace(
        &engram,
        json!({"project_id": PID, "file_path": FILE, "entry_point": "Filter", "output_json": true}),
    )
    .await;
    let v: Value =
        serde_json::from_str(&filter).unwrap_or_else(|e| panic!("not JSON ({e}):\n{filter}"));
    let steps = v["steps"].to_string();
    assert!(
        !steps.contains("map_iomarker_export_ids"),
        "Exporter's Session write was attributed to Filter (file-substring matching):\n{steps}"
    );

    // Positive control: the method that owns the edge does get the step.
    let exporter = trace(
        &engram,
        json!({"project_id": PID, "file_path": FILE, "entry_point": "Exporter", "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&exporter).unwrap();
    assert!(
        v["steps"].to_string().contains("map_iomarker_export_ids"),
        "{}",
        v["steps"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dotted_and_argument_calls_are_steps_resolved_or_unresolved() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);

    let out = trace(
        &engram,
        json!({"project_id": PID, "file_path": FILE, "entry_point": "Filter", "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&out).unwrap();
    let steps = v["steps"].as_array().unwrap();
    let calls: Vec<&Value> = steps
        .iter()
        .filter(|s| s["step_type"] == "MethodCall")
        .collect();
    let text = serde_json::to_string(&calls).unwrap();
    for name in [
        "CheckRead",
        "GetDictionaryIntegerValue",
        "GetAllByCheckingTotalProject",
    ] {
        assert!(text.contains(name), "call {name} is not a step:\n{text}");
    }
    assert!(
        calls.iter().all(|c| c["resolved"].is_boolean()),
        "every call step says whether it resolved to a graph node:\n{text}"
    );
    assert!(
        v["unresolved_calls"].as_u64().unwrap_or(0) >= 1,
        "the header must count unresolved calls: {}",
        v["unresolved_calls"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_is_rendered_and_json_carries_the_full_trace() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);

    let md = trace(
        &engram,
        json!({"project_id": PID, "file_path": FILE, "entry_point": "Exporter"}),
    )
    .await;
    assert!(
        !md.starts_with("Trace: [DataFlowStep"),
        "Debug formatting is not a rendering:\n{md}"
    );
    assert!(
        md.contains("# Data flow") || md.contains("## Steps"),
        "{md}"
    );

    let js = trace(
        &engram,
        json!({"project_id": PID, "file_path": FILE, "entry_point": "Exporter", "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&js).unwrap_or_else(|e| panic!("not JSON ({e}):\n{js}"));
    for key in [
        "entry_point",
        "steps",
        "tables_touched",
        "state_writes",
        "methods_called",
        "modern_flow_hint",
    ] {
        assert!(!v[key].is_null(), "missing {key}: {v}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_event_no_path_message_states_the_searched_depth() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let page = "Site/pages/x.aspx";
    std::fs::create_dir_all(dir.join("Site/pages")).unwrap();
    std::fs::write(
        dir.join(page),
        "<asp:Button ID=\"btnGo\" runat=\"server\" />",
    )
    .unwrap();
    state
        .graph
        .upsert_nodes(
            PID,
            &[Node {
                node_id: format!("control:{page}:btnGo"),
                node_type: "control".into(),
                name: "btnGo".into(),
                namespace: "x".into(),
                language: "aspx".into(),
                file_path: RelPath::new(page),
                start_line: 1,
                end_line: 1,
                generation: 1,
                metadata: None,
            }],
        )
        .unwrap();
    let engram = Engram::new(state);

    let req: TraceUiEventRequest = serde_json::from_value(
        json!({"project_id": PID, "page_path": page, "control_id": "btnGo"}),
    )
    .unwrap();
    let res = engram.handle_trace_ui_event(req).await.unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();
    assert!(
        text.contains("within 8 hops"),
        "the default max_hops (10) is clamped to 8 for the search — the message must say 8:\n{text}"
    );
}
