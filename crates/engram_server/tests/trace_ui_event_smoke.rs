#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_trace_ui_event_smoke() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Setup triad: Page -> Handler -> SQL
    // Default.aspx
    let aspx = r#"<%@ Page Inherits="App.Default" %>
<asp:Button ID="btnSave" runat="server" OnClick="btnSave_Click" />"#;
    std::fs::write(root.join("Default.aspx"), aspx).unwrap();

    // Default.aspx.cs
    let cb = r#"
namespace App {
    public partial class Default {
        protected void btnSave_Click(object sender, System.EventArgs e) {
            var cmd = new SqlCommand("INSERT INTO Logs VALUES ('clicked')");
            cmd.ExecuteNonQuery();
        }
    }
}"#;
    std::fs::write(root.join("Default.aspx.cs"), cb).unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "TraceTest".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Wait for nodes to appear
    let mut i = 0;
    while i < 20 {
        let nodes = state
            .graph
            .query_nodes(project_id, None, None, None, 10)
            .unwrap();
        if nodes.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // Resolve
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // 2. Call trace_ui_event
    let res = engram
        .trace_ui_event(Parameters(engram_server::TraceUiEventRequest {
            project_id: project_id.clone(),
            page_path: "Default.aspx".to_string(),
            control_id: Some("btnSave".to_string()),
            handler_fqn: None,
            max_hops: 10,
            max_paths: 5,
        }))
        .await
        .unwrap();

    let text_content = res.content[0].as_text().unwrap();
    let text = &text_content.text;
    println!("TRACE OUTPUT:\n{}", text);

    assert!(text.contains("btnSave"), "Output should contain start node");
    assert!(
        text.contains("btnSave_Click"),
        "Output should contain handler"
    );
    assert!(
        text.contains("sql:inline"),
        "Output should contain SQL node"
    );
    assert!(
        text.contains("Executes SQL"),
        "Output should contain justification"
    );
}

#[tokio::test]
async fn test_trace_ui_event_smoke_vb() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Setup triad: Page -> Handler -> SQL (VB)
    // Order.aspx
    let aspx = r#"<%@ Page Inherits="VbApp.Order" %>
<asp:Button ID="btnVbSave" runat="server" OnClick="btnVbSave_Click" />"#;
    std::fs::write(root.join("Order.aspx"), aspx).unwrap();

    // Order.aspx.vb
    let cb = r#"
Namespace VbApp
    Partial Class Order
        Protected Sub btnVbSave_Click(sender As Object, e As EventArgs)
            Dim cmd As New SqlCommand("UPDATE Orders SET Status='Saved'")
            cmd.ExecuteNonQuery()
        End Sub
    End Class
End Namespace"#;
    std::fs::write(root.join("Order.aspx.vb"), cb).unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data_vb"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "TraceTestVb".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Wait for nodes to appear
    let mut i = 0;
    while i < 20 {
        let nodes = state
            .graph
            .query_nodes(project_id, None, None, None, 10)
            .unwrap();
        if nodes.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // Resolve
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // 2. Call trace_ui_event
    let res = engram
        .trace_ui_event(Parameters(engram_server::TraceUiEventRequest {
            project_id: project_id.clone(),
            page_path: "Order.aspx".to_string(),
            control_id: Some("btnVbSave".to_string()),
            handler_fqn: None,
            max_hops: 10,
            max_paths: 5,
        }))
        .await
        .unwrap();

    let text_content = res.content[0].as_text().unwrap();
    let text = &text_content.text;
    println!("VB TRACE OUTPUT:\n{}", text);

    assert!(
        text.contains("btnVbSave"),
        "Output should contain start node"
    );
    assert!(
        text.contains("btnVbSave_Click"),
        "Output should contain handler"
    );
    assert!(
        text.contains("sql:inline"),
        "Output should contain SQL node"
    );
}
