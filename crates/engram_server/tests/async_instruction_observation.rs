//! Queued updates deliver instruction evidence without hiding source success.
use anyhow::{Context, Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram, read_report_delivery};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

const GUIDE: &str = "---\napplyTo: Site/**\n---\nPreserve authorized saves.\nException: read-only probes do not save.\nSee [shared](shared.md) and `Site/worker.js`.\n";
const BEFORE: &str = "export function savedInvoiceV1() { return 1; }\n";
const AFTER: &str = "export function savedInvoiceV2() { return 2; }\n";
const GUIDE_PATH: &str = ".github/instructions/orders.instructions.md";

fn text(result: &CallToolResult) -> Result<&str> {
    Ok(&result
        .content
        .first()
        .and_then(|part| part.as_text())
        .context("text result")?
        .text)
}

fn recover(data: &Path, manifest: &Value) -> Result<Value> {
    let mut next = manifest["response_delivery"]["next_call"].clone();
    let mut bytes = String::new();
    while !next.is_null() {
        let mut request: CallToolRequestParam = serde_json::from_value(json!({
            "name":next["tool"],"arguments":next["arguments"]
        }))?;
        let report =
            read_report_delivery::prepare(&mut request, &Engram::tool_registry().list_all())?
                .context("job recovery report")?;
        let page = report.recover(data)?.context("saved observation")?;
        ensure!(text(&page)?.len() <= 40960);
        let page: Value = serde_json::from_str(text(&page)?)?;
        ensure!(page["start_byte"] == bytes.len());
        bytes.push_str(page["content"].as_str().context("page content")?);
        next = page["next_call"].clone();
    }
    ensure!(
        blake3::hash(bytes.as_bytes()).to_hex().as_str()
            == manifest["response_delivery"]["response_id"]
                .as_str()
                .context("response id")?
    );
    let original: CallToolResult = serde_json::from_str(&bytes)?;
    ensure!(original.is_error != Some(true));
    Ok(serde_json::from_str(text(&original)?)?)
}

