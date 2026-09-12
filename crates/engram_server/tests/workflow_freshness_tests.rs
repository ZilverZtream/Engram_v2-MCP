#![allow(clippy::unwrap_used)]
//! Phase-zero freshness must not approve skipped checks or restored old files.
use engram_core::config::Config;
use engram_server::{models::GetIndexFreshnessRequest, state::AppState, tools::Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

async fn fixture() -> (tempfile::TempDir, Engram, String, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Value.vb"), "Public Class Value\nPublic Function Read() As String\nReturn \"old\"\nEnd Function\nEnd Class\n").unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots: vec![root.clone()], data_dir: temp.path().join("data"),
        embedding_backend: "fts_only".into(), ..Default::default()
    }).unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(engram_server::IndexProjectRequest {
        directory: root.to_string_lossy().into_owned(), project_name: "FreshnessFixture".into(),
        project_type: engram_server::models::ProjectType::DotnetWebformsVb,
        wait: true, dedupe_by_directory: false,
    })).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0].project_id.clone();
    (temp, engram, pid, root)
}

async fn check(engram: &Engram, pid: &str, disk: bool) -> String {
    let request: GetIndexFreshnessRequest = serde_json::from_value(json!({
        "project_id": pid, "check_disk": disk
    })).unwrap();
    engram.handle_get_index_freshness(request).await.unwrap()
        .content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn skipped_disk_check_cannot_approve_a_changed_checkout() {
    let (_temp, engram, pid, root) = fixture().await;
    std::fs::write(root.join("Value.vb"), "changed source\n").unwrap();
    let result = check(&engram, &pid, false).await;
    assert!(result.contains("disk_check: not_run"), "{result}");
    assert!(result.contains("freshness unknown"), "{result}");
    assert!(!result.contains("index is current"), "{result}");
    assert!(result.contains(&format!("directory: {}", root.display())), "{result}");
}

#[tokio::test]
async fn restored_older_timestamp_is_compared_to_indexed_content() {
    let (_temp, engram, pid, root) = fixture().await;
    let path = root.join("Value.vb");
    let old = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, old.replace("\"old\"", "\"new\"")).unwrap();
    let restored_time = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options().write(true).open(&path).unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(restored_time)).unwrap();
    let result = check(&engram, &pid, true).await;
    assert!(result.contains("files_changed_or_added_since_index: 1"), "{result}");
    assert!(result.contains("index is stale"), "{result}");
}

#[tokio::test]
async fn deleted_source_is_reported_as_drift() {
    let (_temp, engram, pid, root) = fixture().await;
    std::fs::remove_file(root.join("Value.vb")).unwrap();
    let result = check(&engram, &pid, true).await;
    assert!(result.contains("files_deleted_since_index: 1"), "{result}");
    assert!(!result.contains("index is current"), "{result}");
}

#[tokio::test]
async fn unchanged_metadata_is_qualified_not_claimed_as_content_proof() {
    let (_temp, engram, pid, _root) = fixture().await;
    let result = check(&engram, &pid, true).await;
    assert!(result.contains("disk_check: no_changes_detected"), "{result}");
    assert!(result.contains("unchanged metadata does not prove identical content"), "{result}");
    assert!(result.contains("generation_complete: true"), "{result}");
}
