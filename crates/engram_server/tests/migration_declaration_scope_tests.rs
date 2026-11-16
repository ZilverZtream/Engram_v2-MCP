//! Declared tracking progress cannot stand in for indexed scope or readiness.
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

async fn report(engram: &Engram, pid: &str) -> Value {
    let result = engram
        .handle_get_migration_progress(
            serde_json::from_value(json!({
                "project_id":pid,"output_json":true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap()
}

#[tokio::test]
async fn one_declared_verified_file_does_not_complete_two_indexed_files() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("Page.aspx"),
        "<%@ Page Language=\"VB\" CodeFile=\"Page.aspx.vb\" Inherits=\"DemoPage\" %>\n",
    )
    .unwrap();
    std::fs::write(source.join("Page.aspx.vb"), "Public Class DemoPage\n    Public Function Value() As Integer\n        Return 1\n    End Function\nEnd Class\n").unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![temp.path().to_owned()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(
            serde_json::from_value(json!({
                "directory":source,"project_name":"Declared migration scope",
                "project_type":"dotnet_webforms_vb","wait":true
            }))
            .unwrap(),
        ))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    engram
        .handle_update_migration_status(
            serde_json::from_value(json!({
                "project_id":pid,"file_path":"Page.aspx.vb","status":"verified",
                "notes":"Caller declaration only."
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let before =
        serde_json::to_value(state.migration_progress.list_files(&pid, None).unwrap()).unwrap();
    let progress = report(&engram, &pid).await;
    assert_eq!(progress["total_files"], 1);
    assert_eq!(progress["completion_pct"], 100.0);
    assert_eq!(
        progress["tracking_scope"],
        "caller_declared_tracking_rows_only"
    );
    assert_eq!(progress["project_readiness"], "not_assessed");
    assert!(
        progress["verification_basis"]
            .as_str()
            .unwrap()
            .contains("caller_declared_status")
    );
    let coverage = &progress["source_tracking"];
    assert_eq!(coverage["status"], "available");
    assert_eq!(coverage["selected_source_files"], 2);
    assert_eq!(coverage["tracked_selected_source_files"], 1);
    assert_eq!(coverage["declared_complete_selected_source_files"], 1);
    assert_eq!(coverage["declared_complete_selected_source_pct"], 50.0);
    assert_eq!(coverage["untracked_selected_source_files"], 1);
    assert_eq!(
        coverage["untracked_source_file_sample"],
        json!(["Page.aspx"])
    );
    assert_eq!(coverage["untracked_sample_omitted"], 0);
    let markdown = engram
        .handle_get_migration_progress(serde_json::from_value(json!({"project_id":pid})).unwrap())
        .await
        .unwrap();
    let text = &markdown.content[0].as_text().unwrap().text;
    assert!(
        text.contains("Declared tracked completion")
            && text.contains("50.0%")
            && text.contains("not assessed")
    );
    assert_eq!(
        serde_json::to_value(state.migration_progress.list_files(&pid, None).unwrap()).unwrap(),
        before
    );
    engram
        .handle_update_migration_status(
            serde_json::from_value(json!({
                "project_id":pid,"file_path":"Page.aspx","status":"migrated"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let completed = report(&engram, &pid).await;
    assert_eq!(
        completed["source_tracking"]["declared_complete_selected_source_pct"],
        100.0
    );
    assert_eq!(completed["project_readiness"], "not_assessed");

    // A valid registered project with no managed source selection still has
    // useful tracking declarations, without an invented source percentage.
    let unindexed = temp.path().join("unindexed");
    std::fs::create_dir(&unindexed).unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: "unindexed".into(),
            project_name: "Unindexed declarations".into(),
            project_type: "dotnet_webforms_vb".into(),
            directory: unindexed.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    engram
        .handle_update_migration_status(
            serde_json::from_value(json!({
                "project_id":"unindexed","file_path":"Pending.vb","status":"verified"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let unknown = report(&engram, "unindexed").await;
    assert_eq!(unknown["total_files"], 1);
    assert_eq!(unknown["source_tracking"]["status"], "unavailable");
    assert!(unknown["source_tracking"]["declared_complete_selected_source_pct"].is_null());
    assert_eq!(unknown["project_readiness"], "not_assessed");
    drop(engram);
    drop(state);
    temp.close().unwrap();
}
