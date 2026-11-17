//! Inline guide spellings yield qualified evidence, never an adopted path binding.
use anyhow::{Result, ensure};
use engram_server::services::project_policy::{self, PolicyBudget};
use std::path::Path;

const GUIDE_PATH: &str = ".github/copilot-instructions.md";
const GUIDE: &str =
    "For session changes read `docs/security.md`.\nException: previews never revoke sessions.\n";
const ROOT_PATH: &str = "docs/security.md";
const LOCAL_PATH: &str = ".github/docs/security.md";
const ROOT_BODY: &str = "---\napplyTo: Site/**\n---\nROOT: preserve per-action authorization.\nException: previews never revoke sessions.\n";
const LOCAL_BODY: &str = "DOCUMENT-RELATIVE: retain the legacy opt-in exception.\n";

fn write(root: &Path, path: &str, body: &str) -> Result<()> {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, body)?;
    Ok(())
}

fn fixture(root: &Path) -> Result<()> {
    write(root, GUIDE_PATH, GUIDE)?;
    write(root, ROOT_PATH, ROOT_BODY)?;
    write(root, LOCAL_PATH, LOCAL_BODY)
}

fn targets() -> Vec<String> {
    vec!["Site/session.ts".into()]
}

fn owned_fixture() -> Result<tempfile::TempDir> {
    let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(tempfile::Builder::new()
        .prefix("inline-")
        .tempdir_in(parent)?)
}

#[test]
fn ordinary_inline_guidance_retains_both_path_candidates_with_exact_source_and_recovery()
-> Result<()> {
    let owned = owned_fixture()?;
    fixture(owned.path())?;
    let context =
        project_policy::resolve(owned.path(), &targets(), &[]).with_source_recovery("owned-inline");
    ensure!(context.sources.len() == 1 && context.sources[0].content == GUIDE);
    ensure!(context.linked_sources.len() == 2 && context.reference_coverage == "incomplete");
    ensure!(context.references.iter().any(|r| r.path.is_none()
        && r.target == "docs/security.md"
        && r.status == "inline_path_basis_unverified"));
    for (path, body, basis) in [
        (ROOT_PATH, ROOT_BODY, "project_root_candidate"),
        (LOCAL_PATH, LOCAL_BODY, "document_relative_candidate"),
    ] {
        let source = context
            .linked_sources
            .iter()
            .find(|s| s.path == path)
            .unwrap();
        let reference = context
            .references
            .iter()
            .find(|r| r.path.as_deref() == Some(path))
            .unwrap();
        let hash = blake3::hash(body.as_bytes()).to_hex().to_string();
        ensure!(source.content == body && !source.truncated && source.blake3 == hash);
        ensure!(source.applicability[0].contains("No authority"));
        ensure!(reference.path_basis == Some(basis));
        ensure!(reference.source_blake3 == blake3::hash(GUIDE.as_bytes()).to_hex().to_string());
        ensure!(reference.target_blake3.as_deref() == Some(hash.as_str()));
        let action = reference.read_action.as_ref().unwrap();
        ensure!(
            action.arguments.project_id == "owned-inline" && action.arguments.file_path == path
        );
        ensure!(action.arguments.expected_file_blake3 == hash);
        ensure!(std::fs::read_to_string(owned.path().join(path))? == body);
    }
    let markdown = context.render_markdown();
    ensure!(
        markdown.contains(ROOT_BODY)
            && markdown.contains(LOCAL_BODY)
            && markdown.contains("project_root_candidate")
            && markdown.contains("document_relative_candidate")
    );
    ensure!(std::fs::read_to_string(owned.path().join(GUIDE_PATH))? == GUIDE);
    Ok(())
}

