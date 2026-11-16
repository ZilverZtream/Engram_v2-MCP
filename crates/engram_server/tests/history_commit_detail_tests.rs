//! A lossy history preview must lead to the complete selected source change.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use git2::{ObjectType, Oid, Repository, Signature};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn commit(repo: &Repository, message: &str) -> Result<Oid> {
    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let signature = Signature::now("Owned history author", "owned@example.invalid")?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents = parent.iter().collect::<Vec<_>>();
    Ok(repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?)
}
struct Fixture {
    server: Engram,
    _owned: tempfile::TempDir,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_git_change_retains_full_message_complete_cohort_and_revision_scope() -> Result<()>
{
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("h")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("project");
    std::fs::create_dir(&root)?;
    let repo = Repository::init(&root)?;
    let mut files = (0..47)
        .map(|i| format!("Evidence{i:02}.{}", ["vb", "ts", "sql"][i % 3]))
        .collect::<Vec<_>>();
    files.push("Project.vbproj".into());
    for path in &files {
        std::fs::write(root.join(path), "owned baseline\n")?;
    }
    let base = commit(&repo, "Owned baseline\n")?;
    for path in &files {
        std::fs::write(root.join(path), "owned changed source\n")?;
    }
    let message = format!(
        "Complete cohort behavior\n\n{}ORIGINAL_MESSAGE_END\n",
        "Preserve source provenance and all companion files.\n".repeat(100)
    );
    let selected = commit(&repo, &message)?;
    repo.reference(
        "refs/remotes/origin/main",
        selected,
        true,
        "owned published root",
    )?;
    repo.reference_symbolic(
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        true,
        "owned default branch",
    )?;
    drop(repo);
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        advertise_all_tools: true,
        ..Default::default()
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        _owned: owned,
    };
    let indexed=fixture.server.handle_index_project(serde_json::from_value(json!({"directory":root,
        "project_name":"Owned complete Git history","project_type":"dotnet_webforms_vb","wait":true}))?).await?;
    let project = text(&indexed)
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let args = json!({"project_id":project,"query":selected.to_string(),"mode":"commit","output_json":true,"max_tokens":100000});
    let result = fixture
        .server
        .handle_search_history(serde_json::from_value(args.clone())?)
        .await?;
    let report: Value = serde_json::from_str(text(&result))?;
    let row = &report["results"][0];
    ensure!(
        row["content"] == message
            && row["message_blake3"] == blake3::hash(message.as_bytes()).to_hex().to_string()
    );
    let expected = files
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(
        row["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
            == expected
    );
    ensure!(
        row["file_count"] == 48
            && row["files_truncated"] == false
            && row["content_truncated"] == false
    );
    ensure!(row["parents"] == json!([base.to_string()]));
    let before = Oid::hash_object(ObjectType::Blob, b"owned baseline\n")?.to_string();
    let after = Oid::hash_object(ObjectType::Blob, b"owned changed source\n")?.to_string();
    for delta in row["file_changes"].as_array().unwrap() {
        ensure!(
            delta["status"] == "Modified"
                && delta["old_blob"] == before
                && delta["new_blob"] == after
        );
    }
    ensure!(report["search_executed"] == false && report["semantic_retrieval"] == "not_run");
    let mut cutoff = args.clone();
    cutoff["as_of_rev"] = json!(base.to_string());
    ensure!(
        fixture
            .server
            .handle_search_history(serde_json::from_value(cutoff)?)
            .await
            .is_err()
    );
    let mut filtered = args.clone();
    filtered["file_filter"] = json!("Evidence");
    ensure!(
        fixture
            .server
            .handle_search_history(serde_json::from_value(filtered)?)
            .await
            .is_err()
    );
    let preview=fixture.server.handle_search_history(serde_json::from_value(json!({"project_id":project,
        "query":"Complete cohort","mode":"precedent","as_of_rev":selected.to_string(),"rerank":false,
        "semantic_timeout_ms":0,"output_json":true,"max_tokens":100000}))?).await?;
    let preview: Value = serde_json::from_str(text(&preview))?;
    let lead = preview["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["commit"] == selected.to_string())
        .unwrap();
    let action = &lead["recovery"]["complete_git_commit"];
    ensure!(action["tool"] == "search_history" && action["arguments"]["mode"] == "commit");
    let mut follow = action["arguments"].clone();
    follow.as_object_mut().unwrap().remove("retain_response");
    follow["max_tokens"] = json!(100000);
    let following = fixture
        .server
        .handle_search_history(serde_json::from_value(follow)?)
        .await?;
    let following: Value = serde_json::from_str(text(&following))?;
    ensure!(
        following["results"][0]["file_changes"] == row["file_changes"]
            && following["results"][0]["content"] == message
    );
    Ok(())
}
