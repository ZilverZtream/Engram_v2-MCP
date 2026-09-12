#![allow(clippy::unwrap_used)]
use std::{collections::HashMap, sync::{Arc, Mutex}};
use engram_core::Config;
use engram_ml::DreamingEngine;
use engram_server::services::business_logic_service::{analyze_file_logic, render_file_purpose, render_compact_markdown, ProjectBusinessLogicReport};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SOURCE: &str = "Public Class Rules\n Public ReadOnly Property Limit As Integer\n Get\n Return stored\n End Get\n End Property\nEnd Class\n";

struct Mock {
    engine: DreamingEngine,
    summary: Arc<Mutex<String>>,
    member: Arc<Mutex<String>>,
    calls: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock { fn drop(&mut self) { self.task.abort(); } }
impl Mock {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let summary = Arc::new(Mutex::new(json!({"summary":"The getter returns stored.","member_refs":["Rules.Limit:2"]}).to_string()));
        let member = Arc::new(Mutex::new(json!({"purpose":"Returns stored.","business_rules":[]}).to_string()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (responses, members, captured) = (summary.clone(), member.clone(), calls.clone());
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let body_start = loop {
                    let mut bytes = [0;4096];
                    let count = socket.read(&mut bytes).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&bytes[..count]);
                    if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") { break pos + 4; }
                };
                let headers = String::from_utf8_lossy(&request[..body_start]).to_lowercase();
                let length: usize = headers.lines().find_map(|l| l.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                while request.len() < body_start + length {
                    let mut bytes = [0;4096];
                    let count = socket.read(&mut bytes).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&bytes[..count]);
                }
                let value: Value = serde_json::from_slice(&request[body_start..body_start+length]).unwrap();
                let is_summary = value.to_string().contains("Summarize only the visible behavior");
                captured.lock().unwrap().push(value);
                let answer = if is_summary { responses.lock().unwrap().clone() } else { members.lock().unwrap().clone() };
                let payload = json!({"choices":[{"message":{"content":answer}}]}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{payload}", payload.len()).as_bytes()).await.unwrap();
            }
        });
        let engine = DreamingEngine::with_config(&Config {
            llm_backend: "openai".into(), llm_model: Some("fixture-model".into()),
            llm_openai_api_key: Some("fixture-secret".into()), llm_openai_api_base: Some(format!("http://{address}/v1")),
            ..Default::default()
        });
        Self { engine, summary, member, calls, task }
    }
}

#[tokio::test]
async fn valid_summary_uses_bounded_source_and_safe_provenance() {
    let mock = Mock::new().await;
    let (file, _, _) = analyze_file_logic(&mock.engine, "Rules.vb", SOURCE, &HashMap::new()).await;
    assert_eq!(file.file_purpose, "The getter returns stored.");
    let evidence = file.file_purpose_evidence.as_ref().unwrap();
    assert_eq!(evidence.status, "inferred");
    assert_eq!(evidence.semantic_validation, "not_performed");
    assert_eq!((evidence.members_in_source, evidence.members_supplied), (1,1));
    let provenance = evidence.extraction_provenance.as_ref().unwrap();
    assert_eq!(provenance.requested_model.as_deref(), Some("fixture-model"));
    assert_eq!(provenance.prompt_version, Some("business-logic-file-v1"));
    assert!(provenance.resolved_model.is_none());
    assert!(provenance.extracted_at_utc.ends_with('Z'));
    let calls = mock.calls.lock().unwrap();
    let summary = calls.iter().find(|c| c.to_string().contains("Summarize only the visible behavior")).unwrap();
    assert_eq!(summary["max_tokens"], 512);
    assert!(summary.to_string().contains("Return stored"));
    assert!(summary.to_string().contains("copy these strings exactly"));
    assert!(!serde_json::to_string(&file).unwrap().contains("fixture-secret"));
    assert!(render_file_purpose(&file).contains("semantic validation: not_performed"));
}

