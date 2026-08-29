#![allow(clippy::unwrap_used)]
//! Row-8 audit (docs/audits/08-guards-and-settings.md) — the last open
//! item of the six-endpoint truth table: `ioUpdateBaseTypeInBulk` is
//! really guarded by `CanUserBulkUpdate()` at its top, but the tool
//! credited a `CheckWrite` that sits INSIDE `If isUpdatingAR Then …`.
//! A check that only runs on some branch is CONDITIONAL; when the only
//! own checks are conditional and a directly called helper guards
//! unconditionally, the helper is the guard and the verdict says so.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::MapGuardsAndSettingsRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};

const PID: &str = "guards-cond-test";
const FILE: &str = "Site/App_Code/api/api-bulk.vb";

// A raw string: the conditional-check detection is INDENTATION-based and a
// `\`-continued literal would strip the leading whitespace.
const SRC: &str = r#"Public Class api
    Public Function UpdateInBulk(qry As Query) As String
        If Not CanUserBulkUpdate() Then Return s
        Dim projectID = GetDictionaryIntegerValue(qry.data, "pr_id")
        If isUpdatingAR Then
            If Not _us.UserAccess.CheckWrite(_us.UserAccessObject.vs_ata) Then Return s
        End If
        Return "ok"
    End Function

    Public Function CanUserBulkUpdate() As Boolean
        If Not CheckIfAdminOrArbetsledare() Then Return False
        If Not _us.UserAccess.CheckWrite(_us.UserAccessObject.vs_karta_io_objekt) Then Return False
        Return True
    End Function

    Public Function PlainGuard(qry As Query) As String
        If Not _us.UserAccess.CheckWrite(_us.UserAccessObject.vs_karta_io_objekt) Then Return s
        Return "ok"
    End Function
End Class
"#;

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

fn func(name: &str, start: u32, end: u32, checks: &str) -> Node {
    Node {
        node_id: format!("sym:function:{FILE}:api.{name}:{start}"),
        node_type: "function".into(),
        name: name.into(),
        namespace: "api".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(FILE),
        start_line: start,
        end_line: end,
        generation: 1,
        metadata: Some(json!({"permission_checks": checks, "guard_roles": ""})),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conditional_own_check_yields_to_the_helper_that_guards_unconditionally() {
    let (_tmp, state, dir) = build_state();
    std::fs::create_dir_all(dir.join("Site/App_Code/api")).unwrap();
    std::fs::write(dir.join(FILE), SRC).unwrap();
    // The extractor records the regex-visible CheckWrite on UpdateInBulk
    // (it cannot see CanUserBulkUpdate) — exactly the live metadata.
    let bulk = func("UpdateInBulk", 2, 9, "checkwrite");
    let helper = func(
        "CanUserBulkUpdate",
        11,
        15,
        "checkifadminorarbetsledare;checkwrite",
    );
    let plain = func("PlainGuard", 17, 20, "checkwrite");
    let (bid, hid) = (bulk.node_id.clone(), helper.node_id.clone());
    state
        .graph
        .upsert_nodes(PID, &[bulk, helper, plain])
        .unwrap();
    state.graph.upsert_edges(PID, &[calls(&bid, &hid)]).unwrap();
    let engram = Engram::new(state);
    let req: MapGuardsAndSettingsRequest =
        serde_json::from_value(json!({"project_id": PID, "scope": FILE, "output_json": true}))
            .unwrap();
    let res = engram.handle_map_guards_and_settings(req).await.unwrap();
    let js = res.content[0].as_text().unwrap().text.clone();
    let v: Value = serde_json::from_str(&js).unwrap();
    let fns = v["functions"].as_array().unwrap();
    let find = |n: &str| {
        fns.iter()
            .find(|f| f["name"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("{n} missing: {}", v["functions"]))
    };
    let bulk = find("UpdateInBulk");
    assert_eq!(bulk["verdict"], "guarded", "{bulk}");
    assert_eq!(
        bulk["via"], "CanUserBulkUpdate",
        "the unconditional helper is the real guard: {bulk}"
    );
    assert_eq!(
        bulk["own_check_conditional"], true,
        "the own CheckWrite sits inside `If isUpdatingAR Then`: {bulk}"
    );
    let plain = find("PlainGuard");
    assert_eq!(plain["own_check_conditional"], false, "{plain}");
    assert!(plain["via"].is_null(), "{plain}");
}
