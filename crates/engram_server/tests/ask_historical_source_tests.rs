//! A revision-qualified question must use that revision's source, even while
//! the active index and working tree describe a different implementation.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{
    AskCodebaseRequest, Engram,
    services::ask_engine::{evidence::Authority, historical, planner, report::AskReport},
    state::AppState,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const OLD_VB: &str = "Public Module Zoom\n Public Function MinimumZoom() As Integer\n  Return 6\n End Function\nEnd Module\n";
const NEW_VB: &str = "Public Module Zoom\n Public Function MinimumZoom() As Integer\n  Return 17\n End Function\nEnd Module\n";
const OLD_TS: &str = "export function minimumZoom(): number { return 6; }\n";
const OLD_SQL: &str = "CREATE PROCEDURE dbo.MinimumZoom AS SELECT 6 AS MinimumZoom;\n";

struct Fixture {
    // Close libgit2 handles before removing this fixture's directory.
    repo: git2::Repository,
    root: PathBuf,
    _owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("repo");
        std::fs::create_dir(&root)?;
        let repo = git2::Repository::init(&root)?;
        repo.set_head("refs/heads/main")?;
        Ok(Self {
            repo,
            root,
            _owned: owned,
        })
    }
    fn commit(&self, files: &[(&str, &str)]) -> Result<String> {
        let mut index = self.repo.index()?;
        for (path, content) in files {
            std::fs::create_dir_all(self.root.join(path).parent().unwrap())?;
            std::fs::write(self.root.join(path), content)?;
            index.add_path(Path::new(path))?;
        }
        index.write()?;
        let tree = self.repo.find_tree(index.write_tree()?)?;
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let signature = git2::Signature::now("Fixture", "fixture@example.test")?;
        Ok(self
            .repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Source revision",
                &tree,
                &parent.iter().collect::<Vec<_>>(),
            )?
            .to_string())
    }
    fn ask(&self, question: &str, pin: Value) -> Result<AskReport> {
        let req: AskCodebaseRequest =
            serde_json::from_value(json!({"project_id":"owned","question":question,"as_of":pin}))?;
        historical::gather(&self.root, &req, planner::plan_query(question))
    }
}

#[test]
fn requested_revision_supplies_the_original_vb_typescript_and_sql_not_current_bytes() -> Result<()>
{
    let fixture = Fixture::new()?;
    let old = fixture.commit(&[
        ("Site/Zoom.vb", OLD_VB),
        ("Scripts/zoom.ts", OLD_TS),
        ("db/zoom.sql", OLD_SQL),
    ])?;
    fixture.commit(&[
        ("Site/Zoom.vb", NEW_VB),
        (
            "Scripts/zoom.ts",
            "export function minimumZoom() { return 17; }\n",
        ),
        (
            "db/zoom.sql",
            "CREATE PROCEDURE dbo.MinimumZoom AS SELECT 17;\n",
        ),
    ])?;
    let report = fixture.ask(
        "Explain Site/Zoom.vb and Scripts/zoom.ts and db/zoom.sql",
        json!({"commit":old}),
    )?;
    ensure!(report.evidence.len() == 3);
    for (path, source) in [
        ("Site/Zoom.vb", OLD_VB),
        ("Scripts/zoom.ts", OLD_TS),
        ("db/zoom.sql", OLD_SQL),
    ] {
        let item = report
            .evidence
            .iter()
            .find(|item| item.path.as_deref() == Some(path))
            .unwrap();
        ensure!(item.content == source && item.commit.as_deref() == Some(old.as_str()));
        ensure!(item.authority == Authority::HistoricalCode && item.generation.is_none());
        ensure!(item.warnings.iter().any(|warning| {
            warning.contains(&blake3::hash(source.as_bytes()).to_hex().to_string())
        }));
    }
    ensure!(report.snapshot.git_commit.as_deref() == Some(old.as_str()));
    ensure!(report.snapshot.project_generation.is_none());
    ensure!(
        report
            .unknowns
            .iter()
            .any(|gap| gap.contains("No historical call graph"))
    );
    Ok(())
}

#[test]
fn branch_resolves_once_and_subproject_scope_excludes_parent_repository_files() -> Result<()> {
    let fixture = Fixture::new()?;
    let old = fixture.commit(&[
        ("Site/Zoom.vb", OLD_VB),
        ("ParentOnly.vb", "Public Module ParentOnly\nEnd Module\n"),
    ])?;
    fixture.repo.branch(
        "previous",
        &fixture.repo.find_commit(git2::Oid::from_str(&old)?)?,
        false,
    )?;
    let current = fixture.commit(&[("Site/Zoom.vb", NEW_VB)])?;
    let req: AskCodebaseRequest = serde_json::from_value(
        json!({"project_id":"owned","question":"Explain Zoom.vb","as_of":{"branch":"previous"}}),
    )?;
    let report = historical::gather(
        &fixture.root.join("Site"),
        &req,
        planner::plan_query(&req.question),
    )?;
    fixture.repo.branch(
        "previous",
        &fixture.repo.find_commit(git2::Oid::from_str(&current)?)?,
        true,
    )?;
    ensure!(report.evidence.len() == 1 && report.evidence[0].path.as_deref() == Some("Zoom.vb"));
    ensure!(
        report.evidence[0].content == OLD_VB
            && report.snapshot.git_commit.as_deref() == Some(old.as_str())
    );
    Ok(())
}

