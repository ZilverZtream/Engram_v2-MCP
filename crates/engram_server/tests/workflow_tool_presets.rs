//! Workflow surfaces retain concrete source and maintenance dependencies.
use anyhow::{Result, ensure};
use engram_core::{Config, config::ToolPreset};
use engram_server::{AppState, Engram, tool_surface};
use futures::FutureExt;
use rmcp::{ServerHandler, model::CallToolResult};
use serde_json::{Value, json};
use std::{collections::BTreeSet, panic::AssertUnwindSafe};

const FILE: &str = "Site/Entry.aspx.vb";
const SOURCE: &str = "Public Class Entry\nInherits System.Web.UI.Page\nPublic Function Save() As Boolean\nReturn True\nEnd Function\nEnd Class\n";
const GUIDE: &str = "# Entry guidance\nException: read-only previews never submit.\n";

fn roots(preset: ToolPreset) -> &'static [&'static str] {
    match preset {
        ToolPreset::Development => &[
            "batch_read",
            "get_method_info",
            "search_history",
            "pre_push_audit",
        ],
        ToolPreset::Qa => &[
            "derive_test_matrix",
            "analyze_business_logic",
            "trace_state_usage",
            "query_schema",
            "batch_read",
            "pre_push_audit",
        ],
        ToolPreset::Migration => &[
            "query_schema",
            "analyze_database_intelligence",
            "generate_migration_blueprint",
            "generate_migration_plan",
            "generate_migration_scaffold",
            "pre_push_audit",
        ],
        _ => &[],
    }
}

#[test]
fn omitted_configuration_preserves_legacy_defaults_and_explicit_presets_override_both_legacy_values()
-> Result<()> {
    let mut config = Config::default();
    ensure!(config.effective_tool_preset() == ToolPreset::Full);
    config.advertise_all_tools = false;
    ensure!(config.effective_tool_preset() == ToolPreset::Core);
    let mut value = serde_json::to_value(config)?;
    value.as_object_mut().unwrap().remove("tool_preset");
    ensure!(
        serde_json::from_value::<Config>(value.clone())?.effective_tool_preset()
            == ToolPreset::Core
    );
    for (name, wanted) in [
        ("full", ToolPreset::Full),
        ("core", ToolPreset::Core),
        ("development", ToolPreset::Development),
        ("qa", ToolPreset::Qa),
        ("migration", ToolPreset::Migration),
    ] {
        for legacy in [false, true] {
            value["advertise_all_tools"] = json!(legacy);
            value["tool_preset"] = json!(name);
            ensure!(
                serde_json::from_value::<Config>(value.clone())?.effective_tool_preset() == wanted
            );
        }
    }
    value["tool_preset"] = json!("qaa");
    ensure!(serde_json::from_value::<Config>(value).is_err());
    Ok(())
}

#[test]
fn workflow_surfaces_contain_registered_roots_and_shared_dependencies_with_identical_schemas()
-> Result<()> {
    let all = Engram::tool_registry().list_all();
    let full = tool_surface::advertised(all.clone(), true);
    for (preset, count) in [
        (ToolPreset::Development, 38),
        (ToolPreset::Qa, 40),
        (ToolPreset::Migration, 40),
    ] {
        let selected = tool_surface::advertised_with_preset(all.clone(), preset);
        let actual: BTreeSet<_> = selected.iter().map(|tool| tool.name.as_ref()).collect();
        let expected: BTreeSet<_> = tool_surface::CORE_TOOLS
            .iter()
            .chain(roots(preset))
            .copied()
            .collect();
        ensure!(
            actual == expected && actual.len() == count,
            "{preset:?}: {actual:?}"
        );
        for name in [
            "list_projects",
            "project_info",
            "get_chunk",
            "get_full_method_body",
            "read_memory_bank",
            "get_review_decisions",
            "get_job_status",
            "cancel_job",
            "pre_push_audit",
            "trace_user_request",
        ] {
            ensure!(actual.contains(name), "{preset:?} lacks {name}");
        }
        for tool in &selected {
            let original = full.iter().find(|row| row.name == tool.name).unwrap();
            ensure!(tool.input_schema == original.input_schema);
            let sanitized = tool_surface::sanitize_schema(&tool.input_schema);
            let json = serde_json::to_string(&sanitized)?;
            ensure!(
                !json.contains("\"$ref\"")
                    && !json.contains("\"$defs\"")
                    && !json.contains("\"definitions\"")
            );
        }
    }
    ensure!(tool_surface::advertised_with_preset(all.clone(), ToolPreset::Core).len() == 34);
    ensure!(tool_surface::advertised_with_preset(all.clone(), ToolPreset::Full).len() == all.len());
    Ok(())
}

