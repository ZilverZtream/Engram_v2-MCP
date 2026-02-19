use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_webforms_handler_fqn_resolution() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Create a minimal WebForms project
    // Order.aspx
    let aspx_content = r#"
<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Order.aspx.cs" Inherits="MyApp.Web.Orders" %>
<asp:Button ID="btnPrint" runat="server" OnClick="PrintJob" />
"#;
    std::fs::write(root.join("Order.aspx"), aspx_content).unwrap();

    // Order.aspx.cs
    let cs_content = r#"
namespace MyApp.Web {
    public partial class Orders : System.Web.UI.Page {
        protected void PrintJob(object sender, EventArgs e) {
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
            project_name: "FqnTest".into(),
            project_type: "csharp".into(),
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
        let nodes = engram
            .state
            .graph
            .query_nodes(project_id, None, None, None, 10)
            .unwrap();
        // file:aspx, file:cs, sym:class:MyApp.Web.Orders, sym:function:MyApp.Web.Orders.PrintJob, sym:control:Order.aspx:btnPrint:2
        if nodes.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // 3. Resolve edges
    engram.state.graph.resolve_symbol_edges(project_id).unwrap();

    // 4. Verify graph: control -> handler FQN edge
    let all_nodes = engram
        .state
        .graph
        .query_nodes(project_id, None, None, None, 100)
        .unwrap();
    let ctrl_node = all_nodes
        .iter()
        .find(|n| n.node_type == "control" && n.name == "btnPrint")
        .expect("btnPrint node missing");
    let handler_id = "sym:function:MyApp.Web.Orders.PrintJob";

    // Check if handler node exists
    let _handler_node = engram
        .state
        .graph
        .get_node(project_id, handler_id)
        .unwrap()
        .expect("Handler node missing");

    // Verify neighbors of control node
    // event_wiring is mapped to Dependency or Contains?
    // In webforms.rs: kind: "event_wiring"
    // In tools.rs: match edge.kind.as_str() { ... _ => engram_graph::EdgeKind::Dependency }

    let neighbors = engram
        .state
        .graph
        .neighbors(
            project_id,
            engram_graph::EdgeKind::Dependency,
            &ctrl_node.node_id,
            10,
        )
        .unwrap();
    let has_handler = neighbors.iter().any(|(nid, _)| nid == handler_id);

    assert!(
        has_handler,
        "Control node should have edge to {} in neighbors: {:?}",
        handler_id, neighbors
    );
}
