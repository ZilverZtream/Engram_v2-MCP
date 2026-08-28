#![allow(clippy::unwrap_used)]
//! Row-7 audit (docs/audits/06-causal-trace-engine.md) slice 2 — A3:
//! bounded recursive follow. A resolved call is followed through the
//! callee's own edges (Calls → deeper, table/state/SQL access = terminal)
//! up to a depth cap, and every stop is stated (terminal reached, depth
//! cap, per-node edge cap). G3: a depth-2 follow from the probe method
//! reaches the helper's table access.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::TraceDataFlowRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};

const PID: &str = "follow-test";
const API: &str = "Site/App_Code/api/api-x.vb";
const IO: &str = "Site/App_Code/io/installationsobjektprojekt.vb";

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

const API_SRC: &str = "Public Class api\n\
    Public Function Filter(qry As Query) As String\n\
        Dim rows = _io.installationsobjektprojekt.GetAllByCheckingTotalProject(pr_id, db)\n\
        Return \"ok\"\n\
    End Function\n\
End Class\n";

const IO_SRC: &str = "Public Class installationsobjektprojekt\n\
    Public Function GetAllByCheckingTotalProject(pr_id As Integer, db As Ctx) As List\n\
        Return db.iom_installationsobjektmoments.Where(Function(m) m.pr_id = pr_id).ToList()\n\
    End Function\n\
    Public Function Deep2(x As Integer) As Integer\n\
        Return Deep3(x)\n\
    End Function\n\
    Public Function Deep3(x As Integer) As Integer\n\
        Return Deep4(x)\n\
    End Function\n\
    Public Function Deep4(x As Integer) As Integer\n\
        Return db.rk_far.Count()\n\
    End Function\n\
End Class\n";

/// Filter → GetAllByCheckingTotalProject → table (depth 2, terminal).
/// Filter → Deep2 → Deep3 → Deep4 → table: the table sits at depth 5,
/// beyond the cap — the trace must say where it stopped.
fn seed(state: &AppState, dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("Site/App_Code/api")).unwrap();
    std::fs::create_dir_all(dir.join("Site/App_Code/io")).unwrap();
    std::fs::write(dir.join(API), API_SRC).unwrap();
    std::fs::write(dir.join(IO), IO_SRC).unwrap();
    let filter = func(API, "api", "Filter", 2);
    let get_all = func(
        IO,
        "installationsobjektprojekt",
        "GetAllByCheckingTotalProject",
        2,
    );
    let d2 = func(IO, "installationsobjektprojekt", "Deep2", 5);
    let d3 = func(IO, "installationsobjektprojekt", "Deep3", 8);
    let d4 = func(IO, "installationsobjektprojekt", "Deep4", 11);
    let ids: Vec<String> = [&filter, &get_all, &d2, &d3, &d4]
        .iter()
        .map(|n| n.node_id.clone())
        .collect();
    state
        .graph
        .upsert_nodes(PID, &[filter, get_all, d2, d3, d4])
        .unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[
                edge(&ids[0], &ids[1], EdgeKind::Calls),
                edge(
                    &ids[1],
                    "table:iom_installationsobjektmoments",
                    EdgeKind::QueriesTable,
                ),
                edge(&ids[0], &ids[2], EdgeKind::Calls),
                edge(&ids[2], &ids[3], EdgeKind::Calls),
                edge(&ids[3], &ids[4], EdgeKind::Calls),
                edge(&ids[4], "table:rk_far", EdgeKind::QueriesTable),
            ],
        )
        .unwrap();
}

async fn trace_json(engram: &Engram, body: Value) -> Value {
    let req: TraceDataFlowRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_trace_data_flow(req).await.unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}):\n{text}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resolved_call_is_followed_to_the_helpers_table_access() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);
    let v = trace_json(
        &engram,
        json!({"project_id": PID, "file_path": API, "entry_point": "Filter", "output_json": true}),
    )
    .await;
    let tables = v["tables_touched"].to_string();
    assert!(
        tables.contains("iom_installationsobjektmoments"),
        "the helper's table must be reached at depth 2: {tables}\n{}",
        v["steps"]
    );
    let followed: Vec<&Value> = v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["details"]["depth"].as_str().is_some_and(|d| d != "1"))
        .collect();
    assert!(
        followed
            .iter()
            .any(|s| s.to_string().contains("iom_installationsobjektmoments")),
        "a followed step must carry the depth it was reached at:\n{}",
        v["steps"]
    );
    let follow = &v["follow"];
    assert_eq!(follow["depth_cap"], 3, "{follow}");
    assert!(follow["followed"].as_u64().unwrap_or(0) >= 1, "{follow}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_depth_cap_is_a_stated_stop_not_a_silent_end() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);
    let v = trace_json(
        &engram,
        json!({"project_id": PID, "file_path": API, "entry_point": "Filter", "output_json": true}),
    )
    .await;
    assert!(
        !v["tables_touched"].to_string().contains("rk_far"),
        "rk_far sits at depth 5, beyond the cap of 3: {}",
        v["tables_touched"]
    );
    let stops = v["follow"]["stops"].to_string();
    assert!(
        stops.contains("Deep3") && stops.to_lowercase().contains("depth"),
        "the stop at the depth cap must name the node and the reason: {stops}"
    );
}
