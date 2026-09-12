#![allow(clippy::unwrap_used)]
//! Round-6: prepare_implementation_context must resolve the target method with
//! the SAME exact-name-preferring, ambiguity-refusing resolver the rest of the
//! access layer uses (select_method_node), not a hand-rolled substring scan.
//!
//! The round-5 hand-rolled block matched method names by SUBSTRING (via
//! query_nodes) and then either returned candidates[0] or, after the round-5
//! patch, flagged >1 candidate as AMBIGUOUS. That produced a FALSE ambiguity:
//! asking for `GetAll` also substring-matched `GetAllHistory` in the same
//! class, so a perfectly unambiguous request was refused. These tests drive
//! the REAL handler to prove the exact-name preference now resolves it.

use engram_core::config::Config;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

#[tokio::test]
async fn packed_context_preserves_dependency_caps_in_json_and_markdown() {
    for count in [200, 201] {
        let (_tmp, state, engram, pid) = fixture().await;
        let method = state.graph.query_nodes_in_file(&pid, None, "Site/orders.vb", 20)
            .unwrap().into_iter().find(|n| n.node_type == "function" && n.start_line == 2).unwrap();
        let edges: Vec<_> = (0..count).map(|i| engram_graph::Edge {
            source_id: method.node_id.clone(), target_id: format!("state:session:key{i:03}"),
            edge_kind: engram_graph::EdgeKind::ReadsState, namespace: "memory".into(),
            language: "vbnet".into(), weight: 1, generation: method.generation,
            metadata: None, updated_at_ms: 0,
        }).collect();
        state.graph.upsert_edges(&pid, &edges).unwrap();
        let mut request = prepare_req(&pid, "GetAll");
        request["include_state_context"] = json!(true);
        let result = engram.handle_prepare_implementation_context(serde_json::from_value(request.clone()).unwrap()).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(value["state_context"].as_array().unwrap().len(), 200);
        assert_eq!(value["method_coverage"]["session_reads"]["status"], if count == 201 { "truncated" } else { "complete" });
        let has_warning = value["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("Method session_reads evidence:"));
        assert_eq!(has_warning, count == 201, "{value}");
        assert!(value["coverage_interpretation"].as_str().unwrap().contains("not exhaustive extraction"));
        if count == 201 {
            request["output_json"] = json!(false);
            let result = engram.handle_prepare_implementation_context(serde_json::from_value(request).unwrap()).await.unwrap();
            let text = &result.content[0].as_text().unwrap().text;
            assert!(text.contains("Method session_reads evidence:") && text.contains("capped at 200"), "{text}");
        }
    }
}

async fn fixture() -> (tempfile::TempDir, AppState, Engram, String) {
    fixture_with_overloads(false).await
}

async fn fixture_with_overloads(overloaded: bool) -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site")).unwrap();
    // `GetAll` is a proper substring of `GetAllHistory`: a substring resolver
    // sees two candidates in one class and cannot tell them apart.
    std::fs::write(
        root.join("Site/orders.vb"),
        "Public Class orders\n    Public Function GetAll() As String\n        Return \"all-rows\"\n    End Function\n    Public Function GetAllHistory() As String\n        Return \"history-rows\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    if overloaded {
        std::fs::write(root.join("Site/orders.vb"),
            "Public Class orders\n    Public Function GetAll() As String\n        Return \"all-rows\"\n    End Function\n    Public Function GetAll(id As Integer) As String\n        Return \"selected-row\"\n    End Function\nEnd Class\n").unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(512 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "PrepareFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

/// Only the cheap, deterministic pieces — no git style profile, no caller
/// pattern mining — so the test exercises resolution, not the providers.
fn prepare_req(pid: &str, method: &str) -> serde_json::Value {
    json!({
        "project_id": pid,
        "file_path": "Site/orders.vb",
        "method_name": method,
        "include_style_profile": false,
        "include_pattern_examples": false,
        "include_db_schema": false,
        "include_sp_signatures": false,
        "include_state_context": false,
        "include_control_mappings": false,
        "output_json": true,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn substring_sibling_does_not_create_false_ambiguity() {
    let (_t, _s, engram, pid) = fixture().await;
    let req = serde_json::from_value(prepare_req(&pid, "GetAll")).unwrap();
    let res = engram
        .handle_prepare_implementation_context(req)
        .await
        .expect("GetAll is unambiguous; the resolver must not refuse it");
    let out = res.content[0].as_text().unwrap().text.clone();
    // Resolved to the EXACT method, not its substring sibling, and never the
    // hand-rolled false-ambiguity error. method_name is class-qualified, so the
    // closing quote discriminates `orders.GetAll` from `orders.GetAllHistory`.
    assert!(
        out.contains("\"method_name\": \"orders.GetAll\""),
        "must resolve to exact orders.GetAll:\n{out}"
    );
    assert!(!out.contains("AMBIGUOUS"), "no false ambiguity:\n{out}");
    assert!(
        out.contains("all-rows") && !out.contains("history-rows"),
        "must read GetAll's body, not GetAllHistory's:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_method_surfaces_an_error_not_a_wrong_method() {
    let (_t, _s, engram, pid) = fixture().await;
    let req = serde_json::from_value(prepare_req(&pid, "NoSuchMethod")).unwrap();
    let res = engram.handle_prepare_implementation_context(req).await;
    // The hand-rolled path could silently fall through to candidates[0]; the
    // resolver must instead surface a lookup failure.
    assert!(
        res.is_err(),
        "a nonexistent method must error, not resolve to some other method"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn longer_sibling_still_resolves_exactly() {
    // The reverse direction: asking for the LONGER name must not be dragged to
    // the shorter substring match either.
    let (_t, _s, engram, pid) = fixture().await;
    let req = serde_json::from_value(prepare_req(&pid, "GetAllHistory")).unwrap();
    let res = engram
        .handle_prepare_implementation_context(req)
        .await
        .expect("GetAllHistory is unambiguous");
    let out = res.content[0].as_text().unwrap().text.clone();
    assert!(
        out.contains("\"method_name\": \"orders.GetAllHistory\""),
        "must resolve to exact orders.GetAllHistory:\n{out}"
    );
    assert!(
        out.contains("history-rows"),
        "must read GetAllHistory's body:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_target_is_rejected_before_old_spans_are_sliced() {
    let (tmp, _state, engram, pid) = fixture().await;
    let path = tmp.path().join("proj/Site/orders.vb");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("' inserted line\n{source}")).unwrap();
    let error = engram
        .handle_prepare_implementation_context(
            serde_json::from_value(prepare_req(&pid, "GetAll")).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(
        error.message.contains("Stale method spans withheld"),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn style_context_exposes_deterministic_basis_and_failures() {
    let (_tmp, _state, engram, pid) = fixture().await;
    let mut request = prepare_req(&pid, "GetAll");
    request["include_style_profile"] = json!(true);
    request["max_pattern_examples"] = json!(usize::MAX);
    let result = engram
        .handle_prepare_implementation_context(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(value["style_basis"]["llm_used"], false);
    assert_eq!(value["style_basis"]["file_read"], true);
    assert!(value["style_profile"].is_string());
    assert!(value["warnings"].as_array().unwrap().iter().any(|warning| {
        warning
            .as_str()
            .unwrap()
            .contains("git history unavailable")
    }));
    assert!(
        value["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("capped at 20"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema_context_resolves_canonical_ids_and_preserves_unknown_contracts() {
    let (_tmp, state, engram, pid) = fixture().await;
    let method = state
        .graph
        .query_nodes_in_file(&pid, None, "Site/orders.vb", 20)
        .unwrap()
        .into_iter()
        .find(|node| node.node_type == "function" && node.start_line == 2)
        .unwrap();
    let make_node = |id: &str, name: &str, kind: &str, metadata| engram_graph::Node {
        node_id: id.into(),
        name: name.into(),
        node_type: kind.into(),
        namespace: "memory".into(),
        language: "sql".into(),
        file_path: engram_core::RelPath::new("schema.sql"),
        start_line: 1,
        end_line: 1,
        generation: method.generation,
        metadata,
    };
    state
        .graph
        .upsert_nodes(
            &pid,
            &[
                make_node("table:orders", "orders", "db_table", None),
                make_node(
                    "column:orders:id",
                    "id",
                    "db_column",
                    Some(json!({"data_type":"INT", "nullable":"false"})),
                ),
                make_node("column:orders:legacy", "legacy", "db_column", None),
                make_node("table:orders_history", "orders_history", "db_table", None),
            ],
        )
        .unwrap();
    let edge = |source: &str, target: &str, kind| engram_graph::Edge {
        source_id: source.into(),
        target_id: target.into(),
        edge_kind: kind,
        namespace: "memory".into(),
        language: "sql".into(),
        weight: 1,
        generation: method.generation,
        metadata: None,
        updated_at_ms: 0,
    };
    state
        .graph
        .upsert_edges(
            &pid,
            &[
                edge(
                    &method.node_id,
                    "table:orders",
                    engram_graph::EdgeKind::QueriesTable,
                ),
                edge(
                    "table:orders",
                    "column:orders:id",
                    engram_graph::EdgeKind::HasColumn,
                ),
                edge(
                    "table:orders",
                    "column:orders:legacy",
                    engram_graph::EdgeKind::HasColumn,
                ),
            ],
        )
        .unwrap();
    let mut request = prepare_req(&pid, "GetAll");
    request["include_db_schema"] = json!(true);
    let result = engram
        .handle_prepare_implementation_context(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    let columns = value["schema_snippets"][0]["columns"].as_array().unwrap();
    assert_eq!(columns.len(), 2, "{value}");
    assert_eq!(
        columns
            .iter()
            .find(|column| column["name"] == "id")
            .unwrap()["nullable"],
        false
    );
    assert!(
        columns
            .iter()
            .find(|column| column["name"] == "legacy")
            .unwrap()["nullable"]
            .is_null()
    );
    assert!(value["warnings"].as_array().unwrap().iter().any(|warning| {
        warning
            .as_str()
            .unwrap()
            .contains("incomplete column contract")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packed_context_overload_retry_selects_exact_declaration() {
    let (_tmp, _state, engram, pid) = fixture_with_overloads(true).await;
    let mut request = prepare_req(&pid, "GetAll");
    request["class_name"] = json!("orders");
    let error = engram.handle_prepare_implementation_context(
        serde_json::from_value(request.clone()).unwrap()).await.unwrap_err();
    assert!(error.message.contains("AMBIGUOUS") && error.message.contains("line="));
    for (line, selected, excluded) in [(2, "all-rows", "selected-row"), (5, "selected-row", "all-rows")] {
        request["line"] = json!(line);
        for output_json in [true, false] {
            request["output_json"] = json!(output_json);
            let result = engram.handle_prepare_implementation_context(
                serde_json::from_value(request.clone()).unwrap()).await.unwrap();
            let text = &result.content[0].as_text().unwrap().text;
            assert!(text.contains(selected) && !text.contains(excluded), "{text}");
        }
    }
    request["line"] = json!(3);
    let error = engram.handle_prepare_implementation_context(
        serde_json::from_value(request).unwrap()).await.unwrap_err();
    assert!(error.message.contains("No declaration") && error.message.contains("line 3"));
}
