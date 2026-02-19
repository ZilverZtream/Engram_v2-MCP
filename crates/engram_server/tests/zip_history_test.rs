use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::io::Write;
use tempfile::tempdir;
use zip::write::SimpleFileOptions;

#[tokio::test]
async fn test_ingest_zip_history_smoke() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let zip_dir = root.join("zips");
    std::fs::create_dir_all(&zip_dir).unwrap();

    // 1. Create 3 zip snapshots
    // v1.zip: fileA, fileB
    let v1_path = zip_dir.join("v1.zip");
    {
        let file = std::fs::File::create(&v1_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("fileA.txt", options).unwrap();
        zip.write_all(b"content A v1").unwrap();
        zip.start_file("fileB.txt", options).unwrap();
        zip.write_all(b"content B v1").unwrap();
        zip.finish().unwrap();
    }

    // v2.zip: fileA (changed), fileB, fileC (new)
    // Changes: A, C. Pairs: (A, C)
    let v2_path = zip_dir.join("v2.zip");
    {
        let file = std::fs::File::create(&v2_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("fileA.txt", options).unwrap();
        zip.write_all(b"content A v2").unwrap();
        zip.start_file("fileB.txt", options).unwrap();
        zip.write_all(b"content B v1").unwrap();
        zip.start_file("fileC.txt", options).unwrap();
        zip.write_all(b"content C v1").unwrap();
        zip.finish().unwrap();
    }

    // v3.zip: fileA, fileB (changed), fileC (changed)
    // Changes: B, C. Pairs: (B, C)
    let v3_path = zip_dir.join("v3.zip");
    {
        let file = std::fs::File::create(&v3_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("fileA.txt", options).unwrap();
        zip.write_all(b"content A v2").unwrap();
        zip.start_file("fileB.txt", options).unwrap();
        zip.write_all(b"content B v2").unwrap();
        zip.start_file("fileC.txt", options).unwrap();
        zip.write_all(b"content C v2").unwrap();
        zip.finish().unwrap();
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
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    // 2. Index project (empty project just to get project_id)
    let project_dir = root.join("my_project");
    std::fs::create_dir_all(&project_dir).unwrap();
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "ZipHistoryTest".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // 3. Ingest zip history
    let res = engram
        .ingest_zip_history(Parameters(engram_server::IngestZipHistoryRequest {
            project_id: project_id.clone(),
            directory: zip_dir.to_string_lossy().to_string(),
            wait: true,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    println!(
        "ZIP INGEST RESULT:
{}",
        text
    );

    assert!(
        text.contains("Ingested 3 snapshots"),
        "Should show 3 snapshots ingested"
    );
    assert!(
        text.contains("added 2 temporal edges"),
        "Should show 2 temporal edges added"
    );

    // 4. Verify edges in graph
    let edges = state
        .graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::TemporalCoupling))
        .unwrap();

    // (fileA, fileC) from v2
    // (fileB, fileC) from v3
    assert!(edges.iter().any(|e| (e.source_id == "file:fileA.txt"
        && e.target_id == "file:fileC.txt")
        || (e.source_id == "file:fileC.txt" && e.target_id == "file:fileA.txt")));
    assert!(edges.iter().any(|e| (e.source_id == "file:fileB.txt"
        && e.target_id == "file:fileC.txt")
        || (e.source_id == "file:fileC.txt" && e.target_id == "file:fileB.txt")));
}
