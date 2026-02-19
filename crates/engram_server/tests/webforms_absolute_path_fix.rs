use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_webforms_absolute_path_normalization() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap(); // Use absolute path

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
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // 2. Index project using absolute path
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "AbsPathTest".into(),
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
        if nodes.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // 3. Verify graph: markup -> codebehind edge
    // The source page node is "file:Order.aspx"
    // The target codebehind node should be "file:Order.aspx.cs"

    let page_id = "file:Order.aspx";
    let cb_node_id = "file:Order.aspx.cs";

    // Check nodes exist first
    let page_node = engram
        .state
        .graph
        .get_node(project_id, page_id)
        .unwrap()
        .expect("Order.aspx node missing");
    let cb_node = engram
        .state
        .graph
        .get_node(project_id, cb_node_id)
        .unwrap()
        .expect("Order.aspx.cs node missing");

    assert_eq!(page_node.node_id, page_id);
    assert_eq!(cb_node.node_id, cb_node_id);

    // Verify neighbors
    let neighbors = engram
        .state
        .graph
        .neighbors(project_id, engram_graph::EdgeKind::Dependency, page_id, 10)
        .unwrap();
    let has_cb = neighbors.iter().any(|(nid, _)| nid == cb_node_id);

    assert!(
        has_cb,
        "Page node {} should have edge to {} in neighbors: {:?}",
        page_id, cb_node_id, neighbors
    );
}
