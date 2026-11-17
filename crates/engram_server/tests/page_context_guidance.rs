//! Page orientation must retain authored policy, not just observed house style.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const GUIDE: &str = "Preserve owner approval.\nException: read-only previews never submit.\n";
const CODE_GUIDE: &str =
    "---\napplyTo: Site/Host.aspx.vb\n---\nKeep server authorization before writes.\n";
const WIDGET_GUIDE: &str =
    "---\napplyTo: Site/Widgets/**\n---\nPreserve the shared editor's accepted exceptions.\n";

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("pc-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site/Widgets"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        let mut markup = "<%@ Page Language='VB' CodeFile='Host.aspx.vb' Inherits='Host' %>\n<%@ Register Src='~/Widgets/Editor.ascx' TagPrefix='shared' TagName='Editor' %>\n<shared:Editor ID='Details' runat='server' />\n".to_owned();
        for number in 0..120 {
            markup.push_str(&format!(
                "<asp:TextBox ID='Value{number}' runat='server' />\n"
            ));
        }
        for (path, body) in [
            ("Site/Host.aspx", markup.as_str()),
            (
                "Site/Host.aspx.vb",
                "Public Class Host\nPublic Sub Save()\nEnd Sub\nEnd Class\n",
            ),
            (
                "Site/Widgets/Editor.ascx",
                "<%@ Control Language='VB' CodeFile='Editor.ascx.vb' %>\n<asp:TextBox ID='SharedValue' runat='server' />\n",
            ),
            (
                "Site/Widgets/Editor.ascx.vb",
                "Public Class Editor\nEnd Class\n",
            ),
            ("Site/web.config", "<configuration />\n"),
            ("AGENTS.md", GUIDE),
            (".github/instructions/code.instructions.md", CODE_GUIDE),
            (".github/instructions/widget.instructions.md", WIDGET_GUIDE),
            (
                ".github/instructions/elsewhere.instructions.md",
                "---\napplyTo: Elsewhere/**\n---\nUnrelated requirements.\n",
            ),
        ] {
            std::fs::write(root.join(path), body)?;
        }
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: "page-policy-owned".into(),
            project_name: "Owned page policy".into(),
            project_type: "general".into(),
            directory: root.to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        Ok(Self {
            server: Engram::new(state),
            owned,
        })
    }
    fn args(&self, json_output: bool) -> Value {
        json!({"project_id":"page-policy-owned","aspx_file":"Site/Host.aspx",
            "output_json":json_output,"include_master_page":false,"include_house_style":false})
    }
    async fn read(&self, args: Value) -> Result<String> {
        let result = self
            .server
            .handle_get_page_context(serde_json::from_value(args)?)
            .await?;
        ensure!(result.is_error != Some(true));
        Ok(result.content[0].as_text().unwrap().text.clone())
    }
    fn close(self) -> Result<()> {
        let Self { server, owned } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}
fn has(policy: &Value, path: &str, content: &str) -> bool {
    policy["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["path"] == path && row["content"] == content)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_json_and_markdown_keep_page_companion_policy_and_exclude_unrelated_scope()
-> Result<()> {
    let fixture = Fixture::new()?;
    let json: Value = serde_json::from_str(&fixture.read(fixture.args(true)).await?)?;
    let policy = &json["project_policy_sources"];
    ensure!(has(policy, "AGENTS.md", GUIDE));
    ensure!(has(
        policy,
        ".github/instructions/code.instructions.md",
        CODE_GUIDE
    ));
    ensure!(has(
        policy,
        ".github/instructions/widget.instructions.md",
        WIDGET_GUIDE
    ));
    ensure!(
        !policy["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == ".github/instructions/elsewhere.instructions.md")
    );
    let markdown = fixture.read(fixture.args(false)).await?;
    ensure!(
        markdown.contains("read-only previews never submit")
            && markdown.contains("Keep server authorization before writes")
    );
    let mut markup_only = fixture.args(true);
    markup_only["include_codebehind"] = json!(false);
    let markup_only: Value = serde_json::from_str(&fixture.read(markup_only).await?)?;
    ensure!(!has(
        &markup_only["project_policy_sources"],
        ".github/instructions/code.instructions.md",
        CODE_GUIDE
    ));
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budgeted_pages_keep_policy_and_refuse_old_continuation_after_guide_changes() -> Result<()>
{
    let fixture = Fixture::new()?;
    let mut args = fixture.args(true);
    args["max_tokens"] = json!(4096);
    let first_text = fixture.read(args).await?;
    ensure!(first_text.len() <= 16384);
    let first: Value = serde_json::from_str(&first_text)?;
    ensure!(has(
        &first["required_context"]["project_policy_sources"],
        "AGENTS.md",
        GUIDE
    ));
    ensure!(
        !first["continuation"].is_null(),
        "The owned control inventory must page"
    );
    let next_args = first["continuation"]["arguments"].clone();
    let second: Value = serde_json::from_str(&fixture.read(next_args.clone()).await?)?;
    ensure!(has(
        &second["required_context"]["project_policy_sources"],
        "AGENTS.md",
        GUIDE
    ));
    std::fs::write(
        fixture.owned.path().join("r/AGENTS.md"),
        format!("{GUIDE}\nKeep another owner decision.\n"),
    )?;
    let error = fixture
        .server
        .handle_get_page_context(serde_json::from_value(next_args)?)
        .await
        .unwrap_err();
    ensure!(error.message.contains("context_changed"));
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_authored_guidance_keeps_omission_hash_and_executable_source_recovery() -> Result<()>
{
    let fixture = Fixture::new()?;
    let large = format!(
        "{GUIDE}{}",
        "Retain complete source before changing the approved flow.\n".repeat(400)
    );
    std::fs::write(fixture.owned.path().join("r/AGENTS.md"), &large)?;
    let mut args = fixture.args(true);
    args["max_tokens"] = json!(65536);
    let result: Value = serde_json::from_str(&fixture.read(args).await?)?;
    let policy = &result["required_context"]["project_policy_sources"];
    ensure!(policy["coverage"] != "complete");
    let omitted = policy["omissions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == "AGENTS.md")
        .expect("Large source must have complete recovery metadata");
    ensure!(omitted["blake3"] == blake3::hash(large.as_bytes()).to_hex().as_str());
    let request: engram_server::models::GetFullMethodBodyRequest =
        serde_json::from_value(omitted["read_action"]["arguments"].clone())?;
    ensure!(request.file_path.as_deref() == Some("AGENTS.md"));
    ensure!(
        request.expected_file_blake3.as_deref()
            == Some(blake3::hash(large.as_bytes()).to_hex().as_str())
    );
    let recovered = fixture.server.handle_get_full_method_body(request).await?;
    ensure!(recovered.is_error != Some(true));
    fixture.close()
}
