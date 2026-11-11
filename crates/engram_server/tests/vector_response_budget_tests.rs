use engram_server::{AppState, Engram, models::VectorSearchRequest};
use rmcp::model::CallToolResult;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::collections::HashMap;

fn text(reply: CallToolResult) -> Result<String> {
    ensure!(reply.is_error != Some(true));
    Ok(reply.content.iter().filter_map(|row| row.as_text().map(|t| t.text.as_str())).collect::<Vec<_>>().join("\n"))
}

#[test]
fn vector_response_budget_preserves_ranked_window_and_exact_recovery() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result = runtime.block_on(async {
        let root = owned.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("AGENTS.md"), "# Source rules\nPreserve the citrus vector invariant before editing.\n")?;
        let mut oracle = HashMap::new();
        for i in 0..12 {
            let path = format!("src/item{i}.rs");
            let body = format!("pub fn vectorbudget{i}() -> usize {{\n    // {}\n    {i}\n}}\n", "🍊 unicode evidence ".repeat(80));
            std::fs::write(root.join(&path), &body)?;
            oracle.insert(path, body);
        }
        drop(git2::Repository::init(&root)?);
        let (state, _) = AppState::new(engram_core::Config {
            data_dir: owned.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: if cfg!(feature="vector") { "local" } else { "fts_only" }.into(),
            llm_backend: "none".into(), ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = text(engram.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned vector budget","project_type":"rust","wait":true
        }))?).await?)?;
        let pid = indexed.split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let generation = engram.state.registry.active_source_generation(pid)?;
        let request = |offset: usize, budget: Option<usize>, mmr: bool| -> Result<VectorSearchRequest> {
            Ok(serde_json::from_value(json!({"project_id":pid,"query":"unicode evidence","top_k":8,
                "include_content":true,"max_content_chars":4000,"include_path_prefixes":["src/"],
                "offset":offset,"max_tokens":budget,"use_mmr":mmr}))?)
        };
        let ids = |reply: &str| -> Result<Vec<String>> {
            Ok(regex::Regex::new(r"(?m)^\[\d+\].* doc_id=(\S+) chunk_id=")?
                .captures_iter(reply).map(|m| m[1].to_owned()).collect())
        };
        // More source documents than top_k exercises MMR's candidate selection.
        if cfg!(feature="vector") {
            for mmr in [false, true] {
                let baseline = text(engram.handle_vector_search(request(0,None,mmr)?).await?)?;
                let expected = ids(&baseline)?;
                ensure!(expected.len()==8 && baseline.len()>8192);
                ensure!(!baseline.contains("response_budget:"));
                let mut found = Vec::new();
                while found.len()<expected.len() {
                    let output = text(engram.handle_vector_search(request(found.len(),Some(2048),mmr)?).await?)?;
                    ensure!(output.len()<=8192 && output.contains("non-semantic trigram-projection"));
                    ensure!(output.contains("[engram] gen=") && output.contains("accounting=ceil(utf8_text_bytes/4)"));
                    let visible = ids(&output)?;
                    ensure!(!visible.is_empty() && visible.len()<8);
                    ensure!(visible==expected[found.len()..found.len()+visible.len()], "Window changed while paging, MMR={mmr}");
                    let mut citations=0;
                    for line in output.lines() {
                        if let Some(action)=line.strip_prefix("citation_chunk: get_chunk(").and_then(|line|line.strip_suffix(')')) {
                            let arguments:Value=serde_json::from_str(action)?;
                            ensure!(arguments["project_id"]==pid && arguments["namespace"]=="memory");
                            let recovered:Value=serde_json::from_str(&text(engram.handle_get_chunk(serde_json::from_value(arguments)?).await?)?)?;
                            let body=&oracle[recovered["metadata"]["path"].as_str().unwrap()];
                            ensure!(recovered["content"].as_str()==Some(body.as_str()) && output.contains(body));
                            ensure!(recovered["raw_content_hash"]==format!("blake3-raw-utf8:{}",blake3::hash(body.as_bytes())));
                            ensure!(recovered["document_complete"]==true && recovered["continuation"].is_null());
                            citations+=1;
                        }
                    }
                    ensure!(citations==visible.len());
                    let action=output.lines().find_map(|line|line.strip_prefix("full_chunk: get_chunk(").and_then(|line|line.strip_suffix(')'))).unwrap();
                    let guided=text(engram.handle_get_chunk(serde_json::from_str(action)?).await?)?;
                    ensure!(guided.contains("citrus vector invariant"));
                    found.extend(visible);
                    if found.len()<8 { ensure!(output.contains(&format!("next_page: offset={};",found.len()))); }
                    else { ensure!(output.contains("ranking_window_end: 8") && !output.contains("next_page: offset=")); }
                }
                ensure!(found==expected);
                let error=engram.handle_vector_search(request(0,Some(1),mmr)?).await.unwrap_err();
                let minimum=error.message.split("minimum ").nth(1).unwrap().split_whitespace().next().unwrap().parse::<usize>()?;
                let retry=text(engram.handle_vector_search(request(0,Some(minimum),mmr)?).await?)?;
                ensure!(!ids(&retry)?.is_empty() && retry.len()<=minimum*4);
                let tail=text(engram.handle_vector_search(request(3,None,mmr)?).await?)?;
                ensure!(ids(&tail)?==expected[3..]);
            }
        }
        for invalid in [0,usize::MAX] {
            let error=engram.handle_vector_search(request(0,Some(invalid),false)?).await.unwrap_err();
            ensure!(error.message.contains("max_tokens") && error.message.contains("positive"));
        }
        for offset in [8,200,usize::MAX] {
            let output=text(engram.handle_vector_search(request(offset,Some(512),false)?).await?)?;
            ensure!(ids(&output)?.is_empty() && output.len()<=2048 && output.contains("No search was run"));
        }
        let mut empty=request(0,Some(512),false)?;
        empty.include_path_prefixes=Some(vec!["absent/".into()]);
        let no_hits=text(engram.handle_vector_search(empty.clone()).await?)?;
        ensure!(ids(&no_hits)?.is_empty() && no_hits.len()<=2048 && no_hits.contains("No vector search results"));
        empty.max_tokens=Some(1);
        let error=engram.handle_vector_search(empty.clone()).await.unwrap_err();
        let minimum=error.message.split("minimum ").nth(1).unwrap().split_whitespace().next().unwrap().parse::<usize>()?;
        empty.max_tokens=Some(minimum);
        ensure!(text(engram.handle_vector_search(empty).await?)?.len()<=minimum*4);
        ensure!(engram.state.registry.active_source_generation(pid)?==generation);
        for (path,body) in oracle { ensure!(std::fs::read_to_string(root.join(path))?==body); }
        Ok::<(),anyhow::Error>(())
    });
    drop(runtime);
    let cleanup=owned.close();
    match (result,cleanup) {
        (Err(error),Err(cleanup))=>Err(error.context(format!("Owned vector fixture cleanup failed: {cleanup}"))),
        (Err(error),_)=>Err(error),
        (Ok(()),cleanup)=>cleanup.map_err(Into::into),
    }
}
