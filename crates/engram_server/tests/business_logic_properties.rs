#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn property_only_file_persists_verifies_and_failed_reanalysis_preserves_evidence() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured = prompts.clone();
    let fail_model = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failed_response = fail_model.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let body_start = loop {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..body_start]).to_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            while request.len() < body_start + length {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
            }
            let value: Value =
                serde_json::from_slice(&request[body_start..body_start + length]).unwrap();
            captured.lock().unwrap().push(value.to_string());
            let answer = if failed_response.load(std::sync::atomic::Ordering::SeqCst) {
                "invalid JSON".to_string()
            } else {
                json!({"purpose":"Provides the configured limit","business_rules":["Getter returns the stored limit."],"steps":[],"side_effects_detail":"Setter changes stored value."}).to_string()
            };
            let payload = json!({"choices":[{"message":{"content":answer}}]}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",payload.len()).as_bytes()).await.unwrap();
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Public Class Rules\n Public Property Limit As Integer\n Get\n Return stored\n End Get\n Set(value As Integer)\n stored = value\n End Set\n End Property\nEnd Class\n";
    std::fs::write(root.join("Rules.vb"), source).unwrap();
    let config = Config {
        allowed_roots: vec![root.clone()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        llm_backend: "openai".into(),
        llm_model: Some("fixture".into()),
        llm_openai_api_key: Some("fixture-key".into()),
        llm_openai_api_base: Some(format!("http://{address}/v1")),
        ..Default::default()
    };
    let (state, _) = AppState::new(config).unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(serde_json::from_value(json!({"directory":root,"project_name":"properties","project_type":"dotnet_webforms_vb","wait":true})).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let request = json!({"project_id":pid,"file_path":"Rules.vb","output_json":true});
    let response = engram
        .handle_analyze_business_logic(serde_json::from_value(request.clone()).unwrap())
        .await
        .unwrap();
    let response: Value =
        serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(response["methods"].as_array().unwrap().len(), 1);
    assert_eq!(response["methods"][0]["member_kind"], "property");
    assert_eq!(response["methods"][0]["fqn"], "Rules.Limit");
    let provenance = &response["methods"][0]["extraction_provenance"];
    assert_eq!(provenance["origin"], "llm");
    assert_eq!(provenance["provider"], "openai");
    assert_eq!(provenance["requested_model"], "fixture");
    assert!(provenance["resolved_model"].is_null());
    assert!(
        provenance["extracted_at_utc"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    assert!(
        provenance["prompt_version"]
            .as_str()
            .unwrap()
            .starts_with("business-logic-member-")
    );
    assert!(
        prompts
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.contains("Get and Set as distinct entry points"))
    );
    let search = state.get_project_cached(&pid).unwrap().search;
    let docs = search
        .list_docs_in_namespace(&pid, "business_logic")
        .unwrap();
    assert_eq!(docs.len(), 1);
    let original_doc = search
        .get_doc_by_doc_id(&pid, "business_logic", 0, &docs[0].doc_id)
        .unwrap()
        .unwrap()
        .2;
    assert!(original_doc.contains("**Member kind**: property"));
    assert!(original_doc.contains("\"requested_model\":\"fixture\""));
    assert!(!original_doc.contains("fixture-key"));
    assert!(!original_doc.contains(&address.to_string()));
    let query = json!({"project_id":pid,"query":"configured limit","top_k":5});
    let found = engram
        .handle_query_business_logic(serde_json::from_value(query.clone()).unwrap())
        .await
        .unwrap();
    assert!(
        found.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("VERIFIED_METHOD_HASH")
    );
    let matrix = engram
        .handle_derive_test_matrix(
            serde_json::from_value(json!({"project_id":pid,"files":["Rules.vb"]})).unwrap(),
        )
        .await
        .unwrap();
    let matrix = &matrix.content[0].as_text().unwrap().text;
    assert!(
        matrix.contains("Getter returns the stored limit."),
        "{matrix}"
    );
    assert!(matrix.contains("VERIFIED_METHOD_HASH"), "{matrix}");
    std::fs::write(
        root.join("Rules.vb"),
        source.replace("stored = value", "stored = 0"),
    )
    .unwrap();
    let stale = engram
        .handle_query_business_logic(serde_json::from_value(query.clone()).unwrap())
        .await
        .unwrap();
    assert!(stale.content[0].as_text().unwrap().text.contains("STALE:"));
    std::fs::write(root.join("Rules.vb"), source.replace("End Property", "")).unwrap();
    let failed = engram
        .handle_analyze_business_logic(serde_json::from_value(request.clone()).unwrap())
        .await
        .unwrap();
    assert!(
        failed.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("INCOMPLETE:")
    );
    let preserved = search
        .get_doc_by_doc_id(&pid, "business_logic", 0, &docs[0].doc_id)
        .unwrap()
        .unwrap()
        .2;
    assert_eq!(preserved, original_doc);
    std::fs::write(root.join("Rules.vb"), source).unwrap();
    fail_model.store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = engram
        .handle_analyze_business_logic(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    let failed: Value = serde_json::from_str(&failed.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(
        failed["methods"][0]["extraction_provenance"]["origin"],
        "failed_llm_attempt"
    );
    assert!(
        !failed["methods"][0]["parse_diagnostic"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        search
            .get_doc_by_doc_id(&pid, "business_logic", 0, &docs[0].doc_id)
            .unwrap()
            .unwrap()
            .2,
        original_doc
    );
    server.abort();
}
