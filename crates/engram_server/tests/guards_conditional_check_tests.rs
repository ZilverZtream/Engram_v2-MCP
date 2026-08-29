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
        Dim proj = _gd.projekt.GetByID(projectID)
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

    Public Function ScopedByDal(qry As Query) As String
        If Not _us.UserAccess.CheckRead(_us.UserAccessObject.vs_karta_io_objekt) Then Return s
        Dim projectID = GetDictionaryIntegerValue(qry.data, "pr_id")
        Dim proj = _gd.projekt.GetByID(projectID)
        Return "ok"
    End Function
End Class
"#;

/// The DAL helper every endpoint above calls; it does the object-level
/// scoping (`check_pr_id`) itself — live: `_gd.projekt.GetByID`.
const DAL: &str = "Site/App_Code/gd/projekt.vb";
const DAL_SRC: &str = r#"Public Class projekt
    Public Function GetByID(id As Integer) As pr_projekt
        If Not _us.accessctrl.check_pr_id(id) Then Return Nothing
        Return db.pr_projekts.FirstOrDefault(Function(p) p.pr_id = id)
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
    std::fs::create_dir_all(dir.join("Site/App_Code/gd")).unwrap();
    std::fs::write(dir.join(FILE), SRC).unwrap();
    std::fs::write(dir.join(DAL), DAL_SRC).unwrap();
    // The extractor records the regex-visible CheckWrite on UpdateInBulk
    // (it cannot see CanUserBulkUpdate) — exactly the live metadata.
    let bulk = func("UpdateInBulk", 2, 10, "checkwrite");
    let helper = func(
        "CanUserBulkUpdate",
        12,
        16,
        "checkifadminorarbetsledare;checkwrite",
    );
    let plain = func("PlainGuard", 18, 21, "checkwrite");
    let scoped = func("ScopedByDal", 23, 28, "checkread");
    // The DAL helper is indexed with a qualified name and carries the
    // object-level check itself (live: `_gd.projekt.GetByID` → check_pr_id).
    let dal = Node {
        node_id: format!("sym:function:{DAL}:_gd.projekt.GetByID:2"),
        node_type: "function".into(),
        name: "_gd.projekt.GetByID".into(),
        namespace: "projekt".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(DAL),
        start_line: 2,
        end_line: 5,
        generation: 1,
        metadata: Some(json!({"permission_checks": "check_pr_id", "guard_roles": ""})),
    };
    let (bid, sid, did) = (
        bulk.node_id.clone(),
        scoped.node_id.clone(),
        dal.node_id.clone(),
    );
    state
        .graph
        .upsert_nodes(PID, &[bulk, helper, plain, scoped, dal])
        .unwrap();
    // LIVE SHAPE: the VB extractor emits Calls edges for QUALIFIED calls
    // (`_gd.projekt.GetByID`) but none for the bare in-class
    // `CanUserBulkUpdate()` — the in-file call must be found from the body.
    state
        .graph
        .upsert_edges(PID, &[calls(&bid, &did), calls(&sid, &did)])
        .unwrap();
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

    // Live finding (release 13): the object-level scoping of the bulk
    // endpoints lives INSIDE the DAL helper (`_gd.projekt.GetByID` →
    // check_pr_id). A role-level own check + a called helper that checks
    // the object is OBJECT-level via that helper, not ROLE-ONLY.
    let scoped = find("ScopedByDal");
    assert_eq!(scoped["verdict"], "guarded", "{scoped}");
    assert_eq!(
        scoped["level"], "object",
        "check_pr_id inside the called DAL helper scopes the object: {scoped}"
    );
    assert_eq!(scoped["via"], "GetByID", "{scoped}");
    assert_eq!(
        scoped["role_only"], false,
        "the client pr_id IS object-checked (by the helper): {scoped}"
    );
}
