//! Normal history use must fit the agent context and retain complete evidence.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{Engram, retained_results, state::AppState};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const PID: &str = "history-default-fixture";
const FILE: &str = "Site/History.ts";

fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

struct Fixture {
    app: Engram,
    state: AppState,
    root: PathBuf,
    oid: String,
    message: String,
    before: String,
    after: String,
    temporary: tempfile::TempDir,
}

fn commit(root: &Path, message: &str) -> Result<String> {
    let repo = git2::Repository::open(root)?;
    let mut index = repo.index()?;
    index.add_path(Path::new(FILE))?;
    let tree = repo.find_tree(index.write_tree()?)?;
    let signature = git2::Signature::now("Owned author", "fixture@example.invalid")?;
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents = parent.iter().collect::<Vec<_>>();
    let oid = repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?;
    repo.reference(
        "refs/remotes/origin/main",
        oid,
        true,
        "Owned published history",
    )?;
    repo.reference_symbolic(
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
        true,
        "Owned default branch",
    )?;
    Ok(oid.to_string())
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("Site"))?;
        drop(git2::Repository::init(&root)?);
        let before = (0..1000)
            .map(|n| format!("export const entry{n:04} = 'before';\n"))
            .collect::<String>();
        let after = before.replace("'before'", "'after'");
        std::fs::write(root.join(FILE), &before)?;
        commit(&root, "Before evidence\n")?;
        std::fs::write(root.join(FILE), &after)?;
        let message = format!(
            "Complete selected change\n\n{}ORIGINAL_MESSAGE_END\n",
            "Keep companion files and complete exceptions.\n".repeat(1200)
        );
        let oid = commit(&root, &message)?;
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: "Owned bounded history".into(),
            directory: root.to_string_lossy().into(),
            project_type: "general".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        Ok(Self {
            app: Engram::new(state.clone()),
            state,
            root,
            oid,
            message,
            before,
            after,
            temporary,
        })
    }

    fn args(&self, mode: &str, output_json: bool) -> Value {
        let mut args = json!({"project_id":PID,"query":self.oid,"as_of_rev":self.oid,
            "mode":mode,"output_json":output_json});
        if mode == "patch" {
            args["file_filter"] = json!(FILE);
        }
        args
    }

    fn close(self) -> Result<()> {
        let Self {
            app,
            state,
            temporary,
            ..
        } = self;
        drop(app);
        drop(state);
        temporary.close()?;
        Ok(())
    }
}

fn prepare(
    args: Value,
) -> Result<(
    CallToolRequestParam,
    Option<engram_server::read_report_delivery::Report>,
)> {
    let mut call = serde_json::from_value(json!({"name":"search_history","arguments":args}))?;
    let delivery = retained_results::prepare(&mut call, &Engram::tool_registry().list_all())?;
    Ok((call, delivery))
}