#[tokio::test]
async fn truncated_empty_and_unreferenced_summaries_are_not_successful_strings() {
    let mock = Mock::new().await;
    for response in [
        "The class manages records while calculating",
        "{\"summary\":\"The getter returns",
        r#"{"summary":"The class is calculating","member_refs":["Rules.Limit:2"]}"#,
        r#"{"summary":"","member_refs":[]}"#,
        r#"{"summary":"Returns stored.","member_refs":["Unknown.Save:8"]}"#,
    ] {
        *mock.summary.lock().unwrap() = response.into();
        let (file, _, _) = analyze_file_logic(&mock.engine, "Rules.vb", SOURCE, &HashMap::new()).await;
        assert!(file.file_purpose.is_empty(), "{response}");
        let evidence = file.file_purpose_evidence.unwrap();
        assert_eq!(evidence.status, "incomplete");
        assert!(!evidence.warnings.is_empty());
        if response.contains("Unknown.Save") { assert!(evidence.warnings.iter().any(|w| w.contains("Unknown.Save:8"))); }
        assert_eq!(evidence.extraction_provenance.unwrap().origin, "failed_llm_attempt");
    }
}

#[tokio::test]
async fn absent_or_failed_member_evidence_never_triggers_summary_generation() {
    let mock = Mock::new().await;
    let (empty, _, _) = analyze_file_logic(&mock.engine, "Rules.vb", "Class Rules\nEnd Class", &HashMap::new()).await;
    assert_eq!(empty.file_purpose_evidence.unwrap().status, "unavailable");
    assert!(mock.calls.lock().unwrap().is_empty());
    *mock.member.lock().unwrap() = "invalid member JSON".into();
    let (failed, _, _) = analyze_file_logic(&mock.engine, "Rules.vb", SOURCE, &HashMap::new()).await;
    let evidence = failed.file_purpose_evidence.unwrap();
    assert_eq!(evidence.status, "unavailable");
    assert_eq!(evidence.members_failed, 1);
    assert!(evidence.extraction_provenance.is_none());
    assert!(!mock.calls.lock().unwrap().iter().any(|c| c.to_string().contains("Summarize only the visible behavior")));
}

#[tokio::test]
async fn cached_and_oversized_members_have_explicit_partial_coverage() {
    let mock = Mock::new().await;
    let second = " Public ReadOnly Property Other As Integer\n Get\n Return another\n End Get\n End Property\n";
    let source = SOURCE.replace("End Class", &format!("{second}End Class"));
    let (first, _, _) = analyze_file_logic(&mock.engine, "Rules.vb", &source, &HashMap::new()).await;
    let other = first.methods.iter().find(|m| m.method_name == "Other").unwrap();
    let cache = HashMap::from([("Rules.vb|Rules.Other|None".into(), other.outcome_evidence.as_ref().unwrap().analysis_fingerprint.clone())]);
    let (partial, _, skipped) = analyze_file_logic(&mock.engine, "Rules.vb", &source, &cache).await;
    assert_eq!(skipped,1);
    let evidence = partial.file_purpose_evidence.unwrap();
    assert_eq!(evidence.status,"incomplete");
    assert_eq!((evidence.members_in_source,evidence.members_supplied,evidence.members_skipped_cached),(2,1,1));
    // A large second source member is omitted whole, without clipping its closing tokens.
    let big = second.replace(" Return another", &format!("{} Return another", " ' bounded fixture padding\n".repeat(400)));
    let source = SOURCE.replace("End Class", &format!("{big}End Class"));
    let (partial, _, _) = analyze_file_logic(&mock.engine, "Rules.vb", &source, &HashMap::new()).await;
    let evidence = partial.file_purpose_evidence.unwrap();
    assert_eq!(evidence.status,"incomplete");
    assert_eq!(evidence.members_omitted_budget,1);
    for call in mock.calls.lock().unwrap().iter().filter(|c| c.to_string().contains("Summarize only the visible behavior")) {
        assert!(call.to_string().len() < 30 * 1024);
    }
}

