use super::*;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::HashMap;

fn text(response: CallToolResult) -> Result<String> {
    ensure!(response.is_error != Some(true));
    Ok(response.content.iter().filter_map(|item| item.as_text().map(|item| item.text.as_str()))
        .collect::<Vec<_>>().join("\n"))
}

fn ids(output: &str) -> Vec<String> {
    output.lines().filter_map(|line| line.strip_prefix("doc_id: ").map(str::to_owned)).collect()
}

#[test]
fn search_response_budget_preserves_complete_source_pages_guidance_and_observed_hits() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let root = owned.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("AGENTS.md"), "# Project instructions\n\nPreserve the citrus budget invariant before editing source.\n")?;
        let mut oracle = HashMap::new();
        for i in 0..8 {
            let path = format!("src/item{i}.rs");
            let source = format!("pub fn budgetmarker{i}() -> usize {{\n    // {}\n    {i}\n}}\n", "🍊 unicode evidence ".repeat(80));
            std::fs::write(root.join(&path), &source)?;
            oracle.insert(path, source);
        }
        {
            let git = git2::Repository::init(&root)?;
            let mut index = git.index()?;
            index.add_all(["."], git2::IndexAddOption::DEFAULT, None)?;
            let tree = git.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned budget fixture", "fixture@example.invalid")?;
            git.commit(Some("HEAD"), &signature, &signature, "owned source baseline", &tree, &[])?;
        }
        let (state, _) = crate::AppState::new(engram_core::Config {
            data_dir: owned.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(), ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = text(engram.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned response budget","project_type":"rust","wait":true
        }))?).await?)?;
        let pid = indexed.split("project_id: ").nth(1).and_then(|s| s.lines().next()).unwrap().trim().to_string();
        let generation = engram.state.registry.active_source_generation(&pid)?;
        let cache = engram.state.projects.get(&pid).unwrap().pagerank_cache.clone();
        let permit = engram.state.pagerank_workers.clone().try_acquire_owned()?;
        cache.start(generation, permit).unwrap().complete(engram_graph::analysis::CentralityMetrics { pagerank: HashMap::new() });
        let request = |offset: usize, tokens: Option<usize>| -> Result<SearchMemoryRequest> {
            let mut value = json!({"project_id":pid,"query":"unicode evidence","semantic":false,
                "use_mmr":false,"include_content":true,"max_content_chars_per_result":4000,
                "max_results":8,"offset":offset,"include_path_prefixes":["src/"]});
            if let Some(tokens) = tokens { value["max_tokens"] = json!(tokens); }
            Ok(serde_json::from_value(value)?)
        };
        let baseline = text(engram.handle_search_memory(request(0, None)?).await?)?;
        let expected = ids(&baseline);
        ensure!(expected.len() == 8, "Fixture must return all eight source files");
        ensure!(baseline.len() > 8192, "Fixture must exceed the requested aggregate budget");
        eprintln!("unbudgeted_source_reply_bytes={}", baseline.len());
        let mut events = engram.state.events_tx.subscribe();
        let mut collected = Vec::new();
        while collected.len() < expected.len() {
            while events.try_recv().is_ok() {}
            let output = text(engram.handle_search_memory(request(collected.len(), Some(2048))?).await?)?;
            ensure!(output.len() <= 8192, "Whole reply exceeds explicit estimated-token accounting");
            ensure!(output.contains("citrus budget invariant"), "Budget dropped applicable live guidance");
            ensure!(output.contains("[engram] gen=") && output.contains("response_budget:"));
            let visible = ids(&output);
            ensure!(!visible.is_empty() && visible.len() < 8);
            ensure!(visible == expected[collected.len()..collected.len()+visible.len()]);
            let mut seen_event = false;
            while let Ok(event) = events.try_recv() {
                if let AppEvent::SearchSession { hits, .. } = event {
                    ensure!(hits.iter().map(|hit| hit.doc_id.clone()).collect::<Vec<_>>() == visible,
                        "Search event included withheld hits");
                    seen_event = true;
                }
            }
            ensure!(seen_event);
            let actions = output.lines().filter_map(|line| line.strip_prefix("citation_chunk: get_chunk("))
                .map(|line| serde_json::from_str::<Value>(line.strip_suffix(')').unwrap())).collect::<Result<Vec<_>,_>>()?;
            ensure!(actions.len() == visible.len());
            for arguments in actions {
                let recovered: Value = serde_json::from_str(&text(engram.handle_get_chunk(serde_json::from_value(arguments)?).await?)?)?;
                let path = recovered["metadata"]["path"].as_str().unwrap();
                ensure!(recovered["content"].as_str() == Some(oracle[path].as_str()));
                ensure!(recovered["document_complete"] == true && recovered["continuation"].is_null());
                ensure!(output.contains(oracle[path].as_str()), "Returned hit body was cut mid-record");
            }
            collected.extend(visible);
            ensure!(output.contains(&format!("next_page: offset={};", collected.len())), "Pagination skipped omitted hits");
        }
        ensure!(collected == expected);
        while events.try_recv().is_ok() {}
        let error = engram.handle_search_memory(request(0, Some(1))?).await.unwrap_err();
        ensure!(error.message.contains("max_tokens") && error.message.contains("minimum"));
        ensure!(!std::iter::from_fn(|| events.try_recv().ok()).any(|e| matches!(e, AppEvent::SearchSession { .. })));
        let minimum = error.message.split("minimum ").nth(1).unwrap().split_whitespace().next().unwrap().parse::<usize>()?;
        let recovered = text(engram.handle_search_memory(request(0, Some(minimum))?).await?)?;
        ensure!(!ids(&recovered).is_empty() && recovered.len() <= minimum * 4,
            "Following the reported minimum must recover a complete hit");
        for invalid in [0, usize::MAX] {
            let error = engram.handle_search_memory(request(0, Some(invalid))?).await.unwrap_err();
            ensure!(error.message.contains("max_tokens") && error.message.contains("positive"));
        }
        let empty = text(engram.handle_search_memory(request(200, Some(2048))?).await?)?;
        ensure!(ids(&empty).is_empty() && empty.len() <= 8192 && empty.contains("result_cap_reached: 200"));
        Ok::<(), anyhow::Error>(())
    });
    drop(runtime);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("Owned budget fixture cleanup failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
