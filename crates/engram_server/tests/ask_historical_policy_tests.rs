//! Historical answers must carry their own authored guidance, never today's.
use anyhow::{Result, ensure};
use engram_server::{
    AskCodebaseRequest,
    services::ask_engine::{historical, planner, report::AskReport},
};
use serde_json::json;
use std::path::{Path, PathBuf};

const SOURCE: &str = "Public Module Billing\nPublic Function Minimum() As Integer\nReturn 4\nEnd Function\nEnd Module\n";
const TOPIC: &str =
    "---\napplyTo: 'Site/Billing.vb'\n---\nPreserve imported-entry rate exceptions.\n";
struct Fixture {
    repo: git2::Repository,
    root: PathBuf,
    _owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let owned = tempfile::Builder::new()
            .prefix("h")
            .rand_bytes(2)
            .tempdir()?;
        let root = owned.path().join("r");
        std::fs::create_dir(&root)?;
        let repo = git2::Repository::init(&root)?;
        repo.set_head("refs/heads/main")?;
        Ok(Self {
            repo,
            root,
            _owned: owned,
        })
    }
    fn commit(&self, files: &[(&str, &str)]) -> Result<git2::Oid> {
        let mut index = self.repo.index()?;
        for (path, text) in files {
            let target = self.root.join(path);
            std::fs::create_dir_all(target.parent().unwrap())?;
            std::fs::write(target, text)?;
            index.add_path(Path::new(path))?;
        }
        index.write()?;
        let tree = self.repo.find_tree(index.write_tree()?)?;
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let signature = git2::Signature::now("Owned fixture", "fixture@example.test")?;
        Ok(self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Authored source",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )?)
    }
    fn ask(&self, root: &Path, question: &str, pin: git2::Oid) -> Result<AskReport> {
        let request: AskCodebaseRequest = serde_json::from_value(
            json!({"project_id":"owned","question":question,"as_of":{"commit":pin.to_string()}}),
        )?;
        historical::gather(root, &request, planner::plan_query(question))
    }
}

#[test]
fn pinned_answer_retains_scoped_policy_exceptions_hashes_and_linked_sources() -> Result<()> {
    let fixture = Fixture::new()?;
    let shared = "Read [approved exception details](docs/exception.md). Preserve regional billing exceptions.\n";
    let linked = "Imported inactive entries retain their archived rates.\n";
    let old = fixture.commit(&[
        ("Site/Billing.vb", SOURCE),
        ("AGENTS.md", shared),
        (".github/instructions/billing.instructions.md", TOPIC),
        ("docs/exception.md", linked),
        (
            ".github/instructions/script.instructions.md",
            "---\napplyTo: 'Scripts/**'\n---\nUNRELATED_SCRIPT_RULE\n",
        ),
    ])?;
    fixture.commit(&[
        ("Site/Billing.vb", &SOURCE.replace("Return 4", "Return 19")),
        ("AGENTS.md", "FUTURE_SHARED_RULE\n"),
        (
            ".github/instructions/billing.instructions.md",
            "FUTURE_TOPIC_RULE\n",
        ),
        ("docs/exception.md", "FUTURE_EXCEPTION_RULE\n"),
    ])?;
    let report = fixture.ask(&fixture.root, "Explain Site/Billing.vb", old)?;
    ensure!(report.evidence.len() == 1 && report.evidence[0].content == SOURCE);
    let policy = report.project_policy_sources.as_ref().unwrap();
    ensure!(policy.source_revision.as_deref() == Some(old.to_string().as_str()));
    ensure!(
        policy.sources.len() == 2
            && policy
                .sources
                .iter()
                .any(|source| source.path == "AGENTS.md" && source.content == shared)
    );
    ensure!(policy.sources.iter().any(|source| source.path
        == ".github/instructions/billing.instructions.md"
        && source.content == TOPIC));
    ensure!(policy.sources.iter().all(
        |source| source.blake3 == blake3::hash(source.content.as_bytes()).to_hex().to_string()
    ));
    ensure!(
        policy
            .linked_sources
            .iter()
            .any(|source| source.path == "docs/exception.md" && source.content == linked)
    );
    let rendered = policy.render_markdown();
    ensure!(
        rendered.contains(&old.to_string()) && rendered.contains("imported-entry rate exceptions")
    );
    ensure!(!rendered.contains("FUTURE_") && !rendered.contains("UNRELATED_SCRIPT_RULE"));
    ensure!(report.status == engram_server::services::ask_engine::status::AnswerStatus::Partial);
    Ok(())
}

#[test]
fn historical_subproject_guidance_uses_project_relative_scope_at_the_same_pin() -> Result<()> {
    let fixture = Fixture::new()?;
    let old = fixture.commit(&[
        ("App/Pages/Billing.vb", SOURCE),
        ("AGENTS.md", "REPO_PARENT_POLICY\n"),
        ("App/AGENTS.md", "App-only historical exception.\n"),
        (
            "App/.github/instructions/page.instructions.md",
            "---\napplyTo: 'Pages/Billing.vb'\n---\nKeep the page-specific historical rule.\n",
        ),
    ])?;
    fixture.commit(&[("App/AGENTS.md", "FUTURE_APP_POLICY\n")])?;
    let report = fixture.ask(&fixture.root.join("App"), "Explain Pages/Billing.vb", old)?;
    let policy = report.project_policy_sources.as_ref().unwrap();
    ensure!(report.snapshot.git_commit == policy.source_revision);
    ensure!(policy.sources.len() == 2);
    ensure!(
        policy
            .sources
            .iter()
            .any(|source| source.path == "AGENTS.md"
                && source.content == "App-only historical exception.\n")
    );
    ensure!(
        policy
            .sources
            .iter()
            .any(|source| source.path == ".github/instructions/page.instructions.md")
    );
    ensure!(
        !policy.render_markdown().contains("REPO_PARENT_POLICY")
            && !policy.render_markdown().contains("FUTURE_APP_POLICY")
    );
    Ok(())
}

#[test]
fn malformed_old_scope_is_disclosed_and_no_source_does_not_adopt_global_guidance() -> Result<()> {
    let fixture = Fixture::new()?;
    let old = fixture.commit(&[
        ("Site/Billing.vb", SOURCE),
        ("AGENTS.md", "Historical shared guidance.\n"),
        (
            ".github/instructions/billing.instructions.md",
            "---\napplyTo: '['\n---\nMALFORMED_SCOPE_MUST_NOT_BECOME_GLOBAL\n",
        ),
    ])?;
    fixture.commit(&[(".github/instructions/billing.instructions.md", TOPIC)])?;
    let report = fixture.ask(&fixture.root, "Explain Site/Billing.vb", old)?;
    let policy = report.project_policy_sources.as_ref().unwrap();
    ensure!(policy.coverage != "complete" && !policy.omissions.is_empty());
    ensure!(
        !policy
            .sources
            .iter()
            .any(|source| source.content.contains("MALFORMED_SCOPE"))
    );
    ensure!(
        report.evidence[0].content == SOURCE
            && report
                .unknowns
                .iter()
                .any(|gap| gap.contains("guidance is incomplete"))
    );
    let empty = fixture.ask(&fixture.root, "Explain Absent/Undiscovered.vb", old)?;
    ensure!(empty.evidence.is_empty());
    let withheld = empty.project_policy_sources.as_ref().unwrap();
    ensure!(withheld.coverage == "not_evaluated" && withheld.sources.is_empty());
    ensure!(withheld.source_revision.as_deref() == Some(old.to_string().as_str()));
    Ok(())
}