struct Fixture {
    e: Option<Engram>,
    project: String,
    base: String,
    // Drop closes every queued worker before stores and their owned directory.
    temporary: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temporary = tempfile::Builder::new().prefix("io-").tempdir_in(parent)?;
        let root = temporary.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(root.join("Site/worker.js"), BEFORE)?;
        std::fs::write(
            root.join("Site/Orders.sql"),
            "CREATE TABLE dbo.Orders (Id int);\n",
        )?;
        std::fs::write(root.join(GUIDE_PATH), GUIDE)?;
        std::fs::write(
            root.join(".github/instructions/shared.md"),
            "Preserve authorization.\n",
        )?;
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
                "Owned instruction baseline",
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
        let e = Engram::new(state);
        let indexed = e.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Queued instruction fixture","project_type":"general","wait":true
        }))?).await?;
        ensure!(indexed.is_error != Some(true), "{}", text(&indexed)?);
        let project = text(&indexed)?
            .split("project_id: ")
            .nth(1)
            .context("indexed project id")?
            .lines()
            .next()
            .context("project id line")?
            .trim()
            .to_owned();
        std::fs::write(root.join("Site/worker.js"), AFTER)?;
        Ok(Self {
            e: Some(e),
            project,
            base,
            temporary,
        })
    }
    fn server(&self) -> Result<&Engram> {
        self.e.as_ref().context("fixture server")
    }
    async fn queue(&self, base: &str) -> Result<String> {
        let result = self
            .server()?
            .handle_update_project(serde_json::from_value(json!({
                "project_id":self.project,"wait":false,"max_commits":0,
                "instruction_maintenance":{"base_commit":base,"changed_paths":["Site/worker.js"]}
            }))?)
            .await?;
        ensure!(result.is_error != Some(true));
        Ok(text(&result)?
            .split("job_id: ")
            .nth(1)
            .context("queued job id")?
            .lines()
            .next()
            .context("job id line")?
            .to_owned())
    }
    async fn settle(&self, job: &str) -> Result<String> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let e = self.server()?;
                let row = e.state.registry.get_job(job)?.context("job record")?;
                if matches!(
                    row.status.as_str(),
                    "done" | "degraded" | "failed" | "cancelled"
                ) && !e.state.active_jobs.read().await.contains_key(job)
                    && !e.state.cancellation_tokens.read().await.contains_key(job)
                {
                    return Ok(row.status);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("queued update completion deadline")?
    }
    async fn observation(&self, job: &str) -> Result<Value> {
        let result = self
            .server()?
            .handle_get_job_status(serde_json::from_value(json!({"job_id":job}))?)
            .await?;
        let line = text(&result)?
            .lines()
            .find_map(|line| line.strip_prefix("instruction_observation_delivery: "))
            .context("instruction delivery reference")?;
        ensure!(
            line.len() < 4096,
            "The job row must retain a bounded reference, not the entire manifest"
        );
        Ok(serde_json::from_str(line)?)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(e) = &self.e {
            // Both tests use a multithread runtime. Unwinding cancels and joins
            // owned workers before database handles and temporary paths drop.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    for token in e.state.cancellation_tokens.read().await.values() {
                        token.cancel();
                    }
                    let workers: Vec<_> = e
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
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_update_recovers_job_bound_manifest_after_runtime_and_guidance_change() -> Result<()>
{
    let mut f = Fixture::new().await?;
    let job = f.queue(&f.base).await?;
    ensure!(matches!(
        f.settle(&job).await?.as_str(),
        "done" | "degraded"
    ));
    let manifest = f.observation(&job).await?;
    ensure!(
        manifest["response_delivery"]["next_call"]["arguments"]
            .get("job_id")
            .is_none()
    );
    let data = f.temporary.path().join("data");
    let captured = recover(&data, &manifest)?;
    ensure!(captured["job_id"] == job);
    let observed = &captured["observation"]["instruction_reference_observation"];
    ensure!(observed["status"] == "observed" && observed["authored_files_written"] == 0);
    let state = observed["manifest"]["instructions"]
        .as_array()
        .context("instruction states")?
        .iter()
        .find(|state| state["instruction_path"] == GUIDE_PATH)
        .context("scoped guide")?;
    ensure!(state["source_blake3"] == blake3::hash(GUIDE.as_bytes()).to_hex().as_str());
    ensure!(
        state["dependent_changes"]
            .as_array()
            .context("dependencies")?
            .iter()
            .any(|change| change["path"] == "Site/worker.js"
                && change["before_blake3"] == blake3::hash(BEFORE.as_bytes()).to_hex().as_str()
                && change["after_blake3"] == blake3::hash(AFTER.as_bytes()).to_hex().as_str())
    );
    ensure!(std::fs::read_to_string(f.temporary.path().join("r").join(GUIDE_PATH))? == GUIDE);
    std::fs::write(
        f.temporary.path().join("r").join(GUIDE_PATH),
        "Later human guidance\n",
    )?;
    drop(f.e.take());
    ensure!(recover(&data, &manifest)? == captured);
    ensure!(
        std::fs::read_to_string(f.temporary.path().join("r").join(GUIDE_PATH))?
            == "Later human guidance\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_drafts_refuse_before_queue_and_failed_observation_preserves_published_source()
-> Result<()> {
    let f = Fixture::new().await?;
    let before = f
        .server()?
        .state
        .registry
        .list_jobs(Some(&f.project))?
        .len();
    let invalid = f
        .server()?
        .handle_update_project(serde_json::from_value(json!({
            "project_id":f.project,"wait":false,
            "instruction_maintenance":{"base_commit":f.base,"changed_paths":["Site/worker.js"],
                "draft":{"expected_packet_blake3":"not-a-proposal","decisions":[]}}
        }))?)
        .await;
    ensure!(
        invalid.is_err(),
        "A queued update must not accept policy drafts"
    );
    ensure!(
        f.server()?
            .state
            .registry
            .list_jobs(Some(&f.project))?
            .len()
            == before
    );
    let job = f.queue(&"0".repeat(40)).await?;
    ensure!(f.settle(&job).await? == "degraded");
    let captured = recover(
        &f.temporary.path().join("data"),
        &f.observation(&job).await?,
    )?;
    ensure!(
        captured["observation"]["instruction_reference_observation"]["status"] == "unavailable"
    );
    let result = f
        .server()?
        .handle_grep_project(serde_json::from_value(json!({
            "project_id":f.project,"pattern":"savedInvoiceV2","output_json":true
        }))?)
        .await?;
    ensure!(
        text(&result)?.contains("savedInvoiceV2"),
        "Instruction observation failure hid source publication"
    );
    ensure!(std::fs::read_to_string(f.temporary.path().join("r").join(GUIDE_PATH))? == GUIDE);
    let ordinary = f.server()?.handle_update_project(serde_json::from_value(json!({
        "project_id":f.project,"wait":false,"max_commits":0
    }))?).await?;
    let ordinary_job = text(&ordinary)?.split("job_id: ").nth(1).context("ordinary queued job")?.lines().next().context("ordinary job id")?;
    ensure!(matches!(f.settle(ordinary_job).await?.as_str(), "done"|"degraded"));
    let ordinary = f.server()?.handle_get_job_status(serde_json::from_value(json!({"job_id":ordinary_job}))?).await?;
    ensure!(!text(&ordinary)?.contains("instruction_observation_delivery:"));
    Ok(())
}
