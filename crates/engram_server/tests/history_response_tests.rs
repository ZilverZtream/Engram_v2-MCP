//! Agent tasks: bound complete replies, page without skipping changes, recover
//! authored messages, preserve historical exclusions, disclose lossy cards.
use anyhow::{Context, Result, ensure};
use engram_core::Config;
use engram_server::{Engram, SearchHistoryRequest, state::AppState};
use rmcp::model::{CallToolResult, RawContent};
use serde_json::{Value, json};

fn text(result: CallToolResult) -> String {
    match &result.content[0].raw {RawContent::Text(t)=>t.text.clone(),_=>panic!("expected text")}
}

fn minimum(error: &rmcp::ErrorData) -> usize {
    error.message.split("minimum ").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap()
}

fn fixture_commit(repo: &git2::Repository, ordinal: usize, message: &str) -> Result<String> {
    let mut index=repo.index()?;
    let count=if ordinal==1 {15} else {1};
    for i in 0..count {
        let name=format!("historyprobe{i:02}.rs");
        std::fs::write(repo.workdir().unwrap().join(&name),format!("pub fn historyprobe{i:02}() -> u32 {{ {} }}\n",ordinal+1))?;
        index.add_path(std::path::Path::new(&name))?;
    }
    index.write()?;
    let tree=repo.find_tree(index.write_tree()?)?;
    let parents:Vec<_>=repo.head().ok().and_then(|h|h.peel_to_commit().ok()).into_iter().collect();
    let parents:Vec<_>=parents.iter().collect();
    let sig=git2::Signature::new("History Fixture","fixture@example.invalid",&git2::Time::new(1_700_000_000+ordinal as i64*86400,0))?;
    Ok(repo.commit(Some("HEAD"),&sig,&sig,message,&tree,&parents)?.to_string())
}

