//! A rollback is evidence of an event, not accepted repository policy.
use engram_core::{Config, ProjectRecord};
use engram_graph::EdgeKind;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::Path;

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "-c",
            "user.name=Owned fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn real_git_revert_keeps_rulings_and_returns_source_bound_evidence() {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT");
    let temporary = match fixture_root {
        Some(root) => tempfile::tempdir_in(root).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let source = temporary.path().join("source");
    let data = temporary.path().join("data");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    git(&source, &["init", "--quiet", "--initial-branch=main"]);
    let file = source.join("Decision.js");
    std::fs::write(&file, "export const sidebarVisible = false;\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "Initial setting"]);
    std::fs::write(&file, "export const sidebarVisible = true;\n").unwrap();
    git(&source, &["add", "."]);
    git(
        &source,
        &["commit", "--quiet", "-m", "Add optional sidebar visibility"],
    );
    let original = git(&source, &["rev-parse", "HEAD"]);
    git(&source, &["revert", "--no-edit", &original]);
    let reverted = git(&source, &["rev-parse", "HEAD"]);
    let (state, _) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![source.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: "revert-evidence".into(),
            project_name: "Revert evidence".into(),
            project_type: "general".into(),
            directory: source.to_string_lossy().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    let engram = Engram::new(state.clone());
    state
        .registry
        .set_meta("revert-evidence", "active_generation", "1")
        .unwrap();
    for (id, provenance) in [
        ("manual-guidance", "manual"),
        ("previous-learned", "revert-derived rule"),
    ] {
        engram
            .handle_add_repo_rule(
                serde_json::from_value(json!({
                    "project_id":"revert-evidence","rule_id":id,"file_pattern":"**/*.js",
                    "rule_text":"Preserve the existing accepted exception.","priority":7,
                    "introduced_at":"2026-11-01","provenance":provenance
                }))
                .unwrap(),
            )
            .await
            .unwrap();
    }
    let before = state.registry.list_repo_rules("revert-evidence").unwrap();
    let result = engram
        .handle_analyze_reverts(
            serde_json::from_value(json!({
                "project_id":"revert-evidence","max_commits":8
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        state.registry.list_repo_rules("revert-evidence").unwrap(),
        before
    );
    let facts: Value = result
        .content
        .iter()
        .filter_map(|part| part.as_text())
        .filter_map(|text| serde_json::from_str::<Value>(&text.text).ok())
        .find(|value| value["tool"] == "analyze_reverts")
        .unwrap();
    assert_eq!(facts["repo_rules_written"], 0);
    assert_eq!(facts["llm_generation_requests"], 0);
    let evidence = facts["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 1);
    let item = &evidence[0];
    assert_eq!(item["original_commit"], original);
    assert_eq!(item["revert_commit"], reverted);
    assert_eq!(item["path"], "Decision.js");
    assert_eq!(item["evidence_basis"], "git_revert_message_reference");
    assert_eq!(item["policy_acceptance"], "not_inferred");
    assert_eq!(item["revert_reason"], "not_established");
    assert_eq!(item["recovery"]["tool"], "get_chunk");
    let chunk = engram
        .handle_get_chunk(serde_json::from_value(item["recovery"]["arguments"].clone()).unwrap())
        .await
        .unwrap();
    let content = chunk
        .content
        .iter()
        .filter_map(|part| part.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(content.contains("Revert evidence only"));
    assert!(content.contains(&original) && content.contains(&reverted));
    assert!(content.contains("sidebarVisible = true"));
    let (edges, truncated) = state
        .graph
        .outgoing_edges_of_kind(
            "revert-evidence",
            EdgeKind::AntiPattern,
            "file:Decision.js",
            10,
        )
        .unwrap();
    assert!(!truncated && edges.len() == 1);
    let metadata = edges[0].metadata.as_ref().unwrap();
    assert_eq!(metadata["evidence_kind"], "git_revert_message_reference");
    assert_eq!(metadata["original_commit"], original);
    assert_eq!(metadata["revert_commit"], reverted);
    assert_eq!(metadata["policy_acceptance"], "not_inferred");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "export const sidebarVisible = false;\n"
    );
    drop(engram);
    drop(state);
    temporary.close().unwrap();
}
