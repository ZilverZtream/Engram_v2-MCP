#![allow(clippy::unwrap_used)]
//! Row-2 audit (docs/audits/02-edit-context-and-edit-safety.md): the
//! pre-edit oracle must tell the truth about what it could not see. These
//! tests drive the real handlers against a temp graph + working tree.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::{
    CheckEditSafetyRequest, GetMethodEditContextRequest, GetPageContextRequest,
};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::{Value, json};
use std::path::PathBuf;

// ─── fixture ────────────────────────────────────────────────────────────────

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
    (tmp, state)
}

const PID: &str = "edit-ctx-test";

fn register_project(state: &AppState, tmp: &tempfile::TempDir) -> PathBuf {
    let project_dir = tmp.path().join("project");
    let rec = engram_core::ProjectRecord {
        project_id: PID.into(),
        project_name: PID.into(),
        directory: project_dir.to_string_lossy().into_owned(),
        project_type: "dotnet_webforms_vb".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        reindex_required_since_ms: None,
    };
    state.registry.put_project(&rec).unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    project_dir
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
        metadata: Some(json!({ "signature": format!("{name}()"), "access_level": "Public" })),
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

/// A VB method whose body has `ifs` decision points (complexity ≈ ifs + 1).
fn vb_method(name: &str, ifs: usize) -> String {
    let mut s = format!("    Public Sub {name}()\n");
    for i in 0..ifs {
        s.push_str(&format!(
            "        If x = {i} Then\n            y = {i}\n        End If\n"
        ));
    }
    s.push_str("    End Sub\n");
    s
}

fn write_file(dir: &std::path::Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn text(res: &rmcp::model::CallToolResult) -> String {
    res.content[0].as_text().unwrap().text.clone()
}

fn json_of(res: &rmcp::model::CallToolResult) -> Value {
    let t = text(res);
    serde_json::from_str(&t).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

async fn edit_context(engram: &Engram, body: Value) -> Result<rmcp::model::CallToolResult, String> {
    let req: GetMethodEditContextRequest = serde_json::from_value(body).unwrap();
    engram
        .handle_get_method_edit_context(req)
        .await
        .map_err(|e| e.to_string())
}

async fn edit_safety(engram: &Engram, body: Value) -> Result<rmcp::model::CallToolResult, String> {
    let req: CheckEditSafetyRequest = serde_json::from_value(body).unwrap();
    engram
        .handle_check_edit_safety(req)
        .await
        .map_err(|e| e.to_string())
}

async fn page_context(engram: &Engram, body: Value) -> Result<rmcp::model::CallToolResult, String> {
    let req: GetPageContextRequest = serde_json::from_value(body).unwrap();
    engram
        .handle_get_page_context(req)
        .await
        .map_err(|e| e.to_string())
}

/// One target method `Svc.M` in `Site/App_Code/svc.vb` (body with `ifs`
/// decision points) plus `callers` calling functions in `Site/App_Code/callers.vb`.
fn seed_method(state: &AppState, dir: &std::path::Path, ifs: usize, callers: usize) -> String {
    let path = "Site/App_Code/svc.vb";
    let src = format!("Public Class Svc\n{}End Class\n", vb_method("M", ifs));
    write_file(dir, path, &src);
    let end = 1 + 1 + ifs as u32 * 3 + 1;
    let target = func_node(path, "Svc", "M", 2, end);
    let target_id = target.node_id.clone();
    let helper = func_node(path, "Svc", "Helper", end + 1, end + 3);
    let mut edges = vec![edge(&target_id, &helper.node_id, EdgeKind::Calls)];
    let mut nodes = vec![target, helper];
    for i in 0..callers {
        let c = func_node(
            "Site/App_Code/callers.vb",
            "Callers",
            &format!("C{i}"),
            10 + i as u32,
            12 + i as u32,
        );
        edges.push(edge(&c.node_id, &target_id, EdgeKind::Calls));
        nodes.push(c);
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    target_id
}

// ─── callers ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capped_callers_report_the_exact_total_not_the_cap() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 1, 55);
    let engram = Engram::new(state);

    let md = text(
        &edit_context(
            &engram,
            json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M"}),
        )
        .await
        .unwrap(),
    );
    // Row-4 A9: the count names its rule ("distinct callers — calls+dependency").
    assert!(
        md.contains("## Callers (3 shown of 55 distinct callers"),
        "renderer must show the true total behind the display cap:\n{md}"
    );
    assert!(
        md.contains("55 distinct callers"),
        "the verdict reason must use the exact total, not the 50 cap:\n{md}"
    );
    assert!(
        !md.contains("50 callers"),
        "bare capped count leaked:\n{md}"
    );

    let v = json_of(
        &edit_context(&engram, json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true}))
            .await
            .unwrap(),
    );
    let callers = &v["edit_safety"]["completeness"]["callers"];
    assert_eq!(callers["status"], "truncated", "{callers}");
    assert_eq!(callers["known_total"], 55, "{callers}");
    assert_eq!(callers["shown"], 50, "{callers}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangling_caller_is_counted_and_does_not_make_an_orphan_red() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    let target = seed_method(&state, &dir, 1, 0);
    // An incoming Calls edge whose source node does not exist.
    state
        .graph
        .upsert_edges(
            PID,
            &[edge(
                "sym:function:Site/App_Code/ghost.vb:Ghost.G:1",
                &target,
                EdgeKind::Calls,
            )],
        )
        .unwrap();
    let engram = Engram::new(state);

    let v = json_of(
        &edit_safety(&engram, json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true}))
            .await
            .unwrap(),
    );
    assert_eq!(v["completeness"]["callers_dangling"], 1, "{v}");
    assert_ne!(
        v["verdict"], "red",
        "dangling-only fan-in must not be an orphan RED: {v}"
    );
    let reasons = v["reasons"].to_string();
    assert!(reasons.contains("dangling"), "{reasons}");
    assert!(!reasons.contains("reflection"), "{reasons}");
}

