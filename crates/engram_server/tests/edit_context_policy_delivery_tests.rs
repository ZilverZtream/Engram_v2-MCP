//! The pre-edit entry point delivers authored exceptions before edits are proposed.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_graph::{Edge, EdgeKind};
use engram_server::{AppState, Engram, read_report_delivery as delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};
use std::path::PathBuf;

const GUIDE: &str = "Preserve ownership checks. Exception: read-only previews never save.\n";
const METHOD: &str = "---\napplyTo: Site/**\n---\nPreserve the method exception.\n";
const CALLER: &str = "---\napplyTo: Caller/**\n---\nKeep caller ordering.\n";
struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    root: PathBuf,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        for name in ["Site", "Caller", ".github/instructions"] {
            std::fs::create_dir_all(root.join(name))?;
        }
        for (path, body) in [
            (
                "Site/order.ts",
                "export function saveOrder() { return true; }\n",
            ),
            (
                "Caller/dispatch.ts",
                "import { saveOrder } from '../Site/order';\nexport function dispatchOrder() { return saveOrder(); }\n",
            ),
            ("AGENTS.md", GUIDE),
            (".github/instructions/method.instructions.md", METHOD),
            (".github/instructions/caller.instructions.md", CALLER),
            (
                ".github/instructions/other.instructions.md",
                "---\napplyTo: Other/**\n---\nUNRELATED_POLICY\n",
            ),
        ] {
            std::fs::write(root.join(path), body)?;
        }
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: true,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned pre-edit policy","project_type":"general","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().unwrap().text;
        let project = text
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        // The test selects a known call between real indexed source declarations;
        // parser/resolver recall is outside this policy-delivery contract.
        let target = server
            .state
            .graph
            .query_nodes_by_symbol_name(&project, "saveOrder", Some("Site/order.ts"), 8)?
            .into_iter()
            .find(|n| n.node_type == "function")
            .unwrap();
        let caller = server
            .state
            .graph
            .query_nodes_by_symbol_name(&project, "dispatchOrder", Some("Caller/dispatch.ts"), 8)?
            .into_iter()
            .find(|n| n.node_type == "function")
            .unwrap();
        server.state.graph.upsert_edges(
            &project,
            &[Edge {
                source_id: caller.node_id,
                target_id: target.node_id,
                namespace: "memory".into(),
                language: "typescript".into(),
                edge_kind: EdgeKind::Calls,
                weight: 100,
                generation: target.generation,
                metadata: None,
                updated_at_ms: 1,
            }],
        )?;
        Ok(Self {
            server,
            owned,
            root,
            project,
        })
    }
    async fn context(&self, output_json: bool) -> Result<CallToolResult> {
        Ok(self
            .server
            .handle_get_method_edit_context(serde_json::from_value(json!({
                "project_id":self.project,"file_path":"Site/order.ts","method_name":"saveOrder",
                "include_history":false,"include_business_logic":false,"output_json":output_json
            }))?)
            .await?)
    }
    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}
fn text(reply: &CallToolResult) -> &str {
    &reply.content[0].as_text().unwrap().text
}
fn report(project: &str, controls: Value) -> Result<delivery::Report> {
    let mut arguments = controls;
    arguments["project_id"] = json!(project);
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":"get_method_edit_context","arguments":arguments}))?;
    Ok(delivery::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_edit_context_preserves_live_method_and_returned_caller_guidance_in_both_formats()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let original = fixture.context(true).await?;
    let body: Value = serde_json::from_str(text(&original))?;
    ensure!(
        !body["caller_excerpts"].as_array().unwrap()[0]["numbered_source"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    let sources = body["project_policy_sources"]["sources"]
        .as_array()
        .unwrap();
    for (path, expected) in [
        ("AGENTS.md", GUIDE),
        (".github/instructions/method.instructions.md", METHOD),
        (".github/instructions/caller.instructions.md", CALLER),
    ] {
        let row = sources.iter().find(|s| s["path"] == path).unwrap();
        ensure!(
            row["content"] == expected
                && row["blake3"] == blake3::hash(expected.as_bytes()).to_hex().to_string()
        );
    }
    ensure!(!text(&original).contains("UNRELATED_POLICY"));
    ensure!(body.get("edit_safety").is_some() && body.get("freshness").is_some());
    let markdown = fixture.context(false).await?;
    ensure!(
        text(&markdown).contains(GUIDE)
            && text(&markdown).contains(METHOD)
            && text(&markdown).contains(CALLER)
    );
    let revised = GUIDE.to_owned() + "Retain this new authored qualification.\n";
    std::fs::write(fixture.root.join("AGENTS.md"), &revised)?;
    let updated = fixture.context(true).await?;
    let updated: Value = serde_json::from_str(text(&updated))?;
    ensure!(
        updated["project_policy_sources"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == "AGENTS.md" && s["content"] == revised)
    );
    ensure!(updated["edit_safety"] == body["edit_safety"]);
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_large_pre_edit_policy_recovers_without_repeating_source_or_analysis() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let large = GUIDE.to_owned()
        + &"Authored context retains all source and scope qualifications.\n".repeat(1200);
    std::fs::write(fixture.root.join("AGENTS.md"), &large)?;
    let original = fixture.context(true).await?;
    let exact = serde_json::to_string(&original)?;
    ensure!(text(&original).len() > 40960);
    let mut manifests = Vec::new();
    for controls in [json!({}), json!({"max_tokens":2048})] {
        let deferred = report(&fixture.project, controls)?
            .finish(&fixture.server.state.cfg.data_dir, original.clone())?;
        let manifest: Value = serde_json::from_str(text(&deferred))?;
        ensure!(manifest["report_content"] == "DEFERRED_NOT_SUPPLIED");
        manifests.push(manifest);
    }
    std::fs::remove_file(fixture.root.join("AGENTS.md"))?;
    std::fs::remove_file(fixture.root.join("Site/order.ts"))?;
    for manifest in manifests {
        let mut action = manifest["response_delivery"]["next_call"].clone();
        let mut restored = String::new();
        for _ in 0..100 {
            ensure!(action["arguments"].get("method_name").is_none());
            let page = report(&fixture.project, action["arguments"].clone())?
                .recover(&fixture.server.state.cfg.data_dir)?
                .unwrap();
            let page: Value = serde_json::from_str(text(&page))?;
            ensure!(page["start_byte"] == restored.len());
            restored.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            action = page["next_call"].clone();
        }
        ensure!(restored == exact);
        ensure!(
            blake3::hash(restored.as_bytes()).to_hex().as_str()
                == manifest["response_delivery"]["response_id"]
                    .as_str()
                    .unwrap()
        );
    }
    fixture.close()
}
