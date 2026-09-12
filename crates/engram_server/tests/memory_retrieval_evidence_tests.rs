#![allow(clippy::unwrap_used)]
//! Retrieval must distinguish missing evidence from a current/empty memory bank.
use engram_core::{Config, MemorySection, ProjectRecord, RelPath};
use engram_server::{AppState, Engram};
use serde_json::json;

const PID: &str = "memory-evidence";

fn fixture(corrupt_graph: bool) -> (tempfile::TempDir, AppState, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let graph_path = data.join("graph/graph.redb");
    std::fs::create_dir_all(graph_path.parent().unwrap()).unwrap();
    if corrupt_graph {
        drop(engram_graph::GraphStore::open(&graph_path).unwrap());
        let db = redb::Database::open(&graph_path).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(redb::TableDefinition::<&str, &[u8]>::new("nodes_v2")).unwrap();
            table.insert("memory-evidence\0file:Broken.vb", b"invalid serialized node".as_slice()).unwrap();
        }
        tx.commit().unwrap();
    }
    let (state, _) = AppState::new(Config { data_dir: data, allowed_roots: vec![temp.path().to_path_buf()], embedding_backend: "fts_only".into(), ..Default::default() }).unwrap();
    state.registry.put_project(&ProjectRecord {
        project_id: PID.into(), project_name: PID.into(), project_type: "general".into(),
        directory: temp.path().to_string_lossy().into_owned(), created_at_ms: 0, updated_at_ms: 0,
        reindex_required_since_ms: None,
    }).unwrap();
    let engram = Engram::new(state.clone());
    (temp, state, engram)
}

fn save(state: &AppState, related_files: Vec<&str>) {
    let section: MemorySection = serde_json::from_value(json!({
        "section_id":"decision", "title":"Inventory decision", "content":"Retain the full decision body.",
        "updated_at_ms":1000, "related_files":related_files
    })).unwrap();
    state.registry.put_memory_section(PID, &section).unwrap();
}

async fn read_and_list(engram: &Engram) -> Vec<String> {
    vec![
        engram.handle_read_memory_bank(serde_json::from_value(json!({"project_id":PID,"section":"decision"})).unwrap()).await.unwrap(),
        engram.handle_list_memory_bank(serde_json::from_value(json!({"project_id":PID})).unwrap()).await.unwrap(),
    ].into_iter().map(|result| result.content[0].as_text().unwrap().text.clone()).collect()
}

#[tokio::test]
async fn unknown_projects_are_errors_but_first_user_read_does_not_create_a_project() {
    let (_temp, state, engram) = fixture(false);
    assert!(engram.handle_list_memory_bank(serde_json::from_value(json!({"project_id":"wrong-project"})).unwrap()).await.is_err());
    assert!(engram.handle_read_memory_bank(serde_json::from_value(json!({"project_id":"wrong-project","section":"decision"})).unwrap()).await.is_err());
    let user = engram_core::namespaces::USER_PROJECT_ID;
    assert!(engram.handle_list_memory_bank(serde_json::from_value(json!({"project_id":user})).unwrap()).await.is_ok());
    assert!(engram.handle_read_memory_bank(serde_json::from_value(json!({"project_id":user,"section":"decision"})).unwrap()).await.is_ok());
    assert!(state.registry.get_project(user).unwrap().is_none());
}

#[tokio::test]
async fn indexed_timestamp_identity_is_normalized_and_missing_subjects_are_unknown() {
    let (_temp, state, engram) = fixture(false);
    state.graph.upsert_nodes(PID, &[engram_graph::Node {
        node_id:"file:src/Rules.vb".into(), node_type:"file".into(), name:"Rules.vb".into(),
        namespace:"code".into(), language:"vb".into(), file_path:RelPath::new("src/Rules.vb"),
        start_line:0, end_line:0, generation:1, metadata:Some(json!({"mtime":2})),
    }]).unwrap();
    save(&state, vec![".\\SRC\\Rules.vb", "src/Missing.vb"]);
    for output in read_and_list(&engram).await {
        assert!(output.contains("changed since written"), "{output}");
        assert!(output.contains("UNKNOWN (1/2 related files lack indexed timestamps)"), "{output}");
    }
    save(&state, vec!["src/Rules.vb"]);
    for output in read_and_list(&engram).await {
        assert!(output.contains("working-tree content not verified"), "{output}");
    }
}

#[tokio::test]
async fn corrupt_timestamp_provider_keeps_memory_body_and_reports_unknown_freshness() {
    let (_temp, state, engram) = fixture(true);
    save(&state, vec!["Broken.vb"]);
    assert!(state.graph.list_file_node_metadata(PID).is_err(), "fixture must actually fail the provider");
    let outputs = read_and_list(&engram).await;
    assert!(outputs[0].contains("Retain the full decision body."));
    for output in outputs {
        assert!(output.contains("UNKNOWN (indexed-file lookup failed:"), "{output}");
    }
}

#[tokio::test]
async fn case_distinct_files_keep_exact_timestamps_and_ambiguous_aliases_are_unknown() {
    let (_temp, state, engram) = fixture(false);
    let nodes: Vec<_> = [("src/Rules.vb", 2), ("src/rules.vb", 0)].into_iter().map(|(path, mtime)| {
        engram_graph::Node {
            node_id:format!("file:{path}"), node_type:"file".into(), name:path.into(),
            namespace:"code".into(), language:"vb".into(), file_path:RelPath::new(path),
            start_line:0, end_line:0, generation:1, metadata:Some(json!({"mtime":mtime})),
        }
    }).collect();
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    save(&state, vec!["src/Rules.vb"]);
    for output in read_and_list(&engram).await {
        assert!(output.contains("changed since written"), "{output}");
        assert!(!output.contains("identities are ambiguous"), "{output}");
    }
    save(&state, vec!["src/rules.vb"]);
    for output in read_and_list(&engram).await {
        assert!(!output.contains("changed since written"), "{output}");
        assert!(!output.contains("identities are ambiguous"), "{output}");
    }
    save(&state, vec!["SRC/RULES.VB"]);
    for output in read_and_list(&engram).await {
        assert!(output.contains("UNKNOWN (1/1 related file identities are ambiguous"), "{output}");
        assert!(!output.contains("changed since written"), "{output}");
    }
}
