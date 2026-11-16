#![allow(clippy::unwrap_used)]
//! Durable note content must remain discoverable when search publication fails.
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

const PID: &str = "memory-recovery";

// Fields drop in declaration order: close all registry/search owners before
// TempDir removes the database files, including on an assertion panic.
struct Fixture {
    engram: Engram,
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new(provider: Option<&str>, unavailable_index: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let data = temp.path().join("data");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        if unavailable_index {
            let project = data.join("projects").join(PID);
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(project.join("tantivy"), b"owned unavailable index path").unwrap();
        }
        let config = Config {
            data_dir: data,
            allowed_roots: vec![source.clone()],
            embedding_backend: if provider.is_some() {
                "ollama"
            } else {
                "fts_only"
            }
            .into(),
            ollama_url: Some(provider.unwrap_or("http://127.0.0.1:1").into()),
            embedding_model: Some("owned-memory-fixture".into()),
            ollama_embed_dim: Some(4),
            embedding_request_timeout_secs: 2,
            ..Default::default()
        };
        let (state, _) = AppState::new(config).unwrap();
        state
            .registry
            .put_project(&ProjectRecord {
                project_id: PID.into(),
                project_name: "Memory recovery".into(),
                project_type: "general".into(),
                directory: source.to_string_lossy().into(),
                created_at_ms: 1,
                updated_at_ms: 1,
                reindex_required_since_ms: None,
            })
            .unwrap();
        Self {
            engram: Engram::new(state),
            _temp: temp,
        }
    }

    async fn write(&self, content: &str, append: bool) -> CallToolResult {
        self.engram
            .handle_update_memory_bank(
                serde_json::from_value(json!({
                    "project_id":PID,"section_id":"workflow","section":"Workflow",
                    "content":content,"append":append
                }))
                .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn read(&self, section: &str) -> String {
        let result = self
            .engram
            .handle_read_memory_bank(
                serde_json::from_value(json!({
                    "project_id":PID,"section":section
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        result
            .content
            .iter()
            .filter_map(|part| part.as_text().map(|text| text.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn outcome(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true));
    result
        .content
        .iter()
        .filter_map(|part| part.as_text())
        .filter_map(|text| serde_json::from_str::<Value>(&text.text).ok())
        .find(|value| value["status"] == "stored_search_incomplete")
        .unwrap()
}

struct RejectingProvider {
    url: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl RejectingProvider {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = stop.clone();
        let worker = std::thread::spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                socket
                    .set_read_timeout(Some(Duration::from_millis(250)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buffer = [0u8; 4096];
                    match socket.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => request.extend_from_slice(&buffer[..count]),
                    }
                    if request.len() > 64 * 1024 {
                        break;
                    }
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                line.split_once(':')
                                    .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let body = r#"{"error":"owned embedding outage"}"#;
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for RejectingProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(feature = "vector")]
#[tokio::test]
async fn embedding_outage_reports_saved_append_and_import_with_existing_repair_action() {
    let provider = RejectingProvider::start();
    let fixture = Fixture::new(Some(&provider.url), false);
    fixture.write("Original accepted guidance.", false).await;
    let result = fixture.write("Appended SQL guidance.", true).await;
    let saved = outcome(&result);
    assert_eq!(saved["stored"], true);
    assert_eq!(saved["retry_write"], false);
    assert_eq!(saved["lexical_indexing"], "committed");
    assert!(
        saved["indexing_error"]
            .as_str()
            .unwrap()
            .contains("400 Bad Request")
    );
    let actions = saved["next_tools"].as_array().unwrap();
    assert!(
        actions
            .iter()
            .any(|action| action["tool"] == "read_memory_bank"
                && action["arguments"] == json!({"project_id":PID,"section":"workflow"}))
    );
    assert!(
        actions
            .iter()
            .any(|action| action["tool"] == "repair_project"
                && action["arguments"] == json!({"project_id":PID,"scope":"vector_only"}))
    );
    let read = fixture.read("workflow").await;
    assert!(read.contains("Original accepted guidance."));
    assert_eq!(read.matches("Appended SQL guidance.").count(), 1);
    let imported = fixture
        .engram
        .handle_import_memory_bank(
            serde_json::from_value(json!({
                "project_id":PID,"section_id":"imported","markdown":"Imported TypeScript guidance."
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(outcome(&imported)["stored"], true);
    assert!(
        fixture
            .read("imported")
            .await
            .contains("Imported TypeScript guidance.")
    );
}

#[tokio::test]
async fn unavailable_search_still_reports_saved_note_without_promising_vector_repair() {
    let fixture = Fixture::new(None, true);
    let saved = outcome(&fixture.write("Durable WebForms guidance.", false).await);
    assert_eq!(saved["stored"], true);
    assert_eq!(saved["lexical_indexing"], "unknown");
    let actions = saved["next_tools"].as_array().unwrap();
    assert!(
        !actions
            .iter()
            .any(|action| action["tool"] == "repair_project")
    );
    assert!(
        actions
            .iter()
            .any(|action| action["tool"] == "project_health"
                && action["arguments"] == json!({"project_id":PID}))
    );
    assert!(
        fixture
            .read("workflow")
            .await
            .contains("Durable WebForms guidance.")
    );
}

#[tokio::test]
async fn healthy_write_and_prewrite_validation_keep_their_original_outcomes() {
    let fixture = Fixture::new(None, false);
    let result = fixture.write("Accepted guidance.", false).await;
    assert_ne!(result.is_error, Some(true));
    assert!(
        result.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Updated memory_bank: workflow")
    );
    let invalid = fixture.engram.handle_update_memory_bank(serde_json::from_value(json!({
        "project_id":PID,"section_id":"invalid","section":"Invalid","kind":"unsupported","content":"Do not persist"
    })).unwrap()).await;
    assert!(invalid.is_err());
    assert!(
        fixture
            .read("workflow")
            .await
            .contains("Accepted guidance.")
    );
}
