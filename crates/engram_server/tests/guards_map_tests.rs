#![allow(clippy::unwrap_used)]
//! Row-8 audit (docs/audits/08-guards-and-settings.md) slice 1 —
//! `map_guards_and_settings`: three-state verdict (guarded / unguarded /
//! UNKNOWN when the symbol came from the extraction fallback), guard
//! helpers credited one hop through `Calls`, lists never silently cut,
//! the scan bounded at the store and its coverage reported, `output_json`.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::MapGuardsAndSettingsRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};

const PID: &str = "guards-test";
const FILE: &str = "Site/App_Code/api/api-guards.vb";
const OTHER: &str = "Site/App_Code/other/other.vb";

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

fn func(path: &str, name: &str, line: u32, meta: Option<Value>) -> Node {
    Node {
        node_id: format!("sym:function:{path}:api.{name}:{line}"),
        node_type: "function".into(),
        name: name.into(),
        namespace: "api".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line + 5,
        generation: 1,
        metadata: meta,
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

/// In FILE: `Guarded` (CheckRead), `Wrapped` (no own check, calls the
/// helper `CanUserBulkUpdate` which checks), `Fallback` (extraction
/// fallback, no metadata worth trusting), `Bare` and twelve more
/// unguarded functions. OTHER holds one unguarded function that must not
/// count when the scope is FILE.
fn seed(state: &AppState) {
    let guarded = func(
        FILE,
        "Guarded",
        10,
        Some(json!({"permission_checks": "CheckRead", "guard_roles": "vs_karta_io_objekt"})),
    );
    let wrapped = func(FILE, "Wrapped", 20, None);
    let helper = func(
        FILE,
        "CanUserBulkUpdate",
        30,
        Some(
            json!({"permission_checks": "CheckIfAdminOrArbetsledare;CheckWrite", "guard_roles": ""}),
        ),
    );
    let fallback = func(
        FILE,
        "Fallback",
        40,
        Some(json!({"extraction_fallback": "true"})),
    );
    let bare = func(FILE, "Bare", 50, None);
    let mut nodes = vec![guarded, wrapped.clone(), helper.clone(), fallback, bare];
    for i in 0..12 {
        nodes.push(func(FILE, &format!("Unguarded{i:02}"), 100 + i * 10, None));
    }
    nodes.push(func(OTHER, "OtherBare", 5, None));
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&wrapped.node_id, &helper.node_id)])
        .unwrap();
}

async fn run(engram: &Engram, body: Value) -> String {
    let req: MapGuardsAndSettingsRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_map_guards_and_settings(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unguarded_functions_are_never_silently_cut() {
    let (_tmp, state, _dir) = build_state();
    seed(&state);
    let engram = Engram::new(state);
    let md = run(&engram, json!({"project_id": PID, "scope": FILE})).await;
    let listed = (0..12)
        .filter(|i| md.contains(&format!("Unguarded{i:02}")))
        .count();
    assert!(
        listed == 12 || md.contains("more"),
        "13 unguarded functions, {listed} printed and no '… and N more' line:\n{md}"
    );
    let js = run(
        &engram,
        json!({"project_id": PID, "scope": FILE, "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&js).unwrap_or_else(|e| panic!("not JSON ({e}):\n{js}"));
    let unguarded = v["unguarded"].as_array().unwrap();
    assert!(
        unguarded.len() >= 13,
        "JSON must carry the full list (Bare + 12): {}",
        v["unguarded"]
    );
    assert!(
        !js.contains("OtherBare"),
        "a function outside the scope must not be listed: {js}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helper_wrapped_guards_are_credited_and_fallback_symbols_are_unknown() {
    let (_tmp, state, _dir) = build_state();
    seed(&state);
    let engram = Engram::new(state);
    let js = run(
        &engram,
        json!({"project_id": PID, "scope": FILE, "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&js).unwrap();
    let fns = v["functions"].as_array().unwrap();
    let find = |n: &str| {
        fns.iter()
            .find(|f| f["name"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("{n} missing: {}", v["functions"]))
    };
    let wrapped = find("Wrapped");
    assert_eq!(wrapped["verdict"], "guarded", "{wrapped}");
    assert_eq!(
        wrapped["via"], "CanUserBulkUpdate",
        "the helper must be credited: {wrapped}"
    );
    assert!(
        wrapped["family"].to_string().contains("CheckWrite"),
        "the inherited checks are named: {wrapped}"
    );
    let fallback = find("Fallback");
    assert_eq!(fallback["verdict"], "unknown", "{fallback}");
    assert!(
        fallback["reason"]
            .to_string()
            .to_lowercase()
            .contains("fallback"),
        "{fallback}"
    );
    let bare = find("Bare");
    assert_eq!(bare["verdict"], "unguarded", "{bare}");
    let guarded = find("Guarded");
    assert_eq!(guarded["verdict"], "guarded", "{guarded}");
    assert_eq!(guarded["level"], "role", "{guarded}");
    assert!(
        v["unknown"].as_array().unwrap().len() == 1,
        "{}",
        v["unknown"]
    );

    let md = run(&engram, json!({"project_id": PID, "scope": FILE})).await;
    assert!(md.contains("UNKNOWN") || md.contains("unknown"), "{md}");
    assert!(
        md.contains("via CanUserBulkUpdate") || md.contains("CanUserBulkUpdate"),
        "{md}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scan_is_bounded_at_the_store_and_its_coverage_reported() {
    let (_tmp, state, _dir) = build_state();
    seed(&state);
    let engram = Engram::new(state);
    let js = run(
        &engram,
        json!({"project_id": PID, "scope": FILE, "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&js).unwrap();
    let cov = &v["coverage"];
    assert_eq!(cov["node_scan"], "complete", "{cov}");
    assert_eq!(
        cov["scope_query"], "store",
        "a path scope must be a store-side file query: {cov}"
    );
    assert!(cov["scanned"].as_u64().unwrap_or(0) >= 17, "{cov}");
    let caps = cov["caps"].to_string();
    assert!(
        caps.contains("300") && caps.contains("20"),
        "every cap is reported: {caps}"
    );
    assert!(cov["failures"].as_array().is_some(), "{cov}");
    let md = run(&engram, json!({"project_id": PID, "scope": FILE})).await;
    assert!(md.contains("## Coverage"), "{md}");
}