// ─── parity + complexity ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_edit_safety_measures_complexity_from_the_body() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 20, 1);
    let engram = Engram::new(state);

    let v = json_of(
        &edit_safety(&engram, json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true}))
            .await
            .unwrap(),
    );
    assert_eq!(v["completeness"]["complexity"]["status"], "complete", "{v}");
    assert!(
        v["reasons"].to_string().contains("Complexity"),
        "a 21-branch method must carry a complexity reason: {v}"
    );
    assert_ne!(v["verdict"], "green", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_edit_safety_agrees_with_get_method_edit_context() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 20, 5);
    let engram = Engram::new(state);

    let req = json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true});
    let ctx = json_of(&edit_context(&engram, req.clone()).await.unwrap());
    let safety = json_of(&edit_safety(&engram, req).await.unwrap());
    assert_eq!(
        ctx["edit_safety"], safety,
        "the two tools must compute the verdict from the same facts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn low_risk_method_with_complete_evidence_is_green() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 1, 1);
    let engram = Engram::new(state);

    let v = json_of(
        &edit_safety(&engram, json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true}))
            .await
            .unwrap(),
    );
    assert_eq!(v["verdict"], "green", "{v}");
    for p in [
        "blast",
        "callers",
        "body",
        "complexity",
        "db_tables",
        "session_writes",
    ] {
        assert_eq!(v["completeness"][p]["status"], "complete", "{p}: {v}");
    }
}

