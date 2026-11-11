//! Real WebForms page evidence must survive complete response budgeting.
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
        let v: Value = serde_json::from_str(output)?;
        Ok((
            v.clone(),
            v["records"].as_array().context("records")?.clone(),
            v,
        ))
    } else {
        let mut values = output
            .split("```json\n")
            .skip(1)
            .map(|p| serde_json::from_str::<Value>(p.split("\n```").next().unwrap()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let header = values.remove(0);
        let page = serde_json::from_str(
            output
                .lines()
                .find_map(|l| l.strip_prefix("context_page: "))
                .context("page metadata")?,
        )?;
        Ok((header, values, page))
    }
}
fn field<'a>(target: &'a mut Value, path: &str) -> &'a mut Value {
    if let Some((parent, name)) = path.split_once('.') {
        &mut target[parent][name]
    } else {
        &mut target[path]
    }
}
async fn call(engram: &Engram, args: Value) -> Result<String> {
    Ok(text(
        engram
            .handle_get_page_context(serde_json::from_value(args)?)
            .await?,
    ))
}

async fn recover(engram: &Engram, base: &Value, legacy: &Value, structured: bool) -> Result<Value> {
    let mut args = base.clone();
    args["output_json"] = json!(structured);
    args["max_tokens"] = json!(1);
    let error = engram
        .handle_get_page_context(serde_json::from_value(args.clone())?)
        .await
        .unwrap_err();
    let minimum = error.data.context("minimum")?["minimum_max_tokens"]
        .as_u64()
        .context("tokens")? as usize;
    args["max_tokens"] = json!(minimum);
    let retry = call(engram, args.clone()).await?;
    ensure!(retry.len() <= minimum * 4 && !decode(&retry, structured)?.1.is_empty());
    let budget = minimum.max(4096);
    args["max_tokens"] = json!(budget);
    let mut rebuilt = Value::Null;
    let mut fragments = BTreeMap::<usize, (String, String, usize)>::new();
    let mut observed = 0;
    let mut pages = 0;
    let mut first = None;
    let mut hash = Value::Null;
    loop {
        let output = call(engram, args.clone()).await?;
        ensure!(output.len() <= budget * 4);
        let (header, records, page) = decode(&output, structured)?;
        pages += 1;
        ensure!(pages < 100);
        if hash.is_null() {
            hash = header["context_hash"].clone();
        }
        ensure!(header["context_hash"] == hash);
        let required = &header["required_context"];
        for name in [
            "aspx_file",
            "codebehind_file",
            "class_name",
            "language",
            "completeness",
            "dynamic_ui_detected",
            "dynamic_ui_evidence",
            "runtime_controls_warning",
            "ui_coverage_confidence",
            "vb_traps_summary",
            "requires_authentication",
        ] {
            ensure!(required[name] == legacy[name], "required {name} changed");
        }
        ensure!(required["composition"]["warnings"] == legacy["composition"]["warnings"]);
        ensure!(
            required["composition"]["runtime_verification"]
                == legacy["composition"]["runtime_verification"]
        );
        ensure!(required["house_style"]["note"] == legacy["house_style"]["note"]);
        ensure!(header["freshness"]["interpretation"].is_string());
        ensure!(
            page["response_page"]["offset"] == observed
                && page["response_page"]["snapshot"] == false
        );
        if rebuilt.is_null() {
            rebuilt = required.clone();
            for item in header["field_inventory"].as_array().context("inventory")? {
                if item["available"] == false {
                    continue;
                }
                *field(&mut rebuilt, item["field"].as_str().unwrap()) =
                    json!(vec![Value::Null; item["items"].as_u64().unwrap() as usize]);
            }
        }
        observed += records.len();
        for record in records {
            let name = record["field"].as_str().unwrap();
            let index = record["index"].as_u64().unwrap() as usize;
            if record["kind"] == "whole_value" {
                field(&mut rebuilt, name)[index] = record["value"].clone();
            } else {
                ensure!(name == "methods" && record["content_field"] == "full_body");
                let entry = fragments.entry(index).or_insert_with(|| {
                    (
                        String::new(),
                        record["content_blake3"].as_str().unwrap().to_owned(),
                        record["total_bytes"].as_u64().unwrap() as usize,
                    )
                });
                ensure!(record["byte_start"] == entry.0.len());
                entry.0.push_str(record["content"].as_str().unwrap());
                ensure!(record["byte_end"] == entry.0.len());
                rebuilt["methods"][index] = record["metadata"].clone();
            }
        }
        if page["continuation"].is_null() {
            ensure!(page["response_page"]["total_count"] == observed);
            break;
        }
        ensure!(page["truncated"] == true && page["continuation"]["tool"] == "get_page_context");
        if first.is_none() {
            first = Some(page["continuation"]["arguments"].clone());
        }
        args = page["continuation"]["arguments"].clone();
    }
    ensure!(pages > 1);
    for (index, (content, digest, total)) in fragments {
        ensure!(
            content.len() == total && blake3::hash(content.as_bytes()).to_hex().as_str() == digest
        );
        rebuilt["methods"][index]["full_body"] = json!(content);
    }
    ensure!(
        rebuilt == *legacy,
        "complete typed page context must reconstruct exactly"
    );
    args["context_offset"] = json!(observed);
    args["context_expected_hash"] = hash;
    let empty = call(engram, args.clone()).await?;
    let (_, rows, page) = decode(&empty, structured)?;
    ensure!(rows.is_empty() && page["continuation"].is_null() && empty.len() <= budget * 4);
    args["context_offset"] = json!(observed + 1);
    ensure!(
        engram
            .handle_get_page_context(serde_json::from_value(args)?)
            .await
            .unwrap_err()
            .message
            .contains("context_offset")
    );
    first.context("first continuation")
}

