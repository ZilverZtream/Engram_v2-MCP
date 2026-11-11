#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 — integration regression: the generated
//! pilot-corpus CLAUDE.md told the agent to call `detect_incomplete_changes(files=[...])`
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
    drop(git2::Repository::init(&root).unwrap());
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
    drop(engram);
    _tmp.close().unwrap();
}

fn ledger(text: &str) -> serde_json::Value {
    let section = text.rsplit("# .engram/instruction-generation.json (").next().unwrap();
    let json = section.split_once("```json\n").unwrap().1.split_once("\n```").unwrap().0;
    serde_json::from_str(json).unwrap()
}

async fn add_rule(engram: &Engram, pid: &str, id: &str, scope: &str, body: &str) {
    engram.handle_add_repo_rule(serde_json::from_value(serde_json::json!({
        "project_id":pid,"rule_id":id,"file_pattern":scope,"rule_text":body
    })).unwrap()).await.unwrap();
}

async fn generate(engram: &Engram, pid: &str, write: bool) -> String {
    let result = engram.handle_produce_claude_md(serde_json::from_value(serde_json::json!({
        "project_id":pid,"write_to_disk":write,"generate_agents_md":true,"merge_existing":false
    })).unwrap()).await.unwrap();
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn preview_preserves_manual_scope_exceptions_and_more_than_eight_rules_without_writing_files() {
    let (temp, engram, pid) = build().await;
    let root = temp.path().join("proj");
    for n in 0..12 {
        add_rule(&engram, &pid, &format!("manual-{n}"), if n % 2 == 0 { "Site/*.vb" } else { "Admin/*.vb" },
            &format!("Do not commit borrowed context {n}.\nException: explicitly owned context {n} may be committed.")).await;
    }
    let text = generate(&engram, &pid, false).await;
    let data = ledger(&text);
    let inputs = data["inputs"].as_array().unwrap();
    assert_eq!(inputs.len(), 12);
    assert!(inputs.iter().all(|input| input["authority"] == "accepted_manual" && input["delivery"] == "preview_returned"));
    assert!(inputs.iter().all(|input| input["original"]["rule_text"].as_str().unwrap().contains("Exception:")));
    assert!(inputs.iter().all(|input| input["native_scoped_loading"] == false));
    let root_text = text.split_once("# CLAUDE.md (").unwrap().1.split_once("# .claude/rules/").unwrap().0;
    assert!(!root_text.contains("Do not commit borrowed context"), "no second unqualified root mandate");
    assert!(text.contains("Condition: apply ONLY"));
    assert!(!root.join("CLAUDE.md").exists());
    assert!(!root.join(".claude/rules/engram-repository-policy.md").exists());
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn written_catalog_and_ledger_report_actual_policy_and_preserve_human_edits_on_refresh() {
    let (temp, engram, pid) = build().await;
    let root = temp.path().join("proj");
    add_rule(&engram, &pid, "cr_named_but_manual", "Site/*.vb", "Fix typo only in exported labels.\nException: persisted identifiers stay unchanged.").await;
    let first = ledger(&generate(&engram, &pid, true).await);
    assert_eq!(first["inputs"][0]["delivery"], "written");
    let catalog = root.join(".claude/rules/engram-repository-policy.md");
    let bytes = std::fs::read(&catalog).unwrap();
    assert_eq!(first["proposed_catalog_blake3"], blake3::hash(&bytes).to_hex().to_string());
    assert_eq!(first["observed_catalog_blake3"], first["proposed_catalog_blake3"]);
    let original_root = std::fs::read(root.join("CLAUDE.md")).unwrap();
    let original_agents = std::fs::read(root.join("AGENTS.md")).unwrap();
    let second = ledger(&generate(&engram, &pid, true).await);
    assert_eq!(second["inputs"][0]["delivery"], "unchanged_receipt_owned");
    assert_eq!(std::fs::read(root.join("CLAUDE.md")).unwrap(), original_root);
    assert_eq!(std::fs::read(root.join("AGENTS.md")).unwrap(), original_agents);
    std::fs::write(&catalog, "Human instruction and an accepted exception.\n").unwrap();
    add_rule(&engram, &pid, "cr_named_but_manual", "Site/*.vb", "A proposed new revision; keep the exception.").await;
    let third_text = generate(&engram, &pid, true).await;
    let third = ledger(&third_text);
    assert_eq!(third["inputs"][0]["delivery"], "preserved_conflict");
    assert_ne!(third["observed_catalog_blake3"], third["proposed_catalog_blake3"]);
    assert_eq!(std::fs::read_to_string(&catalog).unwrap(), "Human instruction and an accepted exception.\n");
    assert!(std::fs::read_to_string(root.join(".engram/instruction-generation.json")).unwrap().contains("preserved_conflict"));
    assert!(third_text.contains("A proposed new revision; keep the exception."));
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn catalog_overflow_and_unknown_legacy_authority_are_recoverable_without_scope_promotion() {
    let (temp, engram, pid) = build().await;
    let large = format!("{}\nException: this final qualification must not disappear.", "x".repeat(33 * 1024));
    add_rule(&engram, &pid, "large", "Site/*.vb", &large).await;
    engram.state.registry.put_repo_rule(&pid, &engram_core::RepoRule { rule_id: "legacy".into(), file_pattern: "Admin/*.vb".into(),
        rule_text: "An older scoped constraint with an exception.".into(), priority: 5, updated_at_ms: 1, introduced_at: None, provenance: None }).unwrap();
    let data = ledger(&generate(&engram, &pid, false).await);
    let inputs = data["inputs"].as_array().unwrap();
    let overflow = inputs.iter().find(|row| row["original"]["rule_id"] == "large").unwrap();
    assert_eq!(overflow["original"]["rule_text"], large);
    assert_eq!(overflow["placement"], "recoverable_ledger");
    assert_eq!(overflow["delivery"], "returned_in_recovery_ledger");
    let legacy = inputs.iter().find(|row| row["original"]["rule_id"] == "legacy").unwrap();
    assert_eq!(legacy["authority"], "legacy_unverified");
    assert_eq!(legacy["original"]["file_pattern"], "Admin/*.vb");
    assert_eq!(engram.state.registry.list_repo_rules(&pid).unwrap().len(), 2, "generation does not adopt or rewrite stored policy");
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn publication_failure_returns_completed_writes_and_complete_policy_recovery() {
    let (temp, engram, pid) = build().await;
    let root = temp.path().join("proj");
    add_rule(&engram, &pid, "manual", "Site/*", "Keep the complete rule and exception.").await;
    std::fs::write(root.join(".claude"), "A human file blocks this directory").unwrap();
    let text = generate(&engram, &pid, true).await;
    let data = ledger(&text);
    assert!(text.contains("PARTIAL PUBLICATION"));
    assert_eq!(data["inputs"][0]["delivery"], "preserved_unverifiable");
    assert!(data["observed_catalog_blake3"].is_null());
    assert_eq!(data["inputs"][0]["original"]["rule_text"], "Keep the complete rule and exception.");
    assert_eq!(std::fs::read_to_string(root.join(".claude")).unwrap(), "A human file blocks this directory");
    assert!(!root.join("CLAUDE.md").exists(), "earlier companion failure must not be reported as a completed root write");
    assert_eq!(text.split("# Write-path notes").next().unwrap().trim(),
        "# Written files\n\n- .engram/instruction-generation.json",
        "Only the recovery ledger was written, not the blocked policy artifact");
    assert!(root.join(".engram/instruction-generation.json").is_file());
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn conflicted_companion_cannot_replace_current_root_with_an_incomplete_pointer() {
    let (temp, engram, pid) = build().await;
    let root = temp.path().join("proj");
    add_rule(&engram, &pid, "manual", "Site/*", "Preserve the complete constraint. Exception: keep the approved exception.").await;
    std::fs::create_dir_all(root.join(".claude/rules")).unwrap();
    std::fs::write(root.join(".claude/rules/engram-repository-policy.md"), "Human companion policy").unwrap();
    std::fs::write(root.join("CLAUDE.md"), "Current complete root policy with its accepted exception.\n").unwrap();
    let result = engram.handle_produce_claude_md(serde_json::from_value(serde_json::json!({
        "project_id":pid,"write_to_disk":true,"overwrite_existing":true,"merge_existing":false,"merge_mode":"replace"
    })).unwrap()).await.unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("complete companion dependencies were not installed"));
    assert_eq!(std::fs::read_to_string(root.join("CLAUDE.md")).unwrap(), "Current complete root policy with its accepted exception.\n");
    assert_eq!(ledger(text)["inputs"][0]["delivery"], "preserved_conflict");
    assert!(text.contains("Preserve the complete constraint. Exception: keep the approved exception."));
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn new_file_context_uses_the_same_directory_and_case_scope_as_review() {
    let (temp, engram, pid) = build().await;
    add_rule(&engram, &pid, "area", "Site", "Preserve this area's exception.").await;
    let matching = engram_server::services::project_service::inject_repo_rules(&engram.state, &pid,
        &engram_core::RelPath::new("Other/SITE-tools/new.cs"), "new source").await;
    let unrelated = engram_server::services::project_service::inject_repo_rules(&engram.state, &pid,
        &engram_core::RelPath::new("Admin/new.cs"), "new source").await;
    assert!(matching.contains("Preserve this area's exception."));
    assert_eq!(unrelated, "new source");
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn corrupt_source_head_cannot_be_replaced_by_a_guessed_generation_in_generated_guidance() {
    let (temp, engram, pid) = build().await;
    engram.state.registry.set_meta(&pid, "active_generation", "not-a-generation").unwrap();
    let result = engram.handle_produce_claude_md(serde_json::from_value(serde_json::json!({
        "project_id":pid,"write_to_disk":true
    })).unwrap()).await;
    assert!(result.is_err());
    assert!(!temp.path().join("proj/CLAUDE.md").exists());
    drop(engram);
    temp.close().unwrap();
}
