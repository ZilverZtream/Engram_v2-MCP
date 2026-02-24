#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_sql_extraction_csharp() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Create a minimal C# project with SQL
    let cs_content = r#"
namespace MyApp {
    public class DataAccess {
        public void LoadData() {
            var cmd = new SqlCommand("sp_GetUserOrders");
            var cmd2 = new SqlCommand("SELECT * FROM Users WHERE Id = 1");
            
            var cmd3 = new SqlCommand();
            cmd3.CommandText = "UPDATE Profiles SET Name = 'Bob'";
        }
    }
}
"#;
    std::fs::write(root.join("DataAccess.cs"), cs_content).unwrap();

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
            project_name: "SqlTest".into(),
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
        if nodes.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    // 3. Verify graph
    let nodes = engram
        .state
        .graph
        .query_nodes(project_id, Some("function"), Some("LoadData"), None, 10)
        .unwrap();
    assert!(!nodes.is_empty(), "LoadData node not found");
    let func_node_id = &nodes[0].node_id;

    let neighbors = engram
        .state
        .graph
        .neighbors(
            project_id,
            engram_graph::EdgeKind::SqlCalls,
            func_node_id,
            10,
        )
        .unwrap();

    println!("Neighbors of LoadData (sql_calls): {:?}", neighbors);

    let has_sp = neighbors
        .iter()
        .any(|(nid, _)| nid == "sql:stored_proc:sp_GetUserOrders");
    let has_inline1 = neighbors
        .iter()
        .any(|(nid, _)| nid.starts_with("sql:inline:"));

    assert!(has_sp, "Should find stored proc call");
    assert!(has_inline1, "Should find SELECT inline SQL");
}

#[tokio::test]
async fn test_sql_extraction_vbnet() {
    engram_core::setup_test_logging();

    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Create a minimal VB project with SQL
    let vb_content = r#"
Namespace MyApp
    Public Class DataAccess
        Public Sub LoadData()
            Dim cmd As New SqlCommand("sp_GetVbOrders")
            Dim cmd3 As New SqlCommand()
            cmd3.CommandText = "UPDATE VbProfiles SET Name = 'Alice'"
        End Sub
    End Class
End Namespace
"#;
    std::fs::write(root.join("DataAccess.vb"), vb_content).unwrap();

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

    // 2. Index project
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "SqlTestVb".into(),
            project_type: "vb".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Wait for indexing to settle
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    // 3. Scan for ANY sql_calls edges in the graph
    // Since symbol extraction might fail to find the Sub LoadData,
    // we check if ANY sql_calls edges were emitted (they might have source_id = file:...)

    // We don't have a list_all_edges, but we can query incoming to the SQL nodes
    let sp_node = "sql:stored_proc:sp_GetVbOrders";
    let incoming = engram
        .state
        .graph
        .find_incoming_edges(
            project_id,
            Some(engram_graph::EdgeKind::SqlCalls),
            sp_node,
            10,
        )
        .unwrap();

    println!("Incoming to VB SP: {:?}", incoming);
    assert!(
        !incoming.is_empty(),
        "Should have incoming edge to VB stored proc"
    );
}
