#![allow(clippy::unwrap_used)]
//! `generate_agent_integration` with `write_files=true`: what lands on disk,
//! what is refused, and what the agent is told about it. The pack is the
//! contract between Engram and every agent that opens the repo, so its
//! file-writing behaviour is pinned here.

use engram_core::config::Config;
use engram_server::models::GenerateAgentIntegrationRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;
use std::path::PathBuf;

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    (tmp, state)
}

fn register_project(state: &AppState, tmp: &tempfile::TempDir) -> (String, PathBuf) {
    let project_dir = tmp.path().join("project");
    let rec = engram_core::ProjectRecord {
        project_id: "pack-test".into(),
        project_name: "pack-test".into(),
        directory: project_dir.to_string_lossy().into_owned(),
        project_type: "general".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        reindex_required_since_ms: None,
    };
    state.registry.put_project(&rec).unwrap();
    state
        .registry
        .set_meta("pack-test", "active_generation", "1")
        .unwrap();
    ("pack-test".into(), project_dir)
}

async fn run(engram: &Engram, project_id: &str) -> String {
    let res = engram
        .handle_generate_agent_integration(GenerateAgentIntegrationRequest {
            project_id: project_id.into(),
            write_files: true,
            windows: true,
        })
        .await
        .unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn write_files_installs_agents_md_when_absent() {
    let (tmp, state) = build_state();
    let (pid, dir) = register_project(&state, &tmp);
    let engram = Engram::new(state);

    let out = run(&engram, &pid).await;

    let agents = dir.join("AGENTS.md");
    assert!(agents.exists(), "AGENTS.md not written; output:\n{out}");
    let body = std::fs::read_to_string(&agents).unwrap();
    assert!(
        body.contains(&pid),
        "AGENTS.md must carry the live project id"
    );
    assert!(
        out.contains("AGENTS.md"),
        "output must report the write:\n{out}"
    );
    // The rules file still lands too.
    assert!(dir.join(".claude/rules/engram-workflow.md").exists());
}

#[tokio::test]
async fn write_files_never_clobbers_an_existing_agents_md() {
    let (tmp, state) = build_state();
    let (pid, dir) = register_project(&state, &tmp);
    let agents = dir.join("AGENTS.md");
    std::fs::write(&agents, "# HUMAN AUTHORED\nkeep me\n").unwrap();
    let engram = Engram::new(state);

    let out = run(&engram, &pid).await;

    assert_eq!(
        std::fs::read_to_string(&agents).unwrap(),
        "# HUMAN AUTHORED\nkeep me\n"
    );
    assert!(
        out.contains("SKIPPED") && out.contains("AGENTS.md"),
        "must say the file was left alone:\n{out}"
    );
}

#[tokio::test]
async fn existing_workflow_policy_survives_pack_installation() {
    let (tmp, state) = build_state();
    let (pid, dir) = register_project(&state, &tmp);
    let workflow = dir.join(".claude/rules/engram-workflow.md");
    std::fs::create_dir_all(workflow.parent().unwrap()).unwrap();
    let human = "# Team workflow\r\nReview production changes with the owner.\r\n";
    std::fs::write(&workflow, human).unwrap();
    let engram = Engram::new(state);
    let out = run(&engram, &pid).await;
    assert_eq!(std::fs::read(&workflow).unwrap(), human.as_bytes());
    assert!(out.contains("PRESERVED .claude/rules/engram-workflow.md"));
    assert!(out.contains("## Proposed .claude/rules/engram-workflow.md"),
        "a refused write must still give the caller a reviewable proposal");
}

#[tokio::test]
async fn human_amendment_to_generated_workflow_survives_refresh() {
    let (tmp, state) = build_state();
    let (pid, dir) = register_project(&state, &tmp);
    let engram = Engram::new(state);
    run(&engram, &pid).await;
    let workflow = dir.join(".claude/rules/engram-workflow.md");
    let mut amended = std::fs::read_to_string(&workflow).unwrap();
    amended.push_str("\nHuman exception: emergency recovery requires incident review.\n");
    std::fs::write(&workflow, &amended).unwrap();
    let out = run(&engram, &pid).await;
    assert_eq!(std::fs::read_to_string(&workflow).unwrap(), amended);
    assert!(out.contains("PRESERVED .claude/rules/engram-workflow.md"));
}

#[tokio::test]
async fn write_files_emits_mcp_json_snippet_but_never_writes_it() {
    // .mcp.json is repo-committed config and the engram command is a
    // machine-specific absolute path: emit for a human to merge, never write.
    let (tmp, state) = build_state();
    let (pid, dir) = register_project(&state, &tmp);
    let engram = Engram::new(state);

    let out = run(&engram, &pid).await;

    assert!(
        !dir.join(".mcp.json").exists(),
        ".mcp.json must not be written"
    );
    assert!(
        out.contains("\"mcpServers\"") && out.contains("\"engram\""),
        "output must carry a mergeable .mcp.json snippet:\n{out}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn a_publication_filesystem_failure_still_returns_the_complete_integration_proposal() {
    use std::os::windows::fs::OpenOptionsExt;
    let (tmp, state) = build_state();
    let (pid, dir) = register_project(&state,&tmp);
    // Reads remain possible, but Windows refuses replacement while this
    // handle withholds delete sharing. This fails the actual write branch.
    let blocked = dir.join(".claude/rules/engram-workflow.md");
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    let original = "# Previously generated workflow\n";
    std::fs::write(&blocked,original).unwrap();
    state.registry.set_meta(&pid,"generated_artifacts:agent-integration",&json!({
        "version":1,"root":std::fs::canonicalize(&dir).unwrap().to_string_lossy(),"owner":"agent-integration",
        "files":{".claude/rules/engram-workflow.md":blake3::hash(original.as_bytes()).to_hex().to_string()}
    }).to_string()).unwrap();
    let locked = std::fs::OpenOptions::new().read(true).share_mode(1).open(&blocked).unwrap();
    let hooks = "{\"human_setting\":true}\n";
    let agents = "# Human instructions\nRetain this rule.\n";
    std::fs::write(dir.join(".claude/settings.json"),hooks).unwrap();
    std::fs::write(dir.join("AGENTS.md"),agents).unwrap();
    let engram = Engram::new(state);
    let out = run(&engram,&pid).await;
    assert!(out.contains("PUBLICATION INCOMPLETE"),"{out}");
    for section in ["## Proposed .claude/rules/engram-workflow.md","## hooks block to merge",
        "## AGENTS.md section to merge","## .mcp.json entry"] {
        assert!(out.contains(section),"Missing recoverable proposal {section}: {out}");
    }
    assert_eq!(std::fs::read_to_string(&blocked).unwrap(),original);
    assert_eq!(std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap(),hooks);
    assert_eq!(std::fs::read_to_string(dir.join("AGENTS.md")).unwrap(),agents);
    drop(locked);
    drop(engram);
    tmp.close().unwrap();
}
