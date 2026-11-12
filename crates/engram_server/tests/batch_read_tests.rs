#![allow(clippy::unwrap_used)]
//! Batching preserves the real read handlers, including errors and ambiguity.
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::Node;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

#[test]
fn mixed_batch_matches_individual_reads_and_keeps_independent_errors() {
    // Runtime and all database owners drop before the directory, also on unwind.
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(),
            advertise_all_tools: true, ..Default::default()
        }).unwrap();
        let project = "batch-reads";
        state.registry.put_project(&ProjectRecord {
            project_id: project.into(), project_name: project.into(),
            directory: root.to_string_lossy().into_owned(), project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
        }).unwrap();
        let nodes: Vec<_> = ["first", "second"].into_iter().map(|id| Node {
            node_id: format!("sym:{id}"), node_type: "function".into(), name: "Page.Save".into(),
            namespace: "memory".into(), language: "vbnet".into(),
            file_path: RelPath::new(&format!("{id}.aspx.vb")), start_line: 2, end_line: 4,
            generation: 1, metadata: None,
        }).collect();
        state.graph.upsert_nodes(project, &nodes).unwrap();
        let server = Engram::new(state);
        assert!(server.tool_router.list_all().iter().any(|tool| tool.name == "batch_read"));
        let method = json!({"project_id":project,"fqn_or_name":"Page.Save","file_path":"first.aspx.vb","output_json":true});
        let chunk = json!({"project_id":project,"doc_id":"missing","max_tokens":0});
        let identity = json!({"project_id":project,"id":"Page.Save"});
        let exact = json!({"project_id":project,"id":"sym:first"});
        let expected = [
            serde_json::to_value(server.handle_get_method_info(serde_json::from_value(method.clone()).unwrap()).await.unwrap()).unwrap(),
            serde_json::to_value(server.handle_get_chunk(serde_json::from_value(chunk.clone()).unwrap()).await.unwrap_err()).unwrap(),
            serde_json::to_value(server.handle_resolve_id(serde_json::from_value(identity.clone()).unwrap()).await.unwrap()).unwrap(),
            serde_json::to_value(server.handle_resolve_id(serde_json::from_value(exact.clone()).unwrap()).await.unwrap()).unwrap(),
        ];
        let batch = server.handle_batch_read(serde_json::from_value(json!({"requests":[
            {"request_id":"method","request":{"tool":"get_method_info","arguments":method}},
            {"request_id":"bad-chunk","request":{"tool":"get_chunk","arguments":chunk}},
            {"request_id":"ambiguous","request":{"tool":"resolve_id","arguments":identity}},
            {"request_id":"exact","request":{"tool":"resolve_id","arguments":exact}}
        ]})).unwrap()).await.unwrap();
        let text = &batch.content[0].as_text().unwrap().text;
        assert!(text.len() <= 8192 * 4);
        let result: Value = serde_json::from_str(text).unwrap();
        assert_eq!(result["complete"], true);
        for (index, expected) in expected.iter().enumerate() {
            assert_eq!(&result["results"][index]["response"], expected);
        }
        assert_eq!(result["results"][1]["status"], "request_error");
        assert_eq!(result["results"][2]["status"], "ok");
        assert!(result["results"][2]["response"].to_string().contains("AMBIGUOUS"));
        assert_eq!(result["results"][3]["status"], "ok");
        assert!(result["consistency"].as_str().unwrap().contains("no atomic batch snapshot"));
    });
    drop(runtime);
    temporary.close().unwrap();
}
