#![allow(clippy::unwrap_used)]
//! Exercise publication against the real handler/store with a local text provider.
use engram_core::{Config, ContentHash, DocIdStr, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{services::project_service, state::AppState, tools::Engram};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn preview_does_not_publish_or_overwrite_and_explicit_persist_upserts() {
    tokio::time::timeout(std::time::Duration::from_secs(20), exercise()).await.unwrap();
}

async fn exercise() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let provider = tokio::spawn(async move {
        for answer in ["Initial preview", "Published interpretation", "Later preview", "Revised publication"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let body_start = loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..body_start]).to_lowercase();
            let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:"))
                .unwrap().trim().parse().unwrap();
            while request.len() < body_start + length {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            let payload = json!({"choices":[{"message":{"content":answer}}]}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}", payload.len()).as_bytes()).await.unwrap();
        }
    });

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Public Sub Save()\n Dim path = ConfigurationManager.AppSettings(\"StoragePath\")\nEnd Sub\n";
    std::fs::write(root.join("Rules.vb"), source).unwrap();
    let config = Config {
        data_dir: temp.path().join("data"), allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(), llm_backend: "openai".into(),
        llm_model: Some("fixture-model".into()), llm_openai_api_key: Some("fixture-key".into()),
        llm_openai_api_base: Some(format!("http://{address}/v1")), ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _events) = AppState::new(config).unwrap();
    let pid = "setting-persistence";
    state.registry.put_project(&ProjectRecord {
        project_id: pid.into(), project_name: pid.into(), directory: root.to_string_lossy().into_owned(),
        project_type: "dotnet_webforms_vb".into(), created_at_ms: 0, updated_at_ms: 0,
        reindex_required_since_ms: None,
    }).unwrap();
    state.registry.set_meta(pid, "active_generation", "1").unwrap();
    let node = |id: &str, name: &str, kind: &str, metadata| Node {
        node_id: id.into(), name: name.into(), node_type: kind.into(), namespace: "memory".into(),
        language: "vbnet".into(), file_path: RelPath::new("Rules.vb"), start_line: 1, end_line: 3,
        generation: 1, metadata,
    };
    state.graph.upsert_nodes(pid, &[
        node("file:Rules.vb", "Rules.vb", "file", Some(json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}))),
        node("fixture:Save", "Save", "function", None),
        node("setting:StoragePath", "StoragePath", "app_setting", None),
    ]).unwrap();
    state.graph.upsert_edges(pid, &[Edge {
        source_id: "fixture:Save".into(), target_id: "setting:StoragePath".into(),
        edge_kind: EdgeKind::ReadsSetting, namespace: "memory".into(), language: "vbnet".into(),
        weight: 1, generation: 1, metadata: None, updated_at_ms: 0,
    }]).unwrap();
    let runtime = project_service::ensure_project_runtime(&state, pid).await.unwrap();
    let engram = Engram::new(state.clone());
    let path = "__settings/StoragePath.md";
    let doc_id = DocIdStr::compute(path, 0, 0, &ContentHash::compute(path.as_bytes())).0;
    let baseline = runtime.search.count_docs(pid).unwrap();
    for (persist, expected_count, expected_stored) in [
        (None, baseline, None),
        (Some(true), baseline + 1, Some("Published interpretation")),
        (Some(false), baseline + 1, Some("Published interpretation")),
        (Some(true), baseline + 1, Some("Revised publication")),
    ] {
        let mut request = json!({"project_id":pid,"name":"StoragePath"});
        if let Some(value) = persist { request["persist"] = json!(value); }
        let response = engram.handle_describe_setting(serde_json::from_value(request).unwrap()).await.unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        assert!(text.contains("LLM-inferred"));
        assert!(text.contains(if persist == Some(true) { "_(persisted" } else { "preview only — not persisted" }));
        assert_eq!(runtime.search.count_docs(pid).unwrap(), expected_count);
        let stored = runtime.search.get_doc_by_doc_id(pid, "business_logic", 0, &doc_id).unwrap();
        match expected_stored {
            Some(expected) => {
                let body = stored.unwrap().2;
                assert!(body.contains(expected), "{body}");
                assert!(body.contains("Source positions and runtime enforcement are unverified"));
                assert!(!body.contains("Later preview"));
            }
            None => assert!(stored.is_none()),
        }
    }
    provider.await.unwrap();
}
