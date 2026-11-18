//! An agent needs applicable instructions before creating an unindexed file.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};

const VB_PATH: &str = "Site/Reports/NewInvoice.aspx.vb";
const REPORT_PATH: &str = ".github/instructions/reports.instructions.md";
const REPORT: &str = "---\napplyTo: Site/Reports/**\n---\nKeep approved exports read-only.\nException: authorized draft editors may change unsent invoices.\n";
const QUESTION: &str =
    "What repository instructions apply before creating `Site/Reports/NewInvoice.aspx.vb`?";

struct Fixture {
    // Release application/database handles before the owned directory, including unwinding.
    server: Engram,
    owned: tempfile::TempDir,
    project: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("apg-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        for (path, body) in [
            (
                "bootstrap.js",
                "export function heartbeat() { return 17; }\n",
            ),
            (REPORT_PATH, REPORT),
            (
                ".github/instructions/api.instructions.md",
                "---\napplyTo: Site/Api/**\n---\nUNRELATED_API_RULE\n",
            ),
            (
                ".github/instructions/scripts.instructions.md",
                "---\napplyTo: Site/Scripts/**\n---\nKeep the public callback signature.\n",
            ),
            (
                ".github/instructions/sql.instructions.md",
                "---\napplyTo: Database/**\n---\nPreserve tenant filters on reads.\n",
            ),
        ] {
            std::fs::write(root.join(path), body)?;
        }
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
            "directory":root,"project_name":"Owned prospective guidance","project_type":"general","wait":true
        }))?).await?;
        let project = indexed.content[0]
            .as_text()
            .unwrap()
            .text
            .split("project_id: ")
            .nth(1)
            .and_then(|text| text.lines().next())
            .ok_or_else(|| anyhow::anyhow!("Missing fixture project identity"))?
            .trim()
            .to_owned();
        Ok(Self {
            server,
            owned,
            project,
        })
    }

    async fn ask(&self, question: &str, format: &str) -> Result<String> {
        let result = self
            .server
            .handle_ask_codebase(serde_json::from_value(json!({
                "project_id":self.project,"question":question,"output_format":format,
                "deadline_ms":2000,"max_tokens":16384
            }))?)
            .await?;
        ensure!(result.is_error != Some(true));
        Ok(result.content[0].as_text().unwrap().text.clone())
    }

    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

fn sources(report: &Value) -> &[Value] {
    report["project_policy_sources"]["sources"]
        .as_array()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prospective_webforms_file_receives_full_scoped_guidance_without_code_evidence()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let report: Value = serde_json::from_str(&fixture.ask(QUESTION, "json").await?)?;
    let selected = sources(&report);
    let guide = selected
        .iter()
        .find(|row| row["path"] == REPORT_PATH)
        .ok_or_else(|| anyhow::anyhow!("Missing reports instruction: {report}"))?;
    ensure!(guide["content"] == REPORT);
    ensure!(guide["blake3"] == blake3::hash(REPORT.as_bytes()).to_hex().to_string());
    ensure!(!selected.iter().any(|row| {
        row["path"]
            .as_str()
            .unwrap()
            .ends_with("api.instructions.md")
    }));
    ensure!(
        report["project_policy_sources"]["notes"]
            .to_string()
            .contains(VB_PATH)
    );
    ensure!(
        !report["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == VB_PATH)
    );
    ensure!(!fixture.owned.path().join("r").join(VB_PATH).exists());
    ensure!(report["mode"] == "retrieval_only");
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn javascript_and_sql_planned_paths_keep_their_separate_authored_scopes() -> Result<()> {
    let fixture = Fixture::new().await?;
    let question =
        "Which instructions apply to `Site/Scripts/new-grid.ts` and `Database/new-invoice.sql`?";
    let report: Value = serde_json::from_str(&fixture.ask(question, "json").await?)?;
    let selected = sources(&report);
    ensure!(selected.iter().any(|row| {
        row["content"]
            .as_str()
            .is_some_and(|text| text.contains("Keep the public callback signature."))
    }));
    ensure!(selected.iter().any(|row| {
        row["content"]
            .as_str()
            .is_some_and(|text| text.contains("Preserve tenant filters on reads."))
    }));
    ensure!(!selected.iter().any(|row| {
        row["path"] == REPORT_PATH
            || row["content"]
                .as_str()
                .is_some_and(|text| text.contains("UNRELATED_API_RULE"))
    }));
    for path in ["Site/Scripts/new-grid.ts", "Database/new-invoice.sql"] {
        ensure!(!fixture.owned.path().join("r").join(path).exists());
    }
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guidance_changes_are_live_while_ordinary_code_questions_keep_their_existing_scope()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let ordinary: Value = serde_json::from_str(
        &fixture
            .ask("Explain `Site/Reports/NewInvoice.aspx.vb`", "json")
            .await?,
    )?;
    ensure!(
        !sources(&ordinary)
            .iter()
            .any(|row| row["path"] == REPORT_PATH)
    );
    let before = fixture.ask(QUESTION, "markdown").await?;
    ensure!(before.contains(REPORT));
    let changed = REPORT.replace("unsent invoices", "unsent invoices after manager approval");
    std::fs::write(fixture.owned.path().join("r").join(REPORT_PATH), &changed)?;
    let after: Value = serde_json::from_str(&fixture.ask(QUESTION, "json").await?)?;
    let guide = sources(&after)
        .iter()
        .find(|row| row["path"] == REPORT_PATH)
        .unwrap();
    ensure!(guide["content"] == changed);
    ensure!(guide["blake3"] == blake3::hash(changed.as_bytes()).to_hex().to_string());
    ensure!(!fixture.owned.path().join("r").join(VB_PATH).exists());
    fixture.close()
}
