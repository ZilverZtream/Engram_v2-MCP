#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_ml::DreamingEngine;
use engram_server::services::business_logic_service::analyze_method_logic;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn analyze_with_responses(responses: Vec<&'static str>) -> (String, String, Vec<u64>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut budgets = vec![];
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![];
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
                .find_map(|l| l.strip_prefix("content-length:"))
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
            let body: Value =
                serde_json::from_slice(&request[body_start..body_start + length]).unwrap();
            budgets.push(body["max_tokens"].as_u64().unwrap());
            let payload = json!({"choices":[{"message":{"content":response}}]}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}", payload.len()).as_bytes()).await.unwrap();
        }
        budgets
    });
    let engine = DreamingEngine::with_config(&Config {
        llm_backend: "openai".into(),
        llm_model: Some("test-model".into()),
        llm_openai_api_key: Some("test-key".into()),
        llm_openai_api_base: Some(format!("http://{address}/v1")),
        ..Default::default()
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        analyze_method_logic(
            &engine,
            "Sample.vb",
            "Save",
            "Sub Save()\n model.Save()\nEnd Sub",
            "Sample",
            "vb",
            1,
        ),
    )
    .await
    .unwrap();
    let budgets = tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    (result.purpose, result.parse_diagnostic, budgets)
}

#[tokio::test]
async fn truncated_analysis_recovers_with_larger_budget() {
    let (purpose, diagnostic, budgets) = analyze_with_responses(vec![
        "{\"purpose\":\"Save the model\",\"business_rules\":[",
        "{\"purpose\":\"Save the model\",\"business_rules\":[]}",
    ])
    .await;
    assert_eq!(purpose, "Save the model");
    assert!(diagnostic.is_empty());
    assert_eq!(budgets, [3072, 8192]);
}

#[tokio::test]
async fn failed_retry_preserves_diagnostic_and_stops() {
    let broken = "{\"purpose\":\"Save the model\",\"business_rules\":[";
    let (_, diagnostic, budgets) = analyze_with_responses(vec![broken, "still invalid"]).await;
    assert_eq!(diagnostic, broken);
    assert_eq!(budgets, [3072, 8192]);
}

#[tokio::test]
async fn complete_analysis_needs_no_retry() {
    let (purpose, diagnostic, budgets) = analyze_with_responses(vec![
        "{\"purpose\":\"Save the model\",\"business_rules\":[]}",
    ])
    .await;
    assert_eq!(purpose, "Save the model");
    assert!(diagnostic.is_empty());
    assert_eq!(budgets, [3072]);
}

#[tokio::test]
async fn purpose_missing_analysis_recovers_with_larger_budget() {
    let (purpose, diagnostic, budgets) = analyze_with_responses(vec![
        "{}",
        "{\"purpose\":\"Save the model\",\"business_rules\":[]}",
    ])
    .await;
    assert_eq!(purpose, "Save the model");
    assert!(diagnostic.is_empty());
    assert_eq!(budgets, [3072, 8192]);
}

#[tokio::test]
async fn purpose_missing_analysis_keeps_failure_diagnostic() {
    let (purpose, diagnostic, budgets) = analyze_with_responses(vec!["{}", "{}"]).await;
    assert!(purpose.is_empty());
    assert!(!diagnostic.is_empty());
    assert_eq!(budgets, [3072, 8192]);
}

#[tokio::test]
async fn empty_model_response_recovers_with_larger_budget() {
    let (purpose, diagnostic, budgets) = analyze_with_responses(vec![
        "",
        "{\"purpose\":\"Save the model\",\"business_rules\":[]}",
    ])
    .await;
    assert_eq!(purpose, "Save the model");
    assert!(diagnostic.is_empty());
    assert_eq!(budgets, [3072, 8192]);
}

#[tokio::test]
async fn repeated_empty_model_response_is_an_explicit_failure() {
    let (purpose, diagnostic, budgets) = analyze_with_responses(vec!["", ""]).await;
    assert!(purpose.is_empty());
    assert!(!diagnostic.is_empty());
    assert_eq!(budgets, [3072, 8192]);
}

#[test]
fn malformed_analysis_markdown_does_not_present_a_successful_purpose() {
    use engram_server::services::business_logic_service::{
        parse_llm_response, render_method_as_doc,
    };
    let analysis = parse_llm_response("{", "Sample.vb", "Save", "Sample.Save", "hash");
    let report = render_method_as_doc(&analysis);
    assert!(report.contains("failed or incomplete"));
    assert!(!report.contains("**Purpose**"));
}

#[test]
fn parsed_rules_do_not_imply_verified_semantics() {
    use engram_server::services::business_logic_service::{parse_llm_response, render_method_as_doc};
    let mut analysis = parse_llm_response(
        r#"{"purpose":"Save the invoice","business_rules":["Reject negative amounts"]}"#,
        "Invoice.vb", "Save", "Invoice.Save", "source-hash",
    );
    assert_eq!(analysis.confidence, "unverified");
    assert!(analysis.parse_diagnostic.is_empty());
    assert!(render_method_as_doc(&analysis).contains("semantic accuracy has not been independently verified"));
    // Legacy results with an empty confidence field require the same qualification.
    analysis.confidence.clear();
    assert!(render_method_as_doc(&analysis).contains("model-inferred"));
    analysis.confidence = "deterministic".into();
    assert!(!render_method_as_doc(&analysis).contains("model-inferred"));
}
