//! Source-output admission lasts through its actual consumer, and rejected
//! updates leave the previously published source generation selected.
use anyhow::{Result, ensure};
use engram_core::{
    Config,
    memory::{AllocationGuard, MemoryBudget, Subsystem},
};
use engram_index::HybridSearchEngine;
use engram_server::{Engram, state::AppState};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn graph_output_keeps_its_charge_after_multiple_parse_batches_until_consumed() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let source = owned.path().join("source");
    std::fs::create_dir(&source)?;
    let mut files = Vec::new();
    for i in 0..60 {
        let path = source.join(format!("file{i}.ts"));
        std::fs::write(
            &path,
            format!("export function item_{i}(): number {{ return {i}; }}\n"),
        )?;
        files.push(path);
    }
    let budget = Arc::new(MemoryBudget::new(128 * 1024 * 1024));
    let engine = HybridSearchEngine::new_with_budget(
        owned.path().join("tantivy"),
        owned.path().join("lance"),
        &Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        },
        Some(budget.clone()),
    )
    .await?;
    let before = budget.used();
    let output = engine
        .extract_graph_files(
            "owned",
            &source,
            files,
            2048,
            &CancellationToken::new(),
            |_, _| {},
        )
        .await?;
    ensure!(
        output.all_files.len() == 60
            && output
                .symbols
                .iter()
                .any(|(_, symbol)| symbol.name == "item_59")
    );
    ensure!(
        budget.used() > before,
        "Returned output must not become untracked when parse work ends"
    );
    drop(output);
    ensure!(
        budget.used() == before,
        "Graph consumer completion must release its output admission"
    );
    drop(engine);
    Ok(())
}

#[tokio::test]
async fn indexed_document_buffers_release_while_returned_graph_statistics_stay_charged()
-> Result<()> {
    let owned = tempfile::tempdir()?;
    let source = owned.path().join("source");
    std::fs::create_dir(&source)?;
    let path = source.join("entry.ts");
    let body = (0..1500)
        .map(|i| format!("export function item_{i}(): number {{ return {i}; }}\n"))
        .collect::<String>();
    std::fs::write(&path, body)?;
    let budget = Arc::new(MemoryBudget::new(128 * 1024 * 1024));
    let engine = HybridSearchEngine::new_with_budget(
        owned.path().join("tantivy"),
        owned.path().join("lance"),
        &Config {
            embedding_backend: "fts_only".into(),
            ..Default::default()
        },
        Some(budget.clone()),
    )
    .await?;
    let before = budget.used();
    let output = engine
        .index_files(
            "owned",
            "memory",
            1,
            &source,
            vec![path],
            128,
            &CancellationToken::new(),
            |_, _| {},
        )
        .await?;
    ensure!(
        output.chunks >= 512,
        "Fixture must flush a retained document batch"
    );
    ensure!(
        output
            .symbols
            .iter()
            .any(|(_, symbol)| symbol.name == "item_1499")
    );
    ensure!(budget.used() > before);
    drop(output);
    ensure!(
        budget.used() == before,
        "Finished document and graph consumers must not retain admission"
    );
    drop(engine);
    Ok(())
}

#[tokio::test]
async fn retained_output_refusal_releases_resources_and_preserves_the_published_project()
-> Result<()> {
    let owned = tempfile::tempdir()?;
    let source = owned.path().join("source");
    std::fs::create_dir(&source)?;
    let path = source.join("entry.ts");
    std::fs::write(&path, "export function original(): number { return 1; }\n")?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![source.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        memory_budget_bytes: 128 * 1024 * 1024,
        tantivy_writer_memory: 15 * 1024 * 1024,
        ..Default::default()
    })?;
    let budget = state.memory_budget.clone();
    let registry = state.registry.clone();
    let server = Engram::new(state);
    let indexed = server
        .handle_index_project(serde_json::from_value(json!({
            "directory":source,"project_name":"Retained output","project_type":"general","wait":true
        }))?)
        .await?;
    let text = &indexed.content[0].as_text().unwrap().text;
    let pid = text
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let head = registry.get_meta(pid, "active_generation")?;
    ensure!(head.is_some());
    let body = (0..10000)
        .map(|i| format!("export function changed_item_{i}(): number {{ return {i}; }}\n"))
        .collect::<String>();
    std::fs::write(&path, body)?;
    let before = budget.used();
    let pressure = AllocationGuard::try_new(
        &budget,
        112 * 1024 * 1024,
        Subsystem::Misc,
        "owned concurrent workload",
    )?;
    let pressured = budget.used();
    let attempt = server
        .handle_update_project(serde_json::from_value(
            json!({"project_id":pid,"wait":true,"only_paths":["entry.ts"]}),
        )?)
        .await;
    let failure = match attempt {
        Err(error) => error.message.to_string(),
        Ok(result) => result
            .content
            .iter()
            .filter_map(|part| part.as_text().map(|text| text.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    ensure!(
        failure.contains("memory budget exceeded for retained source statistics"),
        "{failure}"
    );
    ensure!(
        registry.get_meta(pid, "active_generation")? == head,
        "Refused output must not publish a new source head"
    );
    ensure!(
        budget.used() == pressured,
        "Failed output/worker/writer ownership must release all of its reservations"
    );
    drop(pressure);
    ensure!(budget.used() == before);
    drop(server);
    drop(registry);
    drop(budget);
    Ok(())
}
