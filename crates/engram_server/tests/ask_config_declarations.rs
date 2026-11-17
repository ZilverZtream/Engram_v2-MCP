//! Ordinary configuration questions retain authored declarations, not effective values.
use anyhow::{Result, ensure};
use engram_core::{Config, RelPath, memory::MemoryBudget};
use engram_graph::{GraphStore, Node};
use engram_server::{
    AppState, Engram,
    services::ask_engine::{
        config_settings,
        status::{AnswerStatus, ProviderStatus},
    },
};
use futures::FutureExt;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::{collections::BTreeMap, panic::AssertUnwindSafe, path::Path};

const AREA: &str = "Site/Area/Web.config";
const OTHER: &str = "Site/Other/Web.config";
const CONFIG: &str = "<configuration>\n<appSettings>\n<add key=\"PreviewMode\" value=\"compact\"/>\n<add key=\"OtherKey\" value=\"unrelated\"/>\n</appSettings>\n<connectionStrings><add name=\"Main\" connectionString=\"Password=OWNED_SECRET_CANARY\"/></connectionStrings>\n</configuration>\n";
const OTHER_CONFIG: &str = "<configuration>\n<appSettings><add key=\"PreviewMode\" value=\"wide\"/></appSettings>\n</configuration>\n";
const GETTER: &str = "Public Class Reader\nPublic Function ReadMode() As String\nReturn GlobalSettings.GetText(\"PreviewMode\")\nEnd Function\nEnd Class\n";
const GUIDE: &str = "Exception: preview rendering must never publish an invoice.\n";

fn graph_fixture(root: &Path) -> Result<(GraphStore, BTreeMap<String, u64>)> {
    let graph = GraphStore::open(&root.join("graph.redb"))?;
    let mut nodes = Vec::new();
    let mut selected = BTreeMap::new();
    for (path, source) in [(AREA, CONFIG), (OTHER, OTHER_CONFIG)] {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(target, source)?;
        selected.insert(path.into(), 7);
        let file = Node {
            node_id: format!("file:{path}"),
            node_type: "file".into(),
            name: path.into(),
            namespace: "memory".into(),
            language: "xml".into(),
            file_path: RelPath::new(path),
            start_line: 1,
            end_line: source.lines().count() as u32,
            generation: 7,
            metadata: Some(
                json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}),
            ),
        };
        let mut declaration = file.clone();
        declaration.node_id = format!("setting:{path}:PreviewMode");
        declaration.node_type = "app_setting".into();
        declaration.name = "PreviewMode".into();
        declaration.start_line = 0;
        declaration.end_line = 0;
        declaration.metadata = None; // Discovery identity alone supplies no value.
        nodes.extend([file, declaration]);
    }
    graph.upsert_nodes("owned", &nodes)?;
    Ok((graph, selected))
}

