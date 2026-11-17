//! Ordinary concept guidance omissions must be executable without guessing arguments.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

fn guide() -> String {
    format!(
        "{}Exception: read-only previews never submit.\n",
        "Preserve the existing approval and ownership checks before changing this workflow.\n"
            .repeat(180)
    )
}
fn scoped_guide() -> String {
    format!(
        "---\napplyTo: Site/**\n---\n{}",
        "Use existing server-side checks. Preserve all manually approved exceptions.\n".repeat(100)
    )
}
struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    project: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("fg-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        drop(git2::Repository::init(&root)?);
        std::fs::write(
            root.join("Site/language.vb"),
            "Public Class Language\nPublic Sub ChangeLanguage()\nEnd Sub\nEnd Class\n",
        )?;
        std::fs::write(root.join("AGENTS.md"), guide())?;
        std::fs::write(
            root.join(".github/instructions/language.instructions.md"),
            scoped_guide(),
        )?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: true,
            watch_debounce_secs: 3600,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned concept guidance","project_type":"general","wait":true
        }))?).await?;
        let project = text(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            server,
            owned,
            project,
        })
    }
    async fn footprint(&self, output_json: bool, tokens: usize) -> Result<String> {
        let result = self.server.handle_get_concept_footprint(serde_json::from_value(json!({
            "project_id":self.project,"concept":"language","output_json":output_json,"max_tokens":tokens
        }))?).await?;
        ensure!(result.is_error != Some(true));
        Ok(text(&result).to_owned())
    }
    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn action(footprint: &Value) -> Value {
    footprint["project_policy_sources"]["omissions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == "AGENTS.md")
        .unwrap()["read_action"]
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_and_markdown_supply_guarded_recovery_of_the_complete_omitted_guide() -> Result<()> {
    let fixture = Fixture::new().await?;
    let footprint: Value = serde_json::from_str(&fixture.footprint(true, 65536).await?)?;
    ensure!(
        footprint["source_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == "Site/language.vb")
    );
    let original = guide();
    let hash = blake3::hash(original.as_bytes()).to_hex().to_string();
    let first = action(&footprint);
    ensure!(
        first["tool"] == "get_full_method_body"
            && first["arguments"]["project_id"] == fixture.project
    );
    ensure!(
        first["arguments"]["file_path"] == "AGENTS.md"
            && first["arguments"]["expected_file_blake3"] == hash
    );
    let mut next = first;
    let mut lines = Vec::new();
    let mut eof = false;
    for _ in 0..10 {
        let result = fixture
            .server
            .handle_get_full_method_body(serde_json::from_value(next["arguments"].clone())?)
            .await?;
        let page: Value = serde_json::from_str(text(&result))?;
        ensure!(page["source_file_hash"] == hash);
        lines.extend(
            page["source_code"]
                .as_str()
                .unwrap()
                .lines()
                .map(str::to_owned),
        );
        if page["file_excerpt"]["reaches_eof"] == true {
            eof = true;
            break;
        }
        next = page["file_excerpt"]["next_page"].clone();
    }
    ensure!(eof && lines == original.lines().map(str::to_owned).collect::<Vec<_>>());
    let markdown = fixture.footprint(false, 65536).await?;
    ensure!(
        markdown.contains("get_full_method_body")
            && markdown.contains("expected_file_blake3")
            && markdown.contains(&hash)
    );
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn captured_footprint_keeps_old_guards_while_live_source_refuses_and_fresh_footprint_rebinds()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let first: Value = serde_json::from_str(&fixture.footprint(true, 65536).await?)?;
    let old_action = action(&first);
    let bounded_text = fixture.footprint(true, 2048).await?;
    ensure!(bounded_text.len() <= 8192);
    let bounded: Value = serde_json::from_str(&bounded_text)?;
    ensure!(bounded["response_delivery"]["status"] == "DEFERRED_CONCEPT_FOOTPRINT");
    let changed = format!("{}Keep the newly recorded owner decision.\n", guide());
    std::fs::write(fixture.owned.path().join("r/AGENTS.md"), &changed)?;
    ensure!(
        fixture
            .server
            .handle_get_full_method_body(serde_json::from_value(old_action["arguments"].clone())?)
            .await
            .is_err()
    );
    let mut next = bounded["response_delivery"]["next_call"].clone();
    let mut original = String::new();
    let mut complete = false;
    for _ in 0..20 {
        let result = fixture
            .server
            .handle_get_concept_footprint(serde_json::from_value(next["arguments"].clone())?)
            .await?;
        let page: Value = serde_json::from_str(text(&result))?;
        ensure!(page["start_byte"].as_u64() == Some(original.len() as u64));
        original.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            complete = true;
            break;
        }
        next = page["next_call"].clone();
    }
    ensure!(
        complete
            && blake3::hash(original.as_bytes()).to_hex().as_str()
                == bounded["response_delivery"]["response_id"]
                    .as_str()
                    .unwrap()
    );
    let saved: Value = serde_json::from_str(&original)?;
    ensure!(action(&saved) == old_action);
    let fresh: Value = serde_json::from_str(&fixture.footprint(true, 65536).await?)?;
    ensure!(
        action(&fresh)["arguments"]["expected_file_blake3"]
            == blake3::hash(changed.as_bytes()).to_hex().as_str()
    );
    fixture.close()
}
