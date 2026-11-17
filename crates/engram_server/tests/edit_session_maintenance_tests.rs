//! A completed edit leads to grounded maintenance without assuming policy intent.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const PROJECT: &str = "edit-maintenance-handoff";
const FILE: &str = "Site/Preview.vb";
const BEFORE: &str = "Public Class Preview\nPublic Function Allowed() As Boolean\nReturn True\nEnd Function\nEnd Class\n";
const AFTER: &str = "Public Class Preview\nPublic Function Allowed() As Boolean\nReturn False\nEnd Function\nEnd Class\n";
const GUIDE: &str = "# Preview guidance\nReview [the source](Site/Preview.vb).\nException: cached previews are read-only; writes still require approval.\n";
struct Fixture {
    e: Engram,
    state: AppState,
    owned: tempfile::TempDir,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new(full: bool) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("em-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::write(root.join(FILE), BEFORE)?;
        std::fs::write(root.join("AGENTS.md"), GUIDE)?;
        // Even the missing-start case owns an unborn repository, so discovery
        // cannot accidentally inherit the surrounding checkout's HEAD.
        drop(git2::Repository::init(&root)?);
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: full,
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: PROJECT.into(),
            project_name: "Owned edit handoff".into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })?;
        state.registry.set_meta(PROJECT, "active_generation", "1")?;
        Ok(Self {
            e: Engram::new(state.clone()),
            state,
            owned,
            root,
        })
    }
    fn commit(&self) -> Result<String> {
        let repo = git2::Repository::open(&self.root)?;
        let mut index = repo.index()?;
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let author = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
        let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();
        Ok(repo
            .commit(
                Some("HEAD"),
                &author,
                &author,
                "Owned source evidence",
                &tree,
                &parents,
            )?
            .to_string())
    }
    async fn begin(&self, paths: &[String]) -> Result<String> {
        let result=self.e.handle_begin_edit_session(serde_json::from_value(json!({"project_id":PROJECT,"session_id":"worker","planned_files":paths,"output_json":true,"max_tokens":65536}))?).await?;
        let report: Value = serde_json::from_str(body(&result))?;
        Ok(report["summary"]["session_revision"]
            .as_str()
            .unwrap()
            .into())
    }
    async fn complete(&self, revision: &str, paths: &[String], tokens: usize) -> Result<Value> {
        let result=self.e.handle_complete_edit_session(serde_json::from_value(json!({"project_id":PROJECT,"session_id":"worker","session_revision":revision,"edited_files":paths,"output_json":true,"max_tokens":tokens}))?).await?;
        Ok(serde_json::from_str(body(&result))?)
    }
    fn close(self) -> Result<()> {
        let Self {
            e, state, owned, ..
        } = self;
        drop(e);
        drop(state);
        owned.close()?;
        Ok(())
    }
}
fn body(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

#[tokio::test]
async fn completed_edit_handoff_uses_recorded_start_commit_and_collects_real_before_after_evidence()
-> Result<()> {
    let fixture = Fixture::new(true)?;
    let base = fixture.commit()?;
    let paths = vec![FILE.to_owned()];
    let revision = fixture.begin(&paths).await?;
    std::fs::write(fixture.root.join(FILE), AFTER)?;
    ensure!(
        fixture.commit()? != base,
        "HEAD deliberately moves after session start"
    );
    let complete = fixture.complete(&revision, &paths, 65536).await?;
    let handoff = &complete["summary"]["instruction_maintenance"];
    ensure!(handoff["base_commit"] == base && handoff["maintenance_review_performed"] == false);
    ensure!(handoff["calls"].as_array().unwrap().len() == 1);
    let call = &handoff["calls"][0];
    ensure!(call["tool"] == "pre_push_audit");
    ensure!(call["arguments"]["instruction_maintenance"]["changed_paths"] == json!([FILE]));
    let result = fixture
        .e
        .handle_pre_push_audit(serde_json::from_value(call["arguments"].clone())?)
        .await?;
    let report: Value = serde_json::from_str(body(&result))?;
    ensure!(
        report["packet"]["base_commit"] == base && report["authored_files_written"] == 0,
        "{report}"
    );
    let code = report["packet"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == FILE)
        .unwrap();
    ensure!(
        code["before"]["content"] == BEFORE && code["after"]["content"] == AFTER,
        "{code}"
    );
    ensure!(std::fs::read_to_string(fixture.root.join("AGENTS.md"))? == GUIDE);
    ensure!(
        fixture.complete(&revision, &paths, 65536).await.is_err(),
        "Completion remains consumed"
    );
    fixture.close()
}

#[tokio::test]
async fn unavailable_profiles_start_commits_and_explicit_no_edits_do_not_emit_guessed_calls()
-> Result<()> {
    for (full, committed, empty, status) in [
        (false, true, false, "full_profile_required"),
        (true, false, false, "unavailable_start_commit"),
        (true, true, true, "not_requested_no_edits"),
    ] {
        let fixture = Fixture::new(full)?;
        if committed {
            fixture.commit()?;
        }
        let paths = vec![FILE.to_owned()];
        let revision = fixture.begin(&paths).await?;
        let result = fixture
            .complete(&revision, if empty { &[] } else { &paths }, 65536)
            .await?;
        let handoff = &result["summary"]["instruction_maintenance"];
        ensure!(
            handoff["status"] == status && handoff["calls"].as_array().unwrap().is_empty(),
            "{result}"
        );
        ensure!(handoff["automatic_instruction_writes"] == false);
        fixture.close()?;
    }
    Ok(())
}

#[tokio::test]
async fn bounded_completed_report_recovers_every_partitioned_call_without_reopening_the_session()
-> Result<()> {
    let fixture = Fixture::new(true)?;
    let guide = GUIDE.repeat(30);
    std::fs::write(fixture.root.join("AGENTS.md"), &guide)?;
    let paths: Vec<_> = (0..17)
        .map(|n| format!("Site/DeclaredSource{n}.vb"))
        .collect();
    for path in &paths {
        std::fs::write(fixture.root.join(path), BEFORE)?;
    }
    let base = fixture.commit()?;
    let revision = fixture.begin(&paths).await?;
    for path in &paths {
        std::fs::write(fixture.root.join(path), AFTER)?;
    }
    let deferred = fixture.complete(&revision, &paths, 1024).await?;
    ensure!(
        deferred["report_content"] == "DEFERRED_NOT_SUPPLIED",
        "{deferred}"
    );
    let summary = &deferred["summary"]["instruction_maintenance"];
    ensure!(
        summary["call_count"] == 2
            && summary["declared_edited_file_count"] == 17
            && summary.get("calls").is_none()
    );
    let id = deferred["response_delivery"]["response_id"]
        .as_str()
        .unwrap();
    let mut action = deferred["response_delivery"]["next_call"].clone();
    let mut original = String::new();
    for _ in 0..100 {
        let response = fixture
            .e
            .handle_complete_edit_session(serde_json::from_value(action["arguments"].clone())?)
            .await?;
        let page: Value = serde_json::from_str(body(&response))?;
        ensure!(page["start_byte"] == original.len());
        original.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        action = page["next_call"].clone();
    }
    ensure!(blake3::hash(original.as_bytes()).to_hex().as_str() == id);
    let report: Value = serde_json::from_str(&original)?;
    let calls = report["summary"]["instruction_maintenance"]["calls"]
        .as_array()
        .unwrap();
    let recovered: Vec<_> = calls
        .iter()
        .flat_map(|call| {
            call["arguments"]["instruction_maintenance"]["changed_paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|path| path.as_str().unwrap().to_owned())
        })
        .collect();
    ensure!(
        recovered == paths
            && calls
                .iter()
                .all(|call| call["arguments"]["instruction_maintenance"]["base_commit"] == base)
    );
    ensure!(std::fs::read_to_string(fixture.root.join("AGENTS.md"))? == guide);
    ensure!(fixture.complete(&revision, &paths, 65536).await.is_err());
    fixture.close()
}
