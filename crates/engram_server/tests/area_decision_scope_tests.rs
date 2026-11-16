//! Area drafting preserves declared review scope without treating a ruling as policy.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::PathBuf;

const ENTRY: &str = "Site/Orders/order.ts";
const CONDITIONS: &str = "Only read-only previews; saving still requires ownership.";
const POLICY: &str = "---\napplyTo: Site/Orders/**\n---\nRetain ownership checks.\n";

struct Fixture {
    // Runtime/database owners close before their directory, including on unwind.
    server: Engram,
    owned: tempfile::TempDir,
    root: PathBuf,
    project: String,
    base: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(2)
            .tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site/Orders"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(
            root.join(ENTRY),
            "export function saveOrder() { return true; }\n",
        )?;
        std::fs::write(
            root.join("AGENTS.md"),
            "Preserve accepted exceptions and verify source before adopting policy.\n",
        )?;
        std::fs::write(
            root.join(".github/instructions/base.instructions.md"),
            POLICY,
        )?;
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned area fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned area source",
                &tree,
                &[],
            )?
            .to_string()
        };
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: true,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned scoped area","project_type":"general","wait":true
        }))?).await?;
        let text = &indexed.content[0].as_text().unwrap().text;
        let project = text
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
            root,
            project,
            base,
        })
    }

    async fn record(&self, id: &str, rationale: &str, scope: Option<&str>) -> Result<()> {
        let mut event = json!({
            "event_id":id,"finding_id":id,"supersedes":null,"kind":"accepted_exception",
            "source_url":"https://fixture.invalid/review/1","author":"Fixture reviewer",
            "recorded_at":"2026-11-01T00:00:00Z","rationale":rationale,"verification":null
        });
        if let Some(path) = scope {
            event["scope"] =
                json!({"reviewed_commit":self.base,"paths":[path],"conditions":CONDITIONS});
        }
        self.server
            .handle_record_review_decisions(serde_json::from_value(json!({
                "project_id":self.project,"review_id":"area-ruling","decisions":[event]
            }))?)
            .await?;
        Ok(())
    }

    async fn draft(&self) -> Result<Value> {
        let reply = self.server.handle_pre_push_audit(serde_json::from_value(json!({
            "project_id":self.project,"instruction_maintenance":{
                "base_commit":self.base,"area_context":{"scope":"Site/Orders","max_files":2},
                "new_instruction_targets":[{"instruction_path":".github/instructions/orders.instructions.md",
                    "provenance_path":".github/instructions/orders.provenance.json","area_scope":["Site/Orders/**"]}]
            }
        }))?).await?;
        Ok(serde_json::from_str(
            &reply.content[0].as_text().unwrap().text,
        )?)
    }

    fn close(self) -> Result<()> {
        let Self { server, owned, .. } = self;
        drop(server);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_exception_without_prose_path_survives_area_drafting_with_qualifications()
-> Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .record(
            "read-only-preview",
            "Read-only previews do not persist changes.",
            Some(ENTRY),
        )
        .await?;
    let draft = fixture.draft().await?;
    let items = draft["packet"]["area_evidence"]["review_evidence"]["items"]
        .as_array()
        .unwrap();
    ensure!(items.len() == 1, "{draft}");
    let item = &items[0];
    ensure!(item["event"]["event_id"] == "read-only-preview");
    ensure!(item["event"]["scope"]["reviewed_commit"] == fixture.base);
    ensure!(item["event"]["scope"]["conditions"] == CONDITIONS);
    ensure!(item["selection_basis"] == "declared_scope_intersection");
    ensure!(
        item["scope_and_revision_applicability"]
            .as_str()
            .unwrap()
            .starts_with("not_established")
    );
    ensure!(item["record_blake3"].as_str().unwrap().len() == 64);
    ensure!(
        item["recovery"]
            .to_string()
            .contains("get_review_decisions")
    );
    ensure!(draft["authored_files_written"] == 0);
    ensure!(
        !fixture
            .root
            .join(".github/instructions/orders.instructions.md")
            .exists()
    );
    ensure!(
        !fixture
            .root
            .join(".github/instructions/orders.provenance.json")
            .exists()
    );
    ensure!(
        std::fs::read_to_string(
            fixture
                .root
                .join(".github/instructions/base.instructions.md")
        )? == POLICY
    );
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_other_scope_overrides_incidental_selected_path_in_prose() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .record(
            "other-area",
            &format!("Compare {ENTRY}, but this ruling belongs to another area."),
            Some("Site/Accounts/account.ts"),
        )
        .await?;
    let draft = fixture.draft().await?;
    let evidence = &draft["packet"]["area_evidence"]["review_evidence"];
    ensure!(
        evidence["items"].as_array().unwrap().is_empty(),
        "{evidence}"
    );
    ensure!(evidence["coverage"]["unrelated_current_events"] == 1);
    fixture.close()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_unscoped_rulings_require_a_complete_relative_path() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture
        .record(
            "complete-path",
            &format!("Retained lead for /{ENTRY}:1; verify applicability."),
            None,
        )
        .await?;
    fixture
        .record("basename-only", "Retained lead for order.ts.", None)
        .await?;
    fixture
        .record(
            "longer-prefix",
            &format!("Retained lead for Other/{ENTRY}:1."),
            None,
        )
        .await?;
    fixture
        .record(
            "longer-suffix",
            &format!("Retained lead for {ENTRY}.backup:1."),
            None,
        )
        .await?;
    let draft = fixture.draft().await?;
    let evidence = &draft["packet"]["area_evidence"]["review_evidence"];
    let items = evidence["items"].as_array().unwrap();
    ensure!(items.len() == 1, "{evidence}");
    ensure!(items[0]["event"]["event_id"] == "complete-path");
    ensure!(items[0]["selection_basis"] == "unscoped_complete_path_mention");
    ensure!(items[0]["event"].get("scope").is_none());
    ensure!(evidence["coverage"]["unrelated_current_events"] == 3);
    fixture.close()
}