#[tokio::test]
async fn legacy_summary_provenance_is_unknown() {
    let (mut file, _, _) = analyze_file_logic(&DreamingEngine::new(), "Rules.vb", SOURCE, &HashMap::new()).await;
    file.file_purpose = "Legacy summary.".into();
    file.file_purpose_evidence = None;
    let text = render_file_purpose(&file);
    assert!(text.contains("Legacy summary."));
    assert!(text.contains("unknown (not recorded)"));
    assert!(!text.contains("inferred"));
}

#[tokio::test]
async fn all_cached_file_preserves_unavailable_coverage_in_project_markdown() {
    let mock = Mock::new().await;
    let (first, _, _) = analyze_file_logic(&mock.engine, "Rules.vb", SOURCE, &HashMap::new()).await;
    let cache = HashMap::from([("Rules.vb|Rules.Limit|None".into(), first.methods[0].outcome_evidence.as_ref().unwrap().analysis_fingerprint.clone())]);
    mock.calls.lock().unwrap().clear();
    let (file, analyzed, skipped) = analyze_file_logic(&mock.engine, "Rules.vb", SOURCE, &cache).await;
    assert_eq!((analyzed,skipped),(0,1));
    assert!(mock.calls.lock().unwrap().is_empty());
    let evidence = file.file_purpose_evidence.as_ref().unwrap();
    assert_eq!(evidence.status,"unavailable");
    assert_eq!((evidence.members_in_source,evidence.members_supplied,evidence.members_skipped_cached),(1,0,1));
    let report = ProjectBusinessLogicReport { project_id:"fixture".into(), files_analyzed:1, methods_analyzed:0,
        methods_skipped_cached:1, llm_failures:0, file_summaries:vec![file] };
    let rendered = render_compact_markdown(&report);
    assert!(rendered.contains("unavailable"));
    assert!(rendered.contains("cached: 1"));
}

/// Explicit operator-only probe. No server, project index, or database is instantiated.
/// Normal CI never calls a provider. Configuration must come from a trusted local operator.
#[tokio::test]
#[ignore = "requires explicit trusted config and external receipt directory; calls the selected provider"]
async fn operator_selected_model_synthetic_file_purpose_probe() {
    let config_path = std::env::var("ENGRAM_FILE_PURPOSE_PROBE_CONFIG").expect("trusted config path required");
    let output = std::path::PathBuf::from(std::env::var("ENGRAM_FILE_PURPOSE_PROBE_OUTPUT").expect("external output path required"));
    assert!(output.is_dir());
    let config: Config = serde_json::from_slice(&std::fs::read(config_path).unwrap()).unwrap();
    let engine = DreamingEngine::with_config(&config);
    assert_eq!(engine.text_generation_identity().and_then(|(_,model)| model),Some("openai/gpt-5.6-luna"));
    let source = "Public Class Sample\n Private stored As Integer = -1\n Private recorded As Date\n Public Property Amount As Integer\n Get\n If stored >= 0 Then\n Return stored\n Else\n Return 12\n End If\n End Get\n Set(value As Integer)\n stored = value\n End Set\n End Property\n Public ReadOnly Property Caption As String\n Get\n Return \"Ready\"\n End Get\n End Property\n Public ReadOnly Property DisplayDate As String\n Get\n If recorded > #1/1/2000# Then\n Return recorded.ToString(\"yyyy-MM-dd\")\n Else\n Return \"-\"\n End If\n End Get\n End Property\nEnd Class\n";
    std::fs::write(output.join("Sample.vb"), source).unwrap();
    let (file, _, _) = analyze_file_logic(&engine,"Sample.vb",source,&HashMap::new()).await;
    // Save failures too: structural assertions are not semantic certification.
    std::fs::write(output.join("analysis.json"),serde_json::to_vec_pretty(&file).unwrap()).unwrap();
    std::fs::write(output.join("summary.md"),render_file_purpose(&file)).unwrap();
    assert_eq!(file.methods.len(),3);
    assert!(file.methods.iter().all(|m| m.parse_diagnostic.is_empty()));
    let evidence = file.file_purpose_evidence.unwrap();
    assert_eq!(evidence.status,"inferred");
    assert_eq!(evidence.semantic_validation,"not_performed");
    assert!(evidence.extraction_provenance.unwrap().resolved_model.is_none());
}
