#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_designer_integration() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Setup triad: Page, Designer, CodeBehind
    // Foo.aspx
    let aspx = r#"<%@ Page Inherits="App.Foo" %>
<asp:Button ID="btnSubmit" runat="server" OnClick="btnSubmit_Click" />"#;
    std::fs::write(root.join("Foo.aspx"), aspx).unwrap();

    // Foo.aspx.designer.cs
    let designer = r#"
namespace App {
    public partial class Foo {
        protected global::System.Web.UI.WebControls.Button btnSubmit;
    }
}"#;
    std::fs::write(root.join("Foo.aspx.designer.cs"), designer).unwrap();

    // Foo.aspx.cs
    let cb = r#"
namespace App {
    public partial class Foo {
        protected void btnSubmit_Click(object sender, System.EventArgs e) {}
    }
}"#;
    std::fs::write(root.join("Foo.aspx.cs"), cb).unwrap();

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
            project_name: "DesignerTest".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
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
        let nodes = state
            .graph
            .query_nodes(project_id, None, None, None, 10)
            .unwrap();
        if nodes.len() >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // Resolve
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // Verify: a Foo class node contains control:Foo.aspx:btnSubmit.
    // Node IDs are location-based, so each PARTIAL class declaration
    // (codebehind + designer file) is its own node — the designer-field
    // containment edge hangs off the designer-file partial. Check all of
    // them.
    let class_nodes = state
        .graph
        .query_nodes(project_id, Some("class"), Some("Foo"), None, 10)
        .unwrap();
    assert!(!class_nodes.is_empty(), "Foo class node missing");
    let mut all_neighbors = Vec::new();
    for class_node in &class_nodes {
        all_neighbors.extend(
            state
                .graph
                .neighbors(
                    project_id,
                    engram_graph::EdgeKind::Contains,
                    &class_node.node_id,
                    10,
                )
                .unwrap(),
        );
    }

    let has_control = all_neighbors
        .iter()
        .any(|(nid, _)| nid == "control:Foo.aspx:btnSubmit");
    assert!(
        has_control,
        "A Foo class partial should contain btnSubmit control. Neighbors: {:?}",
        all_neighbors
    );
}

#[tokio::test]
async fn test_vb_designer_integration() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Setup triad: Page, Designer, CodeBehind (VB)
    // Bar.aspx
    let aspx = r#"<%@ Page Inherits="VbApp.Bar" %>
<asp:Button ID="btnVb" runat="server" OnClick="btnVb_Click" />"#;
    std::fs::write(root.join("Bar.aspx"), aspx).unwrap();

    // Bar.aspx.designer.vb
    let designer = r#"
Namespace VbApp
    Partial Class Bar
        Protected WithEvents btnVb As Global.System.Web.UI.WebControls.Button
    End Class
End Namespace"#;
    std::fs::write(root.join("Bar.aspx.designer.vb"), designer).unwrap();

    // Bar.aspx.vb
    let cb = r#"
Namespace VbApp
    Partial Class Bar
        Protected Sub btnVb_Click(sender As Object, e As EventArgs)
        End Sub
    End Class
End Namespace"#;
    std::fs::write(root.join("Bar.aspx.vb"), cb).unwrap();

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
            project_name: "VbDesignerTest".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
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
        let nodes = state
            .graph
            .query_nodes(project_id, None, None, None, 10)
            .unwrap();
        if nodes.len() >= 5 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // Resolve
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // Verify: a Bar class partial contains control:Bar.aspx:btnVb (each
    // partial declaration is its own location-based node — check all).
    let class_nodes = state
        .graph
        .query_nodes(project_id, Some("class"), Some("Bar"), None, 10)
        .unwrap();
    assert!(!class_nodes.is_empty(), "Bar class node missing");
    let mut neighbors = Vec::new();
    for class_node in &class_nodes {
        neighbors.extend(
            state
                .graph
                .neighbors(
                    project_id,
                    engram_graph::EdgeKind::Contains,
                    &class_node.node_id,
                    10,
                )
                .unwrap(),
        );
    }

    let has_control = neighbors
        .iter()
        .any(|(nid, _)| nid == "control:Bar.aspx:btnVb");
    assert!(
        has_control,
        "Class VbApp.Bar should contain btnVb control. Neighbors: {:?}",
        neighbors
    );
}
