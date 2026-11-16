//! Immutable selected vector evidence, independent of subsequent retrieval.
use engram_server::{Engram, retained_results, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content};
use serde_json::{Value, json};

fn request(arguments: Value) -> CallToolRequestParam {
    serde_json::from_value(json!({"name":"vector_search","arguments":arguments})).unwrap()
}

#[test]
fn vector_capture_keeps_the_callers_selection_and_ordinary_calls_stay_live() {
    let registry = Engram::tool_registry().list_all();
    let full = tool_surface::advertised(registry.clone(), true);
    let tool = full
        .iter()
        .find(|tool| tool.name == "vector_search")
        .unwrap();
    assert_eq!(tool.input_schema["required"], json!(["project_id"]));
    for control in ["retain_response", "response_id", "response_offset"] {
        assert!(tool.input_schema["properties"].get(control).is_some());
    }
    let args = json!({"project_id":"owned","query":"accepted WebForms guidance",
        "namespace":"memory_bank","top_k":20,"offset":3,"use_mmr":true,
        "include_path_prefixes":["memory_bank:accepted"],"language_filters":["markdown"],
        "include_content":true,"max_content_chars":5000,"output_json":true,"max_tokens":1024});
    let mut ordinary = request(args.clone());
    assert!(
        retained_results::prepare(&mut ordinary, &registry)
            .unwrap()
            .is_none()
    );
    assert_eq!(serde_json::to_value(ordinary.arguments).unwrap(), args);
    let mut capture_args = args.clone();
    capture_args["retain_response"] = json!(true);
    let mut capture = request(capture_args);
    assert!(
        retained_results::prepare(&mut capture, &registry)
            .unwrap()
            .is_some()
    );
    let mut expected = args;
    expected.as_object_mut().unwrap().remove("max_tokens");
    assert_eq!(serde_json::to_value(capture.arguments).unwrap(), expected);
}

#[test]
fn vector_recovery_preserves_original_hits_citations_and_warnings_without_query_inputs() {
    let owned = tempfile::tempdir().unwrap();
    let registry = Engram::tool_registry().list_all();
    let original = CallToolResult::success(vec![Content::text(json!({
        "results":[{"path":"memory_bank:accepted","doc_id":"original-doc",
            "content":"Keep the authored exception. ".repeat(600),
            "recovery":{"project_id":"owned","namespace":"memory_bank","doc_id":"original-doc"}}],
        "warnings":["Selected ranking window only; source objects are not retained."],
        "response_page":{"offset":3,"window_limit":20}
    }).to_string())]);
    let expected = serde_json::to_string(&original).unwrap();
    let mut capture = request(json!({"project_id":"owned","query":"accepted",
        "output_json":true,"retain_response":true,"max_tokens":1024}));
    let report = retained_results::prepare(&mut capture, &registry)
        .unwrap()
        .unwrap();
    let first = report.finish(owned.path(), original).unwrap();
    let manifest: Value = serde_json::from_str(&first.content[0].as_text().unwrap().text).unwrap();
    let mut next = manifest["response_delivery"]["next_call"].clone();
    let mut exact = String::new();
    loop {
        assert!(next["arguments"].get("query").is_none());
        let mut recovery = request(next["arguments"].clone());
        let page = retained_results::prepare(&mut recovery, &registry)
            .unwrap()
            .unwrap()
            .recover(owned.path())
            .unwrap()
            .unwrap();
        let text = &page.content[0].as_text().unwrap().text;
        assert!(text.len() <= 4096);
        let page: Value = serde_json::from_str(text).unwrap();
        assert_eq!(page["start_byte"], exact.len());
        exact.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        next = page["next_call"].clone();
    }
    assert_eq!(exact, expected);
    assert_eq!(
        blake3::hash(exact.as_bytes()).to_hex().as_str(),
        manifest["response_delivery"]["response_id"]
            .as_str()
            .unwrap()
    );
}