#[test]
fn missing_or_conflicting_revisions_never_fall_back_to_the_working_tree() -> Result<()> {
    let fixture = Fixture::new()?;
    let old = fixture.commit(&[("Site/Zoom.vb", OLD_VB)])?;
    fixture.commit(&[("Site/Zoom.vb", NEW_VB)])?;
    let missing = fixture
        .ask(
            "Explain Site/Zoom.vb",
            json!({"commit":"unavailable-revision"}),
        )
        .unwrap_err();
    ensure!(format!("{missing:#}").contains("current source was not substituted"));
    let mismatch = fixture
        .ask(
            "Explain Site/Zoom.vb",
            json!({"commit":old,"branch":"main"}),
        )
        .unwrap_err();
    ensure!(mismatch.to_string().contains("different revisions"));
    ensure!(fixture.ask("Explain Site/Zoom.vb", json!({})).is_err());
    Ok(())
}

#[test]
fn explicitly_named_file_survives_unrelated_historical_scan_noise() -> Result<()> {
    let fixture = Fixture::new()?;
    let files: Vec<_> = (0..180)
        .map(|i| (format!("noise/{i:03}.txt"), "unrelated source\n".to_owned()))
        .collect();
    let mut inputs: Vec<_> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    inputs.push(("zzz/Zoom.vb", OLD_VB));
    let old = fixture.commit(&inputs)?;
    let report = fixture.ask("What did zzz/Zoom.vb return?", json!({"commit":old}))?;
    ensure!(report.evidence.len() == 1 && report.evidence[0].content == OLD_VB);
    ensure!(report.providers[0].examined == 1);
    Ok(())
}

#[test]
fn quoted_historical_path_keeps_spaces_and_selects_the_exact_blob() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = "Site/Report Views/Zoom.vb";
    let revision = fixture.commit(&[
        (path, OLD_VB),
        ("Zoom.vb", NEW_VB),
        ("AGENTS.md", "Preserve the report minimum in the scoped preview.\n"),
    ])?;
    let report = fixture.ask(
        "Explain `Site/Report Views/Zoom.vb` and the message 'not found'.",
        json!({"commit":revision}),
    )?;
    ensure!(report.evidence.len() == 1);
    ensure!(report.evidence[0].path.as_deref() == Some(path));
    ensure!(report.evidence[0].content == OLD_VB);
    ensure!(report.providers[0].examined == 1);
    let plan = report.plan.entities;
    ensure!(plan.iter().any(|entity|
        entity.text == "not found"
            && entity.guessed_kind == engram_server::services::ask_engine::plan::EntityKind::Unknown));
    Ok(())
}

#[test]
fn oversized_historical_source_is_disclosed_without_returning_current_source() -> Result<()> {
    let fixture = Fixture::new()?;
    let oversized = "' original historical source\n".repeat(6000);
    let old = fixture.commit(&[("Site/Zoom.vb", &oversized)])?;
    std::fs::write(fixture.root.join("Site/Zoom.vb"), NEW_VB)?;
    let report = fixture.ask("Explain Site/Zoom.vb", json!({"commit":old}))?;
    ensure!(report.evidence.is_empty() && report.providers[0].truncated);
    ensure!(
        report
            .unknowns
            .iter()
            .any(|gap| gap.contains("Site/Zoom.vb") && gap.contains("omitted"))
    );
    Ok(())
}

#[test]
fn public_ask_entrypoint_keeps_requested_source_separate_from_an_active_newer_index() -> Result<()>
{
    let fixture = Fixture::new()?;
    let old = fixture.commit(&[("Site/Zoom.vb", OLD_VB)])?;
    fixture.commit(&[("Site/Zoom.vb", NEW_VB)])?;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let (state,_) = AppState::new(Config {
            data_dir:fixture._owned.path().join("data"),allowed_roots:vec![fixture.root.clone()],
            embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()
        })?;
        let registry = state.registry.clone();
        let engram = Engram::new(state);
        let indexed = engram.handle_index_project(serde_json::from_value(json!({
            "directory":fixture.root,"project_name":"Historical question","project_type":"general","wait":true
        }))?).await?;
        let index_text = &indexed.content[0].as_text().unwrap().text;
        let pid = index_text.split("project_id: ").nth(1).unwrap().lines().next().unwrap().trim();
        let before = registry.get_meta(pid,"last_git_oid")?;
        let result = engram.handle_ask_codebase(serde_json::from_value(json!({
            "project_id":pid,"question":"What does MinimumZoom return in Site/Zoom.vb?","as_of":{"commit":old},"output_format":"json"
        }))?).await?;
        let body: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text)?;
        ensure!(body["snapshot"]["git_commit"]==old);
        ensure!(body["evidence"][0]["content"]==OLD_VB);
        ensure!(body["evidence"][0]["authority"]=="historical_code");
        ensure!(body["snapshot"]["project_generation"].is_null());
        let history = engram.handle_ask_codebase(serde_json::from_value(json!({
            "project_id":pid,"question":"What changed recently in Site/Zoom.vb?",
            "as_of":{"commit":old},"output_format":"json"
        }))?).await?;
        let history: Value = serde_json::from_str(&history.content[0].as_text().unwrap().text)?;
        let evidence = history["evidence"].as_array().unwrap();
        ensure!(evidence.len() == 1 && evidence[0]["kind"] == "history_commit");
        ensure!(evidence[0]["commit"] == old);
        ensure!(history["snapshot"]["git_commit"] == old);
        let action = history["next_best"][0].as_str().unwrap();
        let args: Value = serde_json::from_str(action.split_once("search_history(").unwrap().1.strip_suffix(')').unwrap())?;
        ensure!(args["as_of_rev"] == old && args["file_filter"] == "Site/Zoom.vb");
        ensure!(registry.get_meta(pid,"last_git_oid")?==before);
        Ok(())
    })
}
