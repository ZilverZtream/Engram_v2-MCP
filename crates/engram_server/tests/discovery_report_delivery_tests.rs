//! Discovery works before a project is known and preserves complete old catalogs.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram, read_report_delivery as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn prepare(tool: &str, arguments: Value) -> Result<delivery::Report> {
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":tool,"arguments":arguments}))?;
    Ok(delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[tokio::test]
async fn discovery_budget_recovers_original_projects_after_removal_and_owner_shutdown() -> Result<()>
{
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("d")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    for i in 0..24 {
        state.registry.put_project(&ProjectRecord {
            project_id: format!("owned-discovery-{i:02}"),
            project_name: format!(
                "Owned catalog {i:02}: {}",
                "Preserve complete project identity. ".repeat(3)
            ),
            project_type: "general".into(),
            directory: root.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
    }
    let fixture = Fixture {
        server: Engram::new(state),
        owned,
    };
    let original = fixture.server.handle_list_projects().await?;
    ensure!(text(&original).len() > 2048);
    let exact = serde_json::to_string(&original)?;
    let data = fixture.owned.path().join("data");
    let bounded = prepare("list_projects", json!({"max_tokens":512}))?.finish(&data, original)?;
    ensure!(text(&bounded).len() <= 2048);
    let bounded: Value = serde_json::from_str(text(&bounded))?;
    ensure!(bounded["report_scope"] == "server_discovery" && bounded.get("project_id").is_none());
    ensure!(
        bounded["response_delivery"]["next_call"]["arguments"]
            .get("project_id")
            .is_none()
    );
    fixture
        .server
        .handle_delete_project(serde_json::from_value(
            json!({"project_id":"owned-discovery-23"}),
        )?)
        .await?;
    let current = fixture.server.handle_list_projects().await?;
    ensure!(!text(&current).contains("owned-discovery-23"));
    let Fixture { server, owned } = fixture;
    drop(server);
    let mut action = bounded["response_delivery"]["next_call"].clone();
    let mut recovered = String::new();
    while !action.is_null() {
        ensure!(action["arguments"].get("project_id").is_none());
        let mut request: CallToolRequestParam =
            serde_json::from_value(json!({"name":action["tool"],"arguments":action["arguments"]}))?;
        let report = delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap();
        let page = report.recover(&data)?.unwrap();
        ensure!(text(&page).len() <= 2048);
        let page: Value = serde_json::from_str(text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(
        recovered == exact,
        "Catalog recovery must preserve every originally supplied project after deletion and owner shutdown"
    );
    ensure!(
        bounded["response_delivery"]["response_id"]
            == blake3::hash(recovered.as_bytes()).to_hex().to_string()
    );
    drop(owned);
    Ok(())
}

#[test]
fn discovery_schemas_and_json_need_no_project_but_project_info_keeps_its_binding() -> Result<()> {
    let registry = Engram::tool_registry().list_all();
    for catalog in [
        tool_surface::advertised(registry.clone(), true),
        tool_surface::advertised(registry.clone(), false),
    ] {
        let listed = catalog
            .iter()
            .find(|tool| tool.name == "list_projects")
            .unwrap();
        ensure!(
            listed.input_schema["properties"]
                .get("max_tokens")
                .is_some()
        );
        ensure!(
            !listed.input_schema["required"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == "project_id"))
        );
    }
    let owned = tempfile::tempdir()?;
    let original = CallToolResult::success(vec![rmcp::model::Content::text(
        "No projects indexed. Start with index_project.",
    )]);
    let json = prepare("list_projects", json!({"output_json":true}))?
        .finish(owned.path(), original.clone())?;
    let json: Value = serde_json::from_str(text(&json))?;
    ensure!(
        json["result"] == serde_json::to_value(&original)?
            && json["report_scope"] == "server_discovery"
            && json.get("project_id").is_none()
    );
    let mut no_arguments: CallToolRequestParam =
        serde_json::from_value(json!({"name":"list_projects"}))?;
    ensure!(delivery::prepare(&mut no_arguments, &registry)?.is_some());
    let mut no_project: CallToolRequestParam =
        serde_json::from_value(json!({"name":"project_info","arguments":{}}))?;
    ensure!(delivery::prepare(&mut no_project, &registry).is_err());
    Ok(())
}
