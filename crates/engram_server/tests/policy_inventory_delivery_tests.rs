//! Recover every selected policy row and its original ownership qualifications.
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram, read_report_delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};
use std::path::Path;

fn request(arguments: Value) -> CallToolRequestParam {
    serde_json::from_value(json!({"name":"list_repo_rules","arguments":arguments})).unwrap()
}

fn recover(data: &Path, first: &CallToolResult) -> String {
    let registry = Engram::tool_registry().list_all();
    let manifest: Value = serde_json::from_str(&first.content[0].as_text().unwrap().text).unwrap();
    let delivery = &manifest["response_delivery"];
    let mut next = delivery["next_call"].clone();
    let mut exact = String::new();
    loop {
        assert!(next["arguments"].get("ownership_inventory").is_none());
        assert!(next["arguments"].get("source_paths").is_none());
        let mut call = request(next["arguments"].clone());
        let saved = read_report_delivery::prepare(&mut call, &registry)
            .unwrap()
            .unwrap();
        let result = saved.recover(data).unwrap().unwrap();
        let text = &result.content[0].as_text().unwrap().text;
        assert!(text.len() <= 8192);
        let page: Value = serde_json::from_str(text).unwrap();
        assert_eq!(page["start_byte"], exact.len());
        exact.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        next = page["next_call"].clone();
    }
    assert_eq!(
        blake3::hash(exact.as_bytes()).to_hex().as_str(),
        delivery["response_id"].as_str().unwrap()
    );
    exact
}

#[tokio::test]
async fn complete_lists_and_ownership_rows_survive_policy_and_source_changes() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    let data = temporary.path().join("data");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        source.join("Rules.md"),
        "Preserve the authored SQL exception.\n",
    )
    .unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: data.clone(),
        allowed_roots: vec![source.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: "policy-delivery".into(),
            project_name: "Policy inventory".into(),
            project_type: "general".into(),
            directory: source.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    let engram = Engram::new(state.clone());
    let bodies: Vec<_> = (0..24)
        .map(|id| {
            format!(
                "Rule {id:02}. {}\nException: preserve the authored SQL exception.",
                "Retain scoped VB and TypeScript guidance. ".repeat(70)
            )
        })
        .collect();
    for (id, body) in bodies.iter().enumerate() {
        engram
            .handle_add_repo_rule(
                serde_json::from_value(json!({
                    "project_id":"policy-delivery","rule_id":format!("rule-{id:02}"),
                    "file_pattern":"Site/App_Code/**/*.vb","rule_text":body,
                    "priority":7,"introduced_at":"2026-11-01","provenance":"owned-manual-guidance"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
    }
    let registry = Engram::tool_registry().list_all();
    let mut captures = Vec::new();
    for ownership in [false, true] {
        let mut call = request(
            json!({"project_id":"policy-delivery","ownership_inventory":ownership,
            "source_paths":if ownership {vec!["Rules.md"]} else {vec![]},"max_tokens":2048}),
        );
        let delivery = read_report_delivery::prepare(&mut call, &registry)
            .unwrap()
            .unwrap();
        let args = serde_json::from_value(Value::Object(call.arguments.unwrap())).unwrap();
        let original = engram
            .handle_list_repo_rules_with_ownership(args)
            .await
            .unwrap();
        let body = &original.content[0].as_text().unwrap().text;
        assert!(body.len() > 40960);
        for id in 0..24 {
            assert!(body.contains(&format!("rule-{id:02}")));
        }
        if ownership {
            let inventory: Value = serde_json::from_str(body).unwrap();
            assert_eq!(inventory["rows"].as_array().unwrap().len(), 24);
            for row in inventory["rows"].as_array().unwrap() {
                assert_eq!(row["decoded"]["file_pattern"], "Site/App_Code/**/*.vb");
                assert_eq!(row["decoded"]["priority"], 7);
                assert!(
                    row["decoded"]["rule_text"]
                        .as_str()
                        .unwrap()
                        .contains("Exception: preserve the authored SQL exception.")
                );
            }
        }
        let expected = serde_json::to_string(&original).unwrap();
        let first = delivery.finish(&data, original).unwrap();
        captures.push((first, expected));
    }
    engram
        .handle_delete_repo_rule(
            serde_json::from_value(json!({
                "project_id":"policy-delivery","rule_id":"rule-00"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    std::fs::write(source.join("Rules.md"), "Revised current source.\n").unwrap();
    for (first, expected) in captures {
        assert_eq!(recover(&data, &first), expected);
    }
    assert_eq!(
        state
            .registry
            .list_repo_rules("policy-delivery")
            .unwrap()
            .len(),
        23
    );
    drop(engram);
    drop(state);
    temporary.close().unwrap();
}
