//! Small, checked-in source oracles for the delivered language module resolvers.
//! Expected locations and edges are authored independently in expected.json.
use anyhow::{ensure, Context, Result};
use engram_core::{Config, NodeId};
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Symbol {
    file: String,
    name: String,
    line: u32,
}

impl Symbol {
    fn id(&self) -> String {
        NodeId::symbol("function", None, &self.file, &self.name, self.line).0
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    schema_version: u32,
    description: String,
    indexing_preset: String,
    source_files: Vec<String>,
    symbols: BTreeMap<String, Symbol>,
    calls: BTreeMap<String, Vec<String>>,
}

/// Own the async runtime too: on assertion failure it still drains index jobs,
/// closes database owners and waits for blocking workers before temp teardown.
struct Fixture {
    server: Option<Engram>,
    runtime: Option<tokio::runtime::Runtime>,
    temporary: Option<tempfile::TempDir>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Some(server), Some(runtime)) = (&self.server, &self.runtime) {
            runtime.block_on(async {
                loop {
                    let jobs: Vec<_> = server
                        .state
                        .active_jobs
                        .write()
                        .await
                        .drain()
                        .map(|(_, task)| task)
                        .collect();
                    if jobs.is_empty() {
                        break;
                    }
                    for job in jobs {
                        let _finished = job.await;
                    }
                }
            });
        }
        drop(self.server.take());
        drop(self.runtime.take());
        if let Some(temporary) = self.temporary.take() {
            let path = temporary.path().to_owned();
            if let Err(error) = temporary.close() {
                if std::thread::panicking() {
                    eprintln!(
                        "Owned language fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                } else {
                    panic!(
                        "Owned language fixture cleanup failed at {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
}

fn files_under(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "Fixture source must not follow symlinks"
        );
        if kind.is_dir() {
            files_under(root, &entry.path(), files)?;
        } else {
            ensure!(kind.is_file(), "Fixture contains a non-file entry");
            files.insert(
                entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}

fn text(result: CallToolResult) -> Result<String> {
    ensure!(
        result.is_error != Some(true),
        "Tool returned an error: {result:?}"
    );
    Ok(result
        .content
        .iter()
        .filter_map(|item| item.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn git_commit(root: &Path) -> Result<()> {
    let repository = git2::Repository::init(root)?;
    let mut index = repository.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repository.find_tree(tree_id)?;
    let identity = git2::Signature::now("Owned Fixture", "fixture@example.invalid")?;
    repository.commit(
        Some("HEAD"),
        &identity,
        &identity,
        "Authored language corpus",
        &tree,
        &[],
    )?;
    Ok(())
}

fn verify_corpus(name: &str) -> Result<()> {
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name);
    let expected: Expected =
        serde_json::from_slice(&std::fs::read(fixture_root.join("expected.json"))?)?;
    ensure!(
        expected.schema_version == 1 && !expected.description.is_empty(),
        "Invalid oracle"
    );
    let source = fixture_root.join("project");
    let mut checked_in = BTreeSet::new();
    files_under(&source, &source, &mut checked_in)?;
    ensure!(
        checked_in == expected.source_files.iter().cloned().collect(),
        "Source/oracle file inventory drift: {name}"
    );
    let temporary = tempfile::Builder::new()
        .prefix("engram-language-corpus-")
        .tempdir()?;
    let mut fixture = Fixture {
        server: None,
        runtime: None,
        temporary: Some(temporary),
    };
    let owned = fixture
        .temporary
        .as_ref()
        .context("Fixture owner missing")?
        .path()
        .to_owned();
    let root = owned.join("repo");
    for path in &expected.source_files {
        let destination = root.join(path);
        std::fs::create_dir_all(destination.parent().context("Source parent missing")?)?;
        std::fs::copy(source.join(path), destination)?;
    }
    // Source lines are part of the authored oracle, not extractor-generated goldens.
    for symbol in expected.symbols.values() {
        let body = std::fs::read_to_string(root.join(&symbol.file))?;
        ensure!(
            symbol.line > 0
                && body
                    .lines()
                    .nth(symbol.line as usize - 1)
                    .is_some_and(|line| line.contains(&symbol.name)),
            "Invalid authored source location: {}:{}",
            symbol.file,
            symbol.line
        );
    }
    git_commit(&root)?;
    fixture.runtime = Some(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
    );
    let runtime = fixture.runtime.as_ref().context("Runtime owner missing")?;
    fixture.server = Some(runtime.block_on(async {
        let (state, _) = AppState::new(Config {
            data_dir: owned.join("data"),
            allowed_roots: vec![owned.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            max_project_files: Some(32),
            max_project_bytes: Some(64 * 1024),
            ..Default::default()
        })?;
        Ok::<_, anyhow::Error>(Engram::new(state))
    })?);
    let server = fixture.server.as_ref().context("Server owner missing")?;
    runtime.block_on(async {
        let indexed = text(server.handle_index_project(serde_json::from_value(json!({
            "directory":root.to_string_lossy(),"project_name":name,"wait":true
        }))?).await?)?;
        let project_id = indexed.split_once("project_id: ")
            .and_then(|(_, tail)| tail.lines().next())
            .with_context(|| format!("Indexing did not return a project identity: {indexed}"))?.trim().to_owned();
        let info = text(server.handle_project_info(serde_json::from_value(json!({"project_id":project_id}))?).await?)?;
        let metadata: serde_json::Value = serde_json::from_str(info.lines()
            .find_map(|line| line.strip_prefix("project_capabilities: ")).context("Capability metadata missing")?)?;
        ensure!(metadata["indexing_preset"] == expected.indexing_preset, "Wrong indexing preset: {info}");
        ensure!(metadata["coverage"] == "selected_source_paths" && metadata["detected"]["inspected_files"] == expected.source_files.len(), "Incomplete source coverage: {info}");
        let nodes = text(server.handle_query_graph_nodes(serde_json::from_value(json!({
            "project_id":project_id,"node_type":"function","limit":100
        }))?).await?)?;
        let actual_symbols: BTreeSet<_> = nodes.lines().filter_map(|line| line.strip_prefix("- ")
            .and_then(|row| row.split_once(" | ").map(|(id, _)| id.to_owned()))).collect();
        let expected_symbols: BTreeSet<_> = expected.symbols.values().map(Symbol::id).collect();
        ensure!(actual_symbols == expected_symbols,
            "{name}: function identities differ\nexpected={expected_symbols:?}\nactual={actual_symbols:?}\n{nodes}");
        for (key, caller) in &expected.symbols {
            let references = text(server.handle_find_references(serde_json::from_value(json!({
                "project_id":project_id,"node_id":caller.id(),"edge_kind":"calls","direction":"out"
            }))?).await?)?;
            let actual: BTreeSet<_> = references.lines().filter_map(|line| line.strip_prefix("- ")
                .and_then(|row| row.split_once(" (weight=").map(|(id, _)| id.to_owned()))).collect();
            let wanted = expected.calls.get(key).into_iter().flatten().map(|target| {
                expected.symbols.get(target).map(Symbol::id).with_context(|| format!("Unknown oracle target {target}"))
            }).collect::<Result<BTreeSet<_>>>()?;
            ensure!(actual == wanted,
                "{name}: wrong call targets for {key} at {}:{}\nexpected={wanted:?}\nactual={actual:?}\n{references}", caller.file, caller.line);
        }
        println!("{name}: exact authored function identities and call targets verified through indexing and public handlers");
        Ok(())
    })
}

#[test]
fn typescript_corpus() -> Result<()> {
    verify_corpus("typescript_modules")
}
#[test]
fn python_corpus() -> Result<()> {
    verify_corpus("python_modules")
}
#[test]
fn rust_corpus() -> Result<()> {
    verify_corpus("rust_modules")
}
#[test]
fn go_corpus() -> Result<()> {
    verify_corpus("go_modules")
}
