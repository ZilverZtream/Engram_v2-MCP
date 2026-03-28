#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_stable_fqn_ids() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Create a C# file
    let cs_content = r#"
namespace MyApp {
    public class Order {
        public void PrintJob() { }
    }
}
"#;
    std::fs::write(root.join("Order.cs"), cs_content).unwrap();

    // Initialize Git repo for update_project to work
    let repo = git2::Repository::init(root).unwrap();
    {
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("Order.cs")).unwrap();
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

    // 2. Index project (Generation 1)
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "StableIDTest".into(),
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
    let mut class_id = String::new();
    let mut method_id = String::new();

    while i < 20 {
        let nodes = engram
            .state
            .graph
            .query_nodes(project_id, None, None, None, 100)
            .unwrap();
        if nodes.len() >= 3 {
            for n in &nodes {
                if n.node_type == "class" && n.name == "Order" {
                    class_id = n.node_id.clone();
                }
                if n.node_type == "function" && n.name == "PrintJob" {
                    method_id = n.node_id.clone();
                }
            }
            if !class_id.is_empty() && !method_id.is_empty() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    assert_eq!(class_id, "sym:class:MyApp.Order");
    assert_eq!(method_id, "sym:function:MyApp.Order.PrintJob");

    // 3. Edit file (insert lines to change positions)
    let cs_content_v2 = r#"
// New comment line
// Another line
namespace MyApp {
    public class Order {
        public void PrintJob() { }
    }
}
"#;
    std::fs::write(root.join("Order.cs"), cs_content_v2).unwrap();

    {
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("Order.cs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Second commit", &tree, &[&parent])
            .unwrap();
    }

    // Re-index (Generation 2)
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.clone(),
            wait: true,
            max_commits: 1,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    // Check IDs in new generation
    let nodes_v2 = engram
        .state
        .graph
        .query_nodes(project_id, None, None, None, 100)
        .unwrap();
    let mut class_id_v2 = String::new();
    let mut method_id_v2 = String::new();
    for n in &nodes_v2 {
        if n.node_type == "class" && n.name == "Order" && n.generation == 2 {
            class_id_v2 = n.node_id.clone();
        }
        if n.node_type == "function" && n.name == "PrintJob" && n.generation == 2 {
            method_id_v2 = n.node_id.clone();
        }
    }

    assert_eq!(class_id_v2, "sym:class:MyApp.Order");
    assert_eq!(method_id_v2, "sym:function:MyApp.Order.PrintJob");
    assert_eq!(
        class_id, class_id_v2,
        "Class ID should be stable across edits"
    );
    assert_eq!(
        method_id, method_id_v2,
        "Method ID should be stable across edits"
    );
}

#[tokio::test]
async fn test_sql_stable_ids() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    let cs_content = r#"
namespace MyApp {
    public class Order {
        public void Load() {
            var cmd = new SqlCommand("SELECT * FROM Orders");
            var cmd2 = new SqlCommand("sp_GetOrders");
        }
    }
}
"#;
    std::fs::write(root.join("Order.cs"), cs_content).unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data_sql"),
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

    let (state, _) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "SqlIDTest".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Wait for edges to appear
    let mut i = 0;
    let mut sql_inline_id = String::new();
    let mut sql_proc_id = String::new();

    while i < 20 {
        let edges = engram.state.graph.list_edges(project_id, None).unwrap();
        for e in &edges {
            if e.target_id.starts_with("sql:inline:") {
                sql_inline_id = e.target_id.clone();
            }
            if e.target_id.starts_with("sql:stored_proc:") {
                sql_proc_id = e.target_id.clone();
            }
        }
        if !sql_inline_id.is_empty() && !sql_proc_id.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        i += 1;
    }

    assert!(
        sql_inline_id.starts_with("sql:inline:"),
        "Inline SQL ID mismatch: {}",
        sql_inline_id
    );
    assert_eq!(sql_proc_id, "sql:stored_proc:sp_GetOrders");
}
