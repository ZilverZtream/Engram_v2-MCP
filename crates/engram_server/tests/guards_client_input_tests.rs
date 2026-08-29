#![allow(clippy::unwrap_used)]
//! Row-8 audit (docs/audits/08-guards-and-settings.md) slice 2 — A2: a
//! function that reads a scope key from CLIENT input (`qry.params("pr_id")`,
//! `Request("pr_id")`, `GetDictionaryIntegerValue(qry.params, "pr_id")`)
//! and has no OBJECT-level guard for it (`check_pr_id(...)`,
//! `CheckAccess…(pr_id)`) is ROLE-ONLY — reported as such, never as a
//! plain "guarded". A6: `immune_check` honours `include_content` and
//! labels its match count as capped by `top_k`.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::models::{ImmuneCheckRequest, MapGuardsAndSettingsRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};

const PID: &str = "guards-input-test";
const FILE: &str = "Site/App_Code/api/api-input.vb";

/// Three guarded functions: ROLE-ONLY (reads pr_id, CheckRead only),
/// OBJECT-guarded (check_pr_id on the value), and one that reads no
/// client scope key at all.
const SRC: &str = "Public Class api\n\
    Public Function RoleOnly(qry As Query) As String\n\
        If Not _us.UserAccess.CheckRead(_us.UserAccessObject.vs_karta_io_objekt) Then Return s\n\
        Dim pr_id = GetDictionaryIntegerValue(qry.params, \"pr_id\")\n\
        Return _io.installationsobjektprojekt.GetAllByCheckingTotalProject(pr_id, db)\n\
    End Function\n\
\n\
    Public Function ObjectGuarded(qry As Query) As String\n\
        If Not _us.UserAccess.CheckRead(_us.UserAccessObject.vs_karta_io_objekt) Then Return s\n\
        Dim pr_id = qry.params(\"pr_id\")\n\
        If Not _us.accessctrl.check_pr_id(pr_id) Then Return s\n\
        Return _io.installationsobjektprojekt.GetAllByCheckingTotalProject(pr_id, db)\n\
    End Function\n\
\n\
    Public Function NoInput() As String\n\
        If Not _us.UserAccess.CheckRead(_us.UserAccessObject.vs_karta_io_objekt) Then Return s\n\
        Return \"ok\"\n\
    End Function\n\
\n\
    Public Function BulkPost(qry As Query) As String\n\
        If Not _us.UserAccess.CheckWrite(_us.UserAccessObject.vs_karta_io_objekt) Then Return s\n\
        Dim projectID = GetDictionaryIntegerValue(qry.data, \"pr_id\")\n\
        Dim ids = GetDictionaryStringValue(qry.data, \"markerIDs\")\n\
        Return _io.installationsobjektprojekt.DeleteInBulk(projectID, ids, db)\n\
    End Function\n\
End Class\n";

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

fn func(name: &str, start: u32, end: u32) -> Node {
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
        metadata: Some(
            json!({"permission_checks": "CheckRead", "guard_roles": "vs_karta_io_objekt"}),
        ),
    }
}

fn seed(state: &AppState, dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("Site/App_Code/api")).unwrap();
    std::fs::write(dir.join(FILE), SRC).unwrap();
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                func("RoleOnly", 2, 6),
                func("ObjectGuarded", 8, 13),
                func("NoInput", 15, 18),
                func("BulkPost", 20, 25),
            ],
        )
        .unwrap();
}

