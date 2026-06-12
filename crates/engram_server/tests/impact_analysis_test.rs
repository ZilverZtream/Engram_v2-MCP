#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_impact_analysis_smoke() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Setup dependent code
    let aspx = r#"<%@ Page Inherits="App.Page" CodeBehind="Page.aspx.cs" %>
<asp:Button ID="btn" runat="server" OnClick="btn_Click" />"#;
    std::fs::write(root.join("Page.aspx"), aspx).unwrap();

    let cb = r#"
namespace App {
    public partial class Page {
        protected void btn_Click(object sender, System.EventArgs e) {
            Utility.Helper();
        }
    }
    public class Utility {
        public static void Helper() {}
    }
}"#;
    std::fs::write(root.join("Page.aspx.cs"), cb).unwrap();

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
            project_name: "ImpactTest".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Resolve
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // 2. Analyze impact of Utility.Helper
    let res = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: project_id.clone(),
            file_path: None,
            symbol_fqn: Some("App.Utility.Helper".to_string()),
            limit: 10,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    println!(
        "IMPACT OUTPUT:
{}",
        text
    );

    assert!(
        text.contains("App.Page.btn_Click"),
        "Impact should include the caller"
    );
    // Raw `calls` edges map to EdgeKind::Calls (not Dependency) since the
    // calls edge kind was restored through the ingest pipeline.
    assert!(text.contains("Calls this"), "Should include reason");

    // 3. Analyze impact of file
    let res_file = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: project_id.clone(),
            file_path: Some("Page.aspx.cs".to_string()),
            symbol_fqn: None,
            limit: 10,
        }))
        .await
        .unwrap();

    let text_file = &res_file.content[0].as_text().unwrap().text;
    println!(
        "FILE IMPACT OUTPUT:
{}",
        text_file
    );
    assert!(
        text_file.contains("page:Page.aspx"),
        "Impact should include the markup page"
    );
}