async fn exercise(root: &std::path::Path) -> Result<()> {
    let repo = root.join("repo");
    std::fs::create_dir(&repo)?;
    drop(git2::Repository::init(&repo)?);
    let methods = (0..4).map(|i| format!("    protected void PageAction{i}(object sender, System.EventArgs e)\n    {{\n{}        int result = {i}; // action-{i}-end\n    }}", (0..100).map(|j|format!("        // authored source {i}:{j:03}: quoted \"evidence\" \\ å界.\n")).collect::<String>())).collect::<Vec<_>>();
    let source = format!(
        "public partial class PageFixture : System.Web.UI.Page\n{{\n{}\n}}\n",
        methods.join("\n")
    );
    let markup = format!(
        "<%@ Page Language=\"C#\" CodeFile=\"PageFixture.aspx.cs\" Inherits=\"PageFixture\" MasterPageFile=\"Missing.master\" %>\n<form runat=\"server\">\n{}{}\n</form>",
        (0..150)
            .map(|i| format!("<asp:TextBox ID=\"field{i:03}\" runat=\"server\" />\n"))
            .collect::<String>(),
        (0..4)
            .map(|i| format!(
                "<asp:Button ID=\"action{i}\" runat=\"server\" OnClick=\"PageAction{i}\" />\n"
            ))
            .collect::<String>()
    );
    std::fs::write(repo.join("PageFixture.aspx"), &markup)?;
    std::fs::write(repo.join("PageFixture.aspx.cs"), &source)?;
    std::fs::write(
        repo.join("Sibling.aspx"),
        "<asp:TextBox ID=\"sibling\" runat=\"server\" /><asp:Panel CssClass=\"alert alert-info\" runat=\"server\" />",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: root.join("data"),
        allowed_roots: vec![repo.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let engram = Engram::new(state);
    let indexed = text(engram.handle_index_project(serde_json::from_value(json!({"directory":repo,"project_name":"Page response","project_type":"dotnet_webforms_cs","wait":true}))?).await?);
    let pid = indexed
        .split("project_id: ")
        .nth(1)
        .context("project")?
        .lines()
        .next()
        .unwrap()
        .trim();
    let base = json!({"project_id":pid,"aspx_file":"PageFixture.aspx","include_method_bodies":true,"output_json":true});
    let mut legacy: Value = serde_json::from_str(&call(&engram, base.clone()).await?)?;
    ensure!(
        legacy
            .as_object_mut()
            .unwrap()
            .remove("freshness")
            .is_some()
    );
    ensure!(legacy["methods"].as_array().unwrap().len() == methods.len());
    for method in &methods {
        ensure!(
            legacy["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["full_body"] == *method)
        );
    }
    for i in 0..150 {
        ensure!(
            legacy["controls"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["server_id"] == format!("field{i:03}"))
        );
    }
    ensure!(
        !legacy["composition"]["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let continuation = recover(&engram, &base, &legacy, true).await?;
    recover(&engram, &base, &legacy, false).await?;
    let mut omitted = base.clone();
    omitted["include_codebehind"] = json!(false);
    omitted["include_house_style"] = json!(false);
    let mut omitted_legacy: Value = serde_json::from_str(&call(&engram, omitted.clone()).await?)?;
    ensure!(
        omitted_legacy
            .as_object_mut()
            .unwrap()
            .remove("freshness")
            .is_some()
    );
    ensure!(omitted_legacy["completeness"]["codebehind"]["status"] == "not_run");
    ensure!(omitted_legacy["house_style"].is_null());
    recover(&engram, &omitted, &omitted_legacy, true).await?;
    let mut changed = continuation.clone();
    changed["include_method_bodies"] = json!(false);
    ensure!(
        engram
            .handle_get_page_context(serde_json::from_value(changed)?)
            .await
            .unwrap_err()
            .message
            .contains("context_changed")
    );
    std::fs::write(
        repo.join("PageFixture.aspx"),
        format!("{markup}\n<!-- changed evidence -->"),
    )?;
    let error = engram
        .handle_get_page_context(serde_json::from_value(continuation)?)
        .await
        .unwrap_err();
    ensure!(error.message.contains("context_changed"));
    let restart = &error.data.context("restart")?["restart"];
    ensure!(
        restart["tool"] == "get_page_context"
            && restart["arguments"]["context_offset"] == 0
            && restart["arguments"]["context_expected_hash"].is_null()
    );
    for invalid in [
        json!({"max_tokens":0}),
        json!({"max_tokens":usize::MAX}),
        json!({"context_offset":1}),
        json!({"context_expected_hash":"bad"}),
    ] {
        let mut args = json!({"project_id":"not registered","aspx_file":"absent.aspx"});
        args.as_object_mut()
            .unwrap()
            .extend(invalid.as_object().unwrap().clone());
        let error = engram
            .handle_get_page_context(serde_json::from_value(args)?)
            .await
            .unwrap_err();
        ensure!(error.message.contains("max_tokens") || error.message.contains("context_"));
    }
    Ok(())
}

#[test]
fn bounded_page_context_recovers_source_controls_and_provider_qualifications() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(exercise(temporary.path()));
    drop(runtime);
    let cleanup = temporary.close();
    result?;
    cleanup?;
    Ok(())
}
