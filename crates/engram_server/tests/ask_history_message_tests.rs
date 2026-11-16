//! Ordinary history questions retain authored exceptions and recorded identity.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn commit(repo: &git2::Repository, message: &str) -> Result<git2::Oid> {
    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let signature = git2::Signature::now("Owned historian", "owned@example.invalid")?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    Ok(repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_history_preserves_complete_messages_and_original_report_after_new_head()
-> Result<()> {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    // All repositories/database owners are declared after this outer owner.
    let owned = tempfile::Builder::new()
        .prefix("h")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let repo = git2::Repository::init(&root)?;
    repo.set_head("refs/heads/main")?;
    let path = root.join("Rules.vb");
    let body = |n| {
        format!(
            "Public Module InvoiceApproval\n Public Function Limit() As Integer\n  Return {n}\n End Function\nEnd Module\n"
        )
    };
    std::fs::write(&path, body(1))?;
    std::fs::write(
        root.join("AGENTS.md"),
        "Historical intent is evidence, not a new approval or current policy. Preserve exceptions.\n",
    )?;
    commit(&repo, "Baseline InvoiceApproval")?;
    let mut messages = std::collections::BTreeMap::new();
    let mut latest = None;
    for n in [2, 3] {
        std::fs::write(&path, body(n))?;
        let message = format!("InvoiceApproval changed: retain the approval rationale\n\n{}FINAL_EXCEPTION_{n}: Existing manually approved overrides remain valid; this commit is not a new approval.\n",
            (0..30).map(|i|format!("Rationale {i:02}: InvoiceApproval changes preserve the recorded validation requirement and its exceptions.\n")).collect::<String>());
        let id = commit(&repo, &message)?;
        messages.insert(
            id.to_string(),
            repo.find_commit(id)?.message().unwrap().to_owned(),
        );
        latest = Some(id);
    }
    repo.reference(
        "refs/remotes/origin/main",
        latest.unwrap(),
        true,
        "Owned source oracle",
    )?;
    repo.reference_symbolic(
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        true,
        "Owned default",
    )?;
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: true,
        ..Default::default()
    })?;
    let server = Engram::new(state);
    let indexed = server
        .handle_index_project(serde_json::from_value(json!({"directory":root,
        "project_name":"Owned history question","project_type":"dotnet_webforms_vb","wait":true}))?)
        .await?;
    let project = text(&indexed)
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    server
        .handle_index_git_history(serde_json::from_value(
            json!({"project_id":project,"max_commits":10,
        "wait":true,"index_antipatterns":false}),
        )?)
        .await?;
    let args = json!({"project_id":project,"question":"Why was InvoiceApproval changed?","output_format":"json","max_tokens":1000000});
    let full = server
        .handle_ask_codebase(serde_json::from_value(args.clone())?)
        .await?;
    let full: Value = serde_json::from_str(text(&full))?;
    let retained: Vec<_> = full["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            item["provider"] == "history"
                && messages.contains_key(item["commit"].as_str().unwrap_or_default())
        })
        .collect();
    ensure!(
        retained.len() == 2,
        "Both distinct authored history records must survive selection"
    );
    for item in retained {
        let id = item["commit"].as_str().unwrap();
        ensure!(item["content"].as_str().unwrap().ends_with(&messages[id]));
        ensure!(item["authority"] == "recorded_history");
        ensure!(
            item["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning.as_str().is_some_and(|s| s.contains("approval")))
        );
    }
    let mut bounded_args = args;
    bounded_args["max_tokens"] = json!(2048);
    let bounded = server
        .handle_ask_codebase(serde_json::from_value(bounded_args)?)
        .await?;
    ensure!(text(&bounded).len() <= 8192);
    let bounded: Value = serde_json::from_str(text(&bounded))?;
    let mut action = bounded["response_delivery"]["next_call"].clone();
    ensure!(!action.is_null());
    std::fs::write(&path, body(4))?;
    commit(&repo, "Later checked-out InvoiceApproval change")?;
    let mut recovered = String::new();
    while !action.is_null() {
        let page = server
            .handle_ask_codebase(serde_json::from_value(action["arguments"].clone())?)
            .await?;
        ensure!(text(&page).len() <= 8192);
        let page: Value = serde_json::from_str(text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        action = page["next_call"].clone();
    }
    ensure!(serde_json::from_str::<Value>(&recovered)? == full);
    ensure!(
        bounded["response_delivery"]["response_id"]
            == blake3::hash(recovered.as_bytes()).to_hex().to_string()
    );
    Ok(())
}