async fn guards(engram: &Engram, body: Value) -> String {
    let req: MapGuardsAndSettingsRequest = serde_json::from_value(body).unwrap();
    let res = engram.handle_map_guards_and_settings(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_scope_key_read_without_an_object_guard_is_role_only() {
    let (_tmp, state, dir) = build_state();
    seed(&state, &dir);
    let engram = Engram::new(state);
    let js = guards(
        &engram,
        json!({"project_id": PID, "scope": FILE, "output_json": true}),
    )
    .await;
    let v: Value = serde_json::from_str(&js).unwrap_or_else(|e| panic!("not JSON ({e}):\n{js}"));
    let fns = v["functions"].as_array().unwrap();
    let find = |n: &str| {
        fns.iter()
            .find(|f| f["name"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("{n} missing: {}", v["functions"]))
    };
    let role_only = find("RoleOnly");
    assert_eq!(role_only["verdict"], "guarded", "{role_only}");
    assert_eq!(role_only["level"], "role", "{role_only}");
    assert_eq!(
        role_only["scope_reads"].to_string(),
        "[\"pr_id\"]",
        "the client-supplied scope key must be named: {role_only}"
    );
    assert!(
        role_only["role_only"] == true,
        "a role check that does not cover the client pr_id is ROLE-ONLY: {role_only}"
    );
    let object = find("ObjectGuarded");
    assert_eq!(object["level"], "object", "{object}");
    assert!(object["role_only"] == false, "{object}");
    let no_input = find("NoInput");
    assert!(
        no_input["scope_reads"]
            .as_array()
            .is_some_and(|a| a.is_empty()),
        "{no_input}"
    );
    assert!(no_input["role_only"] == false, "{no_input}");
    // Live (OciusX 2026-08-29): the four bulk endpoints read the POST body
    // — `GetDictionaryIntegerValue(qry.data, "pr_id")` — and were reported
    // with no client reads at all.
    let bulk = find("BulkPost");
    assert!(
        bulk["role_only"] == true,
        "a qry.data reader with a role-level CheckWrite is ROLE-ONLY: {bulk}"
    );
    let reads = bulk["scope_reads"].to_string();
    assert!(
        reads.contains("pr_id") && reads.contains("markerIDs"),
        "both POST-body keys must be named: {bulk}"
    );
    assert_eq!(
        v["role_only"].as_array().unwrap().len(),
        2,
        "{}",
        v["role_only"]
    );

    let md = guards(&engram, json!({"project_id": PID, "scope": FILE})).await;
    assert!(
        md.contains("ROLE-ONLY") && md.contains("RoleOnly") && md.contains("pr_id"),
        "{md}"
    );
}

// ── A6: immune_check ──────────────────────────────────────────────────────

use rmcp::handler::server::tool::Parameters;

async fn build_indexed() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site")).unwrap();
    std::fs::write(root.join("Site/a.vb"), "Public Class a\nEnd Class\n").unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(512 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "ImmuneFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    // Three anti-pattern docs sharing the snippet's vocabulary.
    let engine = state.get_project_cached(&pid).unwrap().search;
    let docs: Vec<engram_index::IndexDoc> = (0..3)
        .map(|i| engram_index::IndexDoc {
            generation: 1,
            chunk_id: 900 + i,
            doc_id: format!("ap:{i}"),
            content_hash: format!("aph{i}"),
            path: RelPath::new(&format!("__antipatterns/reverted_{i}.diff")),
            // The real ingestion format (git_tools): metadata header, blank
            // line, then the reverted code.
            content: format!(
                "ANTI-PATTERN\nOriginal Commit: aaaa{i}\nReverted in Commit: bbbb{i}\nPath: Site/x{i}.vb\n\nSECRET_MARKER_{i} .Where(Function(x) x.pr_id = Request(\"pr_id\")) delete rows without check_pr_id"
            ),
            language: "vb".into(),
            namespace: "antipattern".into(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: 1,
        })
        .collect();
    engine
        .index_docs(&pid, &docs, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    (tmp, state, engram, pid)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn immune_check_honours_include_content_and_labels_its_cap() {
    let (_tmp, _state, engram, pid) = build_indexed().await;
    let body = |include: bool, top_k: usize| {
        json!({"project_id": pid, "code": ".Where(Function(x) x.pr_id = Request(\"pr_id\"))",
               "use_vector": false, "include_content": include, "top_k": top_k})
    };
    let req: ImmuneCheckRequest = serde_json::from_value(body(false, 2)).unwrap();
    let out = engram.handle_immune_check(req).await.unwrap().content[0]
        .as_text()
        .unwrap()
        .text
        .clone();
    assert!(
        !out.contains("SECRET_MARKER_"),
        "snippet content printed although include_content=false:\n{out}"
    );
    assert!(
        out.contains("Original Commit:"),
        "the anti-pattern METADATA header must still be shown without include_content:\n{out}"
    );
    assert!(
        out.contains("Matches Found") && (out.contains("cap") || out.contains("top_k")),
        "the match count must be labelled as capped by top_k:\n{out}"
    );
    let req: ImmuneCheckRequest = serde_json::from_value(body(true, 2)).unwrap();
    let out = engram.handle_immune_check(req).await.unwrap().content[0]
        .as_text()
        .unwrap()
        .text
        .clone();
    assert!(
        out.contains("SECRET_MARKER_"),
        "include_content=true must print the matched content:\n{out}"
    );
}