struct Fixture {
    e: Engram,
    owned: tempfile::TempDir,
    root: std::path::PathBuf,
    project: String,
}
impl Fixture {
    async fn new(preset: ToolPreset) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("wp-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        for (path, content) in [
            (
                "Site/Entry.aspx",
                "<%@ Page Language='VB' CodeFile='Entry.aspx.vb' Inherits='Entry' %>\n",
            ),
            (FILE, SOURCE),
            ("Site/main.js", "function preview() { return true; }\n"),
            ("Site/model.ts", "export const preview: boolean = true;\n"),
            (
                "Site/schema.sql",
                "CREATE TABLE dbo.Customer (Id int NOT NULL);\nCREATE TABLE audit.Customer (Stamp int NULL);\n",
            ),
            ("AGENTS.md", GUIDE),
        ] {
            std::fs::write(root.join(path), content)?;
        }
        {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned preset fixture",
                &tree,
                &[],
            )?;
        }
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: false,
            tool_preset: Some(preset),
            watch_debounce_secs: 3600,
            ..Default::default()
        })?;
        let e = Engram::new(state);
        let indexed = e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned workflow preset","project_type":"general","wait":true}))?).await?;
        let project = body(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            e,
            owned,
            root,
            project,
        })
    }
    fn close(self) -> Result<()> {
        let Self { e, owned, .. } = self;
        drop(e);
        owned.close()?;
        Ok(())
    }
}
fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effective_presets_preserve_schema_clarification_and_executable_edit_maintenance_handoffs()
-> Result<()> {
    for preset in [
        ToolPreset::Development,
        ToolPreset::Qa,
        ToolPreset::Migration,
    ] {
        let fixture = Fixture::new(preset).await?;
        let result = AssertUnwindSafe(async {
            let info = fixture.e.get_info();
            ensure!(info.instructions.as_deref().unwrap().contains(&format!("{} profile", preset.as_str())));
            let answer = fixture.e.handle_ask_codebase(serde_json::from_value(json!({"project_id":fixture.project,
                "question":"What are the columns of the \"Customer\" table?", "output_format":"json", "max_tokens":65536,"deadline_ms":5000}))?).await?;
            let answer: Value = serde_json::from_str(body(&answer))?;
            let candidates: Vec<_> = answer["evidence"].as_array().unwrap().iter().filter_map(|row| {
                let content = row["content"].as_str()?;
                let value: Value = serde_json::from_str(content).ok()?;
                (value["status"] == "ambiguous").then_some(value)
            }).collect();
            ensure!(!candidates.is_empty(), "{answer}");
            let wanted = if preset == ToolPreset::Development { "ask_codebase" } else { "query_schema" };
            for candidate in &candidates { ensure!(candidate["recovery"]["tool"] == wanted); }
            let begin = fixture.e.handle_begin_edit_session(serde_json::from_value(json!({"project_id":fixture.project,
                "session_id":"worker","planned_files":[FILE],"output_json":true,"max_tokens":65536}))?).await?;
            let begin: Value = serde_json::from_str(body(&begin))?;
            let revision = &begin["summary"]["session_revision"];
            let done = fixture.e.handle_complete_edit_session(serde_json::from_value(json!({"project_id":fixture.project,
                "session_id":"worker","session_revision":revision,"edited_files":[FILE],"output_json":true,"max_tokens":65536}))?).await?;
            let done: Value = serde_json::from_str(body(&done))?;
            let calls = done["summary"]["instruction_maintenance"]["calls"].as_array().unwrap();
            ensure!(calls.len() == 1 && calls[0]["tool"] == "pre_push_audit");
            let reviewed = fixture.e.handle_pre_push_audit(serde_json::from_value(calls[0]["arguments"].clone())?).await?;
            let reviewed: Value = serde_json::from_str(body(&reviewed))?;
            ensure!(reviewed["authored_files_written"] == 0);
            ensure!(std::fs::read_to_string(fixture.root.join("AGENTS.md"))? == GUIDE);
            Ok::<(), anyhow::Error>(())
        }).catch_unwind().await;
        fixture.close()?;
        match result {
            Ok(result) => result?,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    Ok(())
}
