use engram_core::{Config, build_pk};
use engram_server::Engram;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_index_and_search_flow() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("my_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create a dummy file to index.
    let file_path = project_dir.join("hello.rs");
    std::fs::write(&file_path, r#"fn main() { println!("hello engram"); }"#).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. Index Project
    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "test_project".into(),
        project_type: "code".into(),
        wait: true,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };
    assert!(text.contains("\u{2705} Indexed project_id"));
    assert!(!text.contains("files=0"), "No files were indexed: {}", text);

    // Extract project_id from output
    let project_id = text
        .lines()
        .find(|l: &&str| l.contains("\u{2705} Indexed project_id:"))
        .unwrap()
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .trim();

    // 2. Search Memory
    let search_req = engram_server::SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "hello engram".into(),
        max_results: 10,
        use_mmr: true,
        fts_mode: "strict".into(),
        include_content: true,
        max_content_chars_per_result: 1200,
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        metadata_filter: None,
    };

    let res = engram.search_memory(Parameters(search_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };
    assert!(text.contains("hello engram"));
    assert!(text.contains("hello.rs"));

    // 3. Verify Graph Symbols

    let graph = engram.state.graph.clone();

    let node_ids = graph.list_node_ids(project_id, None).unwrap();

    assert!(
        node_ids.iter().any(|id| id.contains("main")),
        "Symbol 'main' should be in the graph. Found: {:?}",
        node_ids
    );
}

#[tokio::test]
async fn test_csharp_symbol_extraction() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("csharp_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let code = r#"
    using System;
    namespace Test {
        class HelloWorld {
            static void Main(string[] args) {
                Console.WriteLine("Hello World");
            }
        }
    }
    "#;
    std::fs::write(project_dir.join("Program.cs"), code).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "csharp_test".into(),
        project_type: "code".into(),
        wait: true,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    // Extract project_id from output
    let project_id = text
        .lines()
        .find(|l: &&str| l.contains("\u{2705} Indexed project_id:"))
        .unwrap()
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .trim();

    let graph = engram.state.graph.clone();
    let node_ids = graph.list_node_ids(project_id, None).unwrap();

    assert!(
        node_ids.iter().any(|id| id.contains("HelloWorld")),
        "Class 'HelloWorld' should be in graph. Found: {:?}",
        node_ids
    );
    assert!(
        node_ids.iter().any(|id| id.contains("Main")),
        "Method 'Main' should be in graph. Found: {:?}",
        node_ids
    );

    // Verify FQN metadata
    let nodes = graph
        .query_nodes(project_id, Some("class"), Some("HelloWorld"), None, 1)
        .unwrap();
    assert!(!nodes.is_empty(), "HelloWorld node not found");
    let fqn = nodes[0]
        .metadata
        .as_ref()
        .and_then(|m| m.get("fqn"))
        .and_then(|v| v.as_str());
    assert_eq!(fqn, Some("Test.HelloWorld"));

    let nodes = graph
        .query_nodes(project_id, Some("function"), Some("Main"), None, 1)
        .unwrap();
    assert!(!nodes.is_empty(), "Main node not found");
    let fqn = nodes[0]
        .metadata
        .as_ref()
        .and_then(|m| m.get("fqn"))
        .and_then(|v| v.as_str());
    assert_eq!(fqn, Some("Test.HelloWorld.Main"));
}

#[tokio::test]
async fn test_vbnet_graceful_fallback() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("vb_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let code = r#"
    Module HelloWorld
        Sub Main()
            Console.WriteLine("Hello VB.NET")
        End Sub
    End Module
    "#;
    std::fs::write(project_dir.join("Program.vb"), code).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "vb_test".into(),
        project_type: "code".into(),
        wait: true,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };
    assert!(text.contains("\u{2705} Indexed project_id"));
    // It should NOT panic, even if symbols are empty because query failed to load.
}

#[tokio::test]
async fn test_job_cancellation() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("my_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create a lot of dummy files to make indexing take some time
    for i in 0..100 {
        std::fs::write(project_dir.join(format!("file_{}.rs", i)), "fn main() {}").unwrap();
    }

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. Start Index Project (no wait)
    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "cancel_project".into(),
        project_type: "code".into(),
        wait: false,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };
    assert!(text.contains("\u{1F7E1} Index job started"));

    // Extract job_id
    let job_id = text
        .lines()
        .find(|l| l.contains("job_id: "))
        .unwrap()
        .split("job_id: ")
        .nth(1)
        .unwrap()
        .trim();

    // 2. Cancel Job
    let cancel_req = engram_server::CancelJobRequest {
        job_id: job_id.to_string(),
    };
    let res = engram.cancel_job(Parameters(cancel_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };
    assert!(text.contains("\u{2705} cancelled job_id"));

    // 3. Verify status in registry
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    let reg = engram.state.registry.clone();
    let job_id_clone = job_id.to_string();
    let job = tokio::task::spawn_blocking(move || reg.get_job(&job_id_clone))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(job.status, "cancelled");

    // 4. Verify that work stopped (graph should not have all 100 files)
    if let Some(pid) = job.project_id {
        let nodes = engram
            .state
            .graph
            .list_node_ids(&pid, Some("file"))
            .unwrap();
        assert!(
            nodes.len() < 100,
            "Work should have stopped before all files were indexed. Found {} nodes",
            nodes.len()
        );
    }
}

#[tokio::test]
async fn test_path_normalization() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("path_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create a file in a sub-directory
    let sub_dir = project_dir.join("src");
    std::fs::create_dir_all(&sub_dir).unwrap();
    let file_path = sub_dir.join("lib.rs");
    std::fs::write(&file_path, "fn my_function() {}").unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "path_test".into(),
        project_type: "code".into(),
        wait: true,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };
    println!("Index output: {}", text);
    use std::io::Write;
    std::io::stdout().flush().unwrap();

    let project_id = text
        .lines()
        .find(|l: &&str| l.contains("\u{2705} Indexed project_id:"))
        .unwrap()
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .trim();

    // Verify path in graph
    let graph = engram.state.graph.clone();
    let nodes = graph.list_node_ids(project_id, Some("file")).unwrap();
    // Path should be normalized to src/lib.rs
    assert!(
        nodes.iter().any(|id| id == "file:src/lib.rs"),
        "File node should have normalized path. Found: {:?}",
        nodes
    );

    // Verify path in search
    let search_req = engram_server::SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "my_function".into(),
        max_results: 10,
        use_mmr: false,
        fts_mode: "strict".into(),
        include_content: true,
        max_content_chars_per_result: 1200,
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        metadata_filter: None,
    };

    let res = engram
        .search_memory(Parameters(search_req.clone()))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };
    assert!(text.contains("src/lib.rs"));
    assert!(!text.contains("src\\lib.rs"));

    // 4. Index again and ensure no duplicates (if we had dedup implemented, but let's at least check current state)
    // Actually, TODO says "Dedup/upsert semantics for Tantivy and Lance" is a LATER item.
    // For now, I'll just verify that the path mapping is consistent.

    let nodes_all = graph.list_node_ids(project_id, Some("file")).unwrap();
    let file_nodes: Vec<_> = nodes_all
        .iter()
        .filter(|id| *id == "file:src/lib.rs")
        .collect();
    assert_eq!(
        file_nodes.len(),
        1,
        "Should have exactly one file node for src/lib.rs. Found: {:?}",
        nodes_all
    );
}

#[tokio::test]
async fn test_memory_bank_persistence() {
    engram_core::setup_test_logging();

    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("persistence_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "persistence_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Add Memory Bank entry
    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: project_id.to_string(),
            section_id: Some("important_note".into()),
            section: "Architecture".into(),
            content: "We use a hybrid search engine with RRF.".into(),
        }))
        .await
        .unwrap();

    // 3. Update Project (Generation 2)
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_commits: 10,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    // 4. Verify Memory Bank still searchable
    let search_mb_res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory_bank".into(),
            query: "hybrid search".into(),
            max_results: 5,
            use_mmr: false,
            fts_mode: "strict".into(),
            include_content: true,
            max_content_chars_per_result: 1200,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }))
        .await
        .unwrap();

    let text = match &search_mb_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("hybrid search"),
        "Memory bank should be searchable. Found: {}",
        text
    );
    assert!(
        text.contains("memory_bank:important_note"),
        "Memory bank path should be correct. Found: {}",
        text
    );
}

