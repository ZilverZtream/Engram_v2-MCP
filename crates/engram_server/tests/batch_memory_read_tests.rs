//! Memory batches retain original sections, provenance and citation guards.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

struct Fixture {
    server: Engram,
    _owned: tempfile::TempDir,
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn body(result: &Value) -> Result<Value> {
    Ok(serde_json::from_str(
        result["content"][0]["text"].as_str().unwrap(),
    )?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_batch_preserves_sections_errors_shared_budget_and_live_hash_guards() -> Result<()> {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("m")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("source.ts"), "export const ownedMemory = 1;\n")?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: true,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        _owned: owned,
    };
    let indexed = fixture
        .server
        .handle_index_project(serde_json::from_value(json!({"directory":root,
        "project_name":"Owned memory batch","project_type":"general","wait":true}))?)
        .await?;
    let project = text(&indexed)
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let original = "Preserve the recorded approval exceptions and their source scope. 日本語 remains exact stored UTF-8.\n".repeat(80);
    let mut requests = Vec::new();
    let mut expected = Vec::new();
    for (owner, section, kind) in [
        (project, "owned-project-constraints", "decision"),
        ("__user__", "owned-user-constraints", "preference"),
    ] {
        fixture.server.handle_update_memory_bank(serde_json::from_value(json!({"project_id":owner,
            "section":section,"content":original,"author":"Owned memory author","kind":kind,"tags":["owned-proof"]}))?).await?;
        let args =
            json!({"project_id":owner,"section":section,"output_json":true,"max_tokens":65536});
        let read = fixture
            .server
            .handle_read_memory_bank(serde_json::from_value(args.clone())?)
            .await?;
        expected.push(serde_json::from_str::<Value>(text(&read))?);
        requests.push(
            json!({"request_id":section,"request":{"tool":"read_memory_bank","arguments":args}}),
        );
    }
    let missing = json!({"request_id":"missing","request":{"tool":"read_memory_bank","arguments":{
        "project_id":"missing-owned-memory-project","section":"none","output_json":true}}});
    let reads = json!([requests[0], missing, requests[1]]);
    let full = fixture
        .server
        .handle_batch_read(serde_json::from_value(
            json!({"requests":reads,"max_tokens":65536}),
        )?)
        .await?;
    let full: Value = serde_json::from_str(text(&full))?;
    ensure!(full["complete"] == true && full["results"][1]["status"] == "request_error");
    for (index, row) in [0, 2].into_iter().enumerate() {
        let actual = body(&full["results"][row]["response"])?;
        ensure!(actual["content"] == original);
        for field in ["identity", "content", "raw_content_hash"] {
            ensure!(actual[field] == expected[index][field]);
        }
        ensure!(actual["metadata"]["author"] == "Owned memory author");
        ensure!(actual["metadata"]["kind"] == expected[index]["metadata"]["kind"]);
    }
    let bounded = fixture
        .server
        .handle_batch_read(serde_json::from_value(
            json!({"requests":requests,"max_tokens":1024}),
        )?)
        .await?;
    ensure!(text(&bounded).len() <= 4096);
    let bounded: Value = serde_json::from_str(text(&bounded))?;
    let omitted: Vec<_> = bounded["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["status"] == "omitted_shared_response_budget")
        .collect();
    ensure!(
        omitted.len() == 2,
        "Large sections must be omitted whole, with their ordinary recovery requests"
    );
    for row in &omitted {
        ensure!(row["response"].is_null() && row["recovery"]["tool"] == "read_memory_bank");
        let read = fixture
            .server
            .handle_read_memory_bank(serde_json::from_value(
                row["recovery"]["arguments"].clone(),
            )?)
            .await?;
        ensure!(serde_json::from_str::<Value>(text(&read))?["content"] == original);
    }
    let changed = format!("{original}CHANGED_AFTER_CAPTURE\n");
    fixture.server.handle_update_memory_bank(serde_json::from_value(json!({"project_id":project,
        "section":"owned-project-constraints","content":changed,"author":"Owned later author","kind":"decision"}))?).await?;
    let action = &omitted[0]["recovery"];
    let live = fixture
        .server
        .handle_read_memory_bank(serde_json::from_value(action["arguments"].clone())?)
        .await?;
    ensure!(serde_json::from_str::<Value>(text(&live))?["content"] == changed);
    let guarded = json!({"request_id":"stale-quote","request":{"tool":"read_memory_bank","arguments":{
        "project_id":project,"section":"owned-project-constraints","citation":{"unit":"utf8_bytes","start":0,
            "expected_raw_hash":expected[0]["raw_content_hash"]}}}});
    let result = fixture
        .server
        .handle_batch_read(serde_json::from_value(
            json!({"requests":[guarded,requests[1]],"max_tokens":65536}),
        )?)
        .await?;
    let result: Value = serde_json::from_str(text(&result))?;
    ensure!(
        result["results"][0]["status"] == "request_error" && result["results"][1]["status"] == "ok"
    );
    Ok(())
}
