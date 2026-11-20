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
        Self::with_source("export function saveOrder() { return true; }\n").await
    }
    async fn with_source(source: &str) -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("source");
        for name in ["Site", "Caller", ".github/instructions"] {
            std::fs::create_dir_all(root.join(name))?;
        }
        for (path, body) in [
            ("Site/order.ts", source),
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
    async fn safety(&self, output_json: bool) -> Result<CallToolResult> {
        Ok(self
            .server
            .handle_check_edit_safety(serde_json::from_value(json!({
                "project_id":self.project,"file_path":"Site/order.ts","method_name":"saveOrder",
                "output_json":output_json
            }))?)
            .await?)
    }
    async fn completeness(
        &self,
        output_json: bool,
        max_tokens: Option<usize>,
    ) -> Result<CallToolResult> {
        Ok(self
            .server
            .handle_detect_incomplete_changes(serde_json::from_value(json!({
                "project_id":self.project,"edited_files":["Site/order.ts"],
                "output_json":output_json,"max_tokens":max_tokens
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
    report_for("get_method_edit_context", project, controls)
}
fn report_for(tool: &str, project: &str, controls: Value) -> Result<delivery::Report> {
    let mut arguments = controls;
    arguments["project_id"] = json!(project);
    let mut request: CallToolRequestParam =
        serde_json::from_value(json!({"name":tool,"arguments":arguments}))?;
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
    let safety_reply = fixture.safety(true).await?;
    let mut safety: Value = serde_json::from_str(text(&safety_reply))?;
    let policy = safety
        .as_object_mut()
        .unwrap()
        .remove("project_policy_sources")
        .unwrap();
    ensure!(safety == body["edit_safety"]);
    let safety_sources = policy["sources"].as_array().unwrap();
    for (path, expected) in [
        ("AGENTS.md", GUIDE),
        (".github/instructions/method.instructions.md", METHOD),
    ] {
        let row = safety_sources
            .iter()
            .find(|row| row["path"] == path)
            .unwrap();
        ensure!(
            row["content"] == expected
                && row["blake3"] == blake3::hash(expected.as_bytes()).to_hex().to_string()
        );
    }
    ensure!(!safety_sources.iter().any(|row| row["path"]
        == ".github/instructions/caller.instructions.md"
        || row["path"] == ".github/instructions/other.instructions.md"));
    let standalone_markdown = fixture.safety(false).await?;
    ensure!(
        text(&standalone_markdown).contains(GUIDE)
            && text(&standalone_markdown).contains(METHOD)
            && !text(&standalone_markdown).contains(CALLER)
            && !text(&standalone_markdown).contains("UNRELATED_POLICY")
    );
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
    let revised_safety = fixture.safety(true).await?;
    let mut revised_safety: Value = serde_json::from_str(text(&revised_safety))?;
    let policy = revised_safety
        .as_object_mut()
        .unwrap()
        .remove("project_policy_sources")
        .unwrap();
    ensure!(
        policy["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == "AGENTS.md" && row["content"] == revised)
    );
    ensure!(revised_safety == safety);
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_large_pre_edit_policy_recovers_without_repeating_source_or_analysis() -> Result<()>
{
    let source = format!(
        "export function saveOrder() {{\n{}  return true;\n}}\n",
        "  // Retain this selected method source in the complete pre-edit context.\n".repeat(1000)
    );
    let fixture = Fixture::with_source(&source).await?;
    let original = fixture.context(true).await?;
    let exact = serde_json::to_string(&original)?;
    ensure!(text(&original).len() > 40960);
    let body: Value = serde_json::from_str(text(&original))?;
    ensure!(body["full_source"] == source.trim_end_matches('\n'));
    ensure!(
        body["project_policy_sources"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["path"] == "AGENTS.md" && s["content"] == GUIDE)
    );
    let mut manifests = Vec::new();
    for controls in [json!({}), json!({"max_tokens":2048})] {
        let deferred = report(&fixture.project, controls)?
            .finish(&fixture.server.state.cfg.data_dir, original.clone())?;
        let manifest: Value = serde_json::from_str(text(&deferred))?;
        ensure!(manifest["report_content"] == "PARTIAL_EVIDENCE");
        let first_page = manifest["first_page"].as_str().unwrap();
        ensure!(first_page.contains("saveOrder") && first_page.contains("dispatchOrder"));
        ensure!(first_page.contains("Site/order.ts"));
        manifests.push(manifest);
    }
    let original_safety = fixture.safety(true).await?;
    let exact_safety = serde_json::to_string(&original_safety)?;
    let deferred = report_for(
        "check_edit_safety",
        &fixture.project,
        json!({"max_tokens":1024}),
    )?
    .finish(&fixture.server.state.cfg.data_dir, original_safety)?;
    ensure!(text(&deferred).len() <= 4096);
    let safety_manifest: Value = serde_json::from_str(text(&deferred))?;
    ensure!(safety_manifest["report_content"] == "DEFERRED_NOT_SUPPLIED");
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
    let mut action = safety_manifest["response_delivery"]["next_call"].clone();
    let mut restored = String::new();
    for _ in 0..100 {
        ensure!(
            action["tool"] == "check_edit_safety"
                && action["arguments"].get("method_name").is_none()
        );
        let page = report_for(
            "check_edit_safety",
            &fixture.project,
            action["arguments"].clone(),
        )?
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
    ensure!(restored == exact_safety);
    ensure!(
        blake3::hash(restored.as_bytes()).to_hex().as_str()
            == safety_manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_completeness_keeps_declared_scope_guidance_and_saved_recovery() -> Result<()> {
    let fixture = Fixture::new().await?;
    let small = fixture.completeness(true, None).await?;
    let small: Value = serde_json::from_str(text(&small))?;
    let markdown = small["report_markdown"].as_str().unwrap();
    ensure!(markdown.starts_with("# Edit completeness check"));
    ensure!(markdown.contains(GUIDE) && markdown.contains(METHOD));
    ensure!(markdown.contains(blake3::hash(GUIDE.as_bytes()).to_hex().as_str()));
    ensure!(!markdown.contains(CALLER) && !markdown.contains("UNRELATED_POLICY"));
    let plain = fixture.completeness(false, None).await?;
    ensure!(text(&plain).contains(GUIDE) && text(&plain).contains(METHOD));
    let revised = GUIDE.to_owned() + &"Retain this complete authored qualification.\n".repeat(100);
    std::fs::write(fixture.root.join("AGENTS.md"), &revised)?;
    let current = fixture.completeness(true, None).await?;
    let current: Value = serde_json::from_str(text(&current))?;
    ensure!(
        current["report_markdown"]
            .as_str()
            .unwrap()
            .contains(&revised)
    );
    let deferred = fixture.completeness(true, Some(1024)).await?;
    ensure!(text(&deferred).len() <= 4096);
    let manifest: Value = serde_json::from_str(text(&deferred))?;
    ensure!(manifest["report_content"] == "DEFERRED_NOT_SUPPLIED");
    std::fs::write(
        fixture.root.join("AGENTS.md"),
        "CURRENT_GUIDANCE_MUST_NOT_REPLACE_CAPTURE\n",
    )?;
    std::fs::remove_file(fixture.root.join("Site/order.ts"))?;
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut restored = String::new();
    for _ in 0..100 {
        ensure!(action["arguments"].get("edited_files").is_none());
        let reply = fixture
            .server
            .handle_detect_incomplete_changes(serde_json::from_value(action["arguments"].clone())?)
            .await?;
        let page: Value = serde_json::from_str(text(&reply))?;
        ensure!(page["start_byte"] == restored.len());
        restored.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        action = page["next_call"].clone();
    }
    ensure!(
        restored.len()
            == manifest["response_delivery"]["original_bytes"]
                .as_u64()
                .unwrap() as usize
    );
    ensure!(
        blake3::hash(restored.as_bytes()).to_hex().as_str()
            == manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    let saved: Value = serde_json::from_str(&restored)?;
    ensure!(saved["summary"] == current["summary"] && saved["advisory"] == true);
    let markdown = saved["report_markdown"].as_str().unwrap();
    ensure!(
        markdown.starts_with("# Edit completeness check")
            && markdown.contains(&revised)
            && markdown.contains(METHOD)
    );
    ensure!(
        !markdown.contains(CALLER)
            && !markdown.contains("CURRENT_GUIDANCE_MUST_NOT_REPLACE_CAPTURE")
    );
    fixture.close()
}
