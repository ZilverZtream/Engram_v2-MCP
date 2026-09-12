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
    assert!(text.contains("Shell.master -> ShellCode.cs") && text.contains("Direct code-behind context"), "{text}");
    assert!(text.contains("not a changed file") && text.contains("Test execution: not_run"), "{text}");
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
