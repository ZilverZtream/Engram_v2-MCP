#![allow(clippy::unwrap_used)]
//! Caller expansion must distinguish unsupported, capped and unavailable evidence.
use engram_core::{RelPath, config::Config};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{
    handlers::access_layer_tools::EditContextCompleteness,
    models::{GetFullMethodBodyRequest, GetMethodEditContextRequest},
    state::AppState,
    tools::Engram,
};
use serde_json::{Value, json};

const PID: &str = "caller-expansion-test";

fn fixture(callers: usize) -> (tempfile::TempDir, Engram, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"), allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(), max_concurrent_jobs: 1,
        ..Default::default()
    }).unwrap();
    state.registry.put_project(&engram_core::ProjectRecord {
        project_id: PID.into(), project_name: PID.into(),
        directory: root.to_string_lossy().into_owned(), project_type: "dotnet_webforms_vb".into(),
        created_at_ms: 0, updated_at_ms: 0, reindex_required_since_ms: None,
    }).unwrap();
    state.registry.set_meta(PID, "active_generation", "1").unwrap();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for i in 0..=callers {
        let (file, name, body) = if i == 0 {
            ("target.vb".to_string(), "Run".to_string(), "Public Sub Run()\n Return\nEnd Sub\n".to_string())
        } else {
            (format!("caller{i}.vb"), format!("Caller{i}"), format!("Public Sub Caller{i}()\n Demo.Run()\nEnd Sub\n"))
        };
        std::fs::write(root.join(&file), &body).unwrap();
        // Short indexed names carry the full identity in metadata for FQN lookup.
        let metadata = Some(json!({"fqn": format!("Demo.{name}")}));
        let node = Node {
            node_id: format!("function:{name}"), node_type: "function".into(), name,
            namespace: "Demo".into(), language: "vbnet".into(), file_path: RelPath::new(&file),
            start_line: 1, end_line: 3, generation: 1, metadata,
        };
        nodes.push(Node {
            node_id: format!("file:{file}"), node_type: "file".into(),
            metadata: Some(json!({"file_hash": blake3::hash(body.as_bytes()).to_hex().to_string()})),
            ..node.clone()
        });
        if i > 0 {
            edges.push(Edge {
                source_id: node.node_id.clone(), target_id: "function:Run".into(),
                namespace: "memory".into(), language: "vbnet".into(), edge_kind: EdgeKind::Calls,
                weight: i as u32, generation: 1, metadata: None, updated_at_ms: 0,
            });
        }
        nodes.push(node);
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    (temp, Engram::new(state), root)
}

