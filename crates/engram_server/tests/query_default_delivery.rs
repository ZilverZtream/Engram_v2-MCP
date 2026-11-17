//! Core queries retain complete selected evidence within ordinary reply budgets.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{Engram, retained_results, state::AppState};
use rmcp::model::{CallToolRequestParam, CallToolResult, Content};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TOOLS: &[&str] = &["search_memory", "vector_search", "grep_project"];

fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

fn prepare(
    tool: &str,
    args: Value,
) -> Result<(
    CallToolRequestParam,
    Option<engram_server::read_report_delivery::Report>,
)> {
    let mut call = serde_json::from_value(json!({"name":tool,"arguments":args}))?;
    let report = retained_results::prepare(&mut call, &Engram::tool_registry().list_all())?;
    Ok((call, report))
}

fn recover(data: &Path, manifest: &Value) -> Result<CallToolResult> {
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut original = String::new();
    for _ in 0..200 {
        ensure!(
            action["arguments"].get("query").is_none()
                && action["arguments"].get("pattern").is_none()
        );
        let (_, report) = prepare(
            action["tool"].as_str().unwrap(),
            action["arguments"].clone(),
        )?;
        let reply = report.unwrap().recover(data)?.unwrap();
        ensure!(body(&reply).len() <= 40960);
        let page: Value = serde_json::from_str(body(&reply))?;
        ensure!(page["start_byte"] == original.len());
        original.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        action = page["next_call"].clone();
    }
    ensure!(
        blake3::hash(original.as_bytes()).to_hex().as_str()
            == manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    Ok(serde_json::from_str(&original)?)
}

struct Fixture {
    app: Engram,
    state: AppState,
    root: PathBuf,
    pid: String,
    temporary: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("project");
        std::fs::create_dir_all(root.join("Site"))?;
        let source = (0..40).map(|n| format!("export function operation{n:02}() {{\n")+
            &(0..12).map(|line|format!("  // budgetneedle source evidence {n:02}/{line:02}: preserve complete authorization qualifications and callable original evidence.\n")).collect::<String>()+
            "  return true;\n}\n").collect::<String>();
        std::fs::write(root.join("Site/Query.ts"), source)?;
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state.clone());
        let indexed = app.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned ordinary query delivery","project_type":"general","wait":true
        }))?).await?;
        let pid = body(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            app,
            state,
            root,
            pid,
            temporary,
        })
    }

    async fn original(&self, tool: &str, args: Value) -> Result<CallToolResult> {
        Ok(match tool {
            "grep_project" => {
                self.app
                    .handle_grep_project(serde_json::from_value(args)?)
                    .await?
            }
            "search_memory" => {
                self.app
                    .handle_search_memory(serde_json::from_value(args)?)
                    .await?
            }
            _ => unreachable!(),
        })
    }

    fn close(self) -> Result<()> {
        let Self {
            app,
            state,
            temporary,
            ..
        } = self;
        drop(app);
        drop(state);
        temporary.close()?;
        Ok(())
    }
}

#[tokio::test]
async fn indexed_ts_queries_keep_all_original_evidence_in_json_and_markdown() -> Result<()> {
    let f = Fixture::new().await?;
    for tool in ["grep_project", "search_memory"] {
        for output_json in [false, true] {
            let args = if tool == "grep_project" {
                json!({"project_id":f.pid,"pattern":"budgetneedle","max_results":200,
                    "context_before":2,"context_after":2,"output_json":output_json})
            } else {
                json!({"project_id":f.pid,"query":"budgetneedle","namespace":"memory","max_results":40,
                    "include_content":true,"max_content_chars_per_result":2000,"use_mmr":false,
                    "fts_mode":"loose","output_json":output_json})
            };
            let (call, report) = prepare(tool, args)?;
            let original = f.original(tool, json!(call.arguments.unwrap())).await?;
            let expected = serde_json::to_value(&original)?;
            let original_bytes = body(&original).len();
            if output_json {
                ensure!(
                    original_bytes > 40960,
                    "Authored indexed fixture must exercise ordinary overflow"
                );
            }
            let reply = report.unwrap().finish(&f.state.cfg.data_dir, original)?;
            ensure!(body(&reply).len() <= 40960);
            if original_bytes <= 40960 {
                ensure!(serde_json::to_value(&reply)? == expected);
            } else {
                let manifest: Value = serde_json::from_str(body(&reply))?;
                let recovered = recover(&f.state.cfg.data_dir, &manifest)?;
                ensure!(serde_json::to_value(&recovered)? == expected);
                ensure!(body(&recovered).contains("budgetneedle"));
            }
        }
    }
    ensure!(std::fs::read_to_string(f.root.join("Site/Query.ts"))?.contains("operation39"));
    f.close()
}

