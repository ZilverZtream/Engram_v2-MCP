//! Inspect a real prior source edit through its advertised exact-path action.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram, retained_results};
use git2::{Oid, Repository, Signature};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};
use std::process::Command;

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn commit(repo: &Repository, message: &str) -> Result<Oid> {
    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let sig = Signature::now("Owned author", "owned@example.invalid")?;
    Ok(repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        message,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )?)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_history_file_patch_matches_git_and_recovers_after_later_source_changes()
-> Result<()> {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new()
        .prefix("h")
        .rand_bytes(2)
        .tempdir_in(fixture_root)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let paths = ["Rules.vb", "preferences.ts", "schema.sql"];
    let (base, selected) = {
        let repo = Repository::init(&root)?;
        for path in paths {
            std::fs::write(root.join(path), "owned baseline\n")?;
        }
        let base = commit(&repo, "Original source")?;
        for path in paths {
            std::fs::write(
                root.join(path),
                "Preserve approval and accepted exceptions.\n".repeat(150),
            )?;
        }
        let selected = commit(&repo, "Update the three source companions")?;
        repo.reference(
            "refs/remotes/origin/main",
            selected,
            true,
            "owned history scope",
        )?;
        repo.reference_symbolic(
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
            true,
            "owned history scope",
        )?;
        (base, selected)
    };
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    state.registry.put_project(&ProjectRecord {
        project_id: "owned-patch".into(),
        project_name: "Owned historical source".into(),
        project_type: "dotnet_webforms_vb".into(),
        directory: root.to_string_lossy().into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        reindex_required_since_ms: None,
    })?;
    let fixture = Fixture {
        server: Engram::new(state),
        owned,
    };
    let cohort = fixture
        .server
        .handle_search_history(serde_json::from_value(json!({"project_id":"owned-patch",
        "query":selected.to_string(),"mode":"commit","as_of_rev":selected.to_string(),"output_json":true,"max_tokens":100000}))?)
        .await?;
    let cohort: Value = serde_json::from_str(text(&cohort))?;
    let mut first = None;
    for path in paths {
        let result=fixture.server.handle_search_history(serde_json::from_value(json!({"project_id":"owned-patch",
            "query":selected.to_string(),"mode":"patch","file_filter":path,"as_of_rev":selected.to_string(),
            "output_json":true,"max_tokens":100000}))?).await?;
        let result: Value = serde_json::from_str(text(&result))?;
        let row = &result["results"][0];
        let oracle = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--full-index",
                "-U3",
                &base.to_string(),
                &selected.to_string(),
                "--",
                path,
            ])
            .output()?;
        ensure!(oracle.status.success());
        let expected = String::from_utf8(oracle.stdout)?;
        ensure!(
            row["patch"] == expected
                && row["patch_blake3"] == blake3::hash(expected.as_bytes()).to_hex().to_string()
        );
        ensure!(row["patch_truncated"] == false && row["first_parent"] == base.to_string());
        if first.is_none() {
            first = Some(row.clone());
        }
    }
    let action = &cohort["results"][0]["file_changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == paths[0])
        .unwrap()["patch"];
    ensure!(action["tool"] == "search_history" && action["arguments"]["file_filter"] == paths[0]);
    let mut request: CallToolRequestParam = serde_json::from_value(json!({
        "name":action["tool"],"arguments":action["arguments"]
    }))?;
    request
        .arguments
        .as_mut()
        .unwrap()
        .insert("max_tokens".into(), json!(512));
    let delivery =
        retained_results::prepare(&mut request, &Engram::tool_registry().list_all())?.unwrap();
    let original = fixture
        .server
        .handle_search_history(serde_json::from_value(request.arguments.unwrap())?)
        .await?;
    let data = fixture.owned.path().join("data");
    let captured = delivery.finish(&data, original)?;
    ensure!(text(&captured).len() <= 2048);
    let captured: Value = serde_json::from_str(text(&captured))?;
    std::fs::write(root.join(paths[0]), "Later source\n")?;
    {
        let repo = Repository::open(&root)?;
        commit(&repo, "Later unrelated edit")?;
    }
    let mut recovered = String::new();
    loop {
        let mut req = serde_json::from_value(
            json!({"name":"search_history","arguments":{"project_id":"owned-patch",
            "response_id":captured["response_delivery"]["response_id"],"response_offset":recovered.len(),"max_tokens":512}}),
        )?;
        let delivery =
            retained_results::prepare(&mut req, &Engram::tool_registry().list_all())?.unwrap();
        let page = delivery.recover(&data)?.unwrap();
        ensure!(text(&page).len() <= 2048);
        let page: Value = serde_json::from_str(text(&page))?;
        recovered.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
    }
    ensure!(
        captured["response_delivery"]["response_id"]
            == blake3::hash(recovered.as_bytes()).to_hex().to_string()
    );
    let original: CallToolResult = serde_json::from_str(&recovered)?;
    let original: Value = serde_json::from_str(text(&original))?;
    ensure!(original["results"][0] == first.unwrap());
    let refused=fixture.server.handle_search_history(serde_json::from_value(json!({"project_id":"owned-patch",
        "query":selected.to_string(),"mode":"patch","file_filter":paths[0],"as_of_rev":base.to_string()}))?).await;
    ensure!(
        refused.is_err(),
        "A later patch must not leak into an earlier history scope"
    );
    Ok(())
}
