#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mock_helper_evidence_is_persisted_and_blocks_unverified_return_oracle() {
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
                json!({"purpose":"Reads an item","data_flow":"Reads skippedChoice.","business_rules":[{"when":"invalid","then":"calls the rejection helper and returns Nothing","source_line":3,"refs":[]}],"steps":["calls the rejection helper and returns Nothing","returns a projected list"],"error_handling":"calls the rejection helper and returns Nothing","side_effects_detail":""}).to_string()
            };
            let payload = json!({"choices":[{"message":{"content":answer}}]}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",payload.len()).as_bytes()).await.unwrap();
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let caller = "Public Class Caller\n Public Function ReadItem(invalid As Boolean, items As System.Collections.Generic.IEnumerable(Of Object), skippedChoice As Boolean) As Object\n  If invalid Then\n   Global.Helpers.Reject()\n   Return Nothing\n  End If\n  Return System.Linq.Enumerable.ToList(System.Linq.Enumerable.Select(items, Function(item) item))\n End Function\nEnd Class\n";
    let helper = "Public Class Helpers\n Public Shared Sub Reject()\n  Throw New InvalidOperationException()\n End Sub\nEnd Class\n";
    std::fs::write(root.join("Caller.vb"), caller).unwrap();
    std::fs::write(root.join("Helpers.vb"), helper).unwrap();
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
    engram.index_project(Parameters(serde_json::from_value(json!({"directory":root,"project_name":"outcomes","project_type":"dotnet_webforms_vb","wait":true})).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let request = json!({"project_id":pid,"file_path":"Caller.vb","method_name":"ReadItem","output_json":true});
    let raw = engram
        .handle_analyze_business_logic(serde_json::from_value(request.clone()).unwrap())
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&raw.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(
        result["outcome_evidence"]["dependencies"][0]["evidence_status"], "source_verified",
        "{result}"
    );
    assert_eq!(
        result["outcome_evidence"]["dependencies"][0]["outcome_status"],
        "normal_completion_unverified"
    );
    assert_eq!(result["semantic_validation"], "not_performed");
    assert_eq!(result["rule_source_diagnostics"]["version"], "displayed-rule-source-diagnostics-v1");
    assert_eq!(result["rule_source_diagnostics"]["rules"][0]["displayed_rule_digest"], engram_server::services::business_rule_diagnostics::displayed_rule_digest(result["business_rules"][0].as_str().unwrap()));
    assert_eq!(result["rule_source_diagnostics"]["rules"][0]["source_rule_ordinal"], 1);
    let initial_fingerprint = result["outcome_evidence"]["analysis_fingerprint"].clone();
    let captured = prompts.lock().unwrap().join("\n");
    assert!(
        captured.contains("Throw New InvalidOperationException"),
        "{captured}"
    );
    assert!(
        captured.contains("helper 2:"),
        "helper anchors remain separate"
    );
    assert!(captured.contains("preserving aliases such as row.Total"));
    assert!(!captured.contains("DB table.column"));
    assert!(!captured.contains("database columns (Orders.Total)"));
    assert!(captured.contains("Output contract for EVERY field"));
    assert!(captured.contains("Reconcile data_flow and every other field with supplied parameter zero-occurrence facts"));
    assert!(captured.contains("Do not list declared parameters as reads"));
    assert!(captured.contains("zero body identifier occurrences"));
    assert!(captured.contains("skippedChoice"));
    assert_eq!(result["data_flow"], "Reads skippedChoice.", "raw model inference must remain unchanged");
    assert!(captured.contains("if the call completes normally and execution reaches the return"));
    assert!(captured.contains("Observed source order: call at caller line 4, following return statement at caller line 5"));
    assert!(captured.contains("Unexamined-tail output contract at caller line 7"));
    assert!(captured.contains("Completion of an earlier call is insufficient to assert projection"));
    assert_eq!(result["outcome_evidence"]["reaching_context"]["unexamined_tail_start_line"], 7);
    // Unsupported mock prose stays raw; this does not claim semantic detection.
    assert_eq!(result["steps"][0], "calls the rejection helper and returns Nothing");
    assert_eq!(result["steps"][1], "returns a projected list");
    assert_eq!(result["error_handling"], "calls the rejection helper and returns Nothing");
    assert!(result["business_rules"][0].as_str().unwrap().contains("calls the rejection helper and returns Nothing"));
    let matrix_request = json!({"project_id":pid,"files":["Caller.vb"]});
    let matrix = engram
        .handle_derive_test_matrix(serde_json::from_value(matrix_request.clone()).unwrap())
        .await
        .unwrap();
    let text = &matrix.content[0].as_text().unwrap().text;
    assert!(text.contains("blocked_pending_helper_outcome"), "{text}");
    assert!(text.contains("expected outcome BLOCKED"), "{text}");
    assert!(text.contains("normal_completion_unverified"));
    assert!(text.contains("calls the rejection helper and returns Nothing"), "raw rule remains recoverable rather than rewritten");
    std::fs::write(
        root.join("Helpers.vb"),
        helper.replace("Throw New InvalidOperationException()", "Return"),
    )
    .unwrap();
    let matrix = engram
        .handle_derive_test_matrix(serde_json::from_value(matrix_request).unwrap())
        .await
        .unwrap();
    assert!(
        matrix.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("stale_or_unavailable")
    );
    let raw = engram
        .handle_analyze_business_logic(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&raw.content[0].as_text().unwrap().text).unwrap();
    assert_ne!(
        result["outcome_evidence"]["analysis_fingerprint"],
        initial_fingerprint
    );
    assert_eq!(
        result["outcome_evidence"]["dependencies"][0]["evidence_status"],
        "unavailable"
    );
    server.abort();
}
