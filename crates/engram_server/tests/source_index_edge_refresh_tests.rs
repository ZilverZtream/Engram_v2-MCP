#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_graph::{Edge, EdgeKind};
use engram_server::{AppState, Engram};
use serde_json::json;

async fn refresh_retires_old_calls(migrate: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Public Class Sample\n Public Shared Sub Copy()\n  If IO.Directory.Exists(\"unused\") Then Sample.Keep()\n End Sub\n Public Shared Sub Keep()\n End Sub\nEnd Class\n";
    std::fs::write(root.join("Sample.vb"), source).unwrap();
    std::fs::write(root.join("Other.vb"), "Public Class Other\n Public Shared Sub Incoming()\n Sample.Copy()\n End Sub\n Public Shared Function Exists() As Boolean\n Return True\n End Function\nEnd Class\n").unwrap();
    let repo = git2::Repository::init(&root).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*.vb"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &[])
        .unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots: vec![root.clone()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"edge-refresh","project_type":"general","wait":true})).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let find = |name: &str| {
        state
            .graph
            .query_nodes(&pid, Some("function"), Some(name), None, 20)
            .unwrap()
            .into_iter()
            .find(|n| n.name == name)
            .unwrap()
    };
    let caller = find("Sample.Copy");
    let valid = find("Sample.Keep");
    let wrong = find("Other.Exists");
    let incoming = find("Other.Incoming");
    let old_gen = caller.generation;
    // Simulate the old resolver's wrong persisted binding, with stable IDs.
    let stale = Edge {
        source_id: caller.node_id.clone(),
        target_id: wrong.node_id.clone(),
        edge_kind: EdgeKind::Calls,
        namespace: "memory".into(),
        language: "vb".into(),
        generation: old_gen,
        weight: 1,
        metadata: Some(json!({"resolution":"post_terminal_unique"})),
        updated_at_ms: 1,
    };
    let mut statistical = stale.clone();
    statistical.edge_kind = EdgeKind::ObservedRuntimeSql;
    let mut knowledge = stale.clone();
    knowledge.edge_kind = EdgeKind::QueriesTable;
    knowledge.namespace = "business_logic".into();
    state
        .graph
        .upsert_edges(&pid, &[stale, statistical, knowledge])
        .unwrap();
    let before = state.graph.list_edges(&pid, Some(EdgeKind::Calls)).unwrap();
    assert!(
        before
            .iter()
            .any(|e| e.source_id == caller.node_id && e.target_id == valid.node_id),
        "fresh extraction must provide legitimate outbound call: {before:?}"
    );
    assert!(
        before
            .iter()
            .any(|e| e.source_id == incoming.node_id && e.target_id == caller.node_id),
        "fresh extraction must provide legitimate inbound call: {before:?}"
    );
    if migrate {
        let mut file = state
            .graph
            .get_node(&pid, "file:Sample.vb")
            .unwrap()
            .unwrap();
        file.metadata.as_mut().unwrap()["source_index_version"] =
            json!(engram_index::SOURCE_INDEX_VERSION - 1);
        state.graph.upsert_nodes(&pid, &[file]).unwrap();
    } else {
        // Declaration line changes: old outgoing edges must be retired, not
        // remapped back into the new method alongside its extracted calls.
        std::fs::write(
            root.join("Sample.vb"),
            format!("' shifted declaration\n{source}"),
        )
        .unwrap();
    }
    engram
        .handle_update_project(
            serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1})).unwrap(),
        )
        .await
        .unwrap();
    let updated_caller = find("Sample.Copy");
    let updated_valid = find("Sample.Keep");
    assert!(updated_caller.generation > old_gen);
    assert_eq!(updated_caller.node_id == caller.node_id, migrate);
    assert_eq!(
        find("Other.Incoming").generation,
        old_gen,
        "unchanged caller must not be re-extracted"
    );
    let after = state.graph.list_edges(&pid, Some(EdgeKind::Calls)).unwrap();
    assert!(
        !after
            .iter()
            .any(|e| e.source_id == updated_caller.node_id && e.target_id == wrong.node_id),
        "superseded incorrect edge survived source refresh: {after:?}"
    );
    assert!(after.iter().any(|e| e.source_id == updated_caller.node_id
        && e.target_id.starts_with("::")
        && e.target_id.ends_with("Directory.Exists")));
    assert!(
        after
            .iter()
            .any(|e| e.source_id == updated_caller.node_id && e.target_id == updated_valid.node_id)
    );
    assert!(
        after
            .iter()
            .any(|e| e.source_id == incoming.node_id && e.target_id == updated_caller.node_id)
    );
    assert!(
        state
            .graph
            .neighbors(&pid, EdgeKind::Calls, &updated_caller.node_id, 100)
            .unwrap()
            .iter()
            .all(|(id, _)| id != &wrong.node_id)
    );
    assert!(
        state
            .graph
            .find_incoming_edges(&pid, Some(EdgeKind::Calls), &wrong.node_id, 100)
            .unwrap()
            .iter()
            .all(|(id, _)| id != &caller.node_id && id != &updated_caller.node_id)
    );
    for kind in [EdgeKind::ObservedRuntimeSql, EdgeKind::QueriesTable] {
        let edges = state.graph.list_edges(&pid, Some(kind)).unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e.source_id == updated_caller.node_id && e.target_id == wrong.node_id),
            "independent statistical/knowledge edge lost: {edges:?}"
        );
    }
    assert_eq!(state.registry.list_projects().unwrap().len(), 1);
}

