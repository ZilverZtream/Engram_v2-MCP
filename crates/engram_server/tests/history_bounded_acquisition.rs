//! Finite history acquisition retains access to older, revision-pinned evidence.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

struct Fixture {
    app: Engram,
    pid: String,
    root: PathBuf,
    commits: Vec<String>,
    owned: tempfile::TempDir,
}

fn commit(root: &Path, number: usize) -> Result<String> {
    let repo = git2::Repository::open(root)?;
    std::fs::write(
        root.join("invoice.ts"),
        format!("export const invoiceAudit = {number};\n"),
    )?;
    std::fs::write(
        root.join("invoice.sql"),
        format!("-- invoice audit revision {number}\nSELECT {number};\n"),
    )?;
    let mut index = repo.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let author = if number < 4 {
        "Earlier Author"
    } else {
        "Recent Author"
    };
    let signature = git2::Signature::new(
        author,
        "fixture@example.invalid",
        &git2::Time::new(1_700_000_000 + number as i64, 0),
    )?;
    Ok(repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            &format!("Invoice audit change {number}\n\nPreserve manual replay exception."),
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )?
        .to_string())
}

impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_GIT_FIXTURE_ROOT")
            .or_else(|| std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("hw-").tempdir_in(parent)?;
        let root = owned.path().join("p");
        std::fs::create_dir(&root)?;
        drop(git2::Repository::init(&root)?);
        let commits = (0..6)
            .map(|number| commit(&root, number))
            .collect::<Result<Vec<_>>>()?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("d"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state);
        let indexed = app
            .handle_index_project(serde_json::from_value(json!({"directory":root,
            "project_name":"Owned history acquisition","project_type":"general","wait":true}))?)
            .await?;
        let pid = indexed.content[0]
            .as_text()
            .unwrap()
            .text
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
            pid,
            root,
            commits,
            owned,
        })
    }

    fn args(&self) -> Value {
        json!({"project_id":self.pid,"query":"invoice audit","mode":"precedent",
            "as_of_rev":self.commits[5],"semantic_timeout_ms":0,"rerank_timeout_ms":0,
            "rerank":false,"output_json":true,"limit":20,"max_commits":2})
    }

    async fn raw(&self, args: Value) -> Result<String> {
        let reply = self
            .app
            .handle_search_history(serde_json::from_value(args)?)
            .await?;
        ensure!(reply.is_error != Some(true));
        Ok(reply.content[0].as_text().unwrap().text.clone())
    }

    async fn search(&self, args: Value) -> Result<Value> {
        Ok(serde_json::from_str(&self.raw(args).await?)?)
    }

    fn close(self) -> Result<()> {
        let Self { app, owned, .. } = self;
        drop(app);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test]
async fn recent_lists_matching_history_in_order_and_keeps_older_matches_reachable() -> Result<()> {
    let fixture = Fixture::new().await?;
    let first = fixture.search(json!({"project_id":fixture.pid,"query":"","mode":"recent",
        "as_of_rev":fixture.commits[5],"file_filter":"invoice.sql", "date_after":1_700_000_004u64,
        "max_commits":3,"limit":1,"output_json":true})).await?;
    ensure!(first["results"][0]["commit"] == fixture.commits[5]);
    ensure!(first["semantic_retrieval"] == "not_run" && first["rerank"] == "not_run");
    let patch = &first["results"][0]["recovery"]["selected_file_patch"];
    ensure!(patch["tool"] == "search_history" && patch["arguments"]["as_of_rev"] == fixture.commits[5]);
    let next = &first["next_history_window"]["arguments"];
    ensure!(next["as_of_rev"] == fixture.commits[4] && next["file_filter"] == "invoice.sql");
    let newest = commit(&fixture.root, 6)?;
    let original = fixture.search(patch["arguments"].clone()).await?;
    ensure!(original["results"][0]["commit"] == fixture.commits[5]);
    ensure!(original["results"][0]["path"] == "invoice.sql");
    ensure!(original["results"][0]["patch"].as_str().unwrap().contains("+SELECT 5;"));
    let older = fixture.search(next.clone()).await?;
    ensure!(older["results"][0]["commit"] == fixture.commits[4]);
    ensure!(older["results"][0]["commit"] != newest);
    ensure!(older["results"][0]["recovery"]["selected_file_patch"]["arguments"]["as_of_rev"] == fixture.commits[4]);

    for index in 0..41 {
        std::fs::write(fixture.root.join(format!("Area{index:02}.ts")), "export const value = 1;\n")?;
    }
    for folder in ["zBilling", "zBillingArchive"] {
        std::fs::create_dir(fixture.root.join(folder))?;
        std::fs::write(fixture.root.join(folder).join("Invoice.designer.vb"), "Public Class Invoice\nEnd Class\n")?;
    }
    std::fs::write(fixture.root.join("zBilling/Ignore.sql"), "SELECT 1;\n")?;
    let wide = commit(&fixture.root, 7)?;
    let scoped = fixture.search(json!({"project_id":fixture.pid,"query":"","mode":"recent",
        "as_of_rev":wide,"file_filter":"zBilling","exclude_paths":["zBilling/Ignore.sql"],
        "max_commits":1,"limit":1,"output_json":true})).await?;
    ensure!(scoped["results"][0]["files"] == json!(["zBilling/Invoice.designer.vb"]));
    ensure!(scoped["results"][0]["file_count"] == 1);
    ensure!(scoped["results"][0]["recovery"].get("selected_file_patch").is_none());
    fixture.close()
}

