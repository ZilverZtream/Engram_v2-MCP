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

#[tokio::test]
async fn root_directory_scope_preserves_boundaries_and_exact_symbol_fallback() {
    let (_tmp, state, _dir) = build_state();
    state.graph.upsert_nodes(PID, &[
        func("services/worker.vb", "Inside", 1, None),
        func("services-old/worker.vb", "Outside", 1, None),
        func("nested/services/worker.vb", "NestedWorker", 1, None),
    ]).unwrap();
    let engram = Engram::new(state);
    for scope in ["services", "services/", "services\\worker.vb"] {
        let js = run(&engram, json!({"project_id": PID, "scope": scope, "output_json": true})).await;
        let v: Value = serde_json::from_str(&js).unwrap();
        assert_eq!(v["coverage"]["scope_query"], "store", "{js}");
        assert_eq!(v["coverage"]["in_scope_functions"], 1, "{js}");
        assert_eq!(v["functions"][0]["name"], "Inside", "{js}");
    }
    let js = run(&engram, json!({"project_id": PID, "scope": "NestedWorker", "output_json": true})).await;
    let v: Value = serde_json::from_str(&js).unwrap();
    assert_eq!(v["coverage"]["scope_query"], "symbol", "{js}");
    assert_eq!(v["functions"][0]["name"], "NestedWorker", "{js}");
    let js = run(&engram, json!({"project_id": PID, "scope": "MissingClass", "output_json": true})).await;
    let v: Value = serde_json::from_str(&js).unwrap();
    assert_eq!(v["coverage"]["in_scope_functions"], 0, "{js}");
    assert!(v["coverage"]["failures"].to_string().contains("guard coverage is unknown"), "{js}");
}

#[tokio::test]
async fn indexed_directory_without_functions_never_falls_back_to_other_file_symbol() {
    let (_tmp, state, _dir) = build_state();
    let mut class = func("services/model.vb", "Model", 1, None);
    class.node_type = "class".into();
    state.graph.upsert_nodes(PID, &[class, func("other.vb", "services", 1, None)]).unwrap();
    let engram = Engram::new(state);
    let js = run(&engram, json!({"project_id": PID, "scope": "services", "output_json": true})).await;
    let v: Value = serde_json::from_str(&js).unwrap();
    assert_eq!(v["coverage"]["scope_query"], "store", "{js}");
    assert_eq!(v["coverage"]["in_scope_functions"], 0, "{js}");
    assert!(v["coverage"]["failures"].to_string().contains("guard coverage is unknown"), "{js}");
    assert!(v["functions"].as_array().unwrap().is_empty(), "{js}");
}

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
    // Positive guard fixtures include real source, not metadata-only claims.
    let root =
        std::path::PathBuf::from(state.registry.get_project(PID).unwrap().unwrap().directory);
    for file in [FILE, OTHER] {
        let mut lines = vec![String::new(); 230];
        for node in nodes.iter().filter(|node| node.file_path.as_str() == file) {
            let start = node.start_line as usize - 1;
            lines[start] = format!("Sub {}()", node.name);
            let check = node
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("permission_checks"))
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let statements: Vec<String> = if node.name == "Wrapped" {
                vec!["    CanUserBulkUpdate()".into()]
            } else if !check.is_empty() {
                check
                    .split(';')
                    .map(|name| format!("    {name}()"))
                    .collect()
            } else {
                vec!["    Return".into()]
            };
            let count = statements.len();
            for (offset, statement) in statements.into_iter().enumerate() {
                lines[start + 1 + offset] = statement;
            }
            lines[start + 1 + count] = "End Sub".into();
        }
        let full = root.join(file);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, lines.join("\n")).unwrap();
    }
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

#[tokio::test]
async fn a_library_call_that_cannot_hold_a_guard_does_not_erase_the_verdict() {
    // Live (a VB api file, 9 of 9 functions): every verdict came back `unknown`
    // with "helper evidence is incomplete; cannot conclude that no guard
    // exists". The unresolved "helpers" were `JsonConvert.SerializeObject`,
    // `permitsList.Select(...)`, `s.SetError`, LINQ operators and anonymous
    // types — call targets that can never be project symbols and can never
    // hold a permission check. Counting them as missing guard evidence erases
    // the parity verdict on every file that calls a third-party library.
    //
    // An unresolved PROJECT helper must still force `unknown` — that is the
    // sibling test below, and this fix must not weaken it.
    let (_tmp, state, dir) = build_state();
    let source = dir.join(FILE);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "Sub Plain()\n Dim s = JsonConvert.SerializeObject(x)\nEnd Sub\n")
        .unwrap();
    let plain = func(FILE, "Plain", 1, None);
    state.graph.upsert_nodes(PID, &[plain.clone()]).unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&plain.node_id, "::JsonConvert.SerializeObject")])
        .unwrap();

    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":FILE,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        report["functions"][0]["verdict"], "unguarded",
        "a library call cannot hold a guard, so it must not demote the verdict:\n{text}"
    );
}