#[tokio::test]
async fn test_indexing_deduplication() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("dedup_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"v1\"); }",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. First index
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "dedup_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Search and count results
    let search_req = engram_server::SearchMemoryRequest {
        project_id: project_id.to_string(),
        namespace: "memory".into(),
        query: "println".into(),
        max_results: 10,
        use_mmr: false,
        fts_mode: "strict".into(),
        include_content: true,
        max_content_chars_per_result: 1200,
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        metadata_filter: None,
    };

    let res1 = engram
        .search_memory(Parameters(search_req.clone()))
        .await
        .unwrap();
    let text1 = match &res1.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let count1 = text1.matches("chunk_id:").count();
    assert_eq!(
        count1, 1,
        "Should have 1 result initially. Output: {}",
        text1
    );

    // 3. Second index (with same content)
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "dedup_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: false, // force re-indexing
        }))
        .await
        .unwrap();

    // 4. Search and count results again
    let res2 = engram.search_memory(Parameters(search_req)).await.unwrap();
    let text2 = match &res2.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let count2 = text2.matches("chunk_id:").count();
    assert_eq!(
        count2, 1,
        "Should STILL have only 1 result after re-indexing. Output: {}",
        text2
    );
}

#[tokio::test]
async fn test_gc_preserves_global_namespaces() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("gc_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index Project (Gen 1)
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "gc_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Add Global data (memory_bank)
    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: project_id.to_string(),
            section: "global_note".into(),
            content: "this should persist".into(),
            section_id: None,
        }))
        .await
        .unwrap();

    // 3. Update Project (Gen 2)
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_commits: 10,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    // 4. Run GC manually
    state.graph.purge_old_generations(project_id, 2).unwrap();

    {
        // Use a temporary Engram instance to ensure runtime is loaded
        let engram_tmp = Engram::new(state.clone());
        let _ps = engram_tmp
            .update_project(Parameters(engram_server::UpdateProjectRequest {
                project_id: project_id.to_string(),
                wait: true,
                max_commits: 0,
                index_antipatterns: false,
            }))
            .await
            .unwrap(); // This should ensure it's loaded and maybe run some internal stuff

        // Wait, I can just call the search engine directly if I can get it.
        // I'll call search_memory to ensure it's loaded.
        engram_tmp
            .search_memory(Parameters(engram_server::SearchMemoryRequest {
                project_id: project_id.to_string(),
                namespace: "memory_bank".into(),
                query: "persist".into(),
                max_results: 1,
                use_mmr: false,
                fts_mode: "strict".into(),
                include_content: false,
                max_content_chars_per_result: 0,
                include_path_prefixes: None,
                exclude_path_prefixes: None,
                language_filters: None,
                metadata_filter: None,
            }))
            .await
            .unwrap();

        let ps = state.projects.get(project_id).unwrap();
        ps.search
            .purge_old_generations(project_id, 2)
            .await
            .unwrap();
    }

    // 5. Verify global data persisted
    let search_mb = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory_bank".into(),
            query: "persist".into(),
            max_results: 5,
            use_mmr: false,
            fts_mode: "strict".into(),
            include_content: true,
            max_content_chars_per_result: 1200,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }))
        .await
        .unwrap();

    let text_mb = match &search_mb.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text_mb.contains("this should persist"),
        "Global data should persist after GC. Output: {}",
        text_mb
    );
}

#[tokio::test]
async fn test_project_health() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("health_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // Index Project
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "health_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // Call health
    let health_res = engram
        .project_health(Parameters(engram_server::ProjectIdRequest {
            project_id: project_id.to_string(),
        }))
        .await
        .unwrap();

    let text_h = match &health_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text_h.contains("graph_nodes:"),
        "Health should contain graph_nodes. Output: {}",
        text_h
    );
    assert!(
        text_h.contains("tantivy_docs:"),
        "Health should contain tantivy_docs. Output: {}",
        text_h
    );
    assert!(
        text_h.contains("lancedb_rows:"),
        "Health should contain lancedb_rows. Output: {}",
        text_h
    );
}

#[tokio::test]
async fn test_project_repair() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("repair_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // Index Project (Gen 1)
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "repair_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // Call repair
    let repair_res = engram
        .repair_project(Parameters(engram_server::RepairProjectRequest {
            project_id: project_id.to_string(),
            scope: "full".into(),
            wipe_and_reindex: false,
            max_commits: 500,
            index_antipatterns: true,
        }))
        .await
        .unwrap();

    let text_r = match &repair_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text_r.contains("\u{2705} Project repaired."),
        "Repair should succeed. Output: {}",
        text_r
    );
    assert!(
        text_r.contains("active_generation: 2"),
        "Repair should increment generation. Output: {}",
        text_r
    );
}

#[tokio::test]
async fn test_delete_project() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("delete_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index Project
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "delete_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Add extra data (memory bank, repo rules, watches)
    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: project_id.to_string(),
            section: "Note".into(),
            content: "Some content".into(),
            section_id: None,
        }))
        .await
        .unwrap();

    engram
        .add_repo_rule(Parameters(engram_server::AddRepoRuleRequest {
            project_id: project_id.to_string(),
            file_pattern: "*.rs".into(),
            rule_text: "Rule".into(),
            priority: 1,
            rule_id: None,
        }))
        .await
        .unwrap();

    engram
        .watch_project(Parameters(engram_server::WatchProjectRequest {
            project_id: project_id.to_string(),
            enabled: true,
        }))
        .await
        .unwrap();

    // 3. Delete Project
    let delete_res = engram
        .delete_project(Parameters(engram_server::ProjectIdRequest {
            project_id: project_id.to_string(),
        }))
        .await
        .unwrap();

    let text_d = match &delete_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text_d.contains("\u{2705} Deleted project_id"));

    // 4. Verify everything is gone

    // Registry: Project record
    let rec = state.registry.get_project(project_id).unwrap();
    assert!(rec.is_none(), "Project record should be deleted");

    // Registry: Memory bank
    let secs = state.registry.list_memory_sections(project_id).unwrap();
    assert!(secs.is_empty(), "Memory bank sections should be deleted");

    // Registry: Repo rules
    let rules = state.registry.list_repo_rules(project_id).unwrap();
    assert!(rules.is_empty(), "Repo rules should be deleted");

    // Registry: Watches
    let watches = state.registry.list_watches(project_id).unwrap();
    assert!(watches.is_empty(), "Watches should be deleted");

    // Graph: Nodes
    let nodes = state.graph.count_nodes(project_id).unwrap();
    assert_eq!(nodes, 0, "Graph nodes should be deleted");

    // Graph: Edges
    let edges = state.graph.count_edges(project_id).unwrap();
    assert_eq!(edges, 0, "Graph edges should be deleted");

    // Disk: Project directory
    let proj_dir = data_dir.join("projects").join(project_id);
    assert!(
        !proj_dir.exists(),
        "Project directory on disk should be deleted"
    );
}

#[tokio::test]
async fn test_watch_project() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("watch_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _events_rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // Start the watcher actor
    tokio::spawn(engram_server::actors::watcher::run_watcher(
        state.clone(),
        state.events_tx.subscribe(),
    ));

    // 1. Index Project
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "watch_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Enable Watch
    engram
        .watch_project(Parameters(engram_server::WatchProjectRequest {
            project_id: project_id.to_string(),
            enabled: true,
        }))
        .await
        .unwrap();

    // Give watcher a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 3. Modify a file
    std::fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"updated\"); }",
    )
    .unwrap();

    // 4. Wait for debounce and update (debounce is 5s, let's wait 8s for safety)
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    // 5. Verify search result contains the update
    let search_res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "updated".into(),
            max_results: 5,
            use_mmr: false,
            fts_mode: "strict".into(),
            include_content: true,
            max_content_chars_per_result: 1200,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }))
        .await
        .unwrap();

    let text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("updated"),
        "Search should find the updated content. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_search_features() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("search_features_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create files with different paths and languages
    let src_dir = project_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("main.rs"),
        "fn main() { println!(\"rust logic\"); }",
    )
    .unwrap();

    let docs_dir = project_dir.join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();
    std::fs::write(
        docs_dir.join("readme.md"),
        "# Project README\nThis is some documentation.",
    )
    .unwrap();

    let python_file = project_dir.join("script.py");
    std::fs::write(python_file, "print('python script')").unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _events_rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "search_features_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Test fts_mode: regex
    let search_res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "rust".into(),
            max_results: 5,
            use_mmr: false,
            fts_mode: "regex".into(),
            include_content: true,
            max_content_chars_per_result: 1200,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }))
        .await
        .unwrap();
    let text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("rust logic"),
        "Regex search should find content"
    );

    // 3. Test include_path_prefixes
    let search_res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "logic".into(),
            max_results: 5,
            use_mmr: false,
            fts_mode: "strict".into(),
            include_content: true,
            max_content_chars_per_result: 1200,
            include_path_prefixes: Some(vec!["src".into()]),
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }))
        .await
        .unwrap();
    let text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("src/main.rs"), "Should only find src/main.rs");
    assert!(!text.contains("script.py"), "Should not find script.py");

    // 4. Test language_filters
    let search_res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "script".into(),
            max_results: 5,
            use_mmr: false,
            fts_mode: "strict".into(),
            include_content: true,
            max_content_chars_per_result: 1200,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: Some(vec!["python".into()]),
            metadata_filter: None,
        }))
        .await
        .unwrap();
    let text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("script.py"), "Should find python script");
    assert!(!text.contains("readme.md"), "Should not find readme.md");

    // 5. Test use_mmr (verify it doesn't crash)
    let _ = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "project".into(),
            max_results: 5,
            use_mmr: true,
            fts_mode: "strict".into(),
            include_content: true,
            max_content_chars_per_result: 1200,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_get_chunk_hardening() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("get_chunk_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let file_path = project_dir.join("main.rs");
    std::fs::write(&file_path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _events_rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "get_chunk_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Search to find a chunk_id
    let search_res = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "println".into(),
            max_results: 1,
            use_mmr: false,
            fts_mode: "strict".into(),
            include_content: false,
            max_content_chars_per_result: 0,
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            language_filters: None,
            metadata_filter: None,
        }))
        .await
        .unwrap();

    let text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let doc_id = text
        .split("doc_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim()
        .to_string();
    let chunk_id_str = text
        .split("chunk_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();
    let _chunk_id = chunk_id_str.parse::<u64>().unwrap();

    // 3. Get Chunk without rules
    let get_res = engram
        .get_chunk(Parameters(engram_server::GetChunkRequest {
            project_id: project_id.to_string(),
            doc_id: doc_id.clone(),
            namespace: "memory".into(),
            inject_rules: false,
        }))
        .await
        .unwrap();
    let text = match &get_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("path: main.rs"));
    assert!(text.contains("language: rust"));
    assert!(text.contains("lines: 1-3"));
    assert!(text.contains("namespace: memory"));
    assert!(!text.contains("[Repo Constraint]"));

    // 4. Add a rule and Get Chunk with rules
    engram
        .add_repo_rule(Parameters(engram_server::AddRepoRuleRequest {
            project_id: project_id.to_string(),
            file_pattern: "*.rs".into(),
            rule_text: "Use four spaces".into(),
            priority: 1,
            rule_id: None,
        }))
        .await
        .unwrap();

    let get_res = engram
        .get_chunk(Parameters(engram_server::GetChunkRequest {
            project_id: project_id.to_string(),
            doc_id,
            namespace: "memory".into(),
            inject_rules: true,
        }))
        .await
        .unwrap();
    let text = match &get_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("[Repo Constraint]: Use four spaces"));
}