#[test]
fn inline_guidance_keeps_direct_policy_priority_scope_refusal_and_shared_reference_bounds()
-> Result<()> {
    let owned = owned_fixture()?;
    fixture(owned.path())?;
    let denied = "---\napplyTo: Billing/**\n---\nOTHER_DOMAIN_MUST_NOT_BE_SUPPLIED\n";
    write(owned.path(), LOCAL_PATH, denied)?;
    let context = project_policy::resolve_with_budget(
        owned.path(),
        &targets(),
        &[],
        PolicyBudget {
            max_sources: 8,
            max_characters_per_source: 10000,
            max_characters_total: GUIDE.chars().count() + ROOT_BODY.chars().count() - 1,
        },
    )
    .with_source_recovery("owned-inline");
    ensure!(context.sources[0].content == GUIDE && context.linked_sources.is_empty());
    let omitted = context
        .references
        .iter()
        .find(|r| r.path.as_deref() == Some(ROOT_PATH))
        .unwrap();
    ensure!(omitted.status == "linked_document_character_budget");
    ensure!(
        omitted
            .read_action
            .as_ref()
            .unwrap()
            .arguments
            .expected_file_blake3
            == blake3::hash(ROOT_BODY.as_bytes()).to_hex().to_string()
    );
    ensure!(
        context
            .references
            .iter()
            .any(|r| r.path.as_deref() == Some(LOCAL_PATH)
                && r.status == "linked_document_outside_declared_scope")
    );
    ensure!(!serde_json::to_string(&context)?.contains("OTHER_DOMAIN_MUST_NOT_BE_SUPPLIED"));
    let with_explicit = format!("{GUIDE}Also read [named guidance](../docs/explicit.md).\n");
    write(owned.path(), GUIDE_PATH, &with_explicit)?;
    write(
        owned.path(),
        "docs/explicit.md",
        "EXPLICIT: retain manual approval.\n",
    )?;
    let priority = project_policy::resolve_with_budget(
        owned.path(),
        &targets(),
        &[],
        PolicyBudget {
            max_sources: 2,
            max_characters_per_source: 10000,
            max_characters_total: 24000,
        },
    );
    ensure!(priority.sources.len() == 1 && priority.linked_sources.len() == 1);
    ensure!(priority.linked_sources[0].path == "docs/explicit.md");
    ensure!(priority.references.iter().any(
        |r| r.path.as_deref() == Some(ROOT_PATH) && r.status == "linked_document_selection_budget"
    ));
    let many = (0..20)
        .map(|i| format!("Read `docs/guide{i}.md`.\n"))
        .collect::<String>();
    write(owned.path(), GUIDE_PATH, &many)?;
    let bounded = project_policy::resolve(owned.path(), &targets(), &[]);
    ensure!(bounded.references.len() == 32 && bounded.references_omitted == 28);
    ensure!(bounded.reference_coverage == "incomplete" && bounded.sources[0].content == many);
    ensure!(
        bounded
            .references
            .iter()
            .any(|r| r.status == "linked_document_lookup_limit")
    );
    Ok(())
}

#[test]
fn historical_inline_candidates_use_only_the_requested_tree_and_offer_no_disk_recovery()
-> Result<()> {
    let owned = owned_fixture()?;
    fixture(owned.path())?;
    let base = {
        let repo = git2::Repository::init(owned.path())?;
        let mut index = repo.index()?;
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;
        let tree = repo.find_tree(index.write_tree()?)?;
        let author = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
        repo.commit(
            Some("HEAD"),
            &author,
            &author,
            "Owned inline guidance",
            &tree,
            &[],
        )?
    };
    write(owned.path(), ROOT_PATH, "LIVE_ONLY_REPLACEMENT\n")?;
    std::fs::remove_file(owned.path().join(LOCAL_PATH))?;
    let context = project_policy::resolve_at_revision(owned.path(), base, &targets(), &[])?
        .with_source_recovery("owned-inline");
    ensure!(context.source_revision.as_deref() == Some(base.to_string().as_str()));
    ensure!(context.linked_sources.len() == 2);
    for (path, body) in [(ROOT_PATH, ROOT_BODY), (LOCAL_PATH, LOCAL_BODY)] {
        let source = context
            .linked_sources
            .iter()
            .find(|s| s.path == path)
            .unwrap();
        ensure!(
            source.content == body
                && source.blake3 == blake3::hash(body.as_bytes()).to_hex().to_string()
        );
    }
    ensure!(context.references.iter().all(|r| r.read_action.is_none()));
    ensure!(!serde_json::to_string(&context)?.contains("LIVE_ONLY_REPLACEMENT"));
    ensure!(!owned.path().join(LOCAL_PATH).exists());
    Ok(())
}