#[tokio::test]
async fn bounded_windows_recover_older_changes_without_duplicates_or_new_head_leaks() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let mut answer = fixture.search(fixture.args()).await?;
    ensure!(answer["history_scan"]["commits_scanned"] == 2);
    ensure!(answer["history_scan"]["older_history_not_scanned"] == true);
    ensure!(answer["eligible_changes_before_bulk_exclusion"] == 2);
    let recent: std::collections::BTreeSet<_> = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["commit"].as_str().unwrap().to_owned())
        .collect();
    ensure!(recent == fixture.commits[4..].iter().cloned().collect());
    let original_internal =
        engram_server::services::change_card_service::metadata_for_line_filtered_cached(
            &fixture.root,
            git2::Oid::from_str(&fixture.commits[5])?,
            &fixture.app.state.registry,
            engram_server::services::change_card_service::CardFilter::default(),
        )
        .await?;
    ensure!(original_internal.len() == 6);
    let mut seen = recent;
    let newest = commit(&fixture.root, 6)?;
    let mut windows = 1;
    while !answer["next_history_window"].is_null() {
        let action = &answer["next_history_window"];
        ensure!(action["tool"] == "search_history");
        ensure!(action["arguments"]["offset"] == 0);
        ensure!(action["arguments"]["max_commits"] == 2);
        answer = fixture.search(action["arguments"].clone()).await?;
        ensure!(answer["history_scan"]["commits_scanned"] == 2);
        for row in answer["results"].as_array().unwrap() {
            let oid = row["commit"].as_str().unwrap().to_owned();
            ensure!(oid != newest && seen.insert(oid));
            let mut recovery = row["recovery"]["complete_git_commit"]["arguments"].clone();
            // retain_response is a public delivery-layer control; handlers receive
            // the typed request after the public layer removes it.
            recovery.as_object_mut().unwrap().remove("retain_response");
            recovery["max_tokens"] = json!(65536);
            let complete = fixture.search(recovery).await?;
            ensure!(
                serde_json::to_string(&complete)?.contains("Preserve manual replay exception.")
            );
        }
        windows += 1;
    }
    ensure!(windows == 3 && seen == fixture.commits.iter().cloned().collect());
    ensure!(answer["history_scan"]["complete_first_parent_history"] == true);
    let mut defaults = fixture.args();
    defaults.as_object_mut().unwrap().remove("max_commits");
    let full = fixture.search(defaults).await?;
    ensure!(full["history_scan"]["max_commits"] == 1000);
    ensure!(full["history_scan"]["commits_scanned"] == 6);
    ensure!(
        full["results"].as_array().unwrap().len() == 6 && full["next_history_window"].is_null()
    );
    fixture.close()
}

#[tokio::test]
async fn empty_recent_window_keeps_older_matching_history_and_all_filters_in_both_formats()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let mut args = fixture.args();
    args["author_filter"] = json!("Earlier Author");
    args["date_after"] = json!(1_700_000_001u64);
    args["date_before"] = json!(1_700_000_004u64);
    args["file_filter"] = json!("invoice.ts");
    args["exclude_paths"] = json!(["unrelated/"]);
    args["max_tokens"] = json!(65536);
    args["offset"] = json!(3);
    let empty = fixture.search(args.clone()).await?;
    ensure!(empty["results"].as_array().unwrap().is_empty());
    ensure!(empty["history_scan"]["commits_scanned"] == 2);
    ensure!(empty["history_scan"]["older_history_not_scanned"] == true);
    let next = empty["next_history_window"]["arguments"].clone();
    for key in [
        "project_id",
        "query",
        "author_filter",
        "date_after",
        "date_before",
        "file_filter",
        "exclude_paths",
        "max_tokens",
        "limit",
        "rerank",
        "semantic_timeout_ms",
        "rerank_timeout_ms",
    ] {
        ensure!(next[key] == args[key], "Continuation lost {key}");
    }
    ensure!(next["as_of_rev"] == fixture.commits[3] && next["offset"] == 0);
    let older = fixture.search(next).await?;
    ensure!(older["results"].as_array().unwrap().len() == 2);
    args["output_json"] = json!(false);
    let markdown = fixture.raw(args).await?;
    let scan: Value = serde_json::from_str(
        markdown
            .lines()
            .find_map(|line| line.strip_prefix("History acquisition: "))
            .unwrap(),
    )?;
    ensure!(scan == empty["history_scan"]);
    let action: Value = serde_json::from_str(
        markdown
            .lines()
            .find_map(|line| line.strip_prefix("next_history_window: "))
            .unwrap(),
    )?;
    ensure!(action["arguments"]["as_of_rev"] == fixture.commits[3]);
    ensure!(action["arguments"]["output_json"] == false);
    fixture.close()
}

#[tokio::test]
async fn invalid_or_irrelevant_acquisition_controls_refuse_before_unknown_project_lookup()
-> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::tempdir_in(parent)?;
    let data = owned.path().join("d");
    let (state, _) = AppState::new(Config {
        data_dir: data.clone(),
        allowed_roots: vec![owned.path().to_path_buf()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })?;
    let app = Engram::new(state);
    for (mode, limit) in [
        ("precedent", 0),
        ("precedent", 10001),
        ("text", 2),
        ("commit", 2),
        ("patch", 2),
    ] {
        let error = app
            .handle_search_history(serde_json::from_value(
                json!({"project_id":"unknown-project",
            "query":"invoice audit","mode":mode,"max_commits":limit}),
            )?)
            .await
            .unwrap_err();
        ensure!(error.message.contains("max_commits"), "{error:?}");
        ensure!(!data.join("projects/unknown-project").exists());
    }
    drop(app);
    owned.close()?;
    Ok(())
}
