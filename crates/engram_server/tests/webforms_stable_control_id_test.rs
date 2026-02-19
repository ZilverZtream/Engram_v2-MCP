use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_webforms_stable_control_id() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // Initialize Git repo
    let repo = git2::Repository::init(root).unwrap();

    // 1. Create a minimal WebForms project
    let aspx_content = r#"
<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Order.aspx.cs" Inherits="MyApp.Order" %>
<html><body>
    <asp:Button ID="btnSubmit" runat="server" OnClick="SubmitOrder" />
</body></html>
"#;
    std::fs::write(root.join("Order.aspx"), aspx_content).unwrap();

    let cs_content = r#"
namespace MyApp {
    public partial class Order : System.Web.UI.Page {
        protected void SubmitOrder(object sender, EventArgs e) { }
    }
}
"#;
    std::fs::write(root.join("Order.aspx.cs"), cs_content).unwrap();

    {
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("Order.aspx")).unwrap();
        index
            .add_path(std::path::Path::new("Order.aspx.cs"))
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();
    }

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
            project_name: "StableControlTest".into(),
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
        if nodes.len() >= 4 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    engram.state.graph.resolve_symbol_edges(project_id).unwrap();

    // Verify initial trace
    let trace_res = engram
        .trace_ui_action(Parameters(engram_server::TraceUiActionRequest {
            project_id: project_id.clone(),
            query: "btnSubmit".to_string(),
            max_depth: 5,
            max_paths: 1,
        }))
        .await
        .unwrap();

    let text = match &trace_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("SubmitOrder"),
        "Initial trace should find handler"
    );
    assert!(
        text.contains("control:Order.aspx:btnSubmit"),
        "Control ID should be stable format"
    );

    // 3. Move the button to a different line
    let aspx_content_v2 = r#"
<%@ Page Language="C#" AutoEventWireup="true" CodeBehind="Order.aspx.cs" Inherits="MyApp.Order" %>
<html><body>
    <!-- some extra lines -->
    <br/>
    <br/>
    <asp:Button ID="btnSubmit" runat="server" OnClick="SubmitOrder" />
</body></html>
"#;
    std::fs::write(root.join("Order.aspx"), aspx_content_v2).unwrap();

    {
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("Order.aspx")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Move button", &tree, &[&parent])
            .unwrap();
    }

    // Update project (Generation 2)
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.clone(),
            wait: true,
            max_commits: 1,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    engram.state.graph.resolve_symbol_edges(project_id).unwrap();

    // 4. Verify trace still works and ID is the same
    let trace_res_v2 = engram
        .trace_ui_action(Parameters(engram_server::TraceUiActionRequest {
            project_id: project_id.clone(),
            query: "btnSubmit".to_string(),
            max_depth: 5,
            max_paths: 1,
        }))
        .await
        .unwrap();

    let text_v2 = match &trace_res_v2.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text_v2.contains("SubmitOrder"),
        "Trace after move should still find handler"
    );
    assert!(
        text_v2.contains("control:Order.aspx:btnSubmit"),
        "Control ID should be identical after move"
    );
}
