#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_export_capture_pack() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Create a minimal project
    let code = "public class Foo { public void Bar() {} }";
    std::fs::write(root.join("Foo.cs"), code).unwrap();

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
            project_name: "ExportTest".into(),
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

    // 3. Export Capture Pack
    let res = engram
        .export_capture_pack(Parameters(engram_server::ExportCapturePackRequest {
            project_id: project_id.to_string(),
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text response"),
    };

    println!("EXPORT OUTPUT: {}", text);
    assert!(
        text.contains("Capture pack exported to:"),
        "Output should contain path"
    );

    let zip_path_str = text.split(": ").nth(1).unwrap().trim();
    let zip_path = std::path::Path::new(zip_path_str);
    assert!(
        zip_path.exists(),
        "Zip file should exist at {}",
        zip_path_str
    );

    // 4. Verify Zip contents
    let file = std::fs::File::open(zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();

    assert!(
        archive.by_name("overview.md").is_ok(),
        "Zip should contain overview.md"
    );
    assert!(
        archive.by_name("graph_topology.json").is_ok(),
        "Zip should contain graph_topology.json"
    );
    assert!(
        archive.by_name("ui_wiring.json").is_ok(),
        "Zip should contain ui_wiring.json"
    );
    assert!(
        archive.by_name("sql_map.json").is_ok(),
        "Zip should contain sql_map.json"
    );
}
