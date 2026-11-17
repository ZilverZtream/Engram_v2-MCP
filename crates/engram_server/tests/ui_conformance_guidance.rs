//! Implementation frequency is not authority, and absent examples do not remove policy.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord, RelPath};
use engram_graph::Node;
use engram_server::{AppState, Engram};
use serde_json::json;
use std::path::Path;

async fn report(server: &Engram, region: &str) -> Result<String> {
    let result = server
        .handle_get_ui_conformance(serde_json::from_value(json!({
            "project_id":"owned-ui-patterns","region":region,"candidate_classes":"row form-group"
        }))?)
        .await?;
    ensure!(
        result.is_error != Some(true),
        "Conformance request failed: {result:?}"
    );
    Ok(result.content[0].as_text().unwrap().text.clone())
}

async fn check(owned: &Path, scenario: u8) -> Result<()> {
    let root = owned.join("r");
    std::fs::create_dir_all(root.join(".github/instructions"))?;
    std::fs::write(root.join("AGENTS.md"), "Preserve accepted exceptions.\n")?;
    let common = root.join(".github/instructions/ui.instructions.md");
    std::fs::write(
        &common,
        "---\napplyTo: 'Site/**/*.aspx'\n---\nAuthored UI rule. Existing generated output remains excluded.\n",
    )?;
    std::fs::write(
        root.join(".github/instructions/orphan.instructions.md"),
        "---\napplyTo: 'Site/pages/orphan.aspx'\n---\nOrphanOnlyRule still applies without a repeated UI family.\n",
    )?;
    std::fs::write(
        root.join(".github/instructions/outside.instructions.md"),
        "---\napplyTo: 'Elsewhere/**'\n---\nOutsideOnlyRule is not the target region's authority.\n",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    state.registry.put_project(&ProjectRecord {
        project_id: "owned-ui-patterns".into(),
        project_name: "Owned patterns".into(),
        directory: root.to_string_lossy().into_owned(),
        project_type: "general".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        reindex_required_since_ms: None,
    })?;
    state
        .registry
        .set_meta("owned-ui-patterns", "active_generation", "1")?;
    let mut nodes = Vec::new();
    for path in [
        "Site/pages/orders.aspx",
        "Site/pages/orphan.aspx",
        "Elsewhere/outside.aspx",
    ] {
        nodes.push(Node {
            node_id: format!("file:{path}"),
            node_type: "file".into(),
            name: path.into(),
            namespace: "memory".into(),
            language: "aspx".into(),
            file_path: RelPath::new(path),
            start_line: 1,
            end_line: 5,
            generation: 1,
            metadata: None,
        });
    }
    for (path, id, classes) in [
        ("Site/pages/orders.aspx", "first", "form-group row"),
        ("Site/pages/orders.aspx", "second", "row form-group"),
        ("Elsewhere/outside.aspx", "outside", "form-group row"),
        ("Site/pages/orphan.aspx", "orphan", ""),
    ] {
        nodes.push(Node {
            node_id: format!("ui:{path}:{id}"),
            node_type: "ui_container".into(),
            name: id.into(),
            namespace: "memory".into(),
            language: "aspx".into(),
            file_path: RelPath::new(path),
            start_line: 2,
            end_line: 4,
            generation: 1,
            metadata: Some(
                json!({"container_type":"Panel","layout_style":"Flow","css_class":classes}),
            ),
        });
    }
    state.graph.upsert_nodes("owned-ui-patterns", &nodes)?;
    let server = Engram::new(state);
    match scenario {
        0 => {
            let text = report(&server, "Site/pages/orders.aspx").await?;
            ensure!(
                text.contains("3 instance(s)")
                    && text.contains("derived at generation 1")
                    && text.contains("exemplar: Site/pages/orders.aspx:2"),
                "Observed evidence lost: {text}"
            );
            ensure!(
                text.contains("not authored policy")
                    && text.contains("class set matches")
                    && text.contains("? structure")
                    && !text.contains("✓ structure"),
                "Unassessed structure became a pass: {text}"
            );
            ensure!(
                text.contains("Authored UI rule.")
                    && text.contains("Existing generated output remains excluded.")
                    && !text.contains("OutsideOnlyRule")
                    && !text.contains("OrphanOnlyRule"),
                "Wrong guidance scope: {text}"
            );
            let old = blake3::hash(&std::fs::read(&common)?).to_hex().to_string();
            std::fs::write(
                &common,
                "---\napplyTo: 'Site/**/*.aspx'\n---\nUpdated authored requirement. Preserve the exception.\n",
            )?;
            let fresh = report(&server, "Site/pages/orders.aspx").await?;
            ensure!(
                fresh.contains("Updated authored requirement.") && !fresh.contains(&old),
                "Guidance required reindexing: {fresh}"
            );
            ensure!(
                server
                    .state
                    .registry
                    .get_meta("owned-ui-patterns", "active_generation")?
                    .as_deref()
                    == Some("1")
            );
        }
        1 => {
            for region in ["Site/pages/", "Site/pages/*.aspx"] {
                let text = report(&server, region).await?;
                ensure!(
                    text.contains("OrphanOnlyRule")
                        && text.contains("Authored UI rule.")
                        && !text.contains("OutsideOnlyRule"),
                    "Region lost non-family guidance or gained outside authority: {text}"
                );
                ensure!(text.contains("bounded indexed file observations"));
            }
        }
        _ => {
            let text = report(&server, "Site/pages/planned.aspx").await?;
            ensure!(
                text.contains("result: no_families")
                    && text.contains("Authored UI rule.")
                    && text.contains("Existing generated output remains excluded.")
                    && !text.contains("there is no house contract"),
                "Absent examples removed requirements: {text}"
            );
            ensure!(
                text.contains("existence/index membership not established")
                    && !text.contains("OutsideOnlyRule")
            );
        }
    }
    Ok(())
}

fn run(scenario: u8) -> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("uc-").tempdir_in(parent)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(check(owned.path(), scenario));
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
fn exact_scope_preserves_observations_and_exceptions_without_passing_unassessed_structure()
-> Result<()> {
    run(0)
}
#[test]
fn directory_and_glob_guidance_include_indexed_files_without_ui_families() -> Result<()> {
    run(1)
}
#[test]
fn new_file_without_examples_still_receives_authored_requirements() -> Result<()> {
    run(2)
}
