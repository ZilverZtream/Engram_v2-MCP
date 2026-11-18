//! Agent-visible source counts must exclude retired physical revisions.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::json;

fn text(result: CallToolResult) -> Result<String> {
    ensure!(result.is_error != Some(true));
    Ok(result.content.iter().filter_map(|part| part.as_text())
        .map(|part| part.text.as_str()).collect::<Vec<_>>().join("\n"))
}

fn number(text: &str, name: &str) -> Option<usize> {
    text.lines().find_map(|line| line.strip_prefix(&format!("{name}: "))?.parse().ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_counts_only_published_files_and_chunks_after_edit_and_delete() -> Result<()> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let owned = tempfile::Builder::new().prefix("shc-").tempdir_in(parent)?;
    let root = owned.path().join("r");
    std::fs::create_dir(&root)?;
    let script = root.join("invoice.ts");
    let deleted = root.join("invoice.sql");
    std::fs::write(&script, "export function invoiceHealth() { return 'previous'; }\n")?;
    std::fs::write(&deleted, "CREATE TABLE dbo.InvoiceHealth (Id int NOT NULL);\n")?;
    // Physical storage must differ for a deliberate, owned reason. Otherwise
    // this check could accidentally depend on an enclosing repository's data.
    {
        let repo = git2::Repository::init(&root)?;
        let mut index = repo.index()?;
        index.add_path(std::path::Path::new("invoice.ts"))?;
        index.add_path(std::path::Path::new("invoice.sql"))?;
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let signature = git2::Signature::now("Owned source health", "fixture@example.invalid")?;
        repo.commit(Some("HEAD"), &signature, &signature, "Owned source health base", &tree, &[])?;
    }
    // Database/application handles drop before the owned directory on unwind.
    let (state, _) = AppState::new(Config {
        data_dir: owned.path().join("data"), allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(), llm_backend: "none".into(),
        watch_debounce_secs: 3600, ..Default::default()
    })?;
    let server = Engram::new(state);
    let indexed = text(server.handle_index_project(serde_json::from_value(json!({
        "directory":root,"project_name":"Owned source health","project_type":"general","wait":true
    }))?).await?)?;
    let project = indexed.split("project_id: ").nth(1).and_then(|value| value.lines().next())
        .ok_or_else(|| anyhow::anyhow!("Missing owned project identity"))?.trim();
    std::fs::write(&script, "export function invoiceHealth() { return 'current'; }\n")?;
    std::fs::remove_file(&deleted)?;
    text(server.handle_update_project(serde_json::from_value(json!({
        "project_id":project,"wait":true
    }))?).await?)?;
    let overview = text(server.handle_get_codebase_overview(serde_json::from_value(json!({
        "project_id":project
    }))?).await?)?;
    let health = text(server.handle_project_health(serde_json::from_value(json!({
        "project_id":project
    }))?).await?)?;
    ensure!(number(&overview, "source_files") == Some(1));
    ensure!(number(&health, "source_files") == Some(1));
    let selected = number(&overview, "source_chunks").ok_or_else(|| anyhow::anyhow!("Missing source count"))?;
    ensure!(number(&health, "source_chunks") == Some(selected));
    ensure!(number(&health, "tantivy_docs_total").is_some_and(|physical| physical > selected));
    ensure!(health.contains("storage_count_scope: physical"));
    ensure!(health.contains("source_count_scope: held published source selection"));
    drop(server);
    owned.close()?;
    Ok(())
}
