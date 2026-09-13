#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::json;

async fn fixture(directive: &str) -> (tempfile::TempDir, Engram, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Shell.master"), directive).unwrap();
    std::fs::write(root.join("ShellCode.cs"), "public class ShellCode {\n public object LoadTheme() {\n  return Session[\"Theme\"];\n }\n}\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    }).unwrap();
    let server = Engram::new(state.clone());
    server.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"markup-matrix","project_type":"general","wait":true
    })).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    (temp, server, pid)
}

async fn matrix(server: &Engram, pid: &str) -> String {
    server.handle_derive_test_matrix(serde_json::from_value(json!({
        "project_id":pid,"files":["Shell.master"]
    })).unwrap()).await.unwrap().content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn markup_matrix_exposes_its_declared_companion_and_scope() {
    let (_temp, server, pid) = fixture("<%@ Master Language=\"C#\" CodeFile=\"ShellCode.cs\" %>\n<div>Theme</div>\n").await;
    let text = matrix(&server, &pid).await;
    assert!(text.contains("Session:Theme") && text.contains("LoadTheme (ShellCode.cs:"), "{text}");
    assert!(text.contains("Shell.master -> ShellCode.cs") && text.contains("Direct UI context"), "{text}");
    assert!(text.contains("Runtime interaction / lifecycle axis") && text.contains("Shared-session navigation and multi-window state"), "{text}");
    assert!(text.contains("not a changed file") && text.contains("Test execution: not_run"), "{text}");
}

#[tokio::test]
async fn markup_matrix_proposes_postback_keyboard_and_accessibility_cases() {
    let (_temp, server, pid) = fixture(
        "<%@ Master Language=\"C#\" CodeFile=\"ShellCode.cs\" %>\n<asp:UpdatePanel runat=\"server\"><ContentTemplate><asp:LinkButton ID=\"Search\" runat=\"server\" OnClick=\"Search_Click\"><i class=\"icon-search\"></i></asp:LinkButton></ContentTemplate></asp:UpdatePanel>\n",
    )
    .await;
    let text = matrix(&server, &pid).await;
    for expected in [
        "Initial load and full-postback reconstruction",
        "Partial-postback refresh and handler rebinding",
        "Keyboard activation parity",
        "DOM accessible names for interactive controls",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(text.contains("risk-directed scenarios, not proof"), "{text}");
}

async fn registered_control_fixture() -> (tempfile::TempDir, Engram, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(root.join("Controls")).unwrap();
    std::fs::write(
        root.join("Controls/Panel.ascx"),
        "<%@ Control Language=\"VB\" CodeFile=\"Panel.ascx.vb\" %>\n<div><%# Eval(\"FormattedText\") %></div>\n",
    ).unwrap();
    std::fs::write(
        root.join("Controls/Panel.ascx.vb"),
        "Public Class Panel\n Public Property Filter As String\n Private Sub Page_Init() Handles Me.Init\n  LoadRows(Filter)\n End Sub\nEnd Class\n",
    ).unwrap();
    std::fs::write(
        root.join("Host.aspx"),
        "<%@ Page Language=\"VB\" CodeFile=\"Host.aspx.vb\" %>\n<%@ Register Src=\"~/Controls/Panel.ascx\" TagPrefix=\"uc\" TagName=\"Panel\" %>\n<uc:Panel runat=\"server\" Filter='<%# Model.Filter %>' />\n",
    ).unwrap();
    std::fs::write(
        root.join("Host.aspx.vb"),
        "Public Class Host\n Private Sub Page_Init() Handles Me.Init\n End Sub\nEnd Class\n",
    ).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    }).unwrap();
    let server = Engram::new(state.clone());
    server.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"control-host-matrix","project_type":"dotnet_webforms_vb","wait":true
    })).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    (temp, server, pid)
}

