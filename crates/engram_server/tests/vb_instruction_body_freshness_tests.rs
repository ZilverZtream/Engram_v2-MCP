//! VB freshness compares captured source observations without promoting them to bindings.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const CODE: &str = "Site/OrderActions.vb";
const GUIDE: &str = "rules/orders.md";
const SOURCE: &str = "Public Class OrderActions\n Public Function SaveOrder() As Integer\n  Return 1\n End Function\n Public Function PreviewOrder() As Integer\n  Return 2\n End Function\nEnd Class\n";
struct Fixture {
    owned: tempfile::TempDir,
    root: PathBuf,
    request: MaintenanceRequest,
}
impl Fixture {
    fn new(reference: &str) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(2)
            .tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join("rules"))?;
        std::fs::write(root.join(CODE), SOURCE)?;
        std::fs::write(
            root.join(GUIDE),
            format!("Maintain `{reference}`. Preserve read-only exceptions.\n"),
        )?;
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned VB fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned original source",
                &tree,
                &[],
            )?
            .to_string()
        };
        let request = serde_json::from_value(json!({
            "base_commit":base,"instruction_paths":[GUIDE],"evidence_paths":[CODE]
        }))?;
        Ok(Self {
            owned,
            root,
            request,
        })
    }
    fn observe(&self, source: &str) -> Result<Value> {
        std::fs::write(self.root.join(CODE), source)?;
        Ok(serde_json::to_value(maintenance::capture(
            &self.root,
            "owned-vb",
            &self.request,
        )?)?)
    }
    fn close(self) -> Result<()> {
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn vb_referenced_method_range_distinguishes_its_body_edit_from_an_unrelated_method_edit()
-> Result<()> {
    let fixture = Fixture::new("SaveOrder")?;
    for (after, expected) in [
        (
            SOURCE.replace("Return 2", "Return 3"),
            "same_observed_candidate_bytes",
        ),
        (
            SOURCE.replace("Return 1", "Return 9"),
            "changed_observed_candidate_bytes",
        ),
    ] {
        let packet = fixture.observe(&after)?;
        let manifest = &packet["instruction_manifest"];
        let state = &manifest["instructions"][0];
        let change = &state["symbol_changes"][0];
        ensure!(change["status"] == expected, "{state}");
        ensure!(state["coverage_status"] == "incomplete_reference_coverage");
        ensure!(
            state["status"]
                == if expected == "same_observed_candidate_bytes" {
                    "needs_reference_evidence"
                } else {
                    "review_changed_dependencies"
                }
        );
        for (version, raw, candidates) in [
            ("before", SOURCE, &change["before_candidates"]),
            ("after", after.as_str(), &change["after_candidates"]),
        ] {
            let candidate = &candidates[0];
            let start = candidate["byte_range"][0].as_u64().unwrap() as usize;
            let end = candidate["byte_range"][1].as_u64().unwrap() as usize;
            let span = &raw[start..end];
            ensure!(candidate["span_kind"] == "regex_observed_method_line_range");
            ensure!(candidate["start_line"] == 2 && candidate["end_line"] == 4);
            ensure!(span.contains("Return ") && !span.contains("PreviewOrder"));
            ensure!(
                candidate["source_blake3"] == blake3::hash(raw.as_bytes()).to_hex().to_string()
            );
            ensure!(
                candidate["evidence_blake3"] == blake3::hash(span.as_bytes()).to_hex().to_string()
            );
            ensure!(candidate["absence_proof"] == false);
            ensure!(
                manifest["symbol_references"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["version"] == version
                        && r["literal"] == "SaveOrder"
                        && r["status"] == "unverified_vb_declaration_candidate")
            );
        }
        ensure!(
            change["before_candidates"][0]["source_blake3"]
                != change["after_candidates"][0]["source_blake3"]
        );
    }
    ensure!(
        std::fs::read_to_string(fixture.root.join(GUIDE))?
            == "Maintain `SaveOrder`. Preserve read-only exceptions.\n"
    );
    fixture.close()
}

#[test]
fn vb_class_header_retains_unavailable_body_coverage_instead_of_claiming_unchanged_body()
-> Result<()> {
    let fixture = Fixture::new("OrderActions")?;
    let packet = fixture.observe(&SOURCE.replace("Return 1", "Return 9"))?;
    let change = &packet["instruction_manifest"]["instructions"][0]["symbol_changes"][0];
    ensure!(change["status"] == "candidate_file_changed_body_coverage_unavailable");
    let candidate = &change["after_candidates"][0];
    ensure!(candidate["span_kind"] == "regex_declaration_line_only");
    ensure!(candidate["start_line"] == candidate["end_line"]);
    ensure!(candidate["absence_proof"] == false);
    fixture.close()
}