fn recover(data: &Path, manifest: &Value) -> Result<CallToolResult> {
    let mut action = manifest["response_delivery"]["next_call"].clone();
    let mut original = String::new();
    for _ in 0..200 {
        let (call, delivery) = prepare(action["arguments"].clone())?;
        ensure!(
            call.arguments.as_ref().unwrap().get("query").is_none(),
            "Recovery needs no fresh query"
        );
        let result = delivery.unwrap().recover(data)?.unwrap();
        ensure!(body(&result).len() <= 40960);
        let page: Value = serde_json::from_str(body(&result))?;
        ensure!(page["start_byte"] == original.len());
        original.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        action = page["next_call"].clone();
    }
    ensure!(
        blake3::hash(original.as_bytes()).to_hex().as_str()
            == manifest["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    Ok(serde_json::from_str(&original)?)
}

#[tokio::test]
async fn ordinary_commit_and_patch_replies_preserve_complete_evidence_in_both_formats() -> Result<()>
{
    let f = Fixture::new()?;
    for mode in ["commit", "patch"] {
        for output_json in [false, true] {
            let (call, delivery) = prepare(f.args(mode, output_json))?;
            let delivery =
                delivery.expect("Default ordinary history must get a complete reply budget");
            let original = f
                .app
                .handle_search_history(serde_json::from_value(json!(call.arguments.unwrap()))?)
                .await?;
            ensure!(body(&original).len() > 40960);
            let expected = serde_json::to_value(&original)?;
            let reply = delivery.finish(&f.state.cfg.data_dir, original)?;
            ensure!(body(&reply).len() <= 40960);
            let manifest: Value = serde_json::from_str(body(&reply))?;
            ensure!(manifest["response_delivery"]["status"] == "DEFERRED_READ_REPORT");
            let recovered = recover(&f.state.cfg.data_dir, &manifest)?;
            ensure!(serde_json::to_value(&recovered)? == expected);
            if output_json {
                let result: Value = serde_json::from_str(body(&recovered))?;
                let row = &result["results"][0];
                if mode == "commit" {
                    ensure!(row["content"] == f.message && row["files"] == json!([FILE]));
                } else {
                    let patch = row["patch"].as_str().unwrap();
                    let removed = patch
                        .lines()
                        .filter(|line| !line.starts_with("--- "))
                        .filter_map(|line| line.strip_prefix('-'))
                        .map(|line| format!("{line}\n"))
                        .collect::<String>();
                    let added = patch
                        .lines()
                        .filter(|line| !line.starts_with("+++ "))
                        .filter_map(|line| line.strip_prefix('+'))
                        .map(|line| format!("{line}\n"))
                        .collect::<String>();
                    ensure!(
                        removed == f.before && added == f.after,
                        "Selected patch lost authored source bytes"
                    );
                    ensure!(
                        row["patch_blake3"] == blake3::hash(patch.as_bytes()).to_hex().as_str()
                    );
                }
            } else {
                ensure!(body(&recovered).contains(if mode == "commit" {
                    "ORIGINAL_MESSAGE_END"
                } else {
                    "+export const entry0999 = 'after';"
                }));
            }
        }
    }
    f.close()
}

#[tokio::test]
async fn default_recovery_keeps_original_history_after_subsequent_git_and_disk_changes()
-> Result<()> {
    let f = Fixture::new()?;
    let (call, delivery) = prepare(f.args("commit", true))?;
    let original = f
        .app
        .handle_search_history(serde_json::from_value(json!(call.arguments.unwrap()))?)
        .await?;
    let reply = delivery.unwrap().finish(&f.state.cfg.data_dir, original)?;
    let manifest: Value = serde_json::from_str(body(&reply))?;
    std::fs::write(
        f.root.join(FILE),
        "export const changedAfterCapture = true;\n",
    )?;
    commit(&f.root, "Later unrelated source and history\n")?;
    let recovered = recover(&f.state.cfg.data_dir, &manifest)?;
    let result: Value = serde_json::from_str(body(&recovered))?;
    ensure!(
        result["results"][0]["content"] == f.message && result["results"][0]["commit"] == f.oid
    );
    ensure!(result["scope_commit"] == f.oid);
    ensure!(!body(&recovered).contains("changedAfterCapture"));
    f.close()
}

#[tokio::test]
async fn fitting_default_and_explicit_budget_or_capture_keep_existing_contracts() -> Result<()> {
    let f = Fixture::new()?;
    std::fs::write(f.root.join(FILE), "export const small = true;\n")?;
    let small = commit(&f.root, "Small complete message\n")?;
    let args = json!({"project_id":PID,"query":small,"mode":"commit","output_json":true,"as_of_rev":small});
    let (call, delivery) = prepare(args)?;
    let original = f
        .app
        .handle_search_history(serde_json::from_value(json!(call.arguments.unwrap()))?)
        .await?;
    let expected = serde_json::to_value(&original)?;
    ensure!(body(&original).len() < 40960);
    let reply = delivery.unwrap().finish(&f.state.cfg.data_dir, original)?;
    ensure!(serde_json::to_value(&reply)? == expected);
    let mut explicit = f.args("commit", true);
    explicit["max_tokens"] = json!(65536);
    let (call, delivery) = prepare(explicit)?;
    ensure!(
        delivery.is_none(),
        "Explicit whole-record budgeting remains handler-owned"
    );
    let reply = f
        .app
        .handle_search_history(serde_json::from_value(json!(call.arguments.unwrap()))?)
        .await?;
    let result: Value = serde_json::from_str(body(&reply))?;
    ensure!(result["results"][0]["content"] == f.message);
    let mut capture = f.args("commit", true);
    capture["retain_response"] = json!(true);
    let (call, delivery) = prepare(capture)?;
    let original = f
        .app
        .handle_search_history(serde_json::from_value(json!(call.arguments.unwrap()))?)
        .await?;
    let reply = delivery.unwrap().finish(&f.state.cfg.data_dir, original)?;
    let manifest: Value = serde_json::from_str(body(&reply))?;
    ensure!(
        manifest["response_delivery"]["capture_scope"]
            .as_str()
            .unwrap()
            .contains("requested result window")
    );
    let recovered = recover(&f.state.cfg.data_dir, &manifest)?;
    ensure!(serde_json::from_str::<Value>(body(&recovered))?["results"][0]["content"] == f.message);
    f.close()
}
