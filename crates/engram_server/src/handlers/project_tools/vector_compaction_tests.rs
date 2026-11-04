//! The existing maintenance option performs real work and preserves live data.
use super::*;
use anyhow::{Result, ensure};
use serde_json::json;

fn text(result: CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|v| v.as_text())
        .map(|v| v.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn public_vector_compaction_option_preserves_source_head_and_searchable_declarations() -> Result<()>
{
    let owned = tempfile::tempdir()?;
    let outcome = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let project = owned.path().join("repo");
            std::fs::create_dir(&project)?;
            drop(git2::Repository::init(&project)?);
            std::fs::write(project.join("a.rs"),"pub fn original() -> i32 { 0 }\n")?;
            std::fs::write(project.join("b.rs"),"pub fn unchanged_sibling() -> i32 { 42 }\n")?;
            let engram = Engram::new(crate::state::AppState::new(engram_core::Config {
                data_dir:owned.path().join("data"),allowed_roots:vec![project.clone()],
                embedding_backend:if cfg!(feature="vector") {"local"} else {"fts_only"}.into(),
                ..Default::default()
            })?.0);
            let indexed = text(engram.handle_index_project(serde_json::from_value(json!({
                "directory":project,"project_name":"Compaction behavior","project_type":"rust","wait":true
            }))?).await?);
            let pid = indexed.split("project_id: ").nth(1).ok_or_else(||anyhow::anyhow!("Missing indexed project ID: {indexed}"))?.lines().next().unwrap().trim().to_owned();
            let ps = engram.state.peek_project_cached(&pid).unwrap().unwrap();
            let first_generation = project_service::get_active_generation(&engram.state,&pid).await?;
            let _old_source = ps.search.pin_source_generation(&pid,first_generation)?;
            let old_source_docs = ps.search.count_docs_in_generation(&pid,"memory",first_generation)?;
            ensure!(old_source_docs > 0);
            for n in 1..=4 {
                std::fs::write(project.join("a.rs"),format!("pub fn changed_{n}() -> i32 {{ {n} }}\n//{}\n","x".repeat(n)))?;
                let updated = text(engram.handle_update_project(serde_json::from_value(json!({"project_id":pid,"wait":true,"max_commits":1}))?).await?);
                ensure!(updated.contains("files=1"),"Actual edit must be indexed: {updated}");
            }
            let head = engram.state.registry.capture_source_head(&pid)?;
            let opt_out = text(engram.handle_incremental_indexing_gc(serde_json::from_value(json!({
                "project_id":pid,"target_generation":0,"compact_vectors":false
            }))?).await?);
            ensure!(!opt_out.contains("compaction"),"Opt-out must not claim native compaction");
            let compacted = text(engram.handle_incremental_indexing_gc(serde_json::from_value(json!({
                "project_id":pid,"target_generation":0,"compact_vectors":true
            }))?).await?);
            #[cfg(feature="vector")]
            {
                let metrics = regex::Regex::new(r"Vector compaction completed: (\d+) fragments replaced by (\d+)")?.captures(&compacted).ok_or_else(||anyhow::anyhow!("Missing actual compaction outcome: {compacted}"))?;
                ensure!(metrics[1].parse::<usize>()?>metrics[2].parse::<usize>()?,"Fragmented source must actually compact: {compacted}");
                let history = regex::Regex::new(r"Vector history maintenance removed (\d+) old versions and (\d+) bytes")?
                    .captures(&compacted).ok_or_else(||anyhow::anyhow!("Missing completed native history outcome: {compacted}"))?;
                ensure!(history[1].parse::<u64>()? > 0,"Explicit maintenance must reclaim old versions while preserving selected source rows: {compacted}");
            }
            #[cfg(not(feature="vector"))]
            {
                ensure!(compacted.contains("Skipped vector compaction: no native vector table is available."));
                ensure!(compacted.contains("Skipped vector history maintenance: no native vector table is available."));
            }
            ensure!(engram.state.registry.source_head_is_current(&head)?,"Maintenance must not publish a new source head");
            ensure!(ps.search.count_docs_in_generation(&pid,"memory",first_generation)? == old_source_docs,
                "Native history maintenance must preserve logical source rows retained by an old source reader");
            for declaration in ["changed_4","unchanged_sibling"] {
                let result = text(engram.handle_search_memory(serde_json::from_value(json!({
                    "project_id":pid,"query":declaration,"max_results":10
                }))?).await?);
                ensure!(result.contains(&format!("pub fn {declaration}()")),"Compaction changed a retrieved source declaration: {result}");
            }
            ensure!(std::fs::read_to_string(project.join("b.rs"))?=="pub fn unchanged_sibling() -> i32 { 42 }\n");
            println!("PUBLIC_COMPACTION_BEHAVIOR {compacted}");
            Ok::<_,anyhow::Error>(())
        })
    };
    owned.close()?;
    outcome
}