// ─── identity ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_class_overloads_are_ambiguous_until_line_is_given() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    let path = "Site/App_Code/ov.vb";
    write_file(
        &dir,
        path,
        &format!(
            "Public Class Ov\n{}{}End Class\n",
            vb_method("M", 1),
            vb_method("M", 2)
        ),
    );
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                func_node(path, "Ov", "M", 2, 6),
                func_node(path, "Ov", "M", 7, 14),
            ],
        )
        .unwrap();
    let engram = Engram::new(state);

    let err = edit_safety(
        &engram,
        json!({"project_id": PID, "file_path": path, "method_name": "M"}),
    )
    .await
    .err()
    .expect("two overloads without a selector must be refused");
    assert!(err.contains("AMBIGUOUS"), "{err}");
    assert!(
        err.contains("line="),
        "the error must tell the caller how to select: {err}"
    );

    let v = json_of(
        &edit_context(&engram, json!({"project_id": PID, "file_path": path, "method_name": "M", "line": 7, "output_json": true}))
            .await
            .unwrap(),
    );
    assert_eq!(v["method_info"]["line_start"], 7, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_name_beats_substring_match() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    let path = "Site/App_Code/pg.vb";
    write_file(
        &dir,
        path,
        &format!(
            "Public Class Pg\n{}{}{}End Class\n",
            vb_method("Page_Load", 1),
            vb_method("Load", 1),
            vb_method("ALoad", 1)
        ),
    );
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                func_node(path, "Pg", "Page_Load", 2, 6),
                func_node(path, "Pg", "Load", 7, 11),
                // sorts before "Pg.Load" in key order and substring-matches "load"
                func_node(path, "Pg", "ALoad", 12, 16),
            ],
        )
        .unwrap();
    let engram = Engram::new(state);

    let v = json_of(
        &edit_context(&engram, json!({"project_id": PID, "file_path": path, "method_name": "Load", "output_json": true}))
            .await
            .unwrap(),
    );
    assert_eq!(v["method_info"]["method_name"], "Load", "{v}");
    assert_eq!(v["method_info"]["line_start"], 7, "{v}");
}

// ─── page context ───────────────────────────────────────────────────────────

fn seed_page(state: &AppState, dir: &std::path::Path, name: &str, table: &str, legacy_id: bool) {
    let aspx = format!("Site/pages/{name}.aspx");
    let cb = format!("{aspx}.vb");
    write_file(
        dir,
        &aspx,
        "<%@ Page Language=\"VB\" MasterPageFile=\"~/site.master\" %>\n<asp:Button ID=\"btnSave\" runat=\"server\" />\n",
    );
    write_file(
        dir,
        &cb,
        &format!(
            "Partial Class {name}\n{}End Class\n",
            vb_method("Page_Load", 1)
        ),
    );
    let mut m = func_node(&cb, name, "Page_Load", 2, 6);
    if legacy_id {
        m.node_id = format!("sym:function:{cb}:{name}.Page_Load");
    }
    let mid = m.node_id.clone();
    state.graph.upsert_nodes(PID, &[m]).unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[edge(
                &mid,
                &format!("table:{table}"),
                EdgeKind::QueriesTable,
            )],
        )
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_context_does_not_attribute_another_pages_tables() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_page(&state, &dir, "alpha", "ta", false);
    seed_page(&state, &dir, "beta", "tb", true);
    let engram = Engram::new(state);

    let v = json_of(
        &page_context(
            &engram,
            json!({"project_id": PID, "aspx_file": "Site/pages/alpha.aspx", "output_json": true}),
        )
        .await
        .unwrap(),
    );
    let tables = v["tables_used"].to_string();
    assert!(tables.contains("table:ta"), "{v}");
    assert!(
        !tables.contains("table:tb"),
        "beta's Page_Load table was attributed to alpha (suffix matching): {tables}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_context_honours_its_include_flags() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_page(&state, &dir, "gamma", "tg", false);
    let engram = Engram::new(state);

    let full = json_of(
        &page_context(
            &engram,
            json!({"project_id": PID, "aspx_file": "Site/pages/gamma.aspx", "output_json": true}),
        )
        .await
        .unwrap(),
    );
    assert!(full["master_page"].is_string(), "{full}");
    assert!(!full["methods"].as_array().unwrap().is_empty(), "{full}");

    let trimmed = json_of(
        &page_context(
            &engram,
            json!({"project_id": PID, "aspx_file": "Site/pages/gamma.aspx", "output_json": true,
                   "include_master_page": false, "include_codebehind": false}),
        )
        .await
        .unwrap(),
    );
    assert!(
        trimmed["master_page"].is_null(),
        "include_master_page=false ignored: {trimmed}"
    );
    assert!(
        trimmed["methods"].as_array().unwrap().is_empty(),
        "include_codebehind=false ignored: {trimmed}"
    );
}

