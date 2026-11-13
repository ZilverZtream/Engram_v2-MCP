//! Authored guidance must accompany edit-session intent and actual edit scope.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{Engram, state::AppState};
use serde_json::json;
use std::path::PathBuf;

const PID: &str = "edit-policy-fixture";
const ROOT: &str = "# Project guidance\nPreserve company ownership checks.\n";
const MAP: &str = "---\napplyTo: 'Site/map/**'\n---\n# Map policy\nKeep the original-project restriction in the UI only.\nException: authorized saves from another accessible view remain allowed.\nGenerated report HTML and Site/Q/ are excluded.\n";
const BILLING: &str = "---\napplyTo: 'Site/billing/**'\n---\n# Billing policy\nPreserve the entered amount.\n";

// Stores and their runtime cache drop before the owned directory on unwinding.
struct Fixture {
    engram: Engram,
    state: AppState,
    root: PathBuf,
    temporary: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("project");
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"), allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(), llm_backend: "none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id:PID.into(),project_name:"Edit guidance fixture".into(),
            directory:root.to_string_lossy().into(),project_type:"general".into(),
            created_at_ms:1,updated_at_ms:1,reindex_required_since_ms:None,
        })?;
        state.registry.set_meta(PID,"active_generation","1")?;
        std::fs::write(root.join("AGENTS.md"),ROOT)?;
        std::fs::write(root.join(".github/instructions/map.instructions.md"),MAP)?;
        std::fs::write(root.join(".github/instructions/billing.instructions.md"),BILLING)?;
        Ok(Self {engram:Engram::new(state.clone()),state,root,temporary})
    }

    async fn begin(&self) -> Result<(String,String)> {
        let result=self.engram.handle_begin_edit_session(serde_json::from_value(json!({
            "project_id":PID,"session_id":"policy","planned_files":["Site/map/New.vb"]
        }))?).await?;
        let text=result.content[0].as_text().unwrap().text.clone();
        let revision=text.split("session_revision: ").nth(1).unwrap().lines().next().unwrap().to_owned();
        Ok((text,revision))
    }

    async fn complete(&self, revision:&str, edited:&[&str]) -> Result<String> {
        let result=self.engram.handle_complete_edit_session(serde_json::from_value(json!({
            "project_id":PID,"session_id":"policy","session_revision":revision,"edited_files":edited
        }))?).await?;
        Ok(result.content[0].as_text().unwrap().text.clone())
    }

    fn edit(&self,path:&str) -> Result<()> {
        let path=self.root.join(path);std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path,"Public Class Changed\nEnd Class\n")?;Ok(())
    }

    fn close(self) -> Result<()> {
        let Self {engram,state,temporary,..}=self;drop(engram);drop(state);temporary.close()?;Ok(())
    }
}

#[tokio::test]
async fn new_planned_files_receive_full_scoped_policy_and_no_edits_skip_applicability() -> Result<()> {
    let fixture=Fixture::new()?;
    ensure!(!fixture.root.join("Site/map/New.vb").exists());
    let (opened,revision)=fixture.begin().await?;
    ensure!(opened.contains(ROOT) && opened.contains(MAP) && !opened.contains(BILLING),"{opened}");
    ensure!(opened.contains(blake3::hash(MAP.as_bytes()).to_hex().as_str()));
    ensure!(opened.contains("does not semantically enforce") && opened.contains("paths declared in this call"));
    let closed=fixture.complete(&revision,&[]).await?;
    ensure!(closed.contains("No edited files declared") && !closed.contains("## Repository policy sources"),"{closed}");
    let stored:serde_json::Value=serde_json::from_str(&fixture.state.registry.get_meta(PID,"edit_session_v1")?.unwrap())?;
    ensure!(stored["sessions"].as_object().unwrap().is_empty());
    ensure!(std::fs::read_to_string(fixture.root.join("AGENTS.md"))?==ROOT);
    ensure!(std::fs::read_to_string(fixture.root.join(".github/instructions/map.instructions.md"))?==MAP);
    fixture.close()
}

#[tokio::test]
async fn completion_refreshes_changed_guidance_and_includes_unplanned_edited_scope() -> Result<()> {
    let fixture=Fixture::new()?;
    let (_,revision)=fixture.begin().await?;
    let revised=MAP.replace("accessible view","accessible project view");
    std::fs::write(fixture.root.join(".github/instructions/map.instructions.md"),&revised)?;
    fixture.edit("Site/map/New.vb")?;fixture.edit("Site/billing/Invoice.vb")?;
    let closed=fixture.complete(&revision,&["Site/map/New.vb","Site/billing/Invoice.vb"]).await?;
    ensure!(closed.contains(ROOT) && closed.contains(&revised) && closed.contains(BILLING),"{closed}");
    ensure!(!closed.contains(MAP) && !closed.contains(blake3::hash(MAP.as_bytes()).to_hex().as_str()));
    ensure!(closed.contains(blake3::hash(revised.as_bytes()).to_hex().as_str()));
    fixture.close()
}

#[tokio::test]
async fn oversized_policy_is_recoverable_and_completion_uses_actual_instead_of_planned_scope() -> Result<()> {
    let fixture=Fixture::new()?;
    let large=format!("---\napplyTo: 'Site/map/**'\n---\n{}END_OF_COMPLETE_GUIDE","Read the complete qualifications.\n".repeat(500));
    std::fs::write(fixture.root.join(".github/instructions/large.instructions.md"),&large)?;
    let (opened,revision)=fixture.begin().await?;
    ensure!(opened.contains(MAP) && !opened.contains("END_OF_COMPLETE_GUIDE"));
    ensure!(opened.contains("Policy source omitted/unresolved: `.github/instructions/large.instructions.md`")
        && opened.contains(blake3::hash(large.as_bytes()).to_hex().as_str())
        && opened.contains("Read the complete project-relative file"),"{opened}");
    fixture.edit("Site/billing/Invoice.vb")?;
    let closed=fixture.complete(&revision,&["Site/billing/Invoice.vb"]).await?;
    ensure!(closed.contains(ROOT) && closed.contains(BILLING) && !closed.contains(MAP),"{closed}");
    ensure!(!closed.contains(blake3::hash(large.as_bytes()).to_hex().as_str()));
    fixture.close()
}
