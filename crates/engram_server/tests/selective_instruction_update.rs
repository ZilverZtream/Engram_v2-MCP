//! A selective edit refresh may collect guidance without refreshing outside source.
use anyhow::{Context, Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const GUIDE_PATH: &str = ".github/instructions/saves.instructions.md";
const GUIDE: &str = "---\napplyTo: Site/selected.js\n---\nPreserve authorization before save.\nException: read-only probes do not save.\nSee `Site/selected.js`.\n";
const BEFORE: &str = "export function saveSelectedOld() { return 1; }\n";
const AFTER: &str = "export function saveSelectedNew() { return 2; }\n";

struct Fixture {
    server: Option<Engram>,
    project: String,
    base: String,
    temporary: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .context("Set ENGRAM_TEST_FIXTURE_ROOT to the recorded fixture parent")?;
        let temporary = tempfile::Builder::new().prefix("si-").tempdir_in(parent)?;
        let root = temporary.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(root.join("Site/selected.js"), BEFORE)?;
        std::fs::write(
            root.join("Site/outside.js"),
            "export function outsideIndexedOld() { return 1; }\n",
        )?;
        std::fs::write(
            root.join("Site/Orders.sql"),
            "CREATE TABLE dbo.Orders (Id int);\n",
        )?;
        std::fs::write(root.join(GUIDE_PATH), GUIDE)?;
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned selective baseline",
                &tree,
                &[],
            )?
            .to_string()
        };
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let mut fixture = Self {
            server: Some(Engram::new(state)),
            project: String::new(),
            base,
            temporary,
        };
        let indexed = fixture.server()?.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Selective instruction fixture","project_type":"general","wait":true
        }))?).await?;
        ensure!(indexed.is_error != Some(true));
        fixture.project = indexed.content[0]
            .as_text()
            .context("index text")?
            .text
            .split("project_id: ")
            .nth(1)
            .context("project id")?
            .lines()
            .next()
            .context("id line")?
            .trim()
            .to_owned();
        std::fs::write(root.join("Site/selected.js"), AFTER)?;
        std::fs::write(
            root.join("Site/outside.js"),
            "export function outsideUnindexedNew() { return 2; }\n",
        )?;
        Ok(fixture)
    }

    fn server(&self) -> Result<&Engram> {
        self.server.as_ref().context("fixture server")
    }

    async fn update(&self, maintenance: Value, paths: Vec<String>) -> Result<CallToolResult> {
        Ok(self
            .server()?
            .handle_update_project(serde_json::from_value(json!({
                "project_id":self.project,"wait":true,"only_paths":paths,"max_commits":0,
                "instruction_maintenance":maintenance
            }))?)
            .await?)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    for token in server.state.cancellation_tokens.read().await.values() {
                        token.cancel();
                    }
                    let workers: Vec<_> = server
                        .state
                        .active_jobs
                        .write()
                        .await
                        .drain()
                        .map(|(_, worker)| worker)
                        .collect();
                    for worker in workers {
                        let _ = worker.await;
                    }
                })
            });
        }
        // Fields then drop server/database handles before the owned TempDir.
    }
}

fn observation(result: &CallToolResult) -> Result<Value> {
    result
        .content
        .iter()
        .filter_map(|part| part.as_text())
        .filter_map(|part| serde_json::from_str::<Value>(&part.text).ok())
        .find_map(|value| value.get("instruction_reference_observation").cloned())
        .context("separate instruction observation")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selective_edit_collects_affected_guidance_and_inherits_outside_index_rows() -> Result<()> {
    let f = Fixture::new().await?;
    let outside = f
        .server()?
        .state
        .graph
        .get_node(&f.project, "file:Site/outside.js")?
        .context("outside file")?;
    let result = f
        .update(
            json!({"base_commit":f.base}),
            vec!["Site/selected.js".into()],
        )
        .await?;
    ensure!(result.is_error != Some(true));
    let summary = &result.content[0].as_text().context("source summary")?.text;
    ensure!(
        summary.contains("update_scope: only_paths")
            && summary.contains("outside_scope_disk_check: not_run")
    );
    let observed = observation(&result)?;
    ensure!(
        observed["status"] == "observed" && observed["atomic_index_and_policy_snapshot"] == false
    );
    ensure!(observed["authored_files_written"] == 0);
    ensure!(observed["source_update_scope"]["requested_only_paths"] == json!(["Site/selected.js"]));
    ensure!(observed["source_update_scope"]["outside_source_revisions"] == "inherited");
    let guide = observed["manifest"]["instructions"]
        .as_array()
        .context("instructions")?
        .iter()
        .find(|item| item["instruction_path"] == GUIDE_PATH)
        .context("affected guidance")?;
    ensure!(guide["source_blake3"] == blake3::hash(GUIDE.as_bytes()).to_hex().as_str());
    ensure!(
        guide["dependent_changes"]
            .as_array()
            .context("changes")?
            .iter()
            .any(|change| change["path"] == "Site/selected.js"
                && change["before_blake3"] == blake3::hash(BEFORE.as_bytes()).to_hex().as_str()
                && change["after_blake3"] == blake3::hash(AFTER.as_bytes()).to_hex().as_str())
    );
    let inherited = f
        .server()?
        .state
        .graph
        .get_node(&f.project, "file:Site/outside.js")?
        .context("inherited outside file")?;
    ensure!(inherited.generation == outside.generation && inherited.metadata == outside.metadata);
    let selected = f
        .server()?
        .state
        .graph
        .get_node(&f.project, "file:Site/selected.js")?
        .context("updated file")?;
    ensure!(
        selected.metadata.as_ref().context("file metadata")?["file_hash"]
            == blake3::hash(AFTER.as_bytes()).to_hex().as_str()
    );
    ensure!(std::fs::read_to_string(f.temporary.path().join("r").join(GUIDE_PATH))? == GUIDE);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_maintenance_refuses_before_refresh_but_collection_failure_keeps_published_edit()
-> Result<()> {
    let f = Fixture::new().await?;
    let before = f.server()?.state.registry.capture_source_head(&f.project)?;
    for (maintenance, paths) in [
        (
            json!({"base_commit":f.base,"draft":{"expected_packet_blake3":"0".repeat(64),"decisions":[]}}),
            vec!["Site/selected.js".into()],
        ),
        (
            json!({"base_commit":f.base}),
            (0..17).map(|i| format!("Site/file{i}.js")).collect(),
        ),
    ] {
        ensure!(f.update(maintenance, paths).await.is_err());
        ensure!(
            f.server()?
                .state
                .registry
                .capture_source_head(&f.project)?
                .generation()
                == before.generation()
        );
    }
    let result = f
        .update(
            json!({"base_commit":"0".repeat(40)}),
            vec!["Site/selected.js".into()],
        )
        .await?;
    ensure!(
        result.is_error != Some(true),
        "Optional collection failure must preserve the source result"
    );
    ensure!(observation(&result)?["status"] == "unavailable");
    ensure!(
        f.server()?
            .state
            .registry
            .capture_source_head(&f.project)?
            .generation()
            > before.generation()
    );
    let selected = f
        .server()?
        .state
        .graph
        .get_node(&f.project, "file:Site/selected.js")?
        .context("published edit")?;
    ensure!(
        selected.metadata.as_ref().context("file metadata")?["file_hash"]
            == blake3::hash(AFTER.as_bytes()).to_hex().as_str()
    );
    ensure!(std::fs::read_to_string(f.temporary.path().join("r").join(GUIDE_PATH))? == GUIDE);
    Ok(())
}
