#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 — integration regression: the generated
//! OciusX CLAUDE.md told the agent to call `detect_incomplete_changes(files=[...])`
//! while the request accepts only `edited_files` (deny_unknown_fields), so
//! the instruction fails on first use. The earlier "cannot recur" test
//! covered the AGENTS.md renderer, not produce_claude_md.
//!
//! Contract: every `tool_name(param=` mention in the CLAUDE.md that
//! produce_claude_md generates names a real tool and a parameter that tool's
//! input schema actually has. Checked against the live tool router, so a
//! renamed field or a typo in the generator can never ship again.

use engram_core::config::Config;
use engram_server::models::ProduceClaudeMdRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use std::collections::{BTreeMap, BTreeSet};

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    std::fs::write(
        root.join("Site/App_Code/a.vb"),
        "Public Class a\n    Public Function GetByID(id As Integer) As String\n        Return \"x\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/default.aspx"),
        "<%@ Page Language=\"VB\" %>\n<asp:Label ID=\"lbl\" runat=\"server\" />\n",
    )
    .unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "ClaudeMdFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

/// `tool_name(param=` mentions in generated prose — the shape the CLAUDE.md
/// uses to tell an agent how to call a tool.
fn tool_param_mentions(text: &str) -> Vec<(String, String)> {
    let re = regex::Regex::new(r"`?\b([a-z][a-z0-9_]+)\(([a-z][a-z0-9_]*)=").unwrap();
    re.captures_iter(text)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tool_call_example_in_the_generated_claude_md_is_valid_against_the_tool_schema() {
    let (_tmp, engram, pid) = build().await;

    // The live contract: tool name -> its input-schema property names.
    let mut schema: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in engram.tool_router.list_all() {
        let props = t
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        schema.insert(t.name.to_string(), props);
    }
    assert!(
        schema.contains_key("detect_incomplete_changes"),
        "router lists the tool"
    );

    let req: ProduceClaudeMdRequest =
        serde_json::from_value(serde_json::json!({"project_id": pid, "merge_existing": false}))
            .unwrap();
    let res = engram.handle_produce_claude_md(req).await.unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();

    let mentions = tool_param_mentions(&text);
    assert!(
        mentions
            .iter()
            .any(|(t, _)| t == "detect_incomplete_changes"),
        "the workflow section names detect_incomplete_changes with a parameter:\n{text}"
    );
    let mut bad = Vec::new();
    for (tool, param) in &mentions {
        if let Some(props) = schema.get(tool) {
            if !props.contains(param) {
                bad.push(format!("{tool}({param}=…) — schema has {:?}", props));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "generated CLAUDE.md tells the agent to call tools with parameters that do not exist:\n  {}",
        bad.join("\n  ")
    );
}