#[tokio::test]
async fn a_helper_with_no_permission_check_does_not_erase_the_verdict() {
    // Second half of the same live defect. After library/LINQ targets stopped
    // counting, 81 of 93 remaining failures on a VB api file read
    // "helper <X> is conditional; all-path coverage unknown" for helpers like
    // `_data.records.GetById`, `AppDataContext.New`, `api.JsonResult.New` —
    // none of which carry a permission check at all.
    //
    // The first arm of that test passes the HELPER NAME as the checks, so it
    // asks "is the call to this helper conditional?". For a helper that could
    // never supply a guard the answer is irrelevant, yet it counts as missing
    // guard evidence and demotes a correct `unguarded` to `unknown`.
    let (_tmp, state, dir) = build_state();
    let source = dir.join(FILE);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let mut lines = vec![String::new(); 14];
    lines[0] = "Sub Plain()".into();
    lines[1] = "    If x Then".into();
    lines[2] = "        MakeThing()".into(); // conditional call to a check-less helper
    lines[3] = "    End If".into();
    lines[4] = "End Sub".into();
    lines[7] = "Sub MakeThing()".into();
    lines[8] = "    Return".into();
    lines[9] = "End Sub".into();
    std::fs::write(&source, lines.join("\n")).unwrap();

    let plain = func(FILE, "Plain", 1, None);
    let helper = func(FILE, "MakeThing", 8, None); // no permission_checks
    state
        .graph
        .upsert_nodes(PID, &[plain.clone(), helper.clone()])
        .unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&plain.node_id, &helper.node_id)])
        .unwrap();

    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":FILE,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    let verdict = report["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "Plain")
        .map(|f| f["verdict"].as_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    assert_eq!(
        verdict, "unguarded",
        "a helper that carries no permission check cannot be missing guard evidence:\n{text}"
    );
}

#[tokio::test]
async fn unresolved_helpers_and_fallback_checks_are_unknown() {
    let (_tmp, state, _dir) = build_state();
    let caller = func(FILE, "Caller", 1, None);
    let fallback = func(
        FILE,
        "FallbackChecked",
        10,
        Some(json!({"permission_checks":"CheckRead", "extraction_fallback":"true"})),
    );
    state
        .graph
        .upsert_nodes(PID, &[caller.clone(), fallback])
        .unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&caller.node_id, "::MissingGuard")])
        .unwrap();
    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":FILE,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    assert!(
        report["functions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["verdict"] == "unknown"),
        "{text}"
    );
    assert!(text.contains("unresolved helper"), "{text}");
    assert!(text.contains("runtime enforcement"), "{text}");
}

#[tokio::test]
async fn changed_source_cannot_receive_a_guarded_verdict_from_old_spans() {
    let (_tmp, state, dir) = build_state();
    let source = dir.join(FILE);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let original = "Sub Run()\n CheckRead()\nEnd Sub\n";
    std::fs::write(&source, original).unwrap();
    let method = func(
        FILE,
        "Run",
        1,
        Some(json!({"permission_checks":"CheckRead"})),
    );
    let mut file_node = method.clone();
    file_node.node_id = format!("file:{FILE}");
    file_node.node_type = "file".into();
    file_node.metadata =
        Some(json!({"file_hash":blake3::hash(original.as_bytes()).to_hex().to_string()}));
    state.graph.upsert_nodes(PID, &[method, file_node]).unwrap();
    std::fs::write(&source, "Sub Run()\n Return\nEnd Sub\n").unwrap();
    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":FILE,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["functions"][0]["verdict"], "unknown", "{text}");
    assert!(text.contains("changed since indexing"), "{text}");
}

#[tokio::test]
async fn helper_and_setting_caps_report_actual_incomplete_coverage() {
    let (_tmp, state, _dir) = build_state();
    let caller = func(FILE, "Fanout", 1, None);
    let mut nodes = vec![caller.clone()];
    let mut edges = Vec::new();
    for i in 0..51 {
        let target = func(OTHER, &format!("Helper{i}"), 10 + i, None);
        edges.push(calls(&caller.node_id, &target.node_id));
        nodes.push(target);
    }
    for i in 0..21 {
        let mut edge = calls(&caller.node_id, &format!("::Setting{i}"));
        edge.edge_kind = EdgeKind::ReadsSetting;
        edges.push(edge);
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":FILE,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["functions"][0]["verdict"], "unknown", "{text}");
    assert!(text.contains("helper traversal truncated at 50"), "{text}");
    assert!(text.contains("settings edges truncated at 20"), "{text}");
    assert_eq!(report["settings_read"].as_array().unwrap().len(), 20);
}

// ── Live shape: class-qualified names ────────────────────────────────────────
//
// Every fixture above names a function BARE (`Wrapped`), but the real index
// stores a function node's `name` CLASS-QUALIFIED (`api.LogError`) and keeps the
// bare form only as the trailing segment. Several defects below are invisible
// under a bare-name fixture because `n.name` and its bare form are identical
// there — which is why the suite never caught them. These tests build their own
// nodes so the fixture matches the index.

const PART_A: &str = "Site/App_Code/api/api-part-a.vb";
const PART_B: &str = "Site/App_Code/api/api-part-b.vb";

fn qualified(path: &str, qualified_name: &str, line: u32, meta: Option<Value>) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{qualified_name}:{line}"),
        node_type: "function".into(),
        name: qualified_name.into(),
        namespace: "api".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line + 4,
        generation: 1,
        metadata: meta,
    }
}