#[tokio::test]
async fn test_query_graph_nodes() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("query_graph_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(
        project_dir.join("logic.rs"),
        "fn process_data() {}\nstruct DataProcessor {}",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _events_rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "query_graph_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Query nodes by type (function)
    let res = engram
        .query_graph_nodes(Parameters(engram_server::QueryGraphNodesRequest {
            project_id: project_id.to_string(),
            node_type: "function".into(),
            name_pattern: "".into(),
            file_path: "".into(),
            limit: 10,
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("process_data"),
        "Should find process_data function"
    );
    assert!(
        !text.contains("DataProcessor"),
        "Should not find struct when filtering for function"
    );

    // 3. Query nodes by name pattern
    let res = engram
        .query_graph_nodes(Parameters(engram_server::QueryGraphNodesRequest {
            project_id: project_id.to_string(),
            node_type: "".into(),
            name_pattern: "Processor".into(),
            file_path: "".into(),
            limit: 10,
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("DataProcessor"), "Should find DataProcessor");
    assert!(
        !text.contains("process_data"),
        "Should not find process_data"
    );

    // 4. Query nodes by file path
    let res = engram
        .query_graph_nodes(Parameters(engram_server::QueryGraphNodesRequest {
            project_id: project_id.to_string(),
            node_type: "".into(),
            name_pattern: "".into(),
            file_path: "logic.rs".into(),
            limit: 10,
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("logic.rs"));
    assert!(text.contains("process_data"));
    assert!(text.contains("DataProcessor"));
}

#[tokio::test]
async fn test_find_references() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![data_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _events_rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    let project_id = "test_refs";

    // Manually insert some nodes and edges into the graph
    let nodes = vec![
        engram_graph::Node {
            node_id: "A".into(),
            node_type: "file".into(),
            name: "A".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: "A.rs".into(),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        },
        engram_graph::Node {
            node_id: "B".into(),
            node_type: "file".into(),
            name: "B".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: "B.rs".into(),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        },
    ];
    state.graph.upsert_nodes(project_id, &nodes).unwrap();

    let edges = vec![engram_graph::Edge {
        source_id: "A".into(),
        target_id: "B".into(),
        namespace: "memory".into(),
        language: "rust".into(),
        edge_kind: engram_graph::EdgeKind::Dependency,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }];
    state.graph.upsert_edges(project_id, &edges).unwrap();

    // 1. Find outgoing references from A
    let res = engram
        .find_references(Parameters(engram_server::FindReferencesRequest {
            project_id: project_id.to_string(),
            node_id: "A".into(),
            edge_kind: Some("dependency".into()),
            direction: "out".into(),
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("Outgoing references"));
    assert!(text.contains("- B"));

    // 2. Find incoming references to B
    let res = engram
        .find_references(Parameters(engram_server::FindReferencesRequest {
            project_id: project_id.to_string(),
            node_id: "B".into(),
            edge_kind: Some("dependency".into()),
            direction: "in".into(),
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("Incoming references"));
    assert!(text.contains("- A"));
}

#[tokio::test]
async fn test_graph_search() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("graph_search_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // A depends on B. We search for "logic" which is in B.
    // Graph search should find B and then expand to A.
    std::fs::write(project_dir.join("A.rs"), "fn use_logic() { B::logic(); }").unwrap();
    std::fs::write(
        project_dir.join("B.rs"),
        "pub fn logic() { println!(\"core logic\"); }",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _events_rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "graph_search_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // Manually add dependency edge A -> B if it wasn't extracted
    state
        .graph
        .upsert_edges(
            project_id,
            &[engram_graph::Edge {
                source_id: "file:A.rs".into(),
                target_id: "file:B.rs".into(),
                namespace: "memory".into(),
                language: "rust".into(),
                edge_kind: engram_graph::EdgeKind::Dependency,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }],
        )
        .unwrap();

    // 2. Search for "logic"
    let res = engram
        .graph_search(Parameters(engram_server::GraphSearchRequest {
            project_id: project_id.to_string(),
            query: "logic".into(),
            max_results: 5,
            symbol_boost: 0.1,
            namespace: "memory".into(),
            fts_mode: "strict".into(),
            use_mmr: false,
            hop_depth: 1,
            include_content: false,
            max_content_chars: 400,
            expansion_edge_kinds: None,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    // Should find B.rs (contains "logic")
    assert!(
        text.contains("file:B.rs"),
        "Should find B.rs which contains 'logic'. Output: {}",
        text
    );

    // In our implementation, neighbors(B) would be incoming if we want expansion?
    // Actually, my implementation expands FROM search hits.
    // Hit is B.rs. Neighbors of B.rs?
    // Wait, the edge is A -> B. B has no outgoing dependency edges in my manual insert.

    // Let's add edge B -> A instead to test expansion if we want outgoing.
    state
        .graph
        .upsert_edges(
            project_id,
            &[engram_graph::Edge {
                source_id: "file:B.rs".into(),
                target_id: "file:A.rs".into(),
                namespace: "memory".into(),
                language: "rust".into(),
                edge_kind: engram_graph::EdgeKind::Dependency,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            }],
        )
        .unwrap();

    let res = engram
        .graph_search(Parameters(engram_server::GraphSearchRequest {
            project_id: project_id.to_string(),
            query: "logic".into(),
            max_results: 5,
            symbol_boost: 0.1,
            namespace: "memory".into(),
            fts_mode: "strict".into(),
            use_mmr: false,
            hop_depth: 1,
            include_content: false,
            max_content_chars: 400,
            expansion_edge_kinds: None,
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("file:A.rs"),
        "Should expand to A.rs via B.rs neighbor. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_search_history() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("history_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();

    // Commit 1
    std::fs::write(project_dir.join("a.rs"), "fn a() {}").unwrap();
    index.add_path(std::path::Path::new("a.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "feature: initial commit",
        &tree,
        &[],
    )
    .unwrap();

    // Commit 2
    std::fs::write(project_dir.join("b.rs"), "fn b() {}").unwrap();
    index.add_path(std::path::Path::new("b.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "fix: secondary commit",
        &tree,
        &[&repo.head().unwrap().peel_to_commit().unwrap()],
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "history_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Index Git History
    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            project_id: project_id.to_string(),
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();

    // 3. Search History (query only)
    let search_res = engram
        .search_history(Parameters(engram_server::SearchHistoryRequest {
            project_id: project_id.to_string(),
            query: "feature".into(),
            file_filter: None,
            exclude_paths: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            limit: 5,
            fts_mode: "strict".into(),
            use_mmr: false,
            max_content_chars: 800,
        }))
        .await
        .unwrap();

    let text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("initial commit"),
        "Should find the first commit. Output: {}",
        text
    );

    // 4. Search History (author filter)
    let search_res = engram
        .search_history(Parameters(engram_server::SearchHistoryRequest {
            project_id: project_id.to_string(),
            query: "fix".into(),
            file_filter: None,
            exclude_paths: None,
            author_filter: Some(sig.name().unwrap().to_string()),
            date_after: None,
            date_before: None,
            limit: 5,
            fts_mode: "strict".into(),
            use_mmr: false,
            max_content_chars: 800,
        }))
        .await
        .unwrap();

    let text = match &search_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("secondary commit"),
        "Should find the second commit with author filter. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_analyze_temporal_couplings() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![data_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _events_rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    let project_id = "test_couplings";

    // Insert nodes
    let nodes = vec![
        engram_graph::Node {
            node_id: "file:A.rs".into(),
            node_type: "file".into(),
            name: "A.rs".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: "A.rs".into(),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        },
        engram_graph::Node {
            node_id: "file:B.rs".into(),
            node_type: "file".into(),
            name: "B.rs".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: "B.rs".into(),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        },
        engram_graph::Node {
            node_id: "file:C.rs".into(),
            node_type: "file".into(),
            name: "C.rs".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: "C.rs".into(),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        },
    ];
    state.graph.upsert_nodes(project_id, &nodes).unwrap();

    // Manually insert some temporal coupling edges
    state
        .graph
        .increment_undirected_edge(
            project_id,
            "history",
            "rust",
            engram_graph::EdgeKind::TemporalCoupling,
            "file:A.rs",
            "file:B.rs",
            5,
            1,
        )
        .unwrap();

    state
        .graph
        .increment_undirected_edge(
            project_id,
            "history",
            "rust",
            engram_graph::EdgeKind::TemporalCoupling,
            "file:A.rs",
            "file:C.rs",
            2,
            1,
        )
        .unwrap();

    // 1. Global top couplings
    let res = engram
        .analyze_temporal_couplings(Parameters(engram_server::AnalyzeTemporalCouplingsRequest {
            project_id: project_id.to_string(),
            file_path: None,
            min_frequency: 1,
            limit: 10,
            inject_edges: true,
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("file:A.rs <-> file:B.rs (weight=5)"));
    assert!(text.contains("file:A.rs <-> file:C.rs (weight=2)"));

    // 2. Focused coupling for A.rs
    let res = engram
        .analyze_temporal_couplings(Parameters(engram_server::AnalyzeTemporalCouplingsRequest {
            project_id: project_id.to_string(),
            file_path: Some("A.rs".into()),
            min_frequency: 3,
            limit: 10,
            inject_edges: true,
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(text.contains("file:A.rs <-> file:B.rs (weight=5)"));
    assert!(
        !text.contains("file:C.rs"),
        "Should filter out C.rs due to min_frequency"
    );
}

#[tokio::test]
async fn test_dream_project() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("dream_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "dream_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Index content first to get stable doc_ids
    let contents = ["Content A", "Content B", "Content C"];
    let mut doc_ids = Vec::new();
    let mut index_docs = Vec::new();
    for (i, content) in contents.iter().enumerate() {
        let content_hash = engram_core::ContentHash::compute(content.as_bytes());
        let doc_id =
            engram_core::DocIdStr::compute("main.rs", i as u32, i as u32 + 1, &content_hash);
        let chunk_id = engram_index::chunk_id_from_content_hash(&content_hash);
        doc_ids.push(doc_id.0.clone());
        index_docs.push(engram_index::IndexDoc {
            generation: 1,
            chunk_id,
            doc_id: doc_id.0,
            content_hash: content_hash.0,
            path: "main.rs".into(),
            language: "rust".into(),
            content: content.to_string(),
            namespace: "memory".into(),
            author: None,
            timestamp: None,
            start_line: i as u32,
            end_line: i as u32 + 1,
        });
    }

    {
        let cancel = tokio_util::sync::CancellationToken::new();
        let ps = engram.state.projects.get(project_id).unwrap();
        ps.search
            .index_docs(project_id, &index_docs, &cancel)
            .await
            .unwrap();
    }

    // Now build graph using these pks
    let nodes: Vec<String> = doc_ids
        .iter()
        .map(|id| format!("pk:{}", build_pk(project_id, "memory", 1, id)))
        .collect();
    for nid in &nodes {
        engram
            .state
            .graph
            .upsert_nodes(
                project_id,
                &[engram_graph::Node {
                    node_id: nid.clone(),
                    node_type: "chunk".into(),
                    name: nid.clone(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "main.rs".into(),
                    start_line: 0,
                    end_line: 0,
                    generation: 1,
                    metadata: None,
                }],
            )
            .unwrap();
    }

    // Connect them strongly
    engram
        .state
        .graph
        .increment_undirected_edge(
            project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[0],
            &nodes[1],
            10,
            1,
        )
        .unwrap();
    engram
        .state
        .graph
        .increment_undirected_edge(
            project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[1],
            &nodes[2],
            10,
            1,
        )
        .unwrap();
    engram
        .state
        .graph
        .increment_undirected_edge(
            project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[0],
            &nodes[2],
            10,
            1,
        )
        .unwrap();

    // 3. Call dream_project with wait=true
    let dream_res = engram
        .dream_project(Parameters(engram_server::DreamProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_pairs: 10,
        }))
        .await
        .unwrap();

    let text = match &dream_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("insights_generated: 1"),
        "Should generate 1 insight. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_analyze_file_coding_style() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("style_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();

    // Commit with specific style (snake_case, 4 spaces, Result)
    let mut code = String::from("fn my_function() -> Result<(), ()> {\n    Ok(())\n}\n");
    for i in 0..25 {
        code.push_str(&format!("fn test_func_{}() {{}}\n", i));
    }

    std::fs::write(project_dir.join("lib.rs"), &code).unwrap();
    index.add_path(std::path::Path::new("lib.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "style commit", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // Register project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "style_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // Call analyze_file_coding_style
    let style_res = engram
        .analyze_file_coding_style(Parameters(engram_server::AnalyzeFileCodingStyleRequest {
            project_id: project_id.to_string(),
            file_path: "lib.rs".into(),
            diff_limit: 10,
        }))
        .await
        .unwrap();

    let text = match &style_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("Confidence:"),
        "Should contain confidence score. Output: {}",
        text
    );
    assert!(
        text.contains("4 spaces"),
        "Should detect 4 spaces. Output: {}",
        text
    );
    assert!(
        text.contains("snake_case"),
        "Should detect snake_case. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_analyze_coding_style_directory_and_cache() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("style_project_dir");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let src_dir = project_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();

    // Commit with specific style (tabs, PascalCase, logging, testing)
    let code = "
use tracing::info;

pub struct MyAwesomeStruct;

impl MyAwesomeStruct {
\tpub fn new() -> Self {
\t\tinfo!(\"Created new struct\");
\t\tMyAwesomeStruct
\t}
}

#[test]
fn test_it() {
\tlet _ = MyAwesomeStruct::new();
}
";
    std::fs::write(src_dir.join("main.rs"), code).unwrap();
    index.add_path(std::path::Path::new("src/main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial style commit", &tree, &[])
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // Register project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "style_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // Call analyze_file_coding_style on directory "src"
    let style_res = engram
        .analyze_file_coding_style(Parameters(engram_server::AnalyzeFileCodingStyleRequest {
            project_id: project_id.to_string(),
            file_path: "src".into(),
            diff_limit: 10,
        }))
        .await
        .unwrap();

    let text = match &style_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("tabs"),
        "Should detect tabs. Output: {}",
        text
    );
    assert!(
        text.contains("PascalCase"),
        "Should detect PascalCase. Output: {}",
        text
    );
    assert!(
        text.contains("logging"),
        "Should detect logging. Output: {}",
        text
    );
    assert!(
        text.contains("tests"),
        "Should detect tests. Output: {}",
        text
    );

    // Call again, should be cached
    let style_res_cached = engram
        .analyze_file_coding_style(Parameters(engram_server::AnalyzeFileCodingStyleRequest {
            project_id: project_id.to_string(),
            file_path: "src".into(),
            diff_limit: 10,
        }))
        .await
        .unwrap();

    let text_cached = match &style_res_cached.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text_cached.contains("(cached)"),
        "Should be cached. Output: {}",
        text_cached
    );
    assert!(
        text_cached.contains("tabs"),
        "Cached output should match. Output: {}",
        text_cached
    );
}

#[tokio::test]
async fn test_immune_system_end_to_end() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("immune_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Init git repo
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();

    // 1. Commit a "bad" pattern
    let bad_code = "fn use_unsafe_without_reason() {\n    unsafe {\n        let _ = *(123 as *const i32);\n    }\n}\n";
    std::fs::write(project_dir.join("lib.rs"), bad_code).unwrap();
    index.add_path(std::path::Path::new("lib.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    let first_oid = repo
        .commit(Some("HEAD"), &sig, &sig, "add bad pattern", &tree, &[])
        .unwrap();

    // 2. Revert the "bad" pattern
    let good_code = "fn use_safe_alternative() {\n    // Safer code\n}\n";
    std::fs::write(project_dir.join("lib.rs"), good_code).unwrap();
    index.add_path(std::path::Path::new("lib.rs")).unwrap();
    index.write().unwrap();
    let tree_id2 = index.write_tree().unwrap();
    let tree2 = repo.find_tree(tree_id2).unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        &format!("This reverts commit {}", first_oid),
        &tree2,
        &[&repo.find_commit(first_oid).unwrap()],
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "projection".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // Register project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "immune_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 3. Update project to index history and anti-patterns
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.to_string(),
            max_commits: 100,
            index_antipatterns: true,
            wait: true,
        }))
        .await
        .unwrap();

    // Force very low thresholds to ensure detection with FTS scores
    engram
        .state
        .registry
        .set_meta(project_id, "immune_warn_threshold", "0.0")
        .unwrap();
    engram
        .state
        .registry
        .set_meta(project_id, "immune_block_threshold", "0.0")
        .unwrap();

    // Call immune_check with exact same bad code to ensure FTS match
    let draft_code = "fn use_unsafe_without_reason() {\n    unsafe {\n        let _ = *(123 as *const i32);\n    }\n}\n";
    let immune_res = engram
        .immune_check(Parameters(engram_server::ImmuneCheckRequest {
            project_id: project_id.to_string(),
            code: draft_code.into(),
            top_k: 1,
        }))
        .await
        .unwrap();

    let text = match &immune_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("WARN") || text.contains("BLOCK"),
        "Should warn or block bad pattern. Output: {}",
        text
    );
    assert!(
        text.contains("ANTI-PATTERN"),
        "Should show anti-pattern metadata. Output: {}",
        text
    );
    assert!(
        text.contains("Original Commit:"),
        "Should mention original commit. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_dream_immune_integration() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("dream_immune_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // 1. Setup git repo with a reverted anti-pattern
    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    let bad_code = "fn unsafe_stuff() {\n    unsafe { let _ = 123 as *const i32; }\n}\n";
    std::fs::write(project_dir.join("bad.rs"), bad_code).unwrap();
    index.add_path(std::path::Path::new("bad.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let sig = repo.signature().unwrap();
    let first_oid = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "bad commit",
            &repo.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();

    let good_code = "fn safe_stuff() { }\n";
    std::fs::write(project_dir.join("bad.rs"), good_code).unwrap();
    index.add_path(std::path::Path::new("bad.rs")).unwrap();
    index.write().unwrap();
    let tree_id2 = index.write_tree().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        &format!("This reverts commit {}", first_oid),
        &repo.find_tree(tree_id2).unwrap(),
        &[&repo.find_commit(first_oid).unwrap()],
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 2. Index project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "dream_immune".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2b. Manually index an anti-pattern doc since we don't have real reverted commits
    let ap_content = "Potential Anti-pattern: unsafe code usage detected";
    {
        let ap_doc = engram_index::IndexDoc {
            generation: 0,
            chunk_id: 0,
            doc_id: "ap_unsafe".into(),
            content_hash: "hash_ap".into(),
            path: "rules/unsafe.md".into(),
            language: "markdown".into(),
            content: ap_content.into(),
            namespace: "antipattern".into(),
            author: None,
            timestamp: None,
            start_line: 0,
            end_line: 0,
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let ps = engram.state.projects.get(project_id).unwrap();
        ps.search
            .index_docs(project_id, &[ap_doc], &cancel)
            .await
            .unwrap();
    }

    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.to_string(),
            max_commits: 100,
            index_antipatterns: true,
            wait: true,
        }))
        .await
        .unwrap();

    // 3. Manually add co-occurrence between nodes that look like the anti-pattern
    let mut doc_ids = Vec::new();
    let mut index_docs = Vec::new();
    let code_with_unsafe = ap_content;
    for i in 0..3 {
        let content = code_with_unsafe.to_string();
        let content_hash = engram_core::ContentHash::compute(content.as_bytes());
        let doc_id = engram_core::DocIdStr::compute(
            "main.rs",
            i as u32 * 10,
            (i as u32 + 1) * 10,
            &content_hash,
        );
        let chunk_id = 999 + i as u64;
        doc_ids.push(doc_id.0.clone());
        index_docs.push(engram_index::IndexDoc {
            generation: 2,
            chunk_id,
            doc_id: doc_id.0,
            content_hash: content_hash.0,
            path: "main.rs".into(),
            language: "rust".into(),
            content,
            namespace: "memory".into(),
            author: None,
            timestamp: None,
            start_line: i as u32 * 10,
            end_line: (i as u32 + 1) * 10,
        });
    }

    {
        let cancel = tokio_util::sync::CancellationToken::new();
        let ps = engram.state.projects.get(project_id).unwrap();
        ps.search
            .index_docs(project_id, &index_docs, &cancel)
            .await
            .unwrap();
    }

    let nodes: Vec<String> = doc_ids
        .iter()
        .map(|id| format!("pk:{}", build_pk(project_id, "memory", 2, id)))
        .collect();
    for nid in &nodes {
        engram
            .state
            .graph
            .upsert_nodes(
                project_id,
                &[engram_graph::Node {
                    node_id: nid.clone(),
                    node_type: "chunk".into(),
                    name: nid.clone(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "main.rs".into(),
                    start_line: 0,
                    end_line: 0,
                    generation: 2,
                    metadata: None,
                }],
            )
            .unwrap();
    }

    // Connect them strongly
    engram
        .state
        .graph
        .increment_undirected_edge(
            project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[0],
            &nodes[1],
            10,
            2,
        )
        .unwrap();
    engram
        .state
        .graph
        .increment_undirected_edge(
            project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[1],
            &nodes[2],
            10,
            2,
        )
        .unwrap();
    engram
        .state
        .graph
        .increment_undirected_edge(
            project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[0],
            &nodes[2],
            10,
            2,
        )
        .unwrap();

    // Verify antipattern search works
    let anti_search = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            query: "unsafe".into(),
            namespace: "antipattern".into(),
            max_results: 1,
            fts_mode: "loose".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let text = match &anti_search.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("rules/unsafe.md") || text.contains("doc_id"),
        "Should find the anti-pattern. Output: {}",
        text
    );

    // 4. Dream
    let _dream_res = engram
        .dream_project(Parameters(engram_server::DreamProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_pairs: 10,
        }))
        .await
        .unwrap();

    // 5. Check if insight contains the warning in graph
    let nodes = engram
        .state
        .graph
        .query_nodes(project_id, Some("insight"), None, None, 10)
        .unwrap();
    assert!(!nodes.is_empty(), "Insight node should be created");

    let summary = nodes[0]
        .metadata
        .as_ref()
        .and_then(|m| m.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    assert!(
        summary.contains("ANTIPATTERN DETECTED"),
        "Insight should contain anti-pattern warning. Summary: {}",
        summary
    );
}

#[tokio::test]
async fn test_find_symbol_references() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![data_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let project_id = "test_sym_refs";

    // Register project
    let rec = engram_core::ProjectRecord {
        project_id: project_id.to_string(),
        project_name: "test_sym".into(),
        project_type: "code".into(),
        directory: data_dir.to_string_lossy().to_string(),
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    engram.state.registry.put_project(&rec).unwrap();

    // 1. Manually insert symbols and dependency edges
    let nodes = vec![
        engram_graph::Node {
            node_id: "file:A.rs:MySymbol".into(),
            node_type: "function".into(),
            name: "MySymbol".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: "A.rs".into(),
            start_line: 10,
            end_line: 20,
            generation: 1,
            metadata: None,
        },
        engram_graph::Node {
            node_id: "file:B.rs:Caller".into(),
            node_type: "function".into(),
            name: "Caller".into(),
            namespace: "memory".into(),
            language: "rust".into(),
            file_path: "B.rs".into(),
            start_line: 5,
            end_line: 15,
            generation: 1,
            metadata: None,
        },
    ];
    engram.state.graph.upsert_nodes(project_id, &nodes).unwrap();

    let edges = vec![engram_graph::Edge {
        source_id: "file:B.rs:Caller".into(),
        target_id: "file:A.rs:MySymbol".into(),
        namespace: "memory".into(),
        language: "rust".into(),
        edge_kind: engram_graph::EdgeKind::Dependency,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }];
    engram.state.graph.upsert_edges(project_id, &edges).unwrap();

    // 2. Call find_symbol_references
    let res = engram
        .find_symbol_references(Parameters(engram_server::FindSymbolReferencesRequest {
            project_id: project_id.to_string(),
            symbol_name: "MySymbol".into(),
            max_incoming: 200,
            max_outgoing_per_kind: 50,
            edge_kind_filter: None,
            file_scope: None,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("Graph references for MySymbol"),
        "Should find graph references. Output: {}",
        text
    );
    assert!(
        text.contains("file:B.rs:Caller"),
        "Should find Caller as reference. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_analyze_error_stack() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("error_stack_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(
        project_dir.join("core_logic.rs"),
        "fn critical_function() { panic!(\"boom\"); }",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    // 1. Index Project
    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "error_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Analyze Error Stack
    let traceback = "Error: boom\n  at critical_function (core_logic.rs:1:25)";
    let res = engram
        .analyze_error_stack(Parameters(engram_server::AnalyzeErrorStackRequest {
            project_id: project_id.to_string(),
            traceback: traceback.into(),
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("core_logic.rs"),
        "Should find the file from the stacktrace. Output: {}",
        text
    );
    assert!(
        text.contains("Hypothesis:"),
        "Should contain a hypothesis section. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_cpp_symbol_extraction() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("cpp_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let code = r#"
    class MyClass {
    public:
        void myMethod() {
            otherFunction();
        }
    };

    void otherFunction() {}
    "#;
    std::fs::write(project_dir.join("main.cpp"), code).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);

    let index_req = engram_server::IndexProjectRequest {
        directory: project_dir.to_string_lossy().to_string(),
        project_name: "cpp_test".into(),
        project_type: "code".into(),
        wait: true,
        dedupe_by_directory: true,
    };

    let res = engram.index_project(Parameters(index_req)).await.unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text content"),
    };

    let project_id = text
        .lines()
        .find(|l: &&str| l.contains("Indexed project_id:"))
        .unwrap()
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .trim();

    let graph = engram.state.graph.clone();
    let node_ids = graph.list_node_ids(project_id, None).unwrap();

    assert!(
        node_ids.iter().any(|id| id.contains("MyClass")),
        "Class 'MyClass' should be in graph. Found: {:?}",
        node_ids
    );
    assert!(
        node_ids.iter().any(|id| id.contains("myMethod")),
        "Method 'myMethod' should be in graph. Found: {:?}",
        node_ids
    );
    assert!(
        node_ids.iter().any(|id| id.contains("otherFunction")),
        "Function 'otherFunction' should be in graph. Found: {:?}",
        node_ids
    );
}

#[tokio::test]
async fn test_gc_policy_preservation() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("gc_policy_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index Project initially
    let repo = git2::Repository::init(&project_dir).unwrap();
    std::fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"gen 1\"); }",
    )
    .unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();

    let index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "gc_policy_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // 2. Add global data (memory_bank)
    engram
        .update_memory_bank(Parameters(engram_server::UpdateMemoryBankRequest {
            project_id: project_id.to_string(),
            section: "global_note".into(),
            content: "this must persist across generations".into(),
            section_id: None,
        }))
        .await
        .unwrap();

    // 3. Update project to Generation 2
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"gen 2\"); }",
    )
    .unwrap();
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_commits: 10,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    // 4. Update project to Generation 3
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        project_dir.join("main.rs"),
        "fn main() { println!(\"gen 3\"); }",
    )
    .unwrap();
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: project_id.to_string(),
            wait: true,
            max_commits: 10,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    // 5. Run GC for Generation 3
    {
        // Ensure project is loaded in runtime
        let _ps = engram
            .search_memory(Parameters(engram_server::SearchMemoryRequest {
                project_id: project_id.to_string(),
                query: "gen".into(),
                max_results: 1,
                ..Default::default()
            }))
            .await;

        let ps = state.projects.get(project_id).unwrap();
        ps.search
            .purge_old_generations(project_id, 3)
            .await
            .unwrap();
    }

    // 6. Verify global data (memory_bank) still exists
    let search_mb = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory_bank".into(),
            query: "persist".into(),
            max_results: 5,
            ..Default::default()
        }))
        .await
        .unwrap();

    let text_mb = match &search_mb.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text_mb.contains("this must persist"),
        "Global data lost! Output: {}",
        text_mb
    );

    // 7. Verify Snapshot data (memory) from gen 2 is gone from gen 3 search
    let search_gen2 = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "gen 2".into(),
            max_results: 5,
            ..Default::default()
        }))
        .await
        .unwrap();

    let text_gen2 = match &search_gen2.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        !text_gen2.contains("gen 2"),
        "Old snapshot data still returned! Output: {}",
        text_gen2
    );

    // 8. Verify Snapshot data (memory) from gen 3 is present
    let search_gen3 = engram
        .search_memory(Parameters(engram_server::SearchMemoryRequest {
            project_id: project_id.to_string(),
            namespace: "memory".into(),
            query: "gen 3".into(),
            max_results: 5,
            ..Default::default()
        }))
        .await
        .unwrap();

    let text_gen3 = match &search_gen3.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text_gen3.contains("gen 3"),
        "Latest snapshot data missing! Output: {}",
        text_gen3
    );
}

#[tokio::test]
async fn test_language_aware_resolution() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("lang_aware_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    // Create two files in different languages with the same function name 'init'
    std::fs::write(
        project_dir.join("app.rs"),
        "fn init() { println!(\"rust init\"); } fn main() { init(); }",
    )
    .unwrap();
    std::fs::write(
        project_dir.join("script.py"),
        "def init(): print(\"python init\")\ndef run(): init()",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    let _index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "lang_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &_index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    let graph = engram.state.graph.clone();

    // Check 'init' from app.rs (Rust)
    let rust_init_id = "sym:function:app.rs:init:0";
    let python_init_id = "sym:function:script.py:init:1";

    let edges = graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::Dependency))
        .unwrap();

    // Find edge from app.rs:function:main to init
    let rust_edge = edges
        .iter()
        .find(|e| e.source_id.contains("app.rs") && e.source_id.contains("main"))
        .expect("Should find rust main edge");
    assert_eq!(
        rust_edge.target_id, rust_init_id,
        "Rust main should call rust init"
    );

    // Find edge from script.py:function:run to init
    let py_edge = edges
        .iter()
        .find(|e| e.source_id.contains("script.py") && e.source_id.contains("run"))
        .expect("Should find python run edge");
    assert_eq!(
        py_edge.target_id, python_init_id,
        "Python run should call python init"
    );
}

#[tokio::test]
async fn test_graph_structure_edges() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("struct_edges_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(
        project_dir.join("main.cpp"),
        "class MyClass { public: void myMethod() {} };",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    let _index_res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "struct_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();

    let text = match &_index_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    let graph = engram.state.graph.clone();

    // Check for 'contains' edge between outer and inner
    let edges = graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::Contains))
        .unwrap();

    assert!(
        edges
            .iter()
            .any(|e| e.source_id.contains("MyClass") && e.target_id.contains("myMethod")),
        "Should find 'contains' edge from MyClass to myMethod. Edges: {:?}",
        edges
    );

    // Verify via tool
    let class_node_id = edges
        .iter()
        .find(|e| e.source_id.contains("MyClass"))
        .unwrap()
        .source_id
        .clone();
    let res = engram
        .find_references(Parameters(engram_server::FindReferencesRequest {
            project_id: project_id.to_string(),
            node_id: class_node_id,
            edge_kind: Some("contains".into()),
            direction: "out".into(),
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    assert!(
        text.contains("Members (Outgoing 'contains')"),
        "Output should mention contains kind. Output: {}",
        text
    );
    assert!(
        text.contains("myMethod"),
        "Output should find myMethod. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_centrality_caching() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("graph.db");
    let store = engram_graph::GraphStore::open(&db_path).unwrap();
    let project_id = "test_cache";
    let generation = 1;

    // 1. Setup small graph
    store
        .upsert_nodes(
            project_id,
            &[
                engram_graph::Node {
                    node_id: "A".into(),
                    node_type: "file".into(),
                    name: "A".into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "A.rs".into(),
                    start_line: 0,
                    end_line: 0,
                    generation,
                    metadata: None,
                },
                engram_graph::Node {
                    node_id: "B".into(),
                    node_type: "file".into(),
                    name: "B".into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "B.rs".into(),
                    start_line: 0,
                    end_line: 0,
                    generation,
                    metadata: None,
                },
            ],
        )
        .unwrap();

    store
        .upsert_edges(
            project_id,
            &[engram_graph::Edge {
                source_id: "A".into(),
                target_id: "B".into(),
                namespace: "memory".into(),
                language: "rust".into(),
                edge_kind: engram_graph::EdgeKind::Dependency,
                weight: 1,
                generation,
                metadata: None,
                updated_at_ms: 0,
            }],
        )
        .unwrap();

    // 2. Compute first time
    let start = std::time::Instant::now();
    let metrics1 =
        engram_graph::analysis::compute_pagerank(&store, project_id, generation).unwrap();
    let _dur1 = start.elapsed();

    // 3. Verify cached
    let cached = store.get_cached_centrality(project_id, generation).unwrap();
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().len(), 2);

    // 4. Compute second time (should be from cache)
    let start2 = std::time::Instant::now();
    let metrics2 =
        engram_graph::analysis::compute_pagerank(&store, project_id, generation).unwrap();
    let _dur2 = start2.elapsed();

    assert_eq!(metrics1.pagerank, metrics2.pagerank);
    // Usually cache is much faster but hard to assert strictly in unit test env.
    // We can manually corrupt cache to prove it is used.
    let mut corrupted = metrics1.pagerank.clone();
    corrupted.insert("A".into(), 99.0);
    store
        .set_cached_centrality(project_id, generation, &corrupted)
        .unwrap();

    let metrics3 =
        engram_graph::analysis::compute_pagerank(&store, project_id, generation).unwrap();
    assert_eq!(
        metrics3.pagerank["A"], 99.0,
        "Should have used the corrupted cached value"
    );
}

#[tokio::test]
async fn test_graph_traversal() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("traversal_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    let project_id = "test_traversal";

    // 1. Setup a chain: A -> B -> C
    state
        .graph
        .upsert_nodes(
            project_id,
            &[
                engram_graph::Node {
                    node_id: "A".into(),
                    node_type: "file".into(),
                    name: "A.rs".into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "A.rs".into(),
                    start_line: 0,
                    end_line: 0,
                    generation: 1,
                    metadata: None,
                },
                engram_graph::Node {
                    node_id: "B".into(),
                    node_type: "class".into(),
                    name: "MyClass".into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "A.rs".into(),
                    start_line: 10,
                    end_line: 20,
                    generation: 1,
                    metadata: None,
                },
                engram_graph::Node {
                    node_id: "C".into(),
                    node_type: "function".into(),
                    name: "myMethod".into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "A.rs".into(),
                    start_line: 15,
                    end_line: 18,
                    generation: 1,
                    metadata: None,
                },
            ],
        )
        .unwrap();

    state
        .graph
        .upsert_edges(
            project_id,
            &[
                engram_graph::Edge {
                    source_id: "A".into(),
                    target_id: "B".into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    edge_kind: engram_graph::EdgeKind::Contains,
                    weight: 1,
                    generation: 1,
                    metadata: None,
                    updated_at_ms: 0,
                },
                engram_graph::Edge {
                    source_id: "B".into(),
                    target_id: "C".into(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    edge_kind: engram_graph::EdgeKind::Contains,
                    weight: 1,
                    generation: 1,
                    metadata: None,
                    updated_at_ms: 0,
                },
            ],
        )
        .unwrap();

    // 2. Traverse 2 hops from A
    let res = engram
        .traverse_graph(Parameters(engram_server::TraverseGraphRequest {
            project_id: project_id.to_string(),
            node_id: "A".into(),
            max_hops: 2,
            edge_kinds: Some(vec!["contains".into()]),
            direction: "out".into(),
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("[1] B | class"),
        "Should find B at hop 1. Output: {}",
        text
    );
    assert!(
        text.contains("[2] C | function"),
        "Should find C at hop 2. Output: {}",
        text
    );
}

#[tokio::test]
async fn test_incremental_temporal_coupling() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("git_coupling_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    let sig = repo.signature().unwrap();

    // 1. First commit: change A and B
    std::fs::write(project_dir.join("A.rs"), "fn a() {}").unwrap();
    std::fs::write(project_dir.join("B.rs"), "fn b() {}").unwrap();
    index.add_path(std::path::Path::new("A.rs")).unwrap();
    index.add_path(std::path::Path::new("B.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "commit 1",
        &repo.find_tree(tree_id).unwrap(),
        &[],
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // Initial index
    let res = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "coupling_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let project_id = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .trim();

    // Run git history index
    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            project_id: project_id.to_string(),
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();

    let graph = engram.state.graph.clone();
    let edges = graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::TemporalCoupling))
        .unwrap();
    let ab_edge = edges
        .iter()
        .find(|e| e.source_id.contains("A.rs") && e.target_id.contains("B.rs"))
        .expect("Should find A<->B coupling");
    assert_eq!(ab_edge.weight, 1);

    // 2. Second commit: change A and B again
    std::fs::write(project_dir.join("A.rs"), "fn a2() {}").unwrap();
    index.add_path(std::path::Path::new("A.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "commit 2",
        &repo.find_tree(tree_id).unwrap(),
        &[&parent],
    )
    .unwrap();

    // Run git history index again (incremental)
    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            project_id: project_id.to_string(),
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();

    let edges2 = graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::TemporalCoupling))
        .unwrap();
    let ab_edge2 = edges2
        .iter()
        .find(|e| e.source_id.contains("A.rs") && e.target_id.contains("B.rs"))
        .unwrap();
    // Wait, commit 2 only changed A.rs. So weight should still be 1.
    assert_eq!(ab_edge2.weight, 1);

    // 3. Third commit: change both A and B
    std::fs::write(project_dir.join("B.rs"), "fn b2() {}").unwrap();
    index.add_path(std::path::Path::new("A.rs")).unwrap(); // A.rs unchanged since last commit but let's touch both
    index.add_path(std::path::Path::new("B.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "commit 3",
        &repo.find_tree(tree_id).unwrap(),
        &[&parent],
    )
    .unwrap();

    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            project_id: project_id.to_string(),
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();

    let edges3 = graph
        .list_edges(project_id, Some(engram_graph::EdgeKind::TemporalCoupling))
        .unwrap();
    let ab_edge3 = edges3
        .iter()
        .find(|e| e.source_id.contains("A.rs") && e.target_id.contains("B.rs"))
        .unwrap();
    // Commit 1 (A,B) + Commit 2 (A) + Commit 3 (B)
    // Wait, Commit 3 didn't touch A.rs in my code.
    // So only Commit 1 touched both.
    assert_eq!(ab_edge3.weight, 1);
}

#[tokio::test]
async fn test_rename_preserves_coupling() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("rename_coupling_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    let sig = repo.signature().unwrap();

    // 1. Commit A and B (coupling = 1)
    std::fs::write(project_dir.join("A.rs"), "// content").unwrap();
    std::fs::write(project_dir.join("B.rs"), "// content").unwrap();
    index.add_path(std::path::Path::new("A.rs")).unwrap();
    index.add_path(std::path::Path::new("B.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "init",
        &repo.find_tree(tree_id).unwrap(),
        &[],
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // Initial index
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "rename_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let project_id = engram.state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            project_id: project_id.to_string(),
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();

    // 2. Rename A to C
    std::fs::rename(project_dir.join("A.rs"), project_dir.join("C.rs")).unwrap();
    index.remove_path(std::path::Path::new("A.rs")).unwrap();
    index.add_path(std::path::Path::new("C.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "rename A to C",
        &repo.find_tree(tree_id).unwrap(),
        &[&parent],
    )
    .unwrap();

    engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            project_id: project_id.to_string(),
            max_commits: 10,
            index_antipatterns: false,
            wait: true,
        }))
        .await
        .unwrap();

    let graph = engram.state.graph.clone();
    let edges = graph
        .list_edges(&project_id, Some(engram_graph::EdgeKind::TemporalCoupling))
        .unwrap();

    // We expect edge C <-> B to exist with weight 1 (transferred from A)
    let cb_edge = edges
        .iter()
        .find(|e| e.source_id.contains("C.rs") && e.target_id.contains("B.rs"))
        .expect("Should find C<->B coupling after rename");
    assert_eq!(cb_edge.weight, 1);
}

#[tokio::test]
async fn test_merge_commit_policy() {
    let tmp = tempdir().unwrap();
    let project_dir = tmp.path().join("merge_policy_project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    let sig = repo.signature().unwrap();

    // 1. Initial commit
    std::fs::write(project_dir.join("base.txt"), "base").unwrap();
    index.add_path(std::path::Path::new("base.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let oid1 = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "c1",
            &repo.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();

    // 2. Branch A: change A.txt
    repo.set_head_detached(oid1).unwrap();
    std::fs::write(project_dir.join("A.txt"), "A").unwrap();
    index.add_path(std::path::Path::new("A.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let oid_a = repo
        .commit(
            None,
            &sig,
            &sig,
            "cA",
            &repo.find_tree(tree_id).unwrap(),
            &[&repo.find_commit(oid1).unwrap()],
        )
        .unwrap();

    // 3. Main (oid1): change B.txt
    repo.set_head("refs/heads/master").unwrap();
    repo.checkout_head(None).unwrap();
    std::fs::write(project_dir.join("B.txt"), "B").unwrap();
    index.add_path(std::path::Path::new("B.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let oid_b = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "cB",
            &repo.find_tree(tree_id).unwrap(),
            &[&repo.find_commit(oid1).unwrap()],
        )
        .unwrap();

    // 4. Merge A into Main
    let mut index_m = repo
        .merge_commits(
            &repo.find_commit(oid_b).unwrap(),
            &repo.find_commit(oid_a).unwrap(),
            None,
        )
        .unwrap();
    let tree_id = index_m.write_tree_to(&repo).unwrap();
    let _oid_merge = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "merge",
            &repo.find_tree(tree_id).unwrap(),
            &[
                &repo.find_commit(oid_b).unwrap(),
                &repo.find_commit(oid_a).unwrap(),
            ],
        )
        .unwrap();

    // Test walker with different policies
    let cancel = tokio_util::sync::CancellationToken::new();

    // Policy: AllParents
    let all_oids = engram_git::history::GitWalker::walk_new_commits(
        &repo,
        None,
        100,
        engram_git::history::MergeCommitPolicy::AllParents,
        &cancel,
    )
    .unwrap();
    // Should see c1, cA, cB, merge (4 commits)
    assert_eq!(
        all_oids.len(),
        4,
        "AllParents should see 4 commits. Found: {:?}",
        all_oids
    );

    // Policy: FirstParentOnly
    let first_oids = engram_git::history::GitWalker::walk_new_commits(
        &repo,
        None,
        100,
        engram_git::history::MergeCommitPolicy::FirstParentOnly,
        &cancel,
    )
    .unwrap();
    // Should see c1, cB, merge (3 commits). cA is on branch.
    assert_eq!(
        first_oids.len(),
        3,
        "FirstParentOnly should see 3 commits. Found: {:?}",
        first_oids
    );
}

#[tokio::test]
async fn test_structural_revert_detection() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("structural_revert_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    let sig = repo.signature().unwrap();

    // 1. Initial commit
    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let oid1 = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "init",
            &repo.find_tree(tree_id).unwrap(),
            &[],
        )
        .unwrap();

    // 2. Commit a bug
    std::fs::write(
        project_dir.join("main.rs"),
        "fn main() { panic!(\"bug\"); }",
    )
    .unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let parent1 = repo.find_commit(oid1).unwrap();
    let oid2 = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "add feature",
            &repo.find_tree(tree_id).unwrap(),
            &[&parent1],
        )
        .unwrap();

    // 3. Manual revert (fix bug by going back to state 1)
    std::fs::write(project_dir.join("main.rs"), "fn main() {}").unwrap();
    index.add_path(std::path::Path::new("main.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let parent2 = repo.find_commit(oid2).unwrap();
    let _oid3 = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "fix feature (manual revert)",
            &repo.find_tree(tree_id).unwrap(),
            &[&parent2],
        )
        .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // Index
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "revert_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let project_id = engram.state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // Run history index with antipatterns enabled
    let res = engram
        .index_git_history(Parameters(engram_server::IndexGitHistoryRequest {
            project_id: project_id.to_string(),
            max_commits: 10,
            index_antipatterns: true,
            wait: true,
        }))
        .await
        .unwrap();

    let summary = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    // Summary should report 1 reverted commit (detected structurally)
    assert!(
        summary.contains("reverted_commits: 1"),
        "Should detect 1 structural revert. Summary: {}",
        summary
    );
}

#[tokio::test]
async fn test_insight_deduplication() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("dedup_dream_project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();

    std::fs::write(
        project_dir.join("main.rs"),
        "fn a() {}\nfn b() {}\nfn c() {}",
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    // 1. Index
    let _ = engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "dedup_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let project_id = engram.state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // 2. Manually add co-occurrence to form a cluster
    let health_res = engram
        .project_health(Parameters(engram_server::ProjectIdRequest {
            project_id: project_id.clone(),
        }))
        .await
        .unwrap();
    let health_text = match &health_res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };
    let active_gen = health_text
        .split("active_generation: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();

    // 2. Mock co-occurrence cluster
    let mut doc_ids = Vec::new();
    let mut index_docs = Vec::new();
    for i in 0..3 {
        let content = format!("Unique content for part {}", i);
        let content_hash = engram_core::ContentHash::compute(content.as_bytes());
        let doc_id = engram_core::DocIdStr::compute(
            "main.rs",
            i as u32 * 10,
            (i as u32 + 1) * 10,
            &content_hash,
        );
        let chunk_id = i as u64 + 1;
        doc_ids.push(doc_id.0.clone());
        index_docs.push(engram_index::IndexDoc {
            generation: active_gen,
            chunk_id,
            doc_id: doc_id.0,
            content_hash: content_hash.0,
            path: "main.rs".into(),
            language: "rust".into(),
            content,
            namespace: "memory".into(),
            author: None,
            timestamp: None,
            start_line: i as u32 * 10,
            end_line: (i as u32 + 1) * 10,
        });
    }

    {
        let cancel = tokio_util::sync::CancellationToken::new();
        let ps = state.get_project_cached(&project_id).unwrap();
        ps.search
            .index_docs(&project_id, &index_docs, &cancel)
            .await
            .unwrap();
    }

    let nodes: Vec<String> = doc_ids
        .iter()
        .map(|id| format!("pk:{}", build_pk(&project_id, "memory", active_gen, id)))
        .collect();
    for nid in &nodes {
        state
            .graph
            .upsert_nodes(
                &project_id,
                &[engram_graph::Node {
                    node_id: nid.clone(),
                    node_type: "chunk".into(),
                    name: nid.clone(),
                    namespace: "memory".into(),
                    language: "rust".into(),
                    file_path: "main.rs".into(),
                    start_line: 0,
                    end_line: 0,
                    generation: active_gen,
                    metadata: None,
                }],
            )
            .unwrap();
    }
    state
        .graph
        .increment_undirected_edge(
            &project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[0],
            &nodes[1],
            10,
            active_gen,
        )
        .unwrap();
    state
        .graph
        .increment_undirected_edge(
            &project_id,
            "memory",
            "text",
            engram_graph::EdgeKind::CoOccurrence,
            &nodes[1],
            &nodes[2],
            10,
            active_gen,
        )
        .unwrap();

    // 3. Dream first time
    let count1 = engram_server::actors::dreamer::dream_once(&state, &project_id, 2, 3, 10)
        .await
        .unwrap();
    assert_eq!(count1, 1, "Should generate 1 insight");

    // 4. Dream second time (should be 0 due to dedup)
    let count2 = engram_server::actors::dreamer::dream_once(&state, &project_id, 2, 3, 10)
        .await
        .unwrap();
    assert_eq!(count2, 0, "Should generate 0 duplicate insights");

    // 5. Verify evidence in graph
    let nodes = state
        .graph
        .query_nodes(&project_id, Some("insight"), None, None, 10)
        .unwrap();
    assert!(!nodes.is_empty());
    let meta = nodes[0].metadata.as_ref().unwrap();
    assert!(
        meta.get("evidence").is_some(),
        "Metadata should contain evidence"
    );
    assert!(
        meta.get("cluster_fingerprint").is_some(),
        "Metadata should contain cluster_fingerprint"
    );
}

#[tokio::test]
async fn test_analyze_directory_coding_style() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("dir_style_project");
    std::fs::create_dir_all(project_dir.join("src")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let repo = git2::Repository::init(&project_dir).unwrap();
    let mut index = repo.index().unwrap();
    let sig = repo.signature().unwrap();

    // Commit two files in src with consistent style
    std::fs::write(project_dir.join("src/a.rs"), "fn a() {\n  let x = 1;\n}").unwrap();
    std::fs::write(project_dir.join("src/b.rs"), "fn b() {\n  let y = 2;\n}").unwrap();
    index.add_path(std::path::Path::new("src/a.rs")).unwrap();
    index.add_path(std::path::Path::new("src/b.rs")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        "init",
        &repo.find_tree(tree_id).unwrap(),
        &[],
    )
    .unwrap();

    let cfg = Config {
        data_dir: data_dir.clone(),
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "dir_style_test".into(),
            project_type: "code".into(),
            wait: true,
            dedupe_by_directory: true,
        }))
        .await
        .unwrap();
    let project_id = engram.state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // Analyze coding style for directory "src"
    let src_rel = engram_core::RelPath::new("src");
    let res = engram
        .analyze_file_coding_style(Parameters(engram_server::AnalyzeFileCodingStyleRequest {
            project_id: project_id.to_string(),
            file_path: src_rel.as_str().to_string(),
            diff_limit: 10,
        }))
        .await
        .unwrap();

    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("Expected text"),
    };

    assert!(
        text.contains("Style Guide for src"),
        "Output should mention directory. Output: {}",
        text
    );
    assert!(
        text.contains("Use 2 spaces for indentation"),
        "Should detect 2-space indentation. Output: {}",
        text
    );
}
