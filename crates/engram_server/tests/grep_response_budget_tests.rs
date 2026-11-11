use anyhow::{Result, ensure};
use engram_server::{AppState, Engram, models::GrepProjectRequest};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::collections::HashMap;

fn text(reply:CallToolResult)->Result<String> {
    ensure!(reply.is_error!=Some(true));
    Ok(reply.content.iter().filter_map(|row|row.as_text().map(|v|v.text.as_str())).collect::<Vec<_>>().join("\n"))
}
fn source(i:usize)->String {
    let mut body=format!("pub fn grepbudget{i}() -> usize {{\n");
    for line in 0..80 {
        if line==40 {body.push_str("    // grepbudgetmarker\n");}
        else {body.push_str(&format!("    // file{i} context{line}: {}\n","🍊 orchard evidence ".repeat(6)));}
    }
    body.push_str(&format!("    {i}\n}}\n")); body
}
fn locations(hits:&Value)->Vec<String> {
    hits.as_array().unwrap().iter().map(|hit|format!("{}:{}:{}",hit["file_path"].as_str().unwrap(),hit["line"],hit["column"])).collect()
}

#[test]
fn grep_budget_pages_preserve_json_markdown_overlay_warnings_and_recovery()->Result<()> {
    let owned=tempfile::tempdir()?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result=runtime.block_on(async {
        let root=owned.path().join("repo"); std::fs::create_dir_all(root.join("src"))?;
        drop(git2::Repository::init(&root)?);
        let mut oracle=HashMap::new();
        for i in 0..12 {let path=format!("src/item{i}.rs");let body=source(i);std::fs::write(root.join(&path),&body)?;oracle.insert(path,body);}
        let (state,_)=AppState::new(engram_core::Config {
            data_dir:owned.path().join("data"),allowed_roots:vec![root.clone()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()
        })?;
        let engram=Engram::new(state);
        let indexed=text(engram.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned grep budget","project_type":"rust","wait":true}))?).await?)?;
        let pid=indexed.split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let generation=engram.state.registry.active_source_generation(pid)?;
        let req=|offset:usize,tokens:Option<usize>,json_output:bool,freshness:&str|->Result<GrepProjectRequest> {
            Ok(serde_json::from_value(json!({"project_id":pid,"pattern":"grepbudgetmarker","max_results":12,
                "path_prefix":"src/","context_before":4,"context_after":4,"freshness":freshness,
                "offset":offset,"max_tokens":tokens,"output_json":json_output}))?)
        };
        for overlay in [false,true] {
            if overlay {
                let changed=oracle["src/item0.rs"].replace("grepbudgetmarker","removed_marker");
                std::fs::write(root.join("src/item0.rs"),&changed)?;oracle.insert("src/item0.rs".into(),changed);
                let added=source(99);std::fs::write(root.join("src/new.rs"),&added)?;oracle.insert("src/new.rs".into(),added);
            }
            let freshness=if overlay {"strict"} else {"off"};
            let baseline:Value=serde_json::from_str(&text(engram.handle_grep_project(req(0,None,true,freshness)?).await?)?)?;
            let expected=locations(&baseline["matches"]);ensure!(expected.len()==12);
            let expected_files:std::collections::HashSet<_>=oracle.iter().filter(|(_,body)|body.contains("grepbudgetmarker")).map(|(path,_)|path.as_str()).collect();
            ensure!(baseline["matches"].as_array().unwrap().iter().map(|hit|hit["file_path"].as_str().unwrap()).collect::<std::collections::HashSet<_>>()==expected_files);
            ensure!(baseline["result_limit_reached"]==true);
            if overlay {ensure!(baseline["index_stale_warning"].as_str().unwrap().contains("Working-tree overlay"));}
            for json_output in [false,true] {
                let mut found=Vec::new();
                while found.len()<expected.len() {
                    let output=text(engram.handle_grep_project(req(found.len(),Some(2048),json_output,freshness)?).await?)?;
                    ensure!(output.len()<=8192,"Whole grep reply exceeded the caller estimate");
                    let (visible,page,actions)=if json_output {
                        let value:Value=serde_json::from_str(&output)?;
                        ensure!(value["result_limit_reached"]==baseline["result_limit_reached"] && value["coverage"]==baseline["coverage"]);
                        ensure!(value["stale_paths"]==baseline["stale_paths"] && value["index_stale_warning"]==baseline["index_stale_warning"]);
                        ensure!(value["response_budget"]["accounting"]=="ceil(utf8_text_bytes/4)");
                        for hit in value["matches"].as_array().unwrap() {
                            if let Some(citation)=hit.get("citation_recovery") {
                                let stored:Value=serde_json::from_str(&text(engram.handle_get_chunk(serde_json::from_value(citation["arguments"].clone())?).await?)?)?;
                                let content=stored["content"].as_str().unwrap();
                                ensure!(oracle[hit["file_path"].as_str().unwrap()].contains(content));
                                ensure!(stored["raw_content_hash"]==format!("blake3-raw-utf8:{}",blake3::hash(content.as_bytes())));
                                ensure!(stored["identity"]["doc_id"]==hit["doc_id"] && stored["document_complete"]==true);
                            } else {ensure!(hit["source"]=="working_tree" && hit.get("doc_id").is_none());}
                        }
                        let actions=value["matches"].as_array().unwrap().iter().map(|hit|hit["current_source_recovery"]["arguments"].clone()).collect::<Vec<_>>();
                        (locations(&value["matches"]),value["response_page"].clone(),actions)
                    } else {
                        ensure!(output.contains("Result limit reached") && output.contains("Fewer results do not prove exhaustive"));
                        ensure!(output.contains("[engram] gen=") && output.contains("These reads do not verify stored citations"));
                        if overlay {ensure!(output.contains(baseline["index_stale_warning"].as_str().unwrap()) && output.contains("src/item0.rs") && output.contains("src/new.rs"));}
                        let visible=regex::Regex::new(r"(?m)^\*\*(src/[^:]+:\d+:\d+)\*\*:")?.captures_iter(&output).map(|m|m[1].to_owned()).collect();
                        let page=serde_json::from_str(output.lines().find_map(|line|line.strip_prefix("response_page: ")).unwrap())?;
                        let actions=output.lines().filter_map(|line|line.strip_prefix("current get_full_method_body arguments: `").and_then(|line|line.strip_suffix('`'))).map(serde_json::from_str).collect::<Result<Vec<Value>,_>>()?;
                        (visible,page,actions)
                    };
                    ensure!(!visible.is_empty() && visible.len()<12 && visible==expected[found.len()..found.len()+visible.len()]);
                    ensure!(page["window_matches"]==12 && page["returned"]==visible.len());
                    ensure!(actions.len()==visible.len());
                    for action in actions {
                        let source=&oracle[action["file_path"].as_str().unwrap()];
                        let recovered:Value=serde_json::from_str(&text(engram.handle_get_full_method_body(serde_json::from_value(action)?).await?)?)?;
                        let start=recovered["line_start"].as_u64().unwrap() as usize;
                        let end=recovered["line_end"].as_u64().unwrap() as usize;
                        let lines=source.lines().collect::<Vec<_>>();
                        ensure!(recovered["source_code"]==lines[start-1..end].join("\n"));
                        ensure!(recovered["source_file_hash"]==blake3::hash(source.as_bytes()).to_hex().as_str());
                    }
                    found.extend(visible);
                    if found.len()<12 {ensure!(page["next_offset"]==found.len() && page["window_end"]==false);}
                    else {ensure!(page["next_offset"].is_null() && page["window_end"]==true);}
                }
                ensure!(found==expected);
                let error=engram.handle_grep_project(req(0,Some(1),json_output,freshness)?).await.unwrap_err();
                let minimum=error.message.split("minimum ").nth(1).unwrap().split_whitespace().next().unwrap().parse::<usize>()?;
                let retry=text(engram.handle_grep_project(req(0,Some(minimum),json_output,freshness)?).await?)?;
                ensure!(retry.len()<=minimum*4);
                for offset in [12,1000,usize::MAX] {
                    let output=text(engram.handle_grep_project(req(offset,Some(2048),json_output,freshness)?).await?)?;
                    ensure!(output.len()<=8192);
                    let page:Value=if json_output {serde_json::from_str::<Value>(&output)?["response_page"].clone()}
                        else {serde_json::from_str(output.lines().find_map(|line|line.strip_prefix("response_page: ")).unwrap())?};
                    ensure!(page["returned"]==0 && page["next_offset"].is_null());
                }
            }
        }
        for invalid in [0,usize::MAX] {ensure!(engram.handle_grep_project(req(0,Some(invalid),true,"off")?).await.unwrap_err().message.contains("positive"));}
        let mut empty=req(0,Some(2048),true,"off")?;empty.pattern="absentbudgetfixture".into();
        let output=text(engram.handle_grep_project(empty).await?)?;
        ensure!(output.len()<=8192 && serde_json::from_str::<Value>(&output)?["matches"]==json!([]));
        ensure!(engram.state.registry.active_source_generation(pid)?==generation);
        for (path,body) in oracle {ensure!(std::fs::read_to_string(root.join(path))?==body);}
        Ok::<(),anyhow::Error>(())
    });
    drop(runtime);
    let cleanup=owned.close();
    match (result,cleanup) {
        (Err(error),Err(cleanup))=>Err(error.context(format!("Owned grep fixture cleanup failed: {cleanup}"))),
        (Err(error),_)=>Err(error),(Ok(()),cleanup)=>cleanup.map_err(Into::into),
    }
}
