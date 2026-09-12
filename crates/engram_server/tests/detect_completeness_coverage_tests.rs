#![allow(clippy::unwrap_used)]

use engram_core::{ProjectRecord, RelPath, config::Config};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{state::AppState, tools::Engram};
use serde_json::json;

const PID: &str = "detect-coverage-fixture";

fn fixture() -> (tempfile::TempDir, AppState, Engram) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let (state, _events) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    let server = Engram::new(state.clone());
    (tmp, state, server)
}

fn node(id: &str, kind: &str, file: &str) -> Node {
    Node {
        node_id: id.into(),
        node_type: kind.into(),
        name: id.into(),
        namespace: "fixture".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(file),
        start_line: 1,
        end_line: 2,
        generation: 1,
        // Framework handlers have no static callers by design; avoid unrelated
        // zero-caller candidates while testing state traversal coverage.
        metadata: (kind == "function").then(|| json!({"handles_clause":"Me.Load"})),
    }
}

fn edge(source: &str, target: &str, kind: EdgeKind) -> Edge {
    Edge {
        source_id: source.into(),
        target_id: target.into(),
        edge_kind: kind,
        weight: 25,
        namespace: "memory".into(),
        language: "vbnet".into(),
        generation: 1,
        metadata: None,
        updated_at_ms: 0,
    }
}

async fn detect(server: &Engram, files: Vec<String>) -> String {
    let result = server
        .handle_detect_incomplete_changes(
            serde_json::from_value(json!({"project_id":PID,"edited_files":files})).unwrap(),
        )
        .await
        .unwrap();
    result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn root_and_nested_same_basename_are_distinct_current_files() {
    let (_tmp, state, server) = fixture();
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                node("file:Rules.vb", "file", "Rules.vb"),
                node("file:nested/Rules.vb", "file", "nested/Rules.vb"),
                node("sym:root", "function", "Rules.vb"),
                node("sym:nested", "function", "nested/Rules.vb"),
                node("state:Session:Tenant", "global_state", "__state"),
            ],
        )
        .unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[
                edge(
                    "file:Rules.vb",
                    "file:nested/Rules.vb",
                    EdgeKind::TemporalCoupling,
                ),
                edge("sym:root", "state:Session:Tenant", EdgeKind::ReadsState),
                edge("sym:nested", "state:Session:Tenant", EdgeKind::WritesState),
            ],
        )
        .unwrap();
    let text = detect(&server, vec!["Rules.vb".into()]).await;
    assert!(
        text.contains("`nested/Rules.vb` (25 co-changes with `Rules.vb`)"),
        "{text}"
    );
    assert!(
        text.contains("state key `Session:Tenant` is also read/written in: nested/Rules.vb"),
        "{text}"
    );
    let text = detect(&server, vec!["nested/Rules.vb".into()]).await;
    assert!(
        text.contains("state key `Session:Tenant` is also read/written in: Rules.vb"),
        "{text}"
    );
    let text = detect(
        &server,
        vec![" ./Rules.vb ".into(), "nested\\Rules.vb".into()],
    )
    .await;
    assert!(
        !text.contains("## Shared state with untouched files"),
        "{text}"
    );
    assert!(
        !text.contains("## Co-change partners you did NOT touch"),
        "{text}"
    );
    assert!(!text.contains("NOT found in the index"), "{text}");
}

#[tokio::test]
async fn twenty_state_edges_are_complete_but_twenty_one_report_the_cap() {
    for count in [20, 21] {
        let (_tmp, state, server) = fixture();
        let mut nodes = vec![
            node("file:Rules.vb", "file", "Rules.vb"),
            node("sym:root", "function", "Rules.vb"),
        ];
        let mut edges = Vec::new();
        for index in 0..count {
            let key = format!("state:Session:K{index:02}");
            nodes.push(node(&key, "global_state", "__state"));
            edges.push(edge("sym:root", &key, EdgeKind::ReadsState));
        }
        state.graph.upsert_nodes(PID, &nodes).unwrap();
        state.graph.upsert_edges(PID, &edges).unwrap();
        let text = detect(&server, vec!["Rules.vb".into()]).await;
        assert_eq!(
            text.contains("state edges for sym:root (ReadsState) truncated at 20"),
            count == 21,
            "{text}"
        );
        assert_eq!(
            text.contains("## INCOMPLETE coverage"),
            count == 21,
            "{text}"
        );
    }
}

#[tokio::test]
async fn dangling_state_touching_symbols_are_reported_as_unknown_evidence() {
    let (_tmp, state, server) = fixture();
    state
        .graph
        .upsert_nodes(
            PID,
            &[
                node("file:Rules.vb", "file", "Rules.vb"),
                node("sym:root", "function", "Rules.vb"),
                node("state:Session:Tenant", "global_state", "__state"),
            ],
        )
        .unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[
                edge("sym:root", "state:Session:Tenant", EdgeKind::ReadsState),
                edge(
                    "sym:missing_external",
                    "state:Session:Tenant",
                    EdgeKind::WritesState,
                ),
            ],
        )
        .unwrap();
    let text = detect(&server, vec!["Rules.vb".into()]).await;
    assert!(
        text.contains("dangling toucher sym:missing_external"),
        "{text}"
    );
    assert!(text.contains("## INCOMPLETE coverage"), "{text}");
    assert!(text.contains("completeness is not established"), "{text}");
}

#[tokio::test]
async fn exact_toucher_cap_does_not_report_truncation() {
    for count in [100, 101] {
        let (_tmp, state, server) = fixture();
        let mut nodes = vec![
            node("file:Rules.vb", "file", "Rules.vb"),
            node("sym:root", "function", "Rules.vb"),
            node("state:Session:Tenant", "global_state", "__state"),
        ];
        let mut edges = vec![edge(
            "sym:root",
            "state:Session:Tenant",
            EdgeKind::ReadsState,
        )];
        for index in 1..count {
            let id = format!("sym:consumer:{index:03}");
            nodes.push(node(&id, "function", "outside.vb"));
            edges.push(edge(&id, "state:Session:Tenant", EdgeKind::ReadsState));
        }
        state.graph.upsert_nodes(PID, &nodes).unwrap();
        state.graph.upsert_edges(PID, &edges).unwrap();
        let text = detect(&server, vec!["Rules.vb".into()]).await;
        assert_eq!(
            text.contains("state touchers for state:Session:Tenant truncated at 100"),
            count == 101,
            "{text}"
        );
        assert_eq!(
            text.contains("## INCOMPLETE coverage"),
            count == 101,
            "{text}"
        );
    }
}

#[tokio::test]
async fn zero_indexed_callers_are_advisory_and_blank_paths_are_rejected() {
    let (_tmp, state, server) = fixture();
    let mut method = node("sym:OrdinaryMethod", "function", "Rules.vb");
    method.metadata = None;
    state
        .graph
        .upsert_nodes(PID, &[node("file:Rules.vb", "file", "Rules.vb"), method])
        .unwrap();
    let text = detect(&server, vec!["Rules.vb".into()]).await;
    assert!(
        text.contains("Methods with zero indexed callers (review candidates)"),
        "{text}"
    );
    assert!(
        text.contains("does not establish that a method is new, dead, or unwired"),
        "{text}"
    );
    assert!(!text.contains("Implemented but never wired"), "{text}");
    let request = serde_json::from_value(json!({"project_id":PID,"edited_files":[" "]})).unwrap();
    assert!(
        server
            .handle_detect_incomplete_changes(request)
            .await
            .is_err()
    );
}
