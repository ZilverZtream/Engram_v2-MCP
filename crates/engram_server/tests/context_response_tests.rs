//! Real indexed method/caller evidence must be recoverable under complete reply
//! budgets, without losing policy qualifications or hiding provider truncation.
use anyhow::{Context, Result, ensure};
use engram_core::Config;
use engram_server::{Engram, state::AppState};
use rmcp::model::{CallToolResult, RawContent};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn text(result: CallToolResult) -> String {
    match &result.content[0].raw {
        RawContent::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    }
}
fn decode(output: &str, structured: bool) -> Result<(Value, Vec<Value>, Value)> {
    if structured {
        let value: Value = serde_json::from_str(output)?;
        Ok((
            value.clone(),
            value["records"].as_array().context("records")?.clone(),
            value,
        ))
    } else {
        let mut values = output
            .split("```json\n")
            .skip(1)
            .map(|part| serde_json::from_str::<Value>(part.split("\n```").next().unwrap()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(!values.is_empty());
        let base = values.remove(0);
        let page: Value = serde_json::from_str(
            output
                .lines()
                .find_map(|line| line.strip_prefix("context_page: "))
                .context("page metadata")?,
        )?;
        Ok((base, values, page))
    }
}
fn install_field(target: &mut Value, field: &str, value: Value) {
    if field == "method_info.body_preview" {
        target["method_info"]["body_preview"] = value;
    } else {
        target[field] = value;
    }
}

async fn exercise(root: &std::path::Path) -> Result<()> {
    let repo = root.join("repo");
    std::fs::create_dir(&repo)?;
    let body = format!(
        "pub fn contexttarget(value: usize) -> usize {{\n{}    value + 1 // final-target-marker\n}}",
        (0..500)
            .map(|i| format!(
                "    // independent context line {i:04}: quoted \"text\" \\ å界 details.\n"
            ))
            .collect::<String>()
    );
    let callers = (0..2)
        .map(|i| {
            format!(
                "pub fn caller{i}(value: usize) -> usize {{\n{}    contexttarget(value)\n}}",
                (0..60)
                    .map(|j| format!("    // caller {i} independent example line {j}\n"))
                    .collect::<String>()
            )
        })
        .collect::<Vec<_>>();
    let source = format!("{body}\n{}\n", callers.join("\n"));
    std::fs::write(repo.join("context.rs"), &source)?;
    let policy = "# Context policy\nPreserve validation for callers.\nException: documented internal examples may omit it; keep this qualification. å界\n";
    std::fs::write(repo.join("AGENTS.md"), policy)?;
    let (state, _) = AppState::new(Config {
        data_dir: root.join("data"),
        allowed_roots: vec![repo.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let engram = Engram::new(state.clone());
    let indexed=text(engram.handle_index_project(serde_json::from_value(json!({"directory":repo,"project_name":"Context response fixture","project_type":"rust","wait":true}))?).await?);
    let pid = indexed
        .split("project_id: ")
        .nth(1)
        .context("project")?
        .lines()
        .next()
        .unwrap()
        .trim();
    let node = state
        .graph
        .query_nodes_in_file(pid, None, "context.rs", 20)?
        .into_iter()
        .find(|n| n.name.ends_with("contexttarget"))
        .context("target node")?;
    // Controlled indexed coverage gap. It must remain visible on every page;
    // no missing state nodes are invented to make this warning pass.
    let edges = (0..201)
        .map(|i| engram_graph::Edge {
            source_id: node.node_id.clone(),
            target_id: format!("state:session:probe{i:03}"),
            edge_kind: engram_graph::EdgeKind::ReadsState,
            namespace: "memory".into(),
            language: "rust".into(),
            weight: 1,
            generation: node.generation,
            metadata: None,
            updated_at_ms: 0,
        })
        .collect::<Vec<_>>();
    state.graph.upsert_edges(pid, &edges)?;
    let base = json!({"project_id":pid,"file_path":"context.rs","method_name":"contexttarget","include_style_profile":false,"include_state_context":true,"output_json":true});
    let legacy: Value = serde_json::from_str(&text(
        engram
            .handle_prepare_implementation_context(serde_json::from_value(base.clone())?)
            .await?,
    ))?;
    ensure!(legacy["method_body"] == body);
    let examples = legacy["pattern_examples"].as_array().context("examples")?;
    ensure!(examples.len() == 2);
    for caller in &callers {
        ensure!(examples.iter().any(|p| p["source_code"] == *caller));
    }
    ensure!(legacy["method_coverage"]["session_reads"]["status"] == "truncated");
    ensure!(
        legacy["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("session_reads"))
    );
    let mut first_continuation = None;
    for structured in [true, false] {
        let mut args = base.clone();
        args["output_json"] = json!(structured);
        args["max_tokens"] = json!(1);
        let error = engram
            .handle_prepare_implementation_context(serde_json::from_value(args.clone())?)
            .await
            .unwrap_err();
        let minimum = error.data.as_ref().context("minimum budget")?["minimum_max_tokens"]
            .as_u64()
            .unwrap() as usize;
        // Following the advertised minimum must produce the next whole record.
        args["max_tokens"] = json!(minimum);
        let retry = text(
            engram
                .handle_prepare_implementation_context(serde_json::from_value(args.clone())?)
                .await?,
        );
        ensure!(retry.len() <= minimum * 4);
        ensure!(!decode(&retry, structured)?.1.is_empty());
        let budget = minimum.max(4096);
        args["max_tokens"] = json!(budget);
        let mut rebuilt = Value::Null;
        let mut fragments: BTreeMap<(String, Option<usize>), (String, String, usize)> =
            BTreeMap::new();
        let mut observed = 0usize;
        let mut pages = 0;
        let mut hash = None;
        loop {
            let output = text(
                engram
                    .handle_prepare_implementation_context(serde_json::from_value(args.clone())?)
                    .await?,
            );
            ensure!(output.len() <= budget * 4, "complete reply budget exceeded");
            let (header, records, page) = decode(&output, structured)?;
            pages += 1;
            ensure!(pages < 100);
            if hash.is_none() {
                hash = Some(header["context_hash"].clone());
            }
            ensure!(Some(&header["context_hash"]) == hash.as_ref());
            let mandatory = &header["required_context"];
            ensure!(
                mandatory["method_coverage"] == legacy["method_coverage"]
                    && mandatory["warnings"] == legacy["warnings"]
            );
            ensure!(mandatory["project_policy_sources"] == legacy["project_policy_sources"]);
            ensure!(
                mandatory["project_policy_sources"]["sources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|p| p["content"] == policy)
            );
            ensure!(header["freshness"]["interpretation"].is_string());
            ensure!(
                page["response_page"]["offset"] == observed
                    && page["response_page"]["snapshot"] == false
            );
            if rebuilt.is_null() {
                rebuilt = mandatory.clone();
                for item in header["field_inventory"].as_array().unwrap() {
                    let field = item["field"].as_str().unwrap();
                    let value = if item["available"] == false {
                        Value::Null
                    } else if matches!(
                        field,
                        "method_body" | "style_profile" | "method_info.body_preview"
                    ) {
                        json!("")
                    } else {
                        json!(vec![Value::Null; item["items"].as_u64().unwrap() as usize])
                    };
                    install_field(&mut rebuilt, field, value);
                }
            }
            observed += records.len();
            for record in records {
                let field = record["field"].as_str().unwrap();
                let index = record["index"].as_u64().map(|n| n as usize);
                if record["kind"] == "whole_value" {
                    if let Some(i) = index {
                        rebuilt[field][i] = record["value"].clone();
                    } else {
                        install_field(&mut rebuilt, field, record["value"].clone());
                    }
                } else {
                    let entry = fragments
                        .entry((field.to_string(), index))
                        .or_insert_with(|| {
                            (
                                String::new(),
                                record["content_blake3"].as_str().unwrap().to_string(),
                                record["total_bytes"].as_u64().unwrap() as usize,
                            )
                        });
                    ensure!(record["byte_start"] == entry.0.len());
                    entry.0.push_str(record["content"].as_str().unwrap());
                    ensure!(record["byte_end"] == entry.0.len());
                    if let Some(i) = index {
                        rebuilt[field][i] = record["metadata"].clone();
                    }
                }
            }
            if page["continuation"].is_null() {
                ensure!(page["response_page"]["total_count"] == observed);
                break;
            }
            ensure!(page["truncated"] == true);
            if structured && first_continuation.is_none() {
                first_continuation = Some(page["continuation"]["arguments"].clone());
            }
            ensure!(page["continuation"]["tool"] == "prepare_implementation_context");
            args = page["continuation"]["arguments"].clone();
        }
        ensure!(pages > 1, "large context must actually page");
        for ((field, index), (content, digest, total)) in fragments {
            ensure!(
                content.len() == total
                    && blake3::hash(content.as_bytes()).to_hex().as_str() == digest
            );
            if let Some(i) = index {
                rebuilt[&field][i]["source_code"] = json!(content);
            } else {
                install_field(&mut rebuilt, &field, json!(content));
            }
        }
        ensure!(
            rebuilt == legacy,
            "complete typed context changed during paging"
        );
        args["context_offset"] = json!(observed);
        args["context_expected_hash"] = hash.unwrap();
        let empty = text(
            engram
                .handle_prepare_implementation_context(serde_json::from_value(args)?)
                .await?,
        );
        let (_, records, page) = decode(&empty, structured)?;
        ensure!(records.is_empty() && page["continuation"].is_null() && empty.len() <= budget * 4);
    }
    let continuation = first_continuation.context("literal recovery")?;
    std::fs::write(
        repo.join("AGENTS.md"),
        format!("{policy}\nNew qualification: do not mix old context.\n"),
    )?;
    let error = engram
        .handle_prepare_implementation_context(serde_json::from_value(continuation.clone())?)
        .await
        .unwrap_err();
    ensure!(error.message.contains("context_changed"));
    let restart = &error.data.as_ref().unwrap()["restart"];
    ensure!(
        restart["arguments"]["context_offset"] == 0
            && restart["arguments"]["context_expected_hash"].is_null()
    );
    std::fs::write(repo.join("AGENTS.md"), policy)?;
    let mut changed = continuation;
    changed["include_pattern_examples"] = json!(false);
    ensure!(
        engram
            .handle_prepare_implementation_context(serde_json::from_value(changed)?)
            .await
            .unwrap_err()
            .message
            .contains("context_changed")
    );
    for invalid in [
        json!({"max_tokens":0}),
        json!({"max_tokens":usize::MAX}),
        json!({"context_offset":1}),
        json!({"context_expected_hash":"invalid"}),
    ] {
        let mut args = base.clone();
        args["project_id"] = json!("unknown-project");
        args.as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        let error = engram
            .handle_prepare_implementation_context(serde_json::from_value(args)?)
            .await
            .unwrap_err();
        ensure!(
            error.message.contains("max_tokens") || error.message.contains("context_"),
            "{error}"
        );
    }
    std::fs::write(
        repo.join("context.rs"),
        source.replace("value + 1", "value + 2"),
    )?;
    let mut args = base;
    args["max_tokens"] = json!(4096);
    let error = engram
        .handle_prepare_implementation_context(serde_json::from_value(args)?)
        .await
        .unwrap_err();
    ensure!(
        error.message.contains("changed") || error.message.contains("stale"),
        "{error}"
    );
    drop(engram);
    drop(state);
    Ok(())
}

#[test]
fn bounded_context_recovers_source_and_qualifications_without_mixing_evidence() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(exercise(temporary.path()));
    drop(runtime);
    let cleanup = temporary.close();
    result?;
    cleanup?;
    Ok(())
}
