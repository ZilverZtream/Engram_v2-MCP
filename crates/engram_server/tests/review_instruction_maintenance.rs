//! Ordinary reviews deliver existing maintenance evidence without accepting policy.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::PathBuf;

const BEFORE: &str = "export function SaveInvoice() { return true; }\n";
const AFTER: &str = "export function SaveInvoice() { return false; }\n";
const GUIDE: &str = "---\napplyTo: Site/**\n---\nReview `SaveInvoice()` when invoice audit behavior changes. Preserve the approved manual replay exception.\n";
struct Fixture {
    app: Engram,
    pid: String,
    base: String,
    root: PathBuf,
    guide: String,
    owned: tempfile::TempDir,
}
impl Fixture {
    async fn new(large: bool) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_GIT_FIXTURE_ROOT")
            .or_else(|| std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("rm-").tempdir_in(parent)?;
        let root = owned.path().join("p");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join("Schema"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        let guide = if large {
            format!("{GUIDE}{}","Explanatory context for maintainers; retain the approved manual replay exception. ?.\n".repeat(800))
        } else {
            GUIDE.into()
        };
        std::fs::write(
            root.join(".github/instructions/invoice.instructions.md"),
            &guide,
        )?;
        std::fs::write(
            root.join("AGENTS.md"),
            "Preserve accepted scope and exceptions.\n",
        )?;
        std::fs::write(root.join("Site/invoice.ts"), BEFORE)?;
        std::fs::write(
            root.join("Schema/invoice.sql"),
            "CREATE TABLE dbo.InvoiceAudit (Id int NOT NULL);\n",
        )?;
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Initial source and scoped guidance",
                &tree,
                &[],
            )?
            .to_string()
        };
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("d"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        let app = Engram::new(state);
        let indexed=app.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned review maintenance","project_type":"general","wait":true}))?).await?;
        let pid = indexed.content[0]
            .as_text()
            .unwrap()
            .text
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        std::fs::write(root.join("Site/invoice.ts"), AFTER)?;
        std::fs::write(
            root.join("Schema/invoice.sql"),
            "CREATE TABLE dbo.InvoiceAudit (Id int NOT NULL, Preview bit);\n",
        )?;
        Ok(Self {
            app,
            pid,
            base,
            root,
            guide,
            owned,
        })
    }
    async fn review(&self, diff: &str, output_json: bool, max_tokens: usize) -> Result<String> {
        let reply=self.app.handle_pre_commit_review(serde_json::from_value(json!({"project_id":self.pid,"diff":diff,"output_json":output_json,"max_tokens":max_tokens,"skip_gates":["blast_radius","temporal"]}))?).await?;
        ensure!(reply.is_error != Some(true));
        Ok(reply.content[0].as_text().unwrap().text.clone())
    }
    fn close(self) -> Result<()> {
        let Self { app, owned, .. } = self;
        drop(app);
        owned.close()?;
        Ok(())
    }
}