// ─── business logic flag ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn business_logic_flag_is_honoured() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 1, 1);
    let engram = Engram::new(state);

    let with = json_of(
        &edit_context(&engram, json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true, "include_business_logic": true}))
            .await
            .unwrap(),
    );
    assert!(
        !with["business_logic"].is_null(),
        "flag=true produced no section: {with}"
    );
    assert!(
        with["business_logic"]["note"]
            .to_string()
            .contains("analyze_business_logic"),
        "an empty namespace must say how to populate it: {with}"
    );

    let without = json_of(
        &edit_context(&engram, json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true, "include_business_logic": false}))
            .await
            .unwrap(),
    );
    assert!(
        without["business_logic"].is_null(),
        "flag=false still produced a section: {without}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_without_codebehind_never_collects_unrelated_project_methods() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 1, 0);
    write_file(
        &dir,
        "Site/inline.aspx",
        "<%@ Page Language='VB' %><h1>Inline page</h1>",
    );
    let engram = Engram::new(state);
    let result = page_context(
        &engram,
        json!({"project_id": PID, "aspx_file": "Site/inline.aspx", "output_json": true}),
    )
    .await
    .unwrap();
    let body = text(&result);
    assert!(!body.contains("Svc.M"), "{body}");
    assert!(body.contains("code-behind source not found"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_directive_selects_nonconventional_codebehind_and_exact_file() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    write_file(
        &dir,
        "Site/page.aspx",
        "<%@ Page Language='VB' CodeFile='actual.vb' %>",
    );
    write_file(
        &dir,
        "Site/actual.vb",
        "Public Class Actual\n Public Sub RightMethod()\n End Sub\nEnd Class\n",
    );
    write_file(&dir, "Site/page.aspx.vb", "Public Class Wrong\nEnd Class\n");
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                func_node("Site/actual.vb", "Actual", "RightMethod", 2, 3),
                func_node("Site/actual.vb.old", "Wrong", "WrongSibling", 2, 3),
            ],
        )
        .unwrap();
    let engram = Engram::new(state);
    let result = page_context(
        &engram,
        json!({"project_id": PID, "aspx_file": "Site/page.aspx", "output_json": true}),
    )
    .await
    .unwrap();
    let body = text(&result);
    assert!(body.contains("RightMethod"), "{body}");
    assert!(!body.contains("WrongSibling"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_method_context_preserves_freshness_evidence() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 1, 0);
    let engram = Engram::new(state);
    let result = edit_context(&engram, json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true, "include_business_logic": false})).await.unwrap();
    let body = json_of(&result);
    assert!(body.get("freshness").is_some(), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caller_excerpts_require_current_fingerprint_and_preserve_arguments() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    let target = seed_method(&state, &dir, 1, 0);
    let path = "consumers.vb";
    let source = "Sub Export()\n Svc.M(images)\n Svc.M(images, renderHtml := True)\nEnd Sub\n";
    write_file(&dir, path, source);
    let caller = func_node(path, "Consumers", "Export", 1, 4);
    state.graph.upsert_nodes(PID, &[caller.clone()]).unwrap();
    state
        .graph
        .upsert_edges(PID, &[edge(&caller.node_id, &target, EdgeKind::Calls)])
        .unwrap();
    let engram = Engram::new(state);
    let request = json!({"project_id": PID, "file_path": "Site/App_Code/svc.vb", "method_name": "M", "output_json": true, "include_business_logic": false});
    let unverified = json_of(&edit_context(&engram, request.clone()).await.unwrap());
    assert_eq!(unverified["caller_excerpts"][0]["status"], "withheld");
    assert_eq!(
        unverified["method_info"]["called_by"][0]["line_kind"],
        "declaration"
    );
    assert_eq!(unverified["method_info"]["called_by"][0]["line"], 1);
    let mut file = caller;
    file.node_id = format!("file:{path}");
    file.node_type = "file".into();
    file.metadata =
        Some(json!({"file_hash": blake3::hash(source.as_bytes()).to_hex().to_string()}));
    engram.state.graph.upsert_nodes(PID, &[file]).unwrap();
    let current = json_of(&edit_context(&engram, request.clone()).await.unwrap());
    assert_eq!(current["caller_excerpts"][0]["status"], "verified_source");
    let excerpt = current["caller_excerpts"][0]["numbered_source"]
        .as_str()
        .unwrap();
    assert!(excerpt.contains("2:  Svc.M(images)"));
    assert!(excerpt.contains("renderHtml := True"));
    assert_eq!(current["caller_bodies"][0]["source_code"], "");
    assert_eq!(current["caller_bodies"][0]["line_start"], 1);
    let location = &current["method_info"]["called_by"][0];
    assert_eq!(
        location["line"], 1,
        "declaration stays distinct from calls at 2 and 3"
    );
    assert_eq!(location["line_end"], 4);
    assert_eq!(location["line_kind"], "declaration");
    assert_eq!(location["file_path"], "consumers.vb");
    assert_eq!(location["edge_kind"], "calls");
    let mut markdown_request = request.clone();
    markdown_request["output_json"] = json!(false);
    let rendered = edit_context(&engram, markdown_request.clone())
        .await
        .unwrap();
    let markdown = rendered.content[0].as_text().unwrap().text.as_str();
    assert!(
        markdown.contains("declaration: consumers.vb:1"),
        "{markdown}"
    );
    assert!(markdown.contains("2:  Svc.M(images)"), "{markdown}");
    assert!(
        markdown.contains("3:  Svc.M(images, renderHtml := True)"),
        "{markdown}"
    );
    markdown_request["include_caller_bodies"] = json!(true);
    let full = edit_context(&engram, markdown_request).await.unwrap();
    let full = full.content[0].as_text().unwrap().text.as_str();
    assert!(full.contains("declaration/body lines 1–4"), "{full}");
    write_file(&dir, path, &format!("' line moved\n{source}"));
    let stale = json_of(&edit_context(&engram, request).await.unwrap());
    assert_eq!(stale["caller_excerpts"][0]["status"], "withheld");
    assert_eq!(stale["caller_excerpts"][0]["numbered_source"], "");
    assert!(
        stale["caller_excerpts"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("changed since indexing")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_context_surfaces_older_file_regression_with_cutoff_and_opt_out() {
    let (tmp, state) = build_state();
    let dir = register_project(&state, &tmp);
    seed_method(&state, &dir, 1, 0);
    engram_server::services::project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    let mut docs = Vec::new();
    for (id, date, timestamp) in [(1, "2025-01-01", 1735689600), (2, "2026-08-01", 1785542400)] {
        let path = format!("pr:PR-{id}");
        let content = format!(
            "# PR-{id}: Fix duplicate report images\nmerged: {date} | author: fixture | kinds: backend\n\n## Files shipped together in this change\n- App_Code/svc.vb\n- reports/pdf.vb\n"
        );
        let hash = engram_core::ContentHash::compute(content.as_bytes());
        docs.push(engram_index::IndexDoc {
            generation: 0,
            chunk_id: engram_index::chunk_id_from_content_hash(&hash),
            doc_id: engram_core::DocIdStr::compute(&path, 0, 0, &hash).0,
            content_hash: hash.0,
            path: path.into(),
            language: "markdown".into(),
            content,
            namespace: "history".into(),
            author: None,
            timestamp: Some(timestamp),
            start_line: 0,
            end_line: 0,
        });
    }
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &docs, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let engram = Engram::new(state);
    // New task vocabulary is absent from the old title. File retrieval must
    // still surface the older regression, while withholding future work.
    let mut request = json!({"project_id":PID,"file_path":"Site/App_Code/svc.vb","method_name":"M","include_business_logic":false,"output_json":true,"history_query":"add signature capture","merged_before":"2026-03-01"});
    let result = json_of(&edit_context(&engram, request.clone()).await.unwrap());
    let history = result["historical_changes"].as_str().unwrap();
    assert!(history.contains("# PR-1:"), "{history}");
    assert!(history.contains("reports/pdf.vb"), "{history}");
    assert!(!history.contains("# PR-2:"), "{history}");
    assert!(
        history.contains("review approvals were not fetched"),
        "{history}"
    );
    request["include_history"] = json!(false);
    let result = json_of(&edit_context(&engram, request).await.unwrap());
    assert!(result["historical_changes"].is_null());
}
