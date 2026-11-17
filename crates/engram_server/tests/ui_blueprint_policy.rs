//! Layout context retains authored guidance independently of indexing freshness.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::Node;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::Path;

async fn inspection(
    server: &Engram,
    tool: &str,
    request: Value,
    policy: bool,
) -> Result<rmcp::model::CallToolResult> {
    Ok(match (tool, policy) {
        ("validation", false) => {
            server
                .handle_map_validation_controls(serde_json::from_value(request)?)
                .await?
        }
        ("validation", true) => {
            server
                .handle_map_validation_controls_with_policy(serde_json::from_value(request)?)
                .await?
        }
        ("lifecycle", false) => {
            server
                .handle_map_page_lifecycle(serde_json::from_value(request)?)
                .await?
        }
        ("lifecycle", true) => {
            server
                .handle_map_page_lifecycle_with_policy(serde_json::from_value(request)?)
                .await?
        }
        ("viewstate", false) => {
            server
                .handle_analyze_viewstate_deps(serde_json::from_value(request)?)
                .await?
        }
        ("viewstate", true) => {
            server
                .handle_analyze_viewstate_deps_with_policy(serde_json::from_value(request)?)
                .await?
        }
        ("ajax", false) => {
            server
                .handle_map_ajax_regions(serde_json::from_value(request)?)
                .await?
        }
        ("ajax", true) => {
            server
                .handle_map_ajax_regions_with_policy(serde_json::from_value(request)?)
                .await?
        }
        _ => anyhow::bail!("Unknown owned inspection fixture"),
    })
}

