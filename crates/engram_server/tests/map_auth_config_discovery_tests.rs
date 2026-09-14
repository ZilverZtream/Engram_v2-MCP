#![allow(clippy::unwrap_used)]

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::Node;
use engram_server::models::MapAuthConfigRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;

const PID: &str = "nested-web-config";

#[tokio::test]
async fn discovers_authentication_config_below_the_repository_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(project_dir.join("Site")).unwrap();
    std::fs::write(
        project_dir.join("Site/Web.config"),
        r#"<configuration><system.web><authentication mode="Forms"><forms loginUrl="login.aspx" /></authentication></system.web></configuration>"#,
    )
    .unwrap();

    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    state
        .graph
        .upsert_nodes(
            PID,
            &[Node {
                node_id: "file:Site/Web.config".into(),
                node_type: "file".into(),
                name: "Web.config".into(),
                namespace: "memory".into(),
                language: "xml".into(),
                file_path: RelPath::new("Site/Web.config"),
                start_line: 1,
                end_line: 1,
                generation: 1,
                metadata: None,
            }],
        )
        .unwrap();

    let result = Engram::new(state)
        .handle_map_auth_config(MapAuthConfigRequest {
            project_id: PID.into(),
            file_scope: None,
            output_json: false,
        })
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(
        text.contains("**Configuration source**: `Site/Web.config`"),
        "{text}"
    );
    assert!(text.contains("**Auth Mode**: Forms"), "{text}");
}
