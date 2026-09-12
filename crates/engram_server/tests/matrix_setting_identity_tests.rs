#![allow(clippy::unwrap_used)]
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::{state::AppState, tools::Engram};
use serde_json::json;

const PID: &str = "matrix-setting-identities";

fn node(id: &str, name: &str, kind: &str, owner: &str, file: &str) -> Node {
    Node { node_id:id.into(), name:name.into(), node_type:kind.into(), namespace:owner.into(),
        language:"vbnet".into(), file_path:RelPath::new(file), start_line:1, end_line:3,
        generation:1, metadata:None }
}

fn edge(source: &str, target: &str, kind: EdgeKind) -> Edge {
    Edge { source_id:source.into(), target_id:target.into(), edge_kind:kind, namespace:"memory".into(),
        language:"vbnet".into(), weight:1, generation:1, metadata:None, updated_at_ms:0 }
}

fn fixture() -> (tempfile::TempDir, AppState, Engram) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Public Sub ReadSettings()\nEnd Sub\n";
    std::fs::write(root.join("Reader.vb"), source).unwrap();
    let (state, _events) = AppState::new(Config {
        data_dir:temp.path().join("data"), allowed_roots:vec![root.clone()],
        embedding_backend:"fts_only".into(), ..Default::default()
    }).unwrap();
    state.registry.put_project(&ProjectRecord {
        project_id:PID.into(), project_name:PID.into(), directory:root.to_string_lossy().into_owned(),
        project_type:"general".into(), created_at_ms:0, updated_at_ms:0, reindex_required_since_ms:None,
    }).unwrap();
    state.registry.set_meta(PID,"active_generation","1").unwrap();
    let mut file = node("file:Reader.vb","Reader.vb","file","memory","Reader.vb");
    file.metadata = Some(json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}));
    state.graph.upsert_nodes(PID,&[file,node("reader","ReadSettings","function","Reader","Reader.vb")]).unwrap();
    let engram = Engram::new(state.clone());
    (temp,state,engram)
}

async fn matrix(engram: &Engram) -> String {
    engram.handle_derive_test_matrix(serde_json::from_value(json!({"project_id":PID,"files":["Reader.vb"]})).unwrap())
        .await.unwrap().content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn settings_keep_target_identity_even_when_qualified_labels_collide() {
    let (_temp,state,engram) = fixture();
    state.graph.upsert_nodes(PID,&[
        node("setting:a","Enabled","property","ConfigSettings.A","A.vb"),
        node("setting:b","Enabled","property","ConfigSettings.B","B.vb"),
        node("setting:a-second","Enabled","property","ConfigSettings.A","AnotherA.vb"),
    ]).unwrap();
    state.graph.upsert_edges(PID,&[
        edge("reader","setting:a",EdgeKind::ReadsSetting),
        edge("reader","setting:b",EdgeKind::ReadsSetting),
        edge("reader","setting:a-second",EdgeKind::ReadsSetting),
    ]).unwrap();
    let text = matrix(&engram).await;
    for label in ["ConfigSettings.A.Enabled [node_id: setting:a]",
        "ConfigSettings.B.Enabled [node_id: setting:b]",
        "ConfigSettings.A.Enabled [node_id: setting:a-second]"] {
        assert!(text.contains(label),"{text}");
    }
    assert!(text.contains("3 setting/value references"),"{text}");
    assert!(text.contains("get_setting accepts names, not node IDs"));
    assert!(text.contains("identical qualified labels remain ambiguous"));
    let ambiguous = engram.handle_get_setting(serde_json::from_value(json!({
        "project_id":PID,"name":"ConfigSettings.A.Enabled"
    })).unwrap()).await.unwrap_err();
    assert!(ambiguous.message.contains("AMBIGUOUS"));
}

#[tokio::test]
async fn dangling_setting_target_is_explicitly_incomplete() {
    let (_temp,state,engram) = fixture();
    state.graph.upsert_edges(PID,&[edge("reader","setting:missing",EdgeKind::ReadsSetting)]).unwrap();
    let text = matrix(&engram).await;
    assert!(text.lines().any(|line| line.starts_with("INCOMPLETE:") && line.contains("setting target setting:missing unavailable")),"{text}");
    assert!(text.contains("unresolved setting target [node_id: setting:missing]"));
}

#[tokio::test]
async fn state_axis_labels_requested_sites_without_claiming_external_discovery() {
    let (_temp,state,engram) = fixture();
    state.graph.upsert_nodes(PID,&[node("external","OutsideReader","function","Outside","Outside.vb")]).unwrap();
    state.graph.upsert_edges(PID,&[
        edge("reader","state:Session:Cart",EdgeKind::ReadsState),
        edge("external","state:Session:Cart",EdgeKind::WritesState),
    ]).unwrap();
    let text = matrix(&engram).await;
    assert!(text.contains("Session:Cart") && text.contains("ReadSettings"));
    assert!(!text.contains("OutsideReader"));
    assert!(text.contains("listed sites do not establish cross-page consumers"));
    assert!(text.contains("trace_state_usage"));
    assert!(!text.contains("couple the change to OTHER pages"));
}

#[tokio::test]
async fn matching_source_hash_does_not_validate_obsolete_dependency_extraction() {
    let (_temp, state, engram) = fixture();
    state.graph.upsert_edges(PID, &[
        edge("reader", "setting:obsolete", EdgeKind::ReadsSetting),
        edge("reader", "state:Session:Obsolete", EdgeKind::ReadsState),
    ]).unwrap();
    let mut file = state.graph.get_node(PID, "file:Reader.vb").unwrap().unwrap();
    file.metadata.as_mut().unwrap()["source_index_version"] = json!(engram_index::SOURCE_INDEX_VERSION - 1);
    state.graph.upsert_nodes(PID, &[file]).unwrap();
    let text = matrix(&engram).await;
    assert!(text.contains("obsolete source format") && text.contains("update_project"), "{text}");
    assert!(!text.contains("setting:obsolete") && !text.contains("Session:Obsolete"), "{text}");
}
