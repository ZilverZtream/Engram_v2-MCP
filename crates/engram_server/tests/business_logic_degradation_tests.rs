use engram_core::Config;
use engram_ml::DreamingEngine;
use engram_server::services::business_logic_service::{
    analyze_file_logic, analyze_method_logic, render_file_purpose, render_method_as_doc,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const BODY: &str = "Public Function Save() As Integer\n ' Return secretComment\n Dim text = \"Return ignoredLiteral\"\n If ready Then\n  Return value\n End If\n Return 0\nEnd Function";

struct Provider {
    engine: DreamingEngine,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    stop: CancellationToken,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Provider {
    async fn new(status: u16, hang: bool) -> Self {
        Self::with_summary_status(status, hang, None).await
    }

    async fn with_summary_status(status: u16, hang: bool, summary_status: Option<u16>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let stop = CancellationToken::new();
        let (count, signal, shutdown) = (calls.clone(), entered.clone(), stop.clone());
        let worker = tokio::spawn(async move {
            loop {
                let mut socket = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    result = listener.accept() => result.unwrap().0,
                };
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let n = tokio::select! {
                        _ = shutdown.cancelled() => return,
                        result = socket.read(&mut buffer) => result.unwrap(),
                    };
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    assert!(request.len() < 128 * 1024);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = header
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse::<usize>()
                            .unwrap();
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                signal.notify_one();
                if hang {
                    shutdown.cancelled().await;
                    return;
                }
                let status = if String::from_utf8_lossy(&request)
                    .contains("Summarize only the visible behavior")
                {
                    summary_status.unwrap_or(status)
                } else {
                    status
                };
                let payload = if status == 200 {
                    json!({"choices":[{"message":{"content":"{\"purpose\":\"Returns a source value.\",\"business_rules\":[]}"}}]}).to_string()
                } else {
                    json!({"error":{"message":"private-provider-secret https://private.example/?key=hidden"}}).to_string()
                };
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nRetry-After: 0\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let engine = DreamingEngine::with_config(&Config {
            llm_backend: "openai".into(),
            llm_model: Some("fixture-model".into()),
            llm_openai_api_key: Some("fixture-key-never-public".into()),
            llm_openai_api_base: Some(format!("http://{address}/v1")),
            ..Default::default()
        });
        Self {
            engine,
            calls,
            entered,
            stop,
            worker: Some(worker),
        }
    }
    async fn close(&mut self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            worker.await.unwrap();
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

#[tokio::test]
async fn provider_failures_keep_safe_reasons_and_do_not_start_json_budget_retry() {
    for (status, reason, expected_calls) in [
        (401, "authentication_failed", 1),
        (503, "upstream_failure", 3),
        (429, "rate_limited", 3),
    ] {
        let mut provider = Provider::new(status, false).await;
        let result = analyze_method_logic(
            &provider.engine,
            "Rules.vb",
            "Save",
            BODY,
            "Rules",
            "vb",
            10,
        )
        .await;
        provider.close().await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), expected_calls);
        let degraded = result.degradation.as_ref().unwrap();
        assert_eq!(degraded.reason, reason);
        assert_eq!(degraded.status_code, Some(status));
        let source = degraded.deterministic_summary.as_ref().unwrap();
        assert_eq!(source.content_hash, result.content_hash);
        assert_eq!(source.start_line, 10);
        assert!(
            source
                .observations
                .iter()
                .any(|line| line.line == 14 && line.source.trim() == "Return value")
        );
        assert!(!source.observations.iter().any(|line| line.line == 11));
        assert!(result.purpose.is_empty() && result.business_rules.is_empty());
        assert!(!result.parse_diagnostic.is_empty());
        assert_eq!(
            result.extraction_provenance.as_ref().unwrap().origin,
            "failed_llm_attempt"
        );
        let output = serde_json::to_string(&result).unwrap();
        for secret in [
            "private-provider-secret",
            "private.example",
            "fixture-key-never-public",
        ] {
            assert!(!output.contains(secret));
        }
        assert!(render_method_as_doc(&result).contains("Deterministic source summary"));
    }
}

#[tokio::test]
async fn request_deadline_returns_source_fallback_without_a_second_generation_attempt() {
    let mut provider = Provider::new(200, true).await;
    let engine = provider.engine.clone();
    let request = tokio::spawn(async move {
        analyze_method_logic(&engine, "Rules.vb", "Save", BODY, "Rules", "vb", 10).await
    });
    tokio::time::timeout(Duration::from_secs(5), provider.entered.notified())
        .await
        .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(121)).await;
    let result = request.await.unwrap();
    tokio::time::resume();
    provider.close().await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.degradation.unwrap().reason, "timeout");
    assert!(result.purpose.is_empty());
}

#[tokio::test]
async fn disabled_provider_and_failed_file_summary_expose_separate_source_observations() {
    let engine = DreamingEngine::with_config(&Config {
        llm_backend: "none".into(),
        ..Default::default()
    });
    let content = format!("Public Class Rules\n{BODY}\nEnd Class");
    let (file, _, _) = analyze_file_logic(&engine, "Rules.vb", &content, &Default::default()).await;
    let evidence = file.file_purpose_evidence.as_ref().unwrap();
    assert!(
        file.file_purpose.is_empty(),
        "fallback is not inferred file purpose"
    );
    assert_eq!(evidence.members_failed, 1);
    assert_eq!(
        evidence.degradation.as_ref().unwrap().reason,
        "provider_not_configured"
    );
    assert!(render_file_purpose(&file).contains("Deterministic source summary"));
    assert!(file.methods[0].business_rules.is_empty());
    assert!(!file.methods[0].parse_diagnostic.is_empty());

    let mut provider = Provider::new(401, false).await;
    let (failed, _, _) =
        analyze_file_logic(&provider.engine, "Rules.vb", &content, &Default::default()).await;
    provider.close().await;
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "no summary request for failed members"
    );
    assert_eq!(
        failed
            .file_purpose_evidence
            .unwrap()
            .degradation
            .unwrap()
            .reason,
        "authentication_failed"
    );

    let mut provider = Provider::with_summary_status(200, false, Some(401)).await;
    let (failed_summary, _, _) =
        analyze_file_logic(&provider.engine, "Rules.vb", &content, &Default::default()).await;
    provider.close().await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(failed_summary.methods[0].degradation.is_none());
    assert!(failed_summary.file_purpose.is_empty());
    assert_eq!(
        failed_summary
            .file_purpose_evidence
            .unwrap()
            .degradation
            .unwrap()
            .reason,
        "authentication_failed"
    );
}

#[tokio::test]
async fn successful_model_output_has_no_fallback_and_long_source_is_not_clipped() {
    let mut provider = Provider::new(200, false).await;
    let result = analyze_method_logic(
        &provider.engine,
        "Rules.vb",
        "Save",
        BODY,
        "Rules",
        "vb",
        10,
    )
    .await;
    provider.close().await;
    assert!(result.degradation.is_none());
    assert!(result.parse_diagnostic.is_empty());
    assert_eq!(result.extraction_provenance.unwrap().origin, "llm");
    let engine = DreamingEngine::with_config(&Config {
        llm_backend: "none".into(),
        ..Default::default()
    });
    let body = format!(
        "Function Value() As Integer\n Dim huge = {}\n{}\nEnd Function",
        "x".repeat(500),
        (0..20)
            .map(|n| format!(" Return {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let result = analyze_method_logic(&engine, "Rules.vb", "Value", &body, "Rules", "vb", 1).await;
    let source = result.degradation.unwrap().deterministic_summary.unwrap();
    assert_eq!(source.observations.len(), 8);
    assert_eq!(source.omitted_observations, 13);
    assert!(
        source
            .observations
            .iter()
            .all(|row| row.source.len() <= 240 && !row.source.contains("huge"))
    );
}