#[test]
fn configuration_declarations_keep_independent_authored_values_and_file_level_provenance()
-> Result<()> {
    let owned = tempfile::tempdir()?;
    let (graph, selected) = graph_fixture(owned.path())?;
    let result = config_settings::collect(
        &graph,
        "owned",
        owned.path(),
        &["PreviewMode".into()],
        &selected,
        &[],
        &MemoryBudget::new(64 * 1024 * 1024),
    )?;
    ensure!(result.provider.status == ProviderStatus::Hit && result.items.len() == 2);
    ensure!(matches!(
        result.qualify_status(AnswerStatus::Answered),
        AnswerStatus::Partial
    ));
    let mut values = Vec::new();
    for item in &result.items {
        let value: Value = serde_json::from_str(&item.content)?;
        ensure!(item.lines.is_none() && value["source_scope"] == "file_level_parsed_declarations");
        let bytes = std::fs::read(owned.path().join(item.path.as_ref().unwrap()))?;
        ensure!(value["raw_blake3"] == blake3::hash(&bytes).to_hex().to_string());
        ensure!(value["declarations"].as_array().unwrap().len() == 1);
        ensure!(value["declarations"][0]["key"] == "PreviewMode");
        values.push(
            value["declarations"][0]["authored_value_attribute"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
        ensure!(
            value["effective_value_proven"] == false
                && value["getter_store_binding_proven"] == false
        );
        ensure!(
            !item.content.contains("OWNED_SECRET_CANARY") && !item.content.contains("unrelated")
        );
        ensure!(value["source_recovery"]["tool"] == "get_full_method_body");
        ensure!(
            value["source_recovery"]["arguments"]["expected_file_blake3"] == value["raw_blake3"]
        );
    }
    values.sort();
    ensure!(values == vec!["compact".to_owned(), "wide".to_owned()]);
    drop(graph);
    owned.close()?;
    Ok(())
}

#[test]
fn scoped_configuration_reads_exclude_unreadable_neighbors_and_withhold_changed_source()
-> Result<()> {
    let owned = tempfile::tempdir()?;
    let (graph, selected) = graph_fixture(owned.path())?;
    std::fs::remove_file(owned.path().join(OTHER))?;
    let budget = MemoryBudget::new(64 * 1024 * 1024);
    let scoped = config_settings::collect(
        &graph,
        "owned",
        owned.path(),
        &["PreviewMode".into()],
        &selected,
        &["Site/Area".into()],
        &budget,
    )?;
    ensure!(scoped.items.len() == 1 && scoped.provider.status == ProviderStatus::Hit);
    ensure!(
        !scoped
            .provider
            .note
            .as_ref()
            .unwrap()
            .contains("Site/Other")
    );
    std::fs::write(
        owned.path().join(AREA),
        format!("{CONFIG}\n<!-- Owned source edit -->\n"),
    )?;
    let current = config_settings::collect(
        &graph,
        "owned",
        owned.path(),
        &["PreviewMode".into()],
        &selected,
        &["Site/Area".into()],
        &budget,
    )?;
    ensure!(current.items.is_empty() && current.provider.status == ProviderStatus::Failed);
    ensure!(
        current
            .provider
            .note
            .as_ref()
            .unwrap()
            .contains("source changed")
    );
    drop(graph);
    owned.close()?;
    Ok(())
}

fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_core_question_preserves_configurations_getter_guidance_and_guarded_source_recovery()
-> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("ac-").tempdir_in(parent)?;
    let root = owned.path().join("root");
    for (path, source) in [
        (AREA, CONFIG),
        (OTHER, OTHER_CONFIG),
        ("Site/Area/Reader.vb", GETTER),
        ("AGENTS.md", GUIDE),
    ] {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(target, source)?;
    }
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: false,
        watch_debounce_secs: 3600,
        ..Default::default()
    })?;
    let server = Engram::new(state);
    let result = AssertUnwindSafe(async {
        let indexed = server.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned configuration question","project_type":"general","wait":true}))?).await?;
        let pid = body(&indexed).split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let answer = server.handle_ask_codebase(serde_json::from_value(json!({"project_id":pid,"question":"What declares setting `PreviewMode` and where is it read?","output_format":"json","max_tokens":65536,"deadline_ms":5000}))?).await?;
        let answer: Value = serde_json::from_str(body(&answer))?;
        let evidence = answer["evidence"].as_array().unwrap();
        let rows: Vec<_> = evidence.iter().filter(|row| row["provider"] == "config_settings").collect();
        ensure!(rows.len() == 2 && answer["status"] != "answered");
        ensure!(evidence.iter().any(|row| row["provider"] == "literal_settings"));
        ensure!(answer["project_policy_sources"].to_string().contains("preview rendering must never publish"));
        let content: Value = serde_json::from_str(rows[0]["content"].as_str().unwrap())?;
        let action = &content["source_recovery"];
        let source = server.handle_get_full_method_body(serde_json::from_value(action["arguments"].clone())?).await?;
        ensure!(body(&source).contains("PreviewMode") && body(&source).contains("preview rendering must never publish"));
        Ok::<(), anyhow::Error>(())
    }).catch_unwind().await;
    drop(server);
    owned.close()?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
