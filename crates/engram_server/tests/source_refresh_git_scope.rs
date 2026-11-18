//! Automatic source updates must not ingest an unrelated enclosing repository.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::json;
use std::path::{Path, PathBuf};

fn text(result: CallToolResult) -> Result<String> {
    ensure!(result.is_error != Some(true));
    Ok(result.content.iter().filter_map(|part| part.as_text())
        .map(|part| part.text.as_str()).collect::<Vec<_>>().join("\n"))
}

fn commit(repo: &git2::Repository, paths: &[&str], message: &str) -> Result<String> {
    let mut index = repo.index()?;
    for path in paths { index.add_path(Path::new(path))?; }
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let signature = git2::Signature::now("Owned source fixture", "fixture@example.invalid")?;
    Ok(repo.commit(Some("HEAD"), &signature, &signature, message, &tree,
        &parent.as_ref().into_iter().collect::<Vec<_>>())?.to_string())
}

struct Fixture {
    // Release application/database handles before owned files on unwind.
    server: Engram,
    owned: tempfile::TempDir,
    root: PathBuf,
    project: String,
    head: String,
}

impl Fixture {
    async fn new(mode: &str) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("grs-").tempdir_in(parent)?;
        let repo_root = owned.path().join("repo");
        let root = if mode == "own" { repo_root.clone() } else { repo_root.join("app") };
        std::fs::create_dir_all(&root)?;
        std::fs::write(repo_root.join("outside-a.ts"), "export const outsideParent = 1;\n")?;
        std::fs::write(repo_root.join("outside-b.sql"), "CREATE TABLE dbo.OutsideParent (Id int);\n")?;
        std::fs::write(root.join("invoice.ts"), "export function healthWitness() { return 'previous'; }\n")?;
        let head = {
            let repo = git2::Repository::init(&repo_root)?;
            let mut paths = vec!["outside-a.ts", "outside-b.sql"];
            if mode == "ignored" {
                std::fs::write(repo_root.join(".gitignore"), "app/\n")?;
                paths.push(".gitignore");
            }
            if mode == "tracked" { paths.push("app/invoice.ts"); }
            if mode == "own" { paths.push("invoice.ts"); }
            commit(&repo, &paths, "Owned base")?;
            std::fs::write(repo_root.join("outside-a.ts"), "export const outsideParent = 2;\n")?;
            commit(&repo, &["outside-a.ts"], "Owned outside change")?
        };
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(),
            watch_debounce_secs: 3600, ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = text(server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned source refresh scope","project_type":"general","wait":true
        }))?).await?)?;
        let project = indexed.split("project_id: ").nth(1).and_then(|value| value.lines().next())
            .ok_or_else(|| anyhow::anyhow!("Missing fixture project identity"))?.trim().to_owned();
        Ok(Self { server, owned, root, project, head })
    }

    async fn update(&self) -> Result<String> {
        text(self.server.handle_update_project(serde_json::from_value(json!({
            "project_id":self.project,"wait":true
        }))?).await?)
    }

    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unversioned_children_skip_automatic_parent_history_but_preserve_explicit_requests() -> Result<()> {
    for mode in ["ignored", "untracked"] {
        let fixture = Fixture::new(mode).await?;
        std::fs::write(fixture.root.join("invoice.ts"), "export function healthWitness() { return 'current'; }\n")?;
        let updated = fixture.update().await?;
        ensure!(updated.contains("Source generation 2 published."));
        ensure!(updated.contains("history_enrichment: skipped"));
        ensure!(updated.contains("no versioned project subtree"));
        ensure!(fixture.server.state.registry.get_meta(&fixture.project, "last_git_oid")?.is_none());
        ensure!(!fixture.owned.path().join("data/co_change").join(format!("{}.bin", fixture.project)).exists());
        let noop = fixture.update().await?;
        ensure!(noop.contains("history_enrichment: skipped"));
        ensure!(fixture.server.state.registry.active_source_generation(&fixture.project)? == 2);
        text(fixture.server.handle_index_git_history(serde_json::from_value(json!({
            "project_id":fixture.project,"wait":true,"max_commits":10,"mode":"forward"
        }))?).await?)?;
        ensure!(fixture.server.state.registry.get_meta(&fixture.project, "last_git_oid")?.as_deref() == Some(fixture.head.as_str()));
        fixture.close()?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_roots_and_versioned_subprojects_keep_automatic_history() -> Result<()> {
    for mode in ["own", "tracked"] {
        let fixture = Fixture::new(mode).await?;
        std::fs::write(fixture.root.join("invoice.ts"), "export function healthWitness() { return 'current'; }\n")?;
        let updated = fixture.update().await?;
        ensure!(updated.contains("Source generation 2 published."));
        ensure!(updated.contains("commits_processed: 2"));
        ensure!(fixture.server.state.registry.get_meta(&fixture.project, "last_git_oid")?.as_deref() == Some(fixture.head.as_str()));
        fixture.close()?;
    }
    Ok(())
}