#[tokio::test]
async fn native_review_supplies_before_current_ts_sql_guidance_and_same_markdown_packet()
-> Result<()> {
    let f = Fixture::new(false).await?;
    let result = async {
        let report: Value = serde_json::from_str(&f.review("unstaged", true, 65536).await?)?;
        let context = &report["instruction_maintenance_context"];
        ensure!(
            context["status"] == "maintenance_evidence_collected_draft_required",
            "{context}"
        );
        ensure!(context["comparison_commit"] == f.base);
        ensure!(
            context["authored_files_written"] == 0
                && context["maintenance_semantic_review_performed"] == false
        );
        let packet = &context["report"]["packet"];
        ensure!(packet["base_commit"] == f.base);
        let evidence = packet["evidence"].as_array().unwrap();
        let ts = evidence
            .iter()
            .find(|r| r["path"] == "Site/invoice.ts")
            .unwrap();
        ensure!(ts["before"]["content"] == BEFORE && ts["after"]["content"] == AFTER);
        ensure!(evidence.iter().any(|r| {
            r["path"] == "Schema/invoice.sql"
                && r["after"]["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("Preview bit"))
        }));
        ensure!(
            packet["instructions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["current"]["content"] == f.guide)
        );
        ensure!(
            packet["instruction_manifest"]["instructions"]
                .as_array()
                .is_some_and(|r| !r.is_empty())
        );
        let markdown = f.review("unstaged", false, 65536).await?;
        let section = markdown
            .split("## Instruction maintenance evidence")
            .nth(1)
            .unwrap()
            .split("```json\n")
            .nth(1)
            .unwrap()
            .split("\n```")
            .next()
            .unwrap();
        let same: Value = serde_json::from_str(section)?;
        ensure!(same["packet_blake3"] == context["packet_blake3"]);
        ensure!(
            std::fs::read_to_string(f.root.join(".github/instructions/invoice.instructions.md"))?
                == f.guide
        );
        ensure!(
            report["coverage"]["compilation"] == "not_run"
                && report["coverage"]["test_execution"] == "not_run"
        );
        Ok(())
    }
    .await;
    f.close()?;
    result
}

#[tokio::test]
async fn raw_diff_without_explicit_base_does_not_guess_current_head_as_maintenance_authority()
-> Result<()> {
    let f = Fixture::new(false).await?;
    let result = async {
        let raw = {
            let repo = git2::Repository::open(&f.root)?;
            let tree = repo.head()?.peel_to_tree()?;
            let diff = repo.diff_tree_to_workdir_with_index(Some(&tree), None)?;
            let mut patch = Vec::new();
            diff.print(git2::DiffFormat::Patch, |_, _, line| {
                if matches!(line.origin(), '+' | '-' | ' ') {
                    patch.push(line.origin() as u8);
                }
                patch.extend_from_slice(line.content());
                true
            })?;
            String::from_utf8(patch)?
        };
        let report: Value = serde_json::from_str(&f.review(&raw, true, 65536).await?)?;
        let context = &report["instruction_maintenance_context"];
        ensure!(
            context["status"] == "unavailable_comparison_base"
                && context["comparison_commit"].is_null()
        );
        ensure!(
            context["evidence_collection_performed"] == false && context.get("report").is_none()
        );
        ensure!(
            context["reason"]
                .as_str()
                .unwrap()
                .contains("no current HEAD is guessed")
        );
        ensure!(report["coverage"]["head_before"] == f.base);
        Ok(())
    }
    .await;
    f.close()?;
    result
}

#[tokio::test]
async fn empty_scoped_review_keeps_scope_in_its_alternate_diff_call() -> Result<()> {
    let fixture = Fixture::new(false).await?;
    let reply = fixture.app.handle_pre_commit_review(serde_json::from_value(json!({
        "project_id":fixture.pid, "diff":"staged", "path_prefixes":["Schema/"],
        "output_json":true, "max_tokens":65536, "skip_gates":["blast_radius", "temporal"]
    }))?).await?;
    let empty: Value = serde_json::from_str(&reply.content[0].as_text().unwrap().text)?;
    ensure!(empty["verdict"] == "NOT_REVIEWED");
    ensure!(empty["coverage"]["path_prefixes"] == json!(["Schema"]));
    ensure!(empty["coverage"]["unexamined_files"] == json!(["Schema/invoice.sql"]));
    let action = &empty["review_next_calls"][0];
    ensure!(action["tool"] == "pre_commit_review");
    ensure!(action["arguments"]["diff"] == "unstaged");
    ensure!(action["arguments"]["path_prefixes"] == json!(["Schema"]));
    let reply = fixture.app.handle_pre_commit_review(
        serde_json::from_value(action["arguments"].clone())?).await?;
    let report: Value = serde_json::from_str(&reply.content[0].as_text().unwrap().text)?;
    ensure!(report["coverage"]["submitted_files"] == json!(["Schema/invoice.sql"]));
    fixture.close()
}

#[tokio::test]
async fn deferred_review_recovers_original_complete_maintenance_packet_after_inputs_change()
-> Result<()> {
    let f = Fixture::new(true).await?;
    let result = async {
        let raw = f.review("unstaged", true, 4096).await?;
        ensure!(raw.len() <= 16384);
        let compact: Value = serde_json::from_str(&raw)?;
        ensure!(compact["response_delivery"]["status"] == "DEFERRED_REVIEW");
        ensure!(
            compact["instruction_maintenance_context"]["maintenance_evidence_deferred"] == true
        );
        std::fs::write(
            f.root.join(".github/instructions/invoice.instructions.md"),
            "Changed guidance after review.\n",
        )?;
        std::fs::write(f.root.join("Site/invoice.ts"), "export const later = 2;\n")?;
        let mut next = compact["response_delivery"]["next_call"].clone();
        let mut restored = String::new();
        loop {
            let reply = f
                .app
                .handle_pre_commit_review(serde_json::from_value(next["arguments"].clone())?)
                .await?;
            let raw = &reply.content[0].as_text().unwrap().text;
            ensure!(raw.len() <= 16384);
            let page: Value = serde_json::from_str(raw)?;
            ensure!(page["start_byte"].as_u64() == Some(restored.len() as u64));
            restored.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            next = page["next_call"].clone();
        }
        ensure!(
            blake3::hash(restored.as_bytes()).to_hex().as_str()
                == compact["response_delivery"]["response_id"]
                    .as_str()
                    .unwrap()
        );
        let report: Value = serde_json::from_str(&restored)?;
        let context = &report["instruction_maintenance_context"];
        ensure!(
            context["report"]["packet"]["instructions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["current"]["content"] == f.guide)
        );
        ensure!(
            context["report"]["packet"]["evidence"]
                .to_string()
                .contains("return false")
        );
        ensure!(
            !context
                .to_string()
                .contains("Changed guidance after review")
        );
        Ok(())
    }
    .await;
    f.close()?;
    result
}