async fn run(root: &std::path::Path, mode: &str) -> Result<()> {
    let repo_dir=root.join("repo");std::fs::create_dir(&repo_dir)?;
    let repo=git2::Repository::init(&repo_dir)?;
    let mut oracle=Vec::new();
    for n in 0..3 {
        let title=format!("historyprobe {n} {}",if n==1 {"quoted \"context\" \\ å界 ".repeat(90)} else {"small change".into()});
        let message=format!("{title}\n\n{}final-marker-{n}\n","historyprobe original implementation detail.\n".repeat(50));
        let oid=fixture_commit(&repo,n,&message)?;oracle.push((oid,message));
    }
    let (state,_)=AppState::new(Config{data_dir:root.join("data"),allowed_roots:vec![repo_dir.clone()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()})?;
    let engram=Engram::new(state);
    let indexed=text(engram.handle_index_project(serde_json::from_value(json!({"directory":repo_dir,"project_name":"History response fixture","project_type":"rust","wait":true}))?).await?);
    let pid=indexed.split("project_id: ").nth(1).context("project id")?.lines().next().unwrap().trim();
    engram.handle_index_git_history(serde_json::from_value(json!({"project_id":pid,"max_commits":10,"wait":true}))?).await?;
    let base=json!({"project_id":pid,"query":"historyprobe","mode":mode,"rerank":false,"limit":3,"max_content_chars":80,"output_json":true});
    let full:Value=serde_json::from_str(&text(engram.handle_search_history(serde_json::from_value(base.clone())?).await?))?;
    let rows=full["results"].as_array().unwrap();ensure!(rows.len()==3,"{full}");
    ensure!(full["response_page"]["total_count"]==3 && full["response_page"]["total_count_scope"]=="bounded_ranking_window");
    ensure!(full["retrieval_capability"].as_str().unwrap().contains("lexical"));
    if mode=="precedent" {ensure!(full["rerank"].as_str().unwrap().contains("off"));}
    // A caller must distinguish a complete ranked window from a deliberately
    // limited one, without treating bounded text retrieval as corpus-complete.
    for limit in [1,3] {
        for structured in [true,false] {
            let mut args=base.clone();args["limit"]=json!(limit);
            args["output_json"]=json!(structured);args["max_tokens"]=json!(16_384);
            let page=text(engram.handle_search_history(serde_json::from_value(args)?).await?);
            if structured {
                let value:Value=serde_json::from_str(&page)?;
                if mode=="precedent" {
                    ensure!(value["ranked_matches_before_limit"]==3);
                    ensure!(value["more_ranked_matches"]==json!(limit==1));
                } else {
                    ensure!(value["more_ranked_matches"].is_null());
                }
            } else if mode=="precedent" {
                ensure!(page.contains(if limit==1 {"2 additional ranked matches are outside this window"} else {"no additional ranked matches beyond this window"}),"{page}");
            } else {
                ensure!(page.contains("matches beyond the retrieved window are unknown"),"{page}");
            }
        }
    }
    let expected:Vec<_>=rows.iter().map(|r|r["commit"].as_str().unwrap().to_string()).collect();
    for row in rows {
        let (oid,message)=oracle.iter().find(|(oid,_)|Some(oid.as_str())==row["commit"].as_str()).context("independent commit")?;
        ensure!(row["content_truncated"]==true);
        let recovery=&row["recovery"]["citation_chunk"];
        ensure!(recovery["tool"]=="get_chunk","{row}");
        let mut arguments=recovery["arguments"].clone();let mut recovered=String::new();
        loop {
            let page:Value=serde_json::from_str(&text(engram.handle_get_chunk(serde_json::from_value(arguments.clone())?).await?))?;
            recovered.push_str(page["content"].as_str().unwrap());
            if page["continuation"].is_null(){break;}
            arguments["citation"]=page["continuation"].clone();
        }
        ensure!(recovered.ends_with(message),"exact authored message must survive: {oid}");
        ensure!(row["recovery"]["git_message"]["arguments"].as_array().unwrap().contains(&json!(oid)));
    }
    if mode=="precedent" {
        let wide=rows.iter().find(|r|r["commit"]==oracle[1].0).unwrap();
        ensure!(wide["file_count"]==15 && wide["files"].as_array().unwrap().len()==12 && wide["files_truncated"]==true);
        ensure!(full["truncation"]["file_lists_truncated"]==true);
    }
    for structured in [true,false] {
        let mut seen=Vec::new();let mut offset=0;
        while offset<3 {
            let mut args=base.clone();args["output_json"]=json!(structured);args["offset"]=json!(offset);args["max_tokens"]=json!(1);
            let refusal=engram.handle_search_history(serde_json::from_value(args.clone())?).await.unwrap_err();
            let tokens=minimum(&refusal);args["max_tokens"]=json!(tokens);
            let page=text(engram.handle_search_history(serde_json::from_value(args)?).await?);
            ensure!(page.len()<=tokens*4,"complete reply exceeds advertised minimum");
            let ids=if structured {
                let value:Value=serde_json::from_str(&page)?;
                ensure!(value["response_page"]["snapshot"]==false);
                value["results"].as_array().unwrap().iter().map(|r|r["commit"].as_str().unwrap().to_string()).collect::<Vec<_>>()
            } else {
                ensure!(page.contains("ranking_window:") && page.contains("not a snapshot"));
                page.lines().filter_map(|l|l.strip_prefix("commit: ").map(str::to_string)).collect()
            };
            ensure!(!ids.is_empty());offset+=ids.len();seen.extend(ids);
        }
        ensure!(seen==expected,"paging skips or repeats ranked changes");
    }
    for extra in [json!({"as_of_rev":oracle[1].0}),json!({"date_before":1_700_000_000+2*86400})] {
        let mut args=base.clone();args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let filtered:Value=serde_json::from_str(&text(engram.handle_search_history(serde_json::from_value(args)?).await?))?;
        ensure!(!filtered["results"].as_array().unwrap().iter().any(|r|r["commit"]==oracle[2].0));
    }
    for structured in [true,false] {
        for empty in [json!({"date_before":0}),json!({"offset":200})] {
            let mut args=base.clone();args.as_object_mut().unwrap().extend(empty.as_object().unwrap().clone());
            args["output_json"]=json!(structured);args["max_tokens"]=json!(1);
            let error=engram.handle_search_history(serde_json::from_value(args.clone())?).await.unwrap_err();
            let tokens=minimum(&error);args["max_tokens"]=json!(tokens);
            let output=text(engram.handle_search_history(serde_json::from_value(args)?).await?);
            ensure!(output.len()<=tokens*4);
            if structured {
                let value:Value=serde_json::from_str(&output)?;ensure!(value["results"].as_array().unwrap().is_empty());
                ensure!(value["response_page"]["next_offset"].is_null());
                ensure!(value["next_tools"]==json!([]));
                ensure!(value["next_tools_status"]=="empty_history_page_no_historical_lead");
                if empty.get("offset").is_some() {
                    ensure!(value["empty_page_reason"]=="offset_outside_ranking_window" && value["restart_offset"]==0);
                } else {
                    ensure!(value["empty_page_reason"]=="no_results_in_ranking_window");
                }
            } else {
                ensure!(output.contains("next_tools: []") && output.contains("empty_history_page_no_historical_lead"));
                ensure!(!output.contains("further matches may exist") && !output.contains("increase limit"),"{output}");
                if empty.get("offset").is_some() {
                    ensure!(output.contains("outside this 3-result window; restart at offset=0"),"{output}");
                } else {
                    ensure!(output.contains("ranking_window_empty:") && output.contains("no next page"),"{output}");
                }
            }
        }
    }
    for invalid in [json!({"max_tokens":0}),json!({"max_tokens":usize::MAX}),json!({"offset":201})] {
        let mut args=base.clone();args["project_id"]=json!("not-a-project");args.as_object_mut().unwrap().extend(invalid.as_object().unwrap().clone());
        let req:SearchHistoryRequest=serde_json::from_value(args)?;
        let error=engram.handle_search_history(req).await.unwrap_err();
        ensure!(error.message.contains("max_tokens") || error.message.contains("offset"),"validate before project lookup: {error}");
    }
    drop(engram);drop(repo);
    Ok(())
}

fn owned_test(mode: &str) -> Result<()> {
    // Runtime/background tasks and all stores close before fixture teardown,
    // also when assertions fail or panic unwinds through these bindings.
    let temporary=tempfile::tempdir()?;
    let runtime=tokio::runtime::Runtime::new()?;
    let result=runtime.block_on(run(temporary.path(),mode));
    drop(runtime);
    let cleanup=temporary.close();
    result?;cleanup?;Ok(())
}

#[test]
fn text_history_budget_paging_recovery_and_cutoffs() -> Result<()> {owned_test("text")}

#[test]
fn precedent_budget_paging_recovery_and_cutoffs() -> Result<()> {owned_test("precedent")}
