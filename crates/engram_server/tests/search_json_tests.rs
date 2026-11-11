use anyhow::{Result, ensure};
use engram_server::{AppState, Engram, state::AppEvent};
use rmcp::model::CallToolResult;
use serde_json::{Value,json};
use std::collections::HashMap;

fn text(reply:CallToolResult)->Result<String> {
    ensure!(reply.is_error!=Some(true));
    Ok(reply.content.iter().filter_map(|row|row.as_text().map(|v|v.text.as_str())).collect::<Vec<_>>().join("\n"))
}
async fn call(e:&Engram,tool:&str,args:Value)->Result<String> {
    text(match tool {
        "search_memory"=>e.handle_search_memory(serde_json::from_value(args)?).await?,
        "vector_search"=>e.handle_vector_search(serde_json::from_value(args)?).await?,
        _=>anyhow::bail!("Unexpected fixture tool"),
    })
}
fn ids(value:&Value)->Vec<String> {
    value["results"].as_array().unwrap().iter().map(|hit|hit["doc_id"].as_str().unwrap().to_owned()).collect()
}

#[test]
fn structured_search_preserves_budgets_guidance_recovery_and_scoped_identity()->Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result=runtime.block_on(async {
        let root=owned.path().join("repo"); std::fs::create_dir_all(root.join("src"))?;
        drop(git2::Repository::init(&root)?);
        let guidance="# Project instructions\n\nPreserve the quoted citrus invariant.\n";
        std::fs::write(root.join("AGENTS.md"),guidance)?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(root.join(".github/instructions/unresolved.instructions.md"),"---\napplyTo: 7\n---\nUnresolved scope must remain visible.\n")?;
        let mut oracle=HashMap::new();
        for i in 0..12 {
            let path=format!("src/item{i}.rs");
            let body=format!("pub fn jsonsearch{i}() {{\n    // {}\n}}\n","🍊 jsonneedle quoted \"evidence\" \\ ".repeat(40));
            std::fs::write(root.join(&path),&body)?; oracle.insert(path,body);
        }
        let (state,_)=AppState::new(engram_core::Config {
            data_dir:owned.path().join("data"),allowed_roots:vec![root.clone()],
            embedding_backend:if cfg!(feature="vector"){"local"}else{"fts_only"}.into(),llm_backend:"none".into(),..Default::default()
        })?;
        let e=Engram::new(state);
        let indexed=text(e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned JSON acceptance","project_type":"rust","wait":true}))?).await?)?;
        let pid=indexed.split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let generation=e.state.registry.active_source_generation(pid)?;
        let cache=e.state.projects.get(pid).unwrap().pagerank_cache.clone();
        let permit=e.state.pagerank_workers.clone().try_acquire_owned()?;
        cache.start(generation,permit).unwrap().complete(engram_graph::analysis::CentralityMetrics {pagerank:HashMap::new()});
        let mut modes=vec![("search_memory",false,false),("search_memory",true,false)];
        if cfg!(feature="vector") {modes.extend([("vector_search",true,false),("vector_search",true,true)]);}
        for (tool,semantic,mmr) in modes {
            let mut args=json!({"project_id":pid,"query":"jsonneedle","use_mmr":mmr,"include_content":true,"include_path_prefixes":["src/"]});
            if tool=="search_memory" {args["semantic"]=json!(semantic);args["fts_mode"]=json!("loose");args["max_results"]=json!(12);args["max_content_chars_per_result"]=json!(4000);}
            else {args["top_k"]=json!(8);args["max_content_chars"]=json!(4000);}
            let markdown=call(&e,tool,args.clone()).await?;
            let pattern=if tool=="search_memory" {r"(?m)^doc_id: (\S+)"}else{r"(?m)^\[\d+\].* doc_id=(\S+) chunk_id="};
            let expected=regex::Regex::new(pattern)?.captures_iter(&markdown).map(|m|m[1].to_owned()).collect::<Vec<_>>();
            ensure!(expected.len()==if tool=="search_memory"{12}else{8});
            args["output_json"]=json!(true);
            let raw=call(&e,tool,args.clone()).await?; let baseline:Value=serde_json::from_str(&raw)?;
            ensure!(raw.len()>8192 && ids(&baseline)==expected && baseline["response_budget"].is_null());
            let mut events=e.state.events_tx.subscribe();
            let mut found=Vec::new();
            while found.len()<expected.len() {
                args["offset"]=json!(found.len());args["max_tokens"]=json!(2048);
                while events.try_recv().is_ok() {}
                let raw=call(&e,tool,args.clone()).await?; let value:Value=serde_json::from_str(&raw)?;
                let visible=ids(&value);ensure!(raw.len()<=8192 && !visible.is_empty() && visible.len()<expected.len());
                ensure!(visible==expected[found.len()..found.len()+visible.len()]);
                ensure!(value["schema_version"]==1 && value["tool"]==tool && value["active_generation"]==generation);
                ensure!(value["warnings"]==baseline["warnings"] && value["freshness"].as_str().unwrap().contains("[engram] gen="));
                ensure!(value["response_budget"]["accounting"]=="ceil(utf8_text_bytes/4)");
                if semantic {ensure!(!value["warnings"].as_array().unwrap().is_empty());}
                if tool=="search_memory" {
                    ensure!(value["response_page"]["total_count"].is_null());
                    ensure!(value["guidance"]["coverage"]=="incomplete" && !value["guidance"]["omissions"].as_array().unwrap().is_empty());
                    let policy=value["guidance"]["sources"].as_array().unwrap().iter().find(|p|p["path"]=="AGENTS.md").unwrap();
                    ensure!(policy["content"]==guidance && policy["blake3"]==blake3::hash(guidance.as_bytes()).to_hex().as_str());
                    ensure!(value["guidance_scope"]["paths"].as_array().unwrap().len()>=visible.len());
                    let mut observed=None;
                    while let Ok(event)=events.try_recv() {if let AppEvent::SearchSession {hits,..}=event {observed=Some(hits.into_iter().map(|hit|hit.doc_id).collect::<Vec<_>>());}}
                    ensure!(observed.as_ref()==Some(&visible),"Observed hits differed from delivered JSON records");
                } else {ensure!(value["response_page"]["total_count"]==8 && value["response_page"]["total_count_scope"]=="bounded_ranking_window");}
                for hit in value["results"].as_array().unwrap() {
                    let source=&oracle[hit["path"].as_str().unwrap()];
                    ensure!(hit["content"]==*source && hit["content_truncated"]==false && hit["project_id"]==pid && hit["namespace"]=="memory");
                    ensure!(hit["snippet"].is_null() && hit["snippet_truncated"]==false,"Included content must not duplicate a clipped snippet");
                    let citation=hit["recovery"]["citation_chunk"]["arguments"].clone();
                    let recovered:Value=serde_json::from_str(&text(e.handle_get_chunk(serde_json::from_value(citation)?).await?)?)?;
                    ensure!(recovered["content"]==*source && recovered["raw_content_hash"]==format!("blake3-raw-utf8:{}",blake3::hash(source.as_bytes())));
                    let guided=text(e.handle_get_chunk(serde_json::from_value(hit["recovery"]["full_chunk"]["arguments"].clone())?).await?)?;
                    ensure!(guided.contains("quoted citrus invariant") && guided.contains("unresolved.instructions.md"));
                }
                found.extend(visible);
                if found.len()<expected.len() {ensure!(value["response_page"]["next_offset"]==found.len());}
            }
            ensure!(found==expected);
            args["offset"]=json!(0);args["max_tokens"]=json!(1);
            let error=call(&e,tool,args.clone()).await.unwrap_err().to_string();
            let minimum=error.split("minimum ").nth(1).unwrap().split_whitespace().next().unwrap().parse::<usize>()?;
            args["max_tokens"]=json!(minimum);ensure!(call(&e,tool,args.clone()).await?.len()<=minimum*4);
            for invalid in [0,usize::MAX] {args["max_tokens"]=json!(invalid);ensure!(call(&e,tool,args.clone()).await.unwrap_err().to_string().contains("positive"));}
            args["max_tokens"]=json!(2048);
            for offset in [200,usize::MAX] {args["offset"]=json!(offset);let raw=call(&e,tool,args.clone()).await?;let value:Value=serde_json::from_str(&raw)?;ensure!(raw.len()<=8192 && ids(&value).is_empty() && value["search_executed"]==false && value["response_page"]["next_offset"].is_null());}
            args["offset"]=json!(0);args["include_path_prefixes"]=json!(["absent/"]);
            let empty:Value=serde_json::from_str(&call(&e,tool,args.clone()).await?)?;ensure!(ids(&empty).is_empty() && empty["search_executed"]==true);
            args["include_path_prefixes"]=json!(["src/"]);
            args[if tool=="search_memory"{"max_content_chars_per_result"}else{"max_content_chars"}]=json!(7);
            let clipped:Value=serde_json::from_str(&call(&e,tool,args).await?)?;
            ensure!(clipped["results"][0]["content"]=="pub fn " && clipped["results"][0]["content_truncated"]==true);
        }
        let mut knowledge=HashMap::new();
        for (project,body) in [(pid,"jsonmemoryonly project decision 🍊"),("__user__","jsonmemoryonly user preference 🍊")] {
            text(e.handle_update_memory_bank(serde_json::from_value(json!({"project_id":project,"section_id":"shared","section":"Shared","content":body}))?).await?)?;
            knowledge.insert(project,body);
        }
        let response=call(&e,"search_memory",json!({"project_id":pid,"query":"jsonmemoryonly","search_scope":"knowledge","semantic":false,"output_json":true,"include_content":true,"max_tokens":2048})).await?;
        let value:Value=serde_json::from_str(&response)?;
        ensure!(response.len()<=8192 && ids(&value).len()==2 && value["guidance"].is_null());
        for hit in value["results"].as_array().unwrap() {
            let project=hit["project_id"].as_str().unwrap();let expected=knowledge[project];
            // Indexed chunks are line-normalized; the section citation below
            // must still recover the exact original body without that newline.
            let indexed=format!("{expected}\n");
            ensure!(hit["namespace"]=="memory_bank" && hit["content"]==indexed && hit["symbols"]==json!([]),"Wrong knowledge identity/content: {hit}");
            let chunk:Value=serde_json::from_str(&text(e.handle_get_chunk(serde_json::from_value(hit["recovery"]["citation_chunk"]["arguments"].clone())?).await?)?)?;
            ensure!(chunk["content"]==indexed && chunk["raw_content_hash"]==format!("blake3-raw-utf8:{}",blake3::hash(indexed.as_bytes())));
            ensure!(hit["source"]==if project=="__user__"{"user:memory_bank"}else{"memory_bank"});
            let action=hit["recovery"]["citation_section"]["arguments"].clone();
            ensure!(action["project_id"]==project);
            let recovered:Value=serde_json::from_str(&text(e.handle_read_memory_bank(serde_json::from_value(action)?).await?)?)?;
            ensure!(recovered["content"]==expected);
        }
        ensure!(e.state.registry.active_source_generation(pid)?==generation);
        for (path,source) in oracle {ensure!(std::fs::read_to_string(root.join(path))?==source);}
        Ok::<(),anyhow::Error>(())
    });
    drop(runtime);let cleanup=owned.close();
    match (result,cleanup) {
        (Err(error),Err(cleanup))=>Err(error.context(format!("JSON fixture cleanup: {cleanup}"))),
        (Err(error),_)=>Err(error),(Ok(()),cleanup)=>cleanup.map_err(Into::into),
    }
}
