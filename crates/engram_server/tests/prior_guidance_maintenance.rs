//! A policy scope edit/removal must not erase the prior policy from maintenance.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const GUIDE: &str = ".github/instructions/saves.instructions.md";
const SOURCE: &str = "Site/save.vb";
const BEFORE: &str = "---\napplyTo: Site/**/*.vb\n---\nRequire reviewed writes.\nException: read-only probes do not write.\nSee [shared](shared.md).\n";
const SHARED: &str = "Preserve authorization and accepted exceptions.\n";

struct Fixture {
    root: PathBuf,
    base: String,
    // Repository/index/tree handles remain local and close before this owner.
    _temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temp = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(2)
            .tempdir_in(parent)?;
        let root = temp.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(root.join(GUIDE), BEFORE)?;
        std::fs::write(root.join(".github/instructions/shared.md"), SHARED)?;
        std::fs::write(
            root.join(SOURCE),
            "Public Class Save\nPublic Sub Apply()\nEnd Sub\nEnd Class\n",
        )?;
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Prior policy",
                &tree,
                &[],
            )?
            .to_string()
        };
        std::fs::write(
            root.join(SOURCE),
            "Public Class Save\nPublic Sub Apply()\nDim changed = 1\nEnd Sub\nEnd Class\n",
        )?;
        Ok(Self {
            root,
            base,
            _temp: temp,
        })
    }

    fn request(&self, path: &str) -> Result<MaintenanceRequest> {
        Ok(serde_json::from_value(
            json!({"base_commit":self.base,"changed_paths":[path]}),
        )?)
    }

    fn review(&self, path: &str) -> Result<maintenance::Report> {
        maintenance::review(&self.root, "prior-guidance", &self.request(path)?)
    }
}

fn prior<'a>(report: &'a maintenance::Report) -> &'a Value {
    &report.packet.change_context.as_ref().unwrap()["prior_guidance"]
}

#[test]
fn changed_scope_keeps_complete_prior_and_current_sources_without_adopting_prior_authority()
-> Result<()> {
    let fixture = Fixture::new()?;
    let after = BEFORE.replace("Site/**/*.vb", "Elsewhere/**/*.vb");
    std::fs::write(fixture.root.join(GUIDE), &after)?;
    let report = fixture.review(SOURCE)?;
    let guide = report
        .packet
        .instructions
        .iter()
        .find(|row| row.path == GUIDE)
        .expect("A scope edit must not hide a formerly applicable instruction");
    ensure!(guide.before.as_ref().unwrap().content == BEFORE && guide.current.content == after);
    ensure!(prior(&report)["source_revision"] == fixture.base);
    let previous = prior(&report)["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == GUIDE)
        .unwrap();
    ensure!(
        previous["content"] == BEFORE
            && previous["blake3"] == blake3::hash(BEFORE.as_bytes()).to_hex().as_str()
    );
    let context = report.packet.change_context.as_ref().unwrap();
    ensure!(
        context["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == GUIDE
                && row["selection_basis"] == "guidance_applicable_at_base_commit")
    );
    ensure!(
        context["prior_guidance_interpretation"]
            .as_str()
            .unwrap()
            .contains("does not grant current authority")
    );
    ensure!(std::fs::read_to_string(fixture.root.join(GUIDE))? == after);
    Ok(())
}

#[test]
fn removed_guide_and_its_links_remain_exact_historical_evidence_requiring_review() -> Result<()> {
    let fixture = Fixture::new()?;
    std::fs::remove_file(fixture.root.join(GUIDE))?;
    std::fs::write(
        fixture.root.join(".github/instructions/shared.md"),
        "Current contents must not substitute for the base revision.\n",
    )?;
    let report = fixture.review(SOURCE)?;
    ensure!(
        !report
            .packet
            .instructions
            .iter()
            .any(|row| row.path == GUIDE)
    );
    ensure!(prior(&report)["source_revision"] == fixture.base);
    ensure!(
        prior(&report)["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == GUIDE && row["content"] == BEFORE)
    );
    ensure!(
        prior(&report)["linked_sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == ".github/instructions/shared.md" && row["content"] == SHARED)
    );
    let context = report.packet.change_context.as_ref().unwrap();
    ensure!(context["status"] == "historical_guidance_requires_review");
    ensure!(
        context["prior_guidance_current_observations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == GUIDE
                && row["status"] == "absent_now_historical_evidence_only")
    );
    ensure!(!fixture.root.join(GUIDE).exists());
    Ok(())
}

#[test]
fn directly_changed_guide_is_reviewable_and_later_edits_refuse_stale_proposals() -> Result<()> {
    let fixture = Fixture::new()?;
    let after = BEFORE.replace("Site/**/*.vb", "Elsewhere/**/*.vb");
    std::fs::write(fixture.root.join(GUIDE), &after)?;
    let report = fixture.review(GUIDE)?;
    ensure!(
        report
            .packet
            .instructions
            .iter()
            .any(|row| row.path == GUIDE
                && row.current.content == after
                && row.before.as_ref().unwrap().content == BEFORE)
    );
    let context = report.packet.change_context.as_ref().unwrap();
    ensure!(
        context["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["path"] == GUIDE
                && row["selection_basis"] == "directly_changed_conventional_instruction")
    );
    let mut request = fixture.request(GUIDE)?;
    request.draft = Some(serde_json::from_value(
        json!({"expected_packet_blake3":report.packet_blake3,"decisions":[]}),
    )?);
    std::fs::write(
        fixture.root.join(GUIDE),
        format!("{after}\nRetain an additional owner decision.\n"),
    )?;
    let error = maintenance::review(&fixture.root, "prior-guidance", &request)
        .err()
        .expect("Changing the guide must refuse the old packet-bound proposal");
    ensure!(error.to_string().contains("Stale maintenance proposal"));
    Ok(())
}
