#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 5 v3 slice 3 (owner: "metric → exemplar →
//! ENFORCE"). pre_push_audit gains an ADVISORY gate: for every markup file the
//! diff changes, the added markup is compared with the page's territory
//! (nearest sibling pages) — a class, resource family or user control that no
//! sibling uses is named, with what the siblings use instead. Info severity,
//! never blocking; a page without siblings is silent.

use engram_core::config::Config;
use engram_server::services::pre_commit_review_service::gates::{UiHouseStyleGate, all_gates};
use engram_server::services::pre_commit_review_service::{
    DiffFile, Gate, GateContext, Severity, parse_unified_diff,
};
use engram_server::state::AppState;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

fn temp_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    (tmp, state)
}

struct Ctx {
    state: AppState,
    project_id: String,
    project_dir: PathBuf,
    diff_files: Vec<DiffFile>,
    changed_paths: HashSet<String>,
    _tmp: tempfile::TempDir,
}

fn write(dir: &std::path::Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

const HEAD: &str = "<%@ Page Language=\"VB\" MasterPageFile=\"~/modules/dashboard/dashboard.master\" AutoEventWireup=\"false\" %>\n<asp:Content ID=\"c\" ContentPlaceHolderID=\"contentBody\" runat=\"server\">\n";

/// The territory: two siblings that show messages with `alert alert-info`
/// Panels, read `Resources.text`, and reuse `<uc:files>`; the page under edit
/// AFTER the change (its new markup is what the diff adds).
fn seed(dir: &std::path::Path, page_added: &str) {
    write(
        dir,
        "Site/modules/dashboard/pages/admin/things/list.aspx",
        &format!(
            "{HEAD}<div class=\"row\"><asp:Panel ID=\"panMsg\" runat=\"server\" CssClass=\"alert alert-info\"><asp:Label ID=\"lblMsg\" runat=\"server\" Text=\"<%$ Resources:text, Saved %>\" /></asp:Panel><uc:files ID=\"ucFiles\" runat=\"server\" /></div>\n</asp:Content>\n"
        ),
    );
    write(
        dir,
        "Site/modules/dashboard/pages/admin/things/detail.aspx",
        &format!(
            "{HEAD}<div class=\"row\"><asp:Panel ID=\"panInfo\" runat=\"server\" CssClass=\"alert alert-info\"><asp:Label ID=\"lblInfo\" runat=\"server\" Text=\"<%$ Resources:text, Hint %>\" /></asp:Panel><uc:files ID=\"ucFiles\" runat=\"server\" /></div>\n</asp:Content>\n"
        ),
    );
    write(
        dir,
        "Site/modules/dashboard/pages/admin/things/edit.aspx",
        &format!(
            "{HEAD}<div class=\"row\"><asp:TextBox ID=\"txtName\" runat=\"server\" /></div>\n{page_added}\n</asp:Content>\n"
        ),
    );
}

fn ctx(diff: &str, page_added: &str) -> Ctx {
    let (tmp, state) = temp_state();
    let project_dir = tmp.path().join("project");
    seed(&project_dir, page_added);
    let diff_files = parse_unified_diff(diff);
    let changed_paths = diff_files.iter().map(|f| f.path.clone()).collect();
    Ctx {
        state,
        project_id: "proj-hs".into(),
        project_dir,
        diff_files,
        changed_paths,
        _tmp: tmp,
    }
}

fn gate_ctx<'a>(c: &'a Ctx) -> GateContext<'a> {
    GateContext {
        search_index_note: None,
        state: &c.state,
        graph: c.state.graph.clone(),
        registry: c.state.registry.clone(),
        project_id: &c.project_id,
        project_dir: c.project_dir.as_path(),
        generation: 1,
        diff_files: &c.diff_files,
        changed_paths: &c.changed_paths,
        total_commits: 500,
        repo_rules: Arc::new(Vec::new()),
        files_by_parent: Arc::new(HashMap::new()),
        audit_function: None,
        degraded: std::sync::Mutex::new(Vec::new()),
        caps: std::sync::Mutex::new(Vec::new()),
    }
}

fn diff_for(added: &str) -> String {
    let body: String = added.lines().map(|l| format!("+{l}\n")).collect();
    format!(
        "diff --git a/Site/modules/dashboard/pages/admin/things/edit.aspx b/Site/modules/dashboard/pages/admin/things/edit.aspx\n--- a/Site/modules/dashboard/pages/admin/things/edit.aspx\n+++ b/Site/modules/dashboard/pages/admin/things/edit.aspx\n@@ -3,1 +3,{} @@\n <div class=\"row\"><asp:TextBox ID=\"txtName\" runat=\"server\" /></div>\n{body}",
        1 + added.lines().count()
    )
}

#[test]
fn markup_that_departs_from_the_territory_is_named_with_what_the_siblings_use() {
    let added = "<asp:Panel ID=\"panNew\" runat=\"server\" CssClass=\"alert alert-primary\"><asp:Label ID=\"lblNew\" runat=\"server\" Text=\"<%$ Resources:messages, Done %>\" /></asp:Panel>";
    let c = ctx(&diff_for(added), added);
    let findings = UiHouseStyleGate.run(&gate_ctx(&c)).unwrap();
    assert!(!findings.is_empty(), "the departure is reported");
    let all = findings
        .iter()
        .map(|f| format!("{} | {}", f.title, f.detail))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("alert-primary"),
        "the class no sibling uses is named:\n{all}"
    );
    assert!(
        all.contains("alert-info"),
        "what the siblings use instead is named:\n{all}"
    );
    assert!(
        all.contains("messages"),
        "the resource family no sibling reads is named:\n{all}"
    );
    assert!(
        findings
            .iter()
            .all(|f| matches!(f.severity, Severity::Info)),
        "advisory only — never blocking: {all}"
    );
    assert!(
        !all.contains("`alert`"),
        "a class the siblings DO use is not reported:\n{all}"
    );
}

#[test]
fn markup_written_the_way_the_siblings_write_it_raises_nothing() {
    let added = "<asp:Panel ID=\"panOk\" runat=\"server\" CssClass=\"alert alert-info\"><asp:Label ID=\"lblOk\" runat=\"server\" Text=\"<%$ Resources:text, Done %>\" /></asp:Panel><uc:files ID=\"ucFiles\" runat=\"server\" />";
    let c = ctx(&diff_for(added), added);
    let findings = UiHouseStyleGate.run(&gate_ctx(&c)).unwrap();
    assert!(
        findings.is_empty(),
        "conforming markup is silent: {findings:?}"
    );
}

#[test]
fn a_page_without_siblings_is_silent() {
    let added =
        "<asp:Panel ID=\"panNew\" runat=\"server\" CssClass=\"alert alert-primary\"></asp:Panel>";
    let diff = diff_for(added).replace("admin/things/edit.aspx", "public/lonely/edit.aspx");
    let c = ctx(&diff, added);
    write(
        &c.project_dir,
        "Site/modules/dashboard/pages/public/lonely/edit.aspx",
        &format!("{HEAD}{added}\n</asp:Content>\n"),
    );
    let findings = UiHouseStyleGate.run(&gate_ctx(&c)).unwrap();
    assert!(
        findings.is_empty(),
        "nothing to compare with → no advice: {findings:?}"
    );
}

#[test]
fn the_gate_is_registered_last_in_the_roster() {
    let names: Vec<&str> = all_gates().iter().map(|g| g.name()).collect();
    assert_eq!(names.last().copied(), Some("ui_house_style"), "{names:?}");
}