async fn check(owned: &Path, indexed_layout: bool) -> Result<()> {
    let root = owned.join("r");
    std::fs::create_dir_all(root.join(".github/instructions"))?;
    std::fs::create_dir(root.join("Site"))?;
    std::fs::write(
        root.join("Site/Orders.aspx"),
        "<%@ Page CodeFile='Orders.aspx.vb' %>\n<asp:ScriptManager ID='scripts' runat='server' />\n<asp:UpdatePanel ID='actions' runat='server'><ContentTemplate><asp:TextBox ID='name' runat='server' /><asp:RequiredFieldValidator ID='required' ControlToValidate='name' runat='server' /></ContentTemplate></asp:UpdatePanel>\n",
    )?;
    std::fs::write(
        root.join("Site/Orders.aspx.vb"),
        "Public Class Orders\nPublic Sub Page_Load() Handles Me.Load\nViewState(\"selected\") = \"orders\"\nEnd Sub\nEnd Class\n",
    )?;
    std::fs::write(root.join("AGENTS.md"), "Preserve authored exceptions.\n")?;
    let instruction = root.join(".github/instructions/ui.instructions.md");
    std::fs::write(
        &instruction,
        "---\napplyTo: 'Site/**'\n---\nKeep field actions reachable. Existing generated output remains excluded.\n",
    )?;
    std::fs::write(
        root.join(".github/instructions/sql.instructions.md"),
        "---\napplyTo: '**/*.sql'\n---\nUnrelated SQL policy.\n",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let pid = "owned-ui-guidance";
    state.registry.put_project(&ProjectRecord {
        project_id: pid.into(),
        project_name: pid.into(),
        directory: root.to_string_lossy().into_owned(),
        project_type: "general".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        reindex_required_since_ms: None,
    })?;
    state.registry.set_meta(pid, "active_generation", "1")?;
    if indexed_layout {
        state.graph.upsert_nodes(
            pid,
            &[Node {
                node_id: "owned:panel".into(),
                node_type: "ui_container".into(),
                name: "Actions".into(),
                namespace: "memory".into(),
                language: "aspx".into(),
                file_path: RelPath::new("Site/Orders.aspx"),
                start_line: 1,
                end_line: 1,
                generation: 1,
                metadata: Some(json!({"container_type":"Panel","css_class":"field-actions"})),
            }],
        )?;
    }
    let server = Engram::new(state);
    let request =
        || serde_json::from_value(json!({"project_id":pid,"file_path":"Site/Orders.aspx"}));
    let original = server.handle_get_ui_blueprint(request()?).await?;
    let response = server
        .handle_get_ui_blueprint_with_policy(request()?)
        .await?;
    let mut value: Value = serde_json::from_str(&response.content[0].as_text().unwrap().text)?;
    let policy = value
        .as_object_mut()
        .unwrap()
        .remove("project_policy_sources")
        .unwrap();
    if indexed_layout {
        let before: Value = serde_json::from_str(&original.content[0].as_text().unwrap().text)?;
        ensure!(value == before, "Guidance changed indexed layout: {value}");
    } else {
        ensure!(
            value["status"] == "layout_unavailable"
                && value["message"] == original.content[0].as_text().unwrap().text,
            "Missing layout lost its original explanation: {value}"
        );
    }
    let sources = policy["sources"].as_array().unwrap();
    ensure!(sources.iter().any(|source| source["path"] == "AGENTS.md"));
    let ui = sources
        .iter()
        .find(|source| source["path"] == ".github/instructions/ui.instructions.md")
        .ok_or_else(|| anyhow::anyhow!("Missing applicable guidance: {policy}"))?;
    ensure!(
        ui["content"]
            .as_str()
            .unwrap()
            .contains("Existing generated output remains excluded.")
    );
    ensure!(
        ui["blake3"]
            == blake3::hash(&std::fs::read(&instruction)?)
                .to_hex()
                .to_string()
    );
    ensure!(
        sources
            .iter()
            .all(|source| source["path"] != ".github/instructions/sql.instructions.md"),
        "Unrelated scope entered the UI report: {policy}"
    );
    let old_hash = ui["blake3"].clone();
    for tool in ["validation", "lifecycle", "viewstate", "ajax"] {
        let path = if matches!(tool, "validation" | "ajax") {
            "Site/Orders.aspx"
        } else {
            "Site/Orders.aspx.vb"
        };
        for output_json in [true, false] {
            let request = json!({"project_id":pid,"file_path":path,"output_json":output_json});
            let before = inspection(&server, tool, request.clone(), false).await?;
            let after = inspection(&server, tool, request, true).await?;
            let original = &before.content[0].as_text().unwrap().text;
            let report = &after.content[0].as_text().unwrap().text;
            if output_json {
                let mut value: Value = serde_json::from_str(report)?;
                let policy = value
                    .as_object_mut()
                    .unwrap()
                    .remove("project_policy_sources")
                    .unwrap();
                let expected = serde_json::from_str::<Value>(original)
                    .unwrap_or_else(|_| json!({"report_format":"markdown","report":original}));
                ensure!(
                    value == expected,
                    "{tool} analysis changed while adding guidance: {value}"
                );
                ensure!(
                    policy["sources"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(
                            |source| source["path"] == ".github/instructions/ui.instructions.md"
                                && source["content"]
                                    .as_str()
                                    .unwrap()
                                    .contains("Existing generated output remains excluded.")
                        ),
                    "{tool} lost applicable exceptions: {policy}"
                );
                ensure!(
                    policy["sources"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|source| source["path"] != ".github/instructions/sql.instructions.md")
                );
            } else {
                ensure!(
                    report.starts_with(original)
                        && report.contains("## Repository policy sources")
                        && report.contains("Existing generated output remains excluded."),
                    "{tool} Markdown lost original report or guidance: {report}"
                );
            }
        }
    }
    std::fs::write(
        &instruction,
        "---\napplyTo: 'Site/**'\n---\nUpdated UI rule. Preserve the generated-output exception.\n",
    )?;
    let refreshed = server
        .handle_get_ui_blueprint_with_policy(request()?)
        .await?;
    let refreshed: Value = serde_json::from_str(&refreshed.content[0].as_text().unwrap().text)?;
    let ui = refreshed["project_policy_sources"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["path"] == ".github/instructions/ui.instructions.md")
        .unwrap();
    ensure!(
        ui["blake3"] != old_hash && ui["content"].as_str().unwrap().contains("Updated UI rule."),
        "Fresh guidance required reindexing: {refreshed}"
    );
    ensure!(
        server
            .state
            .registry
            .get_meta(pid, "active_generation")?
            .as_deref()
            == Some("1")
    );
    Ok(())
}

fn run(indexed_layout: bool) -> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("up-").tempdir_in(parent)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(check(owned.path(), indexed_layout));
    drop(runtime);
    match (result, owned.close()) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Fixture cleanup also failed: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
#[test]
fn indexed_layout_keeps_scoped_live_guidance_and_authored_exceptions() -> Result<()> {
    run(true)
}
#[test]
fn missing_layout_still_supplies_applicable_guidance_without_inventing_a_tree() -> Result<()> {
    run(false)
}