async fn body(engram: &Engram, request: Value) -> String {
    let request: GetFullMethodBodyRequest = serde_json::from_value(request).unwrap();
    engram.handle_get_full_method_body(request).await.unwrap()
        .content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn changed_target_is_withheld_but_direct_snapshot_remains_available() {
    let (_temp, engram, root) = fixture(0);
    let changed = "' inserted line\nPublic Sub Run()\n Return\nEnd Sub\n";
    std::fs::write(root.join("target.vb"), changed).unwrap();
    let error = engram.handle_get_full_method_body(serde_json::from_value(json!({
        "project_id":PID, "fqn":"Demo.Run"
    })).unwrap()).await.unwrap_err();
    assert!(error.message.contains("Stale method spans withheld"));
    let direct: Value = serde_json::from_str(&body(&engram, json!({
        "project_id":PID, "file_path":"target.vb", "line_start":1, "line_end":2, "output_json":true
    })).await).unwrap();
    assert_eq!(direct["source_code"], "' inserted line\nPublic Sub Run()");
    assert_eq!(direct["source_file_hash"], blake3::hash(changed.as_bytes()).to_hex().to_string());
    assert_eq!(direct["source_verification"], "explicit_range_current_file_snapshot");
}

#[tokio::test]
async fn conflicting_method_targets_are_rejected_instead_of_ignoring_the_range() {
    let (_temp, engram, _) = fixture(0);
    for extra in [json!({"file_path":"other.vb"}), json!({"line_start":2}), json!({"line_end":2})] {
        let mut request = json!({"project_id":PID, "fqn":"Demo.Run"});
        request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let error = engram.handle_get_full_method_body(serde_json::from_value(request).unwrap()).await.unwrap_err();
        assert!(error.message.contains("mutually exclusive"));
    }
}

#[tokio::test]
async fn legacy_spans_are_unverified_and_file_hash_identifies_the_returned_snapshot() {
    let (_temp, engram, root) = fixture(0);
    let request = json!({"project_id":PID, "fqn":"Demo.Run", "output_json":true});
    let verified: Value = serde_json::from_str(&body(&engram, request.clone()).await).unwrap();
    let expected_hash = blake3::hash(&std::fs::read(root.join("target.vb")).unwrap()).to_hex().to_string();
    assert_eq!(verified["source_verification"], "matched_indexed_fingerprint");
    assert_eq!(verified["source_file_hash"], expected_hash);
    let mut file = engram.state.graph.get_node(PID, "file:target.vb").unwrap().unwrap();
    // None means "preserve existing metadata" to GraphStore; an explicit
    // empty object models a legacy file entry without an indexed fingerprint.
    file.metadata = Some(json!({}));
    engram.state.graph.upsert_nodes(PID, &[file]).unwrap();
    let legacy: Value = serde_json::from_str(&body(&engram, request.clone()).await).unwrap();
    assert_eq!(legacy["source_verification"], "unverified_missing_indexed_fingerprint");
    assert_eq!(legacy["source_file_hash"], expected_hash);
    assert_eq!(legacy["source_code"], verified["source_code"]);
    let mut markdown_request = request;
    markdown_request["output_json"] = json!(false);
    let markdown = body(&engram, markdown_request).await;
    assert!(markdown.contains("Indexed Method Span (source fingerprint unverified)"));
    assert!(!markdown.contains("Full Method Body"));
    let direct: Value = serde_json::from_str(&body(&engram, json!({
        "project_id":PID, "file_path":"target.vb", "line_start":1, "line_end":2, "output_json":true
    })).await).unwrap();
    assert_eq!(direct["source_verification"], "explicit_range_current_file_snapshot");
    assert_eq!(direct["source_file_hash"], expected_hash);
}

#[tokio::test]
async fn invalid_source_ranges_fail_instead_of_returning_empty_or_clipped_bodies() {
    let (_temp, engram, _) = fixture(0);
    for (start, end) in [(0, 2), (3, 2), (1, 4), (4, 4)] {
        let request = serde_json::from_value(json!({"project_id": PID,
            "file_path": "target.vb", "line_start": start, "line_end": end})).unwrap();
        let error = engram.handle_get_full_method_body(request).await.unwrap_err();
        assert!(error.message.contains("Invalid source range"), "{error}");
    }
}

#[tokio::test]
async fn partial_range_is_not_presented_as_a_resolved_method() {
    let (_temp, engram, _) = fixture(0);
    let mut request = json!({"project_id": PID, "file_path": "target.vb",
        "line_start": 1, "line_end": 2, "context_lines": 0, "output_json": true});
    let value: Value = serde_json::from_str(&body(&engram, request.clone()).await).unwrap();
    assert_eq!(value["retrieval_scope"], "explicit_source_range");
    assert_eq!(value["source_code"], "Public Sub Run()\n Return");
    assert!(value["boundary_guidance"].as_str().unwrap().contains("have not been resolved"));
    request["output_json"] = json!(false);
    let markdown = body(&engram, request).await;
    assert!(markdown.contains("# Requested Source Range"));
    assert!(markdown.contains("## Requested Source Lines"));
    assert!(!markdown.contains("Full Method Body"));
    assert!(!markdown.contains("End Sub"));
    let resolved: Value = serde_json::from_str(&body(&engram, json!({
        "project_id": PID, "fqn": "Demo.Run", "output_json": true
    })).await).unwrap();
    assert_eq!(resolved["retrieval_scope"], "indexed_method");
    assert!(resolved["source_code"].as_str().unwrap().ends_with("End Sub"));
}

#[tokio::test]
async fn direct_range_reports_requested_but_unsupported_in_json_and_markdown() {
    let (_temp, engram, _) = fixture(1);
    let mut request = json!({"project_id": PID, "file_path": "target.vb", "line_start": 1,
        "line_end": 3, "include_caller_bodies": true, "output_json": true});
    let value: Value = serde_json::from_str(&body(&engram, request.clone()).await).unwrap();
    assert!(value["source_code"].as_str().unwrap().contains("Public Sub Run()"));
    let expansion = &value["caller_expansion"];
    assert_eq!(expansion["requested"], true);
    assert_eq!(expansion["attempted"], false);
    assert_eq!(expansion["status"], "unsupported_direct_range");
    assert_eq!(expansion["returned"], 0);
    assert!(expansion["omission_reason"].as_str().unwrap().contains("unique indexed method"));
    assert!(expansion["next_action"].as_str().unwrap().contains("get_method_edit_context"));
    assert!(value["caller_bodies"].as_array().unwrap().is_empty());
    request["output_json"] = json!(false);
    let markdown = body(&engram, request).await;
    assert!(markdown.contains("Requested: true; attempted: false; status: unsupported_direct_range; returned: 0"));
    assert!(markdown.contains("Omission reason:"));
    assert!(markdown.contains("Next action:"));
}

#[tokio::test]
async fn fqn_reports_success_and_exact_cap_without_claiming_full_coverage() {
    let (_temp, engram, _) = fixture(2);
    for (cap, expected, truncated) in [(1, "truncated", true), (2, "complete", false)] {
        let value: Value = serde_json::from_str(&body(&engram, json!({
            "project_id": PID, "fqn": "Demo.Run", "include_caller_bodies": true,
            "max_callers": cap, "output_json": true,
        })).await).unwrap();
        let expansion = &value["caller_expansion"];
        assert_eq!(expansion["attempted"], true);
        assert_eq!(expansion["status"], expected);
        assert_eq!(expansion["truncated"], truncated);
        assert_eq!(expansion["returned"], cap);
        assert_eq!(value["caller_bodies"].as_array().unwrap().len(), cap);
        assert!(expansion["coverage_interpretation"].as_str().unwrap().contains("not exhaustive extraction"));
        assert!(value["caller_bodies"][0]["source_code"].as_str().unwrap().contains("Demo.Run()"));
    }
}

#[tokio::test]
async fn stale_and_missing_caller_source_is_explicit_and_does_not_discard_target_body() {
    let (_temp, engram, root) = fixture(3);
    std::fs::write(root.join("caller2.vb"), "changed\n").unwrap();
    std::fs::remove_file(root.join("caller3.vb")).unwrap();
    let value: Value = serde_json::from_str(&body(&engram, json!({
        "project_id": PID, "fqn": "Demo.Run", "include_caller_bodies": true,
        "max_callers": 3, "output_json": true,
    })).await).unwrap();
    assert!(value["source_code"].as_str().unwrap().contains("Public Sub Run()"));
    assert_eq!(value["caller_expansion"]["status"], "partial");
    assert_eq!(value["caller_expansion"]["returned"], 1);
    assert_eq!(value["caller_bodies"][0]["fqn"], "Demo.Caller1");
    let omissions = value["caller_expansion"]["omissions"].as_array().unwrap();
    assert_eq!(omissions.len(), 2);
    assert!(omissions.iter().any(|v| v.as_str().unwrap().contains("changed since indexing")));
    assert!(omissions.iter().any(|v| v.as_str().unwrap().contains("Cannot verify caller3.vb")));
}

#[tokio::test]
async fn empty_indexed_callers_and_disabled_expansion_remain_distinct() {
    let (_temp, engram, _) = fixture(0);
    for (requested, cap, status, attempted) in [
        (true, 3, "complete", true), (true, 0, "omitted", false), (false, 3, "not_requested", false),
    ] {
        let value: Value = serde_json::from_str(&body(&engram, json!({
            "project_id": PID, "fqn": "Demo.Run", "include_caller_bodies": requested,
            "max_callers": cap, "output_json": true,
        })).await).unwrap();
        assert_eq!(value["caller_expansion"]["status"], status);
        assert_eq!(value["caller_expansion"]["attempted"], attempted);
        assert_eq!(value["caller_expansion"]["returned"], 0);
        assert!(value["caller_expansion"]["coverage_interpretation"].as_str().unwrap()
            .contains("do not establish absence of consumers"));
    }
}

#[test]
fn default_and_all_complete_fixtures_serialize_the_same_coverage_qualification() {
    let default = serde_json::to_value(EditContextCompleteness::default()).unwrap();
    let complete = serde_json::to_value(EditContextCompleteness::all_complete()).unwrap();
    assert_eq!(default["coverage_interpretation"], complete["coverage_interpretation"]);
    assert!(complete["coverage_interpretation"].as_str().unwrap().contains("runtime coverage"));
    assert_eq!(complete["callers"]["status"], "complete");
    assert_eq!(default["callers"]["status"], "not_run");
}

#[tokio::test]
async fn runtime_edit_context_qualifies_complete_queries_and_rendered_blast_score() {
    let (_temp, engram, _) = fixture(0);
    for output_json in [true, false] {
        let request: GetMethodEditContextRequest = serde_json::from_value(json!({
            "project_id": PID, "file_path": "target.vb", "method_name": "Run",
            "include_business_logic": false, "output_json": output_json,
        })).unwrap();
        let response = engram.handle_get_method_edit_context(request).await.unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        if output_json {
            let value: Value = serde_json::from_str(text).unwrap();
            let coverage = &value["edit_safety"]["completeness"];
            assert_eq!(coverage["callers"]["status"], "complete");
            assert!(coverage["coverage_interpretation"].as_str().unwrap().contains("not exhaustive extraction"));
        } else {
            let blast = text.lines().find(|line| line.contains("**Blast radius**:")).unwrap();
            assert!(blast.contains("indexed evidence"), "{blast}");
            assert!(blast.contains("omit consumers and side effects"), "{blast}");
            assert!(text.contains("not exhaustive extraction"));
        }
    }
}