fn write_source(dir: &std::path::Path, path: &str, lines: &[&str]) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, lines.join("\n")).unwrap();
}

#[tokio::test]
async fn a_same_class_helper_declared_in_another_file_is_not_unresolved() {
    // A class split across files (VB `Partial Class`, C# `partial class`) emits
    // a BARE `::Name` call edge for a member declared in a sibling file. The
    // only lexical fallback is keyed by (file, name), so it never binds, and the
    // unresolved name then erases the caller's verdict — although the helper IS
    // indexed, one file over, in the same class.
    let (_tmp, state, dir) = build_state();
    write_source(
        &dir,
        PART_A,
        &["Sub Caller()", "    LogThing()", "    Return", "End Sub"],
    );
    write_source(&dir, PART_B, &["Sub LogThing()", "    Return", "End Sub"]);
    let caller = qualified(PART_A, "api.Caller", 1, None);
    let helper = qualified(PART_B, "api.LogThing", 1, None); // carries no permission check
    state
        .graph
        .upsert_nodes(PID, &[caller.clone(), helper])
        .unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&caller.node_id, "::LogThing")])
        .unwrap();

    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":PART_A,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        report["functions"][0]["verdict"], "unguarded",
        "`LogThing` is indexed as `api.LogThing` in the same class one file over, \
         and carries no permission check, so it cannot be missing guard evidence:\n{text}"
    );
}

#[tokio::test]
async fn a_language_builtin_is_not_an_unresolved_project_helper() {
    // `IsNothing` is a VB language intrinsic, not a project symbol. It resolves
    // to no node, and a bare unresolved name is deliberately treated as a
    // possible project helper — so an intrinsic erases the verdict on every
    // function that uses one.
    let (_tmp, state, dir) = build_state();
    write_source(
        &dir,
        PART_A,
        &[
            "Sub Caller()",
            "    If IsNothing(x) Then Return",
            "    Return",
            "End Sub",
        ],
    );
    let caller = qualified(PART_A, "api.Caller", 1, None);
    state.graph.upsert_nodes(PID, &[caller.clone()]).unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&caller.node_id, "::IsNothing")])
        .unwrap();

    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":PART_A,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        report["functions"][0]["verdict"], "unguarded",
        "a language intrinsic cannot hold a permission check:\n{text}"
    );
}

#[tokio::test]
async fn client_keys_read_through_a_dictionary_accessor_are_reported() {
    // `qry.data("k")` is matched, but the `.Item("k")` / `.ContainsKey("k")`
    // accessor forms — the ordinary way to read a dictionary in VB — are not,
    // so the tool reports "no client input" for a function that reads one.
    let (_tmp, state, dir) = build_state();
    write_source(
        &dir,
        PART_A,
        &[
            "Sub Caller()",
            "    If qry.data.ContainsKey(\"record_id\") Then",
            "        Dim v = qry.data.Item(\"record_id\")",
            "    End If",
            "End Sub",
        ],
    );
    let caller = qualified(PART_A, "api.Caller", 1, None);
    state.graph.upsert_nodes(PID, &[caller]).unwrap();

    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":PART_A,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    assert!(
        report["functions"][0]["scope_reads"]
            .to_string()
            .contains("record_id"),
        "a client key read through `.Item(...)` is still client input:\n{text}"
    );
}

#[tokio::test]
async fn every_failure_names_the_function_it_belongs_to() {
    // A failure is what demotes a verdict, but failures are owned by the
    // CLASS-QUALIFIED name while `functions[].name` is bare, so a consumer
    // cannot attribute any failure to any function.
    let (_tmp, state, dir) = build_state();
    write_source(
        &dir,
        PART_A,
        &["Sub Caller()", "    MissingGuard()", "End Sub"],
    );
    let caller = qualified(PART_A, "api.Caller", 1, None);
    state.graph.upsert_nodes(PID, &[caller.clone()]).unwrap();
    state
        .graph
        .upsert_edges(PID, &[calls(&caller.node_id, "::MissingGuard")])
        .unwrap();

    let text = run(
        &Engram::new(state),
        json!({"project_id":PID,"scope":PART_A,"output_json":true}),
    )
    .await;
    let report: Value = serde_json::from_str(&text).unwrap();
    let name = report["functions"][0]["name"].as_str().unwrap().to_string();
    let failures = report["coverage"]["failures"].as_array().unwrap().clone();
    assert!(
        !failures.is_empty(),
        "an unresolved project helper must still be a failure:\n{text}"
    );
    for failure in &failures {
        let line = failure.as_str().unwrap();
        assert!(
            line.starts_with(&format!("{name}: ")),
            "every failure must be attributable to the function it pins (`{name}`): {line}\n{text}"
        );
    }
}
