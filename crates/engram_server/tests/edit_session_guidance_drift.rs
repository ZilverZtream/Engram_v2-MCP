//! Task guidance changes are explicit, scope-stable and recoverable.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};
use std::path::PathBuf;

const PID: &str = "guidance-drift-fixture";
const FILE: &str = "Site/map/Preview.ts";
const ROOT: &str = "# Authorization\nPreserve ownership. Read [shared](docs/shared.md).\n";
const LINK: &str = "# Shared policy evidence\nException: cached previews are read-only.\n";
const MAP: &str = "---\napplyTo: 'Site/map/**'\n---\nPreserve layer visibility.\n";
const BILLING: &str = "---\napplyTo: 'Site/billing/**'\n---\nPreserve entered amounts.\n";

// Server/cache/registry handles drop before the owned directory, including unwind.
struct Fixture {
    app: Engram,
    state: AppState,
    root: PathBuf,
    temporary: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("project");
        std::fs::create_dir_all(&root)?;
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: "Owned guidance drift".into(),
            directory: root.to_string_lossy().into(),
            project_type: "general".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })?;
        state.registry.set_meta(PID, "active_generation", "1")?;
        let fixture = Self {
            app: Engram::new(state.clone()),
            state,
            root,
            temporary,
        };
        fixture.write("AGENTS.md", ROOT)?;
        fixture.write("docs/shared.md", LINK)?;
        fixture.write(".github/instructions/map.instructions.md", MAP)?;
        fixture.write(".github/instructions/billing.instructions.md", BILLING)?;
        fixture.write(FILE, "export function preview() { return true; }\n")?;
        Ok(fixture)
    }

    fn write(&self, path: &str, text: &str) -> Result<()> {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, text)?;
        Ok(())
    }

    async fn begin(&self) -> Result<Value> {
        let reply = self
            .app
            .handle_begin_edit_session(serde_json::from_value(json!({
                "project_id":PID,"session_id":"worker","planned_files":[FILE],
                "output_json":true,"max_tokens":65536
            }))?)
            .await?;
        Ok(serde_json::from_str(
            &reply.content[0].as_text().unwrap().text,
        )?)
    }

    async fn complete(&self, opening: &Value, edited: &[&str], tokens: usize) -> Result<Value> {
        let reply = self
            .app
            .handle_complete_edit_session(serde_json::from_value(json!({
                "project_id":PID,"session_id":"worker",
                "session_revision":opening["summary"]["session_revision"],"edited_files":edited,
                "output_json":true,"max_tokens":tokens
            }))?)
            .await?;
        Ok(serde_json::from_str(
            &reply.content[0].as_text().unwrap().text,
        )?)
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

fn hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

#[tokio::test]
async fn source_changes_name_opening_and_current_evidence_without_conflating_edit_scope()
-> Result<()> {
    let f = Fixture::new()?;
    let opening = f.begin().await?;
    let stored: Value =
        serde_json::from_str(&f.state.registry.get_meta(PID, "edit_session_v1")?.unwrap())?;
    let observed = &stored["sessions"]["worker"]["guidance_observation"];
    ensure!(
        observed["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "docs/shared.md" && r["blake3"] == hash(LINK))
    );
    ensure!(
        !serde_json::to_string(observed)?.contains("cached previews"),
        "Opening metadata must not duplicate guidance bodies"
    );
    let revised_root = ROOT.replace("ownership", "company ownership");
    let revised_link = LINK.replace("cached previews", "authorized cached previews");
    f.write("AGENTS.md", &revised_root)?;
    f.write("docs/shared.md", &revised_link)?;
    std::fs::remove_file(f.root.join(".github/instructions/map.instructions.md"))?;
    f.write(".github/instructions/new.instructions.md", MAP)?;
    let done = f
        .complete(&opening, &[FILE, "Site/billing/Invoice.vb"], 65536)
        .await?;
    let drift = &done["summary"]["guidance_drift"];
    ensure!(
        drift["status"] == "source_changes_observed" && drift["change_count"] == 4,
        "{done}"
    );
    ensure!(drift["comparison_paths"] == json!([FILE]));
    let rows = drift["changes"].as_array().unwrap();
    for (path, before, after) in [
        ("AGENTS.md", ROOT, revised_root.as_str()),
        ("docs/shared.md", LINK, revised_link.as_str()),
    ] {
        let row = rows.iter().find(|r| r["path"] == path).unwrap();
        ensure!(
            row["change"] == "hash_changed"
                && row["before_blake3"] == hash(before)
                && row["current_blake3"] == hash(after)
        );
    }
    ensure!(
        rows.iter()
            .any(|r| r["path"] == ".github/instructions/map.instructions.md"
                && r["change"] == "no_longer_observed")
    );
    ensure!(
        rows.iter()
            .any(|r| r["path"] == ".github/instructions/new.instructions.md"
                && r["change"] == "newly_observed")
    );
    ensure!(
        !rows
            .iter()
            .any(|r| r["path"] == ".github/instructions/billing.instructions.md")
    );
    ensure!(done["report_markdown"].as_str().unwrap().contains(BILLING));
    ensure!(drift["instruction_compliance_verified"] == false);
    ensure!(std::fs::read_to_string(f.root.join("AGENTS.md"))? == revised_root);
    ensure!(std::fs::read_to_string(f.root.join("docs/shared.md"))? == revised_link);
    f.close()
}

#[tokio::test]
async fn scope_changes_unresolved_inputs_legacy_and_no_edits_do_not_claim_policy_freshness()
-> Result<()> {
    let f = Fixture::new()?;
    f.write(
        "AGENTS.md",
        &format!("{ROOT}Read [unavailable](docs/missing.md).\n"),
    )?;
    let opening = f.begin().await?;
    let done = f
        .complete(&opening, &["Site/billing/Invoice.vb"], 65536)
        .await?;
    let drift = &done["summary"]["guidance_drift"];
    ensure!(
        drift["status"] == "no_changes_in_observed_source_hashes" && drift["change_count"] == 0,
        "{done}"
    );
    ensure!(drift["current_observation"]["reference_coverage"] == "incomplete");
    ensure!(
        drift["current_observation"]["unfingerprinted_source_count"]
            .as_u64()
            .unwrap()
            > 0
    );
    let report = done["report_markdown"].as_str().unwrap();
    ensure!(report.contains(BILLING) && !report.contains(MAP));
    // Simulate a valid already-open lifecycle from an older binary.
    let opening = f.begin().await?;
    let mut stored: Value =
        serde_json::from_str(&f.state.registry.get_meta(PID, "edit_session_v1")?.unwrap())?;
    stored["sessions"]["worker"]
        .as_object_mut()
        .unwrap()
        .remove("guidance_observation");
    f.state
        .registry
        .set_meta(PID, "edit_session_v1", &stored.to_string())?;
    let done = f.complete(&opening, &[FILE], 65536).await?;
    ensure!(done["summary"]["guidance_drift"]["status"] == "unavailable_opening_observation");
    ensure!(done["summary"]["guidance_drift"]["comparison_performed"] == false);
    let opening = f.begin().await?;
    f.write("AGENTS.md", "Changed after opening; no edits declared.\n")?;
    let done = f.complete(&opening, &[], 65536).await?;
    ensure!(done["summary"]["guidance_drift"]["status"] == "not_requested_no_edits");
    ensure!(
        !done["report_markdown"]
            .as_str()
            .unwrap()
            .contains("## Repository policy sources")
    );
    f.close()
}

#[tokio::test]
async fn bounded_completion_recovers_original_drift_without_touching_reused_lifecycle() -> Result<()>
{
    let f = Fixture::new()?;
    let opening = f.begin().await?;
    // Make the unchanged policy exceed the normal inline policy-body limit.
    // The omitted complete-source fingerprint must still participate in drift.
    let large =
        "Preserve approval qualifications and do not rewrite authored policy.\n".repeat(400);
    f.write(
        ".github/instructions/map.instructions.md",
        &format!("---\napplyTo: 'Site/map/**'\n---\n{large}"),
    )?;
    let done = f.complete(&opening, &[FILE], 1024).await?;
    ensure!(done.to_string().len() <= 4096);
    ensure!(
        done["summary"]["guidance_drift"]["change_count"] == 1,
        "{done}"
    );
    ensure!(done["response_delivery"]["status"] == "DEFERRED_EDIT_WORKFLOW");
    let delivery = done["response_delivery"].clone();
    let reused = f.begin().await?;
    f.write(
        "AGENTS.md",
        "Current guide changed after the captured completion.\n",
    )?;
    let mut next = delivery["next_call"].clone();
    let mut bytes = String::new();
    for _ in 0..100 {
        let reply = f
            .app
            .handle_complete_edit_session(serde_json::from_value(next["arguments"].clone())?)
            .await?;
        let page: Value = serde_json::from_str(&reply.content[0].as_text().unwrap().text)?;
        bytes.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        next = page["next_call"].clone();
    }
    ensure!(hash(&bytes) == delivery["response_id"].as_str().unwrap());
    let original: Value = serde_json::from_str(&bytes)?;
    let row = &original["summary"]["guidance_drift"]["changes"][0];
    ensure!(
        row["path"] == ".github/instructions/map.instructions.md"
            && row["before_blake3"] == hash(MAP)
    );
    ensure!(original["summary"]["guidance_drift"]["comparison_paths"] == json!([FILE]));
    let stored: Value =
        serde_json::from_str(&f.state.registry.get_meta(PID, "edit_session_v1")?.unwrap())?;
    ensure!(stored["sessions"]["worker"]["revision"] == reused["summary"]["session_revision"]);
    let _ = f.complete(&reused, &[], 65536).await?;
    f.close()
}