#[test]
fn fitting_defaults_explicit_budgets_and_capability_errors_keep_existing_contracts() -> Result<()> {
    let owned = tempfile::tempdir()?;
    for tool in TOOLS {
        let args = if *tool == "grep_project" {
            json!({"project_id":"owned","pattern":"needle"})
        } else {
            json!({"project_id":"owned","query":"needle"})
        };
        let original = CallToolResult::success(vec![Content::text(
            "Complete small selected result; read the stated qualifications.",
        )]);
        let expected = serde_json::to_value(&original)?;
        let (_, report) = prepare(tool, args.clone())?;
        let reply = report.unwrap().finish(owned.path(), original)?;
        ensure!(serde_json::to_value(reply)? == expected);
        let mut explicit = args.clone();
        explicit["max_tokens"] = json!(2048);
        let (call, report) = prepare(tool, explicit.clone())?;
        ensure!(report.is_none() && json!(call.arguments.unwrap()) == explicit);
        let error = CallToolResult::error(vec![Content::text(
            "Provider capability is unavailable; no selected result exists.".repeat(1000),
        )]);
        let expected = serde_json::to_value(&error)?;
        let (_, report) = prepare(tool, args)?;
        ensure!(serde_json::to_value(report.unwrap().finish(owned.path(), error)?)? == expected);
    }
    owned.close()?;
    Ok(())
}

#[test]
fn selected_vector_evidence_recovers_without_fresh_query_inputs_or_provider_work() -> Result<()> {
    let owned = tempfile::tempdir()?;
    let rows = (0..40).map(|n|json!({"rank":n+1,"doc_id":format!("original-{n}"),
        "content":format!("Selected source {n}: {}", "Preserve the accepted exception and original company scope. ".repeat(40)),
        "recovery":{"tool":"get_chunk","arguments":{"project_id":"owned","namespace":"memory","doc_id":format!("original-{n}")}}})).collect::<Vec<_>>();
    let original = CallToolResult::success(vec![Content::text(json!({"results":rows,
        "warnings":["Selected vector window only; original source objects are not retained."],
        "response_page":{"offset":2,"window_limit":42},
        "guidance":{"scope":"owned selected source","conditions":"Cached previews may not write; production approval remains required."}}
    ).to_string())]);
    let expected = serde_json::to_value(&original)?;
    ensure!(body(&original).len() > 40960);
    let (_, report) = prepare(
        "vector_search",
        json!({"project_id":"owned","query":"accepted source","top_k":42,"offset":2,"output_json":true}),
    )?;
    let first = report.unwrap().finish(owned.path(), original)?;
    let manifest: Value = serde_json::from_str(body(&first))?;
    let recovered = recover(owned.path(), &manifest)?;
    ensure!(serde_json::to_value(&recovered)? == expected);
    let result: Value = serde_json::from_str(body(&recovered))?;
    ensure!(
        result["results"].as_array().unwrap().len() == 40
            && result["results"][39]["doc_id"] == "original-39"
    );
    ensure!(
        result["guidance"]["conditions"]
            == "Cached previews may not write; production approval remains required."
    );
    owned.close()?;
    Ok(())
}