#[tokio::test]
async fn format_migration_replaces_edges_of_unchanged_stable_symbols() {
    refresh_retires_old_calls(true).await;
}

#[tokio::test]
async fn ordinary_refresh_replaces_outgoing_edges_and_remaps_unchanged_inbound() {
    refresh_retires_old_calls(false).await;
}

#[tokio::test]
async fn unchanged_source_refresh_removes_quoted_settings_but_keeps_session_and_real_settings() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Public Class Reader\n Public Sub Load()\n  Dim cached = HttpContext.Current.Session(\"PermissionCache.CurrentUser\")\n  Dim enabled = ConfigSettings.General.Enabled\n End Sub\nEnd Class\n";
    std::fs::write(root.join("Reader.vb"), source).unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots: vec![root.clone()],
        data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    }).unwrap();
    let engram = Engram::new(state.clone());
    engram.handle_index_project(serde_json::from_value(json!({
        "directory": root, "project_name": "settings-refresh", "project_type": "general", "wait": true
    })).unwrap()).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    let caller = state.graph.query_nodes(&pid, Some("function"), Some("Reader.Load"), None, 20)
        .unwrap().into_iter().find(|n| n.name == "Reader.Load").unwrap();
    let false_target = "::PermissionCache.CurrentUser";
    // Persist the exact old failure mode, then migrate unchanged source.
    state.graph.upsert_edges(&pid, &[Edge {
        source_id: caller.node_id.clone(), target_id: false_target.into(),
        edge_kind: EdgeKind::ReadsSetting, namespace: "memory".into(), language: "vb".into(),
        generation: caller.generation, weight: 1, metadata: None, updated_at_ms: 0,
    }]).unwrap();
    let mut file = state.graph.get_node(&pid, "file:Reader.vb").unwrap().unwrap();
    file.metadata.as_mut().unwrap()["source_index_version"] = json!(engram_index::SOURCE_INDEX_VERSION - 1);
    state.graph.upsert_nodes(&pid, &[file]).unwrap();
    engram.handle_update_project(serde_json::from_value(json!({
        "project_id": pid, "wait": true, "max_commits": 1
    })).unwrap()).await.unwrap();
    assert_eq!(std::fs::read_to_string(root.join("Reader.vb")).unwrap(), source);
    let settings = state.graph.list_edges(&pid, Some(EdgeKind::ReadsSetting)).unwrap();
    assert!(!settings.iter().any(|e| e.target_id == false_target), "{settings:?}");
    assert!(settings.iter().any(|e| e.target_id.contains("ConfigSettings.General.Enabled")), "{settings:?}");
    let state_edges = state.graph.list_edges(&pid, Some(EdgeKind::ReadsState)).unwrap();
    assert!(state_edges.iter().any(|e| e.target_id == "state:Session:PermissionCache.CurrentUser"), "{state_edges:?}");
    let matrix = engram.handle_derive_test_matrix(serde_json::from_value(json!({
        "project_id": pid, "files": ["Reader.vb"]
    })).unwrap()).await.unwrap();
    let text = &matrix.content[0].as_text().unwrap().text;
    assert!(!text.contains("[node_id: ::PermissionCache.CurrentUser]"), "{text}");
    assert!(text.contains("Session:PermissionCache.CurrentUser") && text.contains("ConfigSettings.General.Enabled"), "{text}");
}