#[tokio::test]
async fn codebehind_matrix_recovers_declaring_markup_and_bound_presenters() {
    let (_temp, server, pid) = fixture(
        "<%@ Master Language=\"C#\" CodeFile=\"ShellCode.cs\" %>\n<asp:BoundField DataField=\"RawText\" /><%# Eval(\"FormattedText\") %>\n",
    ).await;
    let response = server.handle_derive_test_matrix(serde_json::from_value(json!({
        "project_id":pid,
        "files":["ShellCode.cs"],
        "change_intent":"Add a field_id discriminator and canonical event prefix"
    })).unwrap()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(text.contains("ShellCode.cs -> Shell.master"), "{text}");
    assert!(text.contains("Stored-to-presented value parity"), "{text}");
    assert!(text.contains("RawText") && text.contains("FormattedText"), "{text}");
    assert!(text.contains("Planned-behavior risk axis"), "{text}");
    assert!(text.contains("Discriminator compatibility"), "{text}");
    assert!(text.contains("Canonical-token migration"), "{text}");
    assert!(text.contains("1 requested file(s)") && !text.contains("1 changed file(s)"), "{text}");
}

#[tokio::test]
async fn repository_risk_pack_hot_reloads_without_reindex_or_restart() {
    let (temp, server, pid) = fixture("<%@ Master Language=\"C#\" %>\n<% team_marker() %>\n").await;
    let before = matrix(&server, &pid).await;
    assert!(!before.contains("Repository-specific lifecycle v1"), "{before}");

    let rule_dir = temp.path().join("repo/.engram");
    std::fs::create_dir_all(&rule_dir).unwrap();
    let rule_path = rule_dir.join("test-risk-rules.yaml");
    std::fs::write(&rule_path, r#"
version: 1
rules:
  - id: repo.lifecycle
    title: Repository-specific lifecycle v1
    guidance: Exercise the repository's lifecycle contract.
    extensions: [master]
    any_terms: [team_marker]
"#).unwrap();
    let first = matrix(&server, &pid).await;
    assert!(first.contains("Configured risk packs: 1 validated rule(s)"), "{first}");
    assert!(first.contains("Repository-specific lifecycle v1"), "{first}");
    assert!(first.contains("configured rule `repo.lifecycle` from project pack"), "{first}");

    std::fs::write(&rule_path, r#"
version: 1
rules:
  - id: repo.lifecycle
    title: Repository-specific lifecycle v2
    guidance: Reloaded in the same server process.
    extensions: [master]
    any_terms: [team_marker]
"#).unwrap();
    let second = matrix(&server, &pid).await;
    assert!(second.contains("Repository-specific lifecycle v2"), "{second}");
    assert!(!second.contains("Repository-specific lifecycle v1"), "{second}");
}

#[tokio::test]
async fn codebehind_matrix_follows_registered_control_to_host_and_host_codebehind() {
    let (_temp, server, pid) = registered_control_fixture().await;
    let response = server.handle_derive_test_matrix(serde_json::from_value(json!({
        "project_id":pid,"files":["Controls/Panel.ascx.vb"]
    })).unwrap()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(text.contains("Controls/Panel.ascx.vb -> Controls/Panel.ascx"), "{text}");
    assert!(text.contains("Host.aspx registers Controls/Panel.ascx"), "{text}");
    assert!(text.contains("Host.aspx -> Host.aspx.vb"), "{text}");
    assert!(text.contains("Deferred binding lifecycle and refresh parity"), "{text}");
    assert!(text.contains("Host.aspx: WebForms-style"), "{text}");
}

#[tokio::test]
async fn stale_inverse_markup_cannot_supply_a_companion_relationship() {
    let (temp, server, pid) = fixture("<%@ Master Language=\"C#\" %>\n").await;
    std::fs::write(temp.path().join("repo/Shell.master"),
        "<%@ Master Language=\"C#\" CodeFile=\"ShellCode.cs\" %>\n<asp:BoundField DataField=\"RawText\" />\n").unwrap();
    let response = server.handle_derive_test_matrix(serde_json::from_value(json!({
        "project_id":pid,"files":["ShellCode.cs"]
    })).unwrap()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(!text.contains("ShellCode.cs -> Shell.master"), "{text}");
    assert!(!text.contains("Stored-to-presented value parity"), "{text}");
}

#[tokio::test]
async fn differently_cased_request_is_not_silently_dropped() {
    let (_temp, server, pid) = fixture("<%@ Master CodeFile=\"ShellCode.cs\" %>\n").await;
    let response = server.handle_derive_test_matrix(serde_json::from_value(json!({
        "project_id":pid,"files":["Shell.master", "shell.master"]
    })).unwrap()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(text.contains("Session:Theme"), "{text}");
    assert!(text.contains("shell.master: indexed axes are UNVERIFIED"), "{text}");
}

#[tokio::test]
async fn stale_markup_cannot_supply_a_new_companion_relationship() {
    let (temp, server, pid) = fixture("<%@ Master Language=\"C#\" %>\n").await;
    std::fs::write(temp.path().join("repo/Shell.master"), "<%@ Master CodeFile=\"ShellCode.cs\" %>\n").unwrap();
    let text = matrix(&server, &pid).await;
    assert!(!text.contains("Session:Theme"), "{text}");
    assert!(text.contains("STALE"), "{text}");
}

#[tokio::test]
async fn commented_directive_is_not_a_companion_and_missing_declaration_is_reported() {
    let (_temp, server, pid) = fixture("<%-- <%@ Master CodeFile=\"ShellCode.cs\" %> --%>\n<%@ Master CodeFile=\"Missing.cs\" %>\n").await;
    let text = matrix(&server, &pid).await;
    assert!(!text.contains("Session:Theme"), "{text}");
    assert!(text.contains("INCOMPLETE:") && text.contains("Missing.cs"), "{text}");
}

#[tokio::test]
async fn current_markup_does_not_make_stale_companion_axes_trustworthy() {
    let (temp, server, pid) = fixture("<%@ Master CodeFile=\"ShellCode.cs\" %>\n").await;
    std::fs::write(temp.path().join("repo/ShellCode.cs"), "public class ShellCode { public object LoadTheme() { return null; } }\n").unwrap();
    let text = matrix(&server, &pid).await;
    assert!(text.contains("Shell.master -> ShellCode.cs"), "{text}");
    assert!(text.contains("STALE") && text.contains("ShellCode.cs"), "{text}");
    assert!(!text.contains("Session:Theme"), "{text}");
}

#[tokio::test]
async fn declared_companion_cannot_escape_the_registered_project() {
    let (temp, server, pid) = fixture("<%@ Master CodeFile=\"../Outside.cs\" %>\n").await;
    std::fs::write(temp.path().join("Outside.cs"), "public class Outside { public object Load() { return Session[\"OutsideSecret\"]; } }\n").unwrap();
    let text = matrix(&server, &pid).await;
    assert!(text.contains("INCOMPLETE:") && text.contains("escapes the registered project"), "{text}");
    assert!(!text.contains("OutsideSecret") && !text.contains("Shell.master ->"), "{text}");
}
