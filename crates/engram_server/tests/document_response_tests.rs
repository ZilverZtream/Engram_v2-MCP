use anyhow::{Result, ensure};
use engram_server::{AppState, Engram};
use rmcp::{ErrorData, model::CallToolResult};
use serde_json::{Value, json};

fn text(reply: CallToolResult) -> String {
    reply
        .content
        .iter()
        .filter_map(|r| r.as_text().map(|r| r.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}
async fn call(e: &Engram, tool: &str, args: Value) -> Result<String, ErrorData> {
    match tool {
        "get_chunk" => e
            .handle_get_chunk(serde_json::from_value(args).unwrap())
            .await
            .map(text),
        "read_memory_bank" => e
            .handle_read_memory_bank(serde_json::from_value(args).unwrap())
            .await
            .map(text),
        _ => unreachable!(),
    }
}
fn hash(body: &str) -> String {
    format!("blake3-raw-utf8:{}", blake3::hash(body.as_bytes()))
}

async fn recover(e: &Engram, tool: &str, base: &Value, expected: &str) -> Result<()> {
    for json_output in [false, true] {
        let mut args = base.clone();
        args["max_tokens"] = json!(2048);
        args["output_json"] = json!(json_output);
        let first = call(e, tool, args.clone()).await?;
        ensure!(first.len() <= 8192);
        let (mut recovered, page) = if json_output {
            let v: Value = serde_json::from_str(&first)?;
            ensure!(
                v["tool"] == tool
                    && v["identity"] == *base
                    && v["raw_content_hash"] == hash(expected)
            );
            (
                v["content"].as_str().unwrap().to_owned(),
                v["response_page"].clone(),
            )
        } else {
            let line = first
                .lines()
                .find_map(|l| l.strip_prefix("response_page: "))
                .unwrap();
            let page: Value = serde_json::from_str(line)?;
            let end = page["end"].as_u64().unwrap() as usize;
            ensure!(first.contains(&expected[..end]));
            (expected[..end].to_owned(), page)
        };
        ensure!(
            page["total_count"] == expected.len(),
            "Chunk byte count differs from independent source range: {page}, expected {}",
            expected.len()
        );
        ensure!(page["next"].is_null() || page["next"]["tool"] == tool);
        let mut next = page["next"]["arguments"].clone();
        let mut count = 0;
        while !next.is_null() {
            let raw = call(e, tool, next.clone()).await?;
            ensure!(raw.len() <= 8192);
            let value: Value = serde_json::from_str(&raw)?;
            let body = value["content"].as_str().unwrap();
            ensure!(
                !body.is_empty()
                    && value["identity"] == *base
                    && value["raw_content_hash"] == hash(expected)
            );
            ensure!(
                value["returned_range"]["byte_start"] == recovered.len()
                    && value["slice_hash"] == hash(body)
            );
            ensure!(value["budget_scope"] == "complete_serialized_reply_utf8_bytes");
            recovered.push_str(body);
            count += 1;
            if value["continuation"].is_null() {
                break;
            }
            next["citation"] = value["continuation"].clone();
            ensure!(count < 100);
        }
        ensure!(recovered == expected);
        args["max_tokens"] = json!(1);
        let error = call(e, tool, args.clone()).await.unwrap_err();
        let minimum = error.data.unwrap()["minimum_max_tokens"].as_u64().unwrap();
        args["max_tokens"] = json!(minimum);
        ensure!(call(e, tool, args.clone()).await?.len() <= minimum as usize * 4);
        for bad in [0, usize::MAX] {
            args["max_tokens"] = json!(bad);
            ensure!(
                call(e, tool, args.clone())
                    .await
                    .unwrap_err()
                    .message
                    .contains("positive")
            );
        }
    }
    // Whole lines, explicit ranges and quote proof all keep their meaning.
    let mut args = base.clone();
    args["max_tokens"] = json!(2048);
    args["citation"] = json!({});
    let mut reconstructed = String::new();
    loop {
        let raw = call(e, tool, args.clone()).await?;
        ensure!(raw.len() <= 8192);
        let value: Value = serde_json::from_str(&raw)?;
        let content = value["content"].as_str().unwrap();
        reconstructed.push_str(content);
        if value["continuation"].is_null() {
            break;
        }
        ensure!(content.ends_with('\n'));
        args["citation"] = value["continuation"].clone();
    }
    ensure!(reconstructed == expected);
    args["citation"] = json!({"start":1,"end":2});
    args["max_tokens"] = json!(1);
    let error = call(e, tool, args.clone()).await.unwrap_err();
    let minimum = error.data.unwrap()["minimum_max_tokens"].as_u64().unwrap();
    args["max_tokens"] = json!(minimum);
    let explicit: Value = serde_json::from_str(&call(e, tool, args.clone()).await?)?;
    ensure!(explicit["requested_range_complete"] == true && explicit["returned_range"]["end"] == 2);
    args["citation"] =
        json!({"start":1,"end":2,"expected_raw_hash":hash(expected),"verify_quote":"evidence"});
    args["max_tokens"] = json!(2048);
    let proof: Value = serde_json::from_str(&call(e, tool, args.clone()).await?)?;
    ensure!(
        proof.get("content").is_none() && proof["quote_verification"]["status"] == "exact_match"
    );
    args["citation"]["expected_raw_hash"] = json!(hash("changed"));
    ensure!(
        call(e, tool, args)
            .await
            .unwrap_err()
            .message
            .contains("citation_hash_mismatch")
    );
    Ok(())
}

#[test]
fn document_budgets_recover_exact_sources_and_keep_guidance() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result=runtime.block_on(async {
        let root=owned.path().join("repo");std::fs::create_dir_all(root.join("src"))?;
        drop(git2::Repository::init(&root)?);
        let source=format!("pub fn documentbudget() {{\n{} }}\n", "    // 🍊 evidence \"quoted\" \\ café\n".repeat(450));
        std::fs::write(root.join("src/evidence.rs"),&source)?;
        let guidance="# Instructions\nPreserve the citation invariant.\n";
        std::fs::write(root.join("AGENTS.md"),guidance)?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(root.join(".github/instructions/unresolved.instructions.md"),"---\napplyTo: 7\n---\nScope unresolved.\n")?;
        let (state,_)=AppState::new(engram_core::Config {data_dir:owned.path().join("data"),allowed_roots:vec![root.clone()],
            embedding_backend:if cfg!(feature="vector"){"local"}else{"fts_only"}.into(),llm_backend:"none".into(),..Default::default()})?;
        let e=Engram::new(state);
        let indexed=text(e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Document budgets","project_type":"rust","wait":true}))?).await?);
        let pid=indexed.split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let generation=e.state.registry.active_source_generation(pid)?;
        let hits:Value=serde_json::from_str(&text(e.handle_search_memory(serde_json::from_value(json!({"project_id":pid,"query":"documentbudget","semantic":false,"output_json":true}))?).await?))?;
        let hit=&hits["results"][0];
        let id=hit["doc_id"].clone();ensure!(id.is_string());
        let start=hit["line_start"].as_u64().unwrap() as usize;
        let end=hit["line_end"].as_u64().unwrap() as usize;
        let source_chunk=source.split_inclusive('\n').skip(start-1).take(end-start+1).collect::<String>();
        let chunk=json!({"project_id":pid,"namespace":"memory","doc_id":id});
        recover(&e,"get_chunk",&chunk,&source_chunk).await?;
        let body="Exact 🍊 evidence \"quotes\" \\ café\r\n".repeat(3000);
        for project in [pid,"__user__"] {
            e.handle_update_memory_bank(serde_json::from_value(json!({"project_id":project,"section_id":"budget","section":"Budget","content":body,"review_after_ms":1}))?).await?;
            recover(&e,"read_memory_bank",&json!({"project_id":project,"section":"budget"}),&body).await?;
        }
        for json_output in [false,true] {
            let mut guided=chunk.clone();guided["inject_rules"]=json!(true);guided["output_json"]=json!(json_output);guided["max_tokens"]=json!(1);
            let error=call(&e,"get_chunk",guided.clone()).await.unwrap_err();
            let minimum=error.data.unwrap()["minimum_max_tokens"].as_u64().unwrap();guided["max_tokens"]=json!(minimum);
            let raw=call(&e,"get_chunk",guided).await?;ensure!(raw.len()<=minimum as usize*4);
            ensure!(raw.contains("Preserve the citation invariant") && raw.contains("unresolved.instructions.md"));
            if json_output {
                let value:Value=serde_json::from_str(&raw)?;ensure!(value["content"]==source_chunk && value["content_status"]=="transformed" && value["raw_content_hash"].is_null());
                ensure!(value["metadata"]["guidance"]["coverage"]=="incomplete" && value["truncated"]==false);
            } else { ensure!(raw.contains(&source_chunk)); }
        }
        let old_hash=hash(&body);
        e.handle_update_memory_bank(serde_json::from_value(json!({"project_id":pid,"section_id":"budget","section":"Budget","content":"changed"}))?).await?;
        ensure!(call(&e,"read_memory_bank",json!({"project_id":pid,"section":"budget","max_tokens":2048,"citation":{"unit":"utf8_bytes","start":5,"expected_raw_hash":old_hash}})).await.unwrap_err().message.contains("citation_hash_mismatch"));
        let missing:Value=serde_json::from_str(&call(&e,"read_memory_bank",json!({"project_id":pid,"section":"absent","output_json":true,"max_tokens":2048})).await?)?;
        ensure!(missing["status"]=="not_found");
        ensure!(e.state.registry.active_source_generation(pid)?==generation && std::fs::read_to_string(root.join("src/evidence.rs"))?==source);
        Ok::<(),anyhow::Error>(())
    });
    drop(runtime);
    let cleanup = owned.close();
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Document fixture cleanup: {cleanup}")))
        }
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup.map_err(Into::into),
    }
}
