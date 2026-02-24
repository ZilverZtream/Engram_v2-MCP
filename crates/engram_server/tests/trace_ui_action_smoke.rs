#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_trace_ui_action_smoke() {
    engram_core::setup_test_logging();

    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Create a minimal WebForms project
    // Order.aspx
    let aspx_content = r#"
<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Order.aspx.cs" Inherits="MyApp.Order" %>
<asp:Button ID="btnPrint" runat="server" OnClick="PrintJob" />
"#;
    std::fs::write(root.join("Order.aspx"), aspx_content).unwrap();

    // Order.aspx.cs
    let cs_content = r#"
namespace MyApp {
    public partial class Order : System.Web.UI.Page {
        protected void PrintJob(object sender, EventArgs e) {
            InternalPrint();
        }
        private void InternalPrint() {
            // Real logic
        }
    }
}
"#;
    std::fs::write(root.join("Order.aspx.cs"), cs_content).unwrap();

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

    // 2. Index project
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "TraceTest".into(),
            project_type: "csharp".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Wait for nodes
    let mut i = 0;
    while i < 20 {
        let nodes = engram
            .state
            .graph
            .query_nodes(project_id, None, None, None, 10)
            .unwrap();
        if nodes.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // Resolve edges
    engram.state.graph.resolve_symbol_edges(project_id).unwrap();

    // 3. Trace UI Action
    let res = engram
        .trace_ui_action(Parameters(engram_server::TraceUiActionRequest {
            project_id: project_id.to_string(),
            query: "btnPrint".into(),
            max_depth: 5,
            max_paths: 5,
        }))
        .await
        .unwrap();

    let text_content = match &res.content[0].raw {
        rmcp::model::RawContent::Text(text) => &text.text,
        _ => panic!("Expected text response"),
    };

    println!("TRACE OUTPUT:\n{}", text_content);

    assert!(
        text_content.contains("btnPrint"),
        "Should contain control ID"
    );
    assert!(
        text_content.contains("PrintJob"),
        "Should contain handler function"
    );
    assert!(
        text_content.contains("InternalPrint"),
        "Should contain downstream call"
    );
}
