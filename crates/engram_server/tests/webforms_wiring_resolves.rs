use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_webforms_wiring_resolves() {
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
            // Logic
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
        embedding_backend: "local".into(),
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
            project_name: "WebFormsTest".into(),
            project_type: "csharp".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Wait for nodes to appear (processing is async)
    let mut i = 0;
    while i < 20 {
        let nodes = engram
            .state
            .graph
            .query_nodes(project_id, None, None, None, 10)
            .unwrap();
        if nodes.len() >= 3 {
            break;
        } // file:aspx, file:cs, class:Order, etc.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // 3. Resolve edges
    engram.state.graph.resolve_symbol_edges(project_id).unwrap();

    // Debug: List all nodes
    let all_nodes = engram
        .state
        .graph
        .query_nodes(project_id, None, None, None, 100)
        .unwrap();
    println!("ALL NODES:");
    for n in &all_nodes {
        println!("  - {} | {} | {:?}", n.node_id, n.node_type, n.metadata);
    }

    // 4. Verify graph
    // page -> class (codebehind_class)
    let page_id = "page:Order.aspx";
    let neighbors = engram
        .state
        .graph
        .neighbors(project_id, engram_graph::EdgeKind::Contains, page_id, 10)
        .unwrap();
    println!("Neighbors of {}: {:?}", page_id, neighbors);

    let has_class = neighbors
        .iter()
        .any(|(nid, _): &(String, u32)| nid.contains("MyApp.Order"));
    assert!(
        has_class,
        "Page should have edge to code-behind class. Neighbors: {:?}",
        neighbors
    );

    // control -> handler (event_wiring)
    let nodes = engram
        .state
        .graph
        .query_nodes(project_id, Some("control"), Some("btnPrint"), None, 10)
        .unwrap();
    assert!(!nodes.is_empty(), "btnPrint control node not found");
    let ctrl_id = &nodes[0].node_id;

    let deps = engram
        .state
        .graph
        .neighbors(project_id, engram_graph::EdgeKind::Dependency, ctrl_id, 10)
        .unwrap();
    let has_handler = deps
        .iter()
        .any(|(nid, _): &(String, u32)| nid.contains("PrintJob"));
    assert!(
        has_handler,
        "Control should have edge to handler function. Deps: {:?}",
        deps
    );

    // page:Order.aspx -> file:Order.aspx.cs (codebehind_file)
    let file_deps = engram
        .state
        .graph
        .neighbors(project_id, engram_graph::EdgeKind::Contains, page_id, 10)
        .unwrap();
    let has_cb = file_deps
        .iter()
        .any(|(nid, _): &(String, u32)| nid.contains("Order.aspx.cs"));
    assert!(
        has_cb,
        "Page should have codebehind edge to file. Deps: {:?}",
        file_deps
    );
}
