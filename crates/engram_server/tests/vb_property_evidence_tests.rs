//! Instruction maintenance follows authored VB property evidence, not whole files.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const CODE: &str = "Site/OrderActions.vb";
const GUIDE: &str = ".github/instructions/orders.instructions.md";
const SOURCE: &str = r#"Namespace Sales
Public Class OrderActions
 Public Property CanSubmit As Boolean
  Get
   Return True
  End Get
  Private Set(value As Boolean)
   StoreFlag(value)
  End Set
 End Property
 Public Property Caption As String = "Orders"
 Public Function PreviewOrder() As Integer
  Return 2
 End Function
End Class
End Namespace
"#;

struct Fixture {
    owned: tempfile::TempDir,
    root: PathBuf,
    request: MaintenanceRequest,
    guidance: String,
}
impl Fixture {
    fn new(reference: &str) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("vb-properties-")
            .tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join("Site"))?;
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        std::fs::write(root.join(CODE), SOURCE)?;
        // This source does not apply to Site/**; a reference selects it for
        // maintenance review without extending its authored authority.
        let guidance = format!(
            "---\napplyTo: Elsewhere/**\n---\nMaintain `{reference}`. Preserve read-only exceptions.\n"
        );
        std::fs::write(root.join(GUIDE), &guidance)?;
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
                "Owned evidence",
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
            guidance,
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
        ensure!(std::fs::read_to_string(self.root.join(GUIDE))? == self.guidance);
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn referenced_property_tracks_its_accessors_without_invalidating_unrelated_methods() -> Result<()> {
    let fixture = Fixture::new("Sales.OrderActions.CanSubmit")?;
    for (after, expected) in [
        (
            SOURCE.replace("Return 2", "Return 3"),
            "same_observed_candidate_bytes",
        ),
        (
            SOURCE.replace("Return True", "Return False"),
            "changed_observed_candidate_bytes",
        ),
        (
            SOURCE.replace("StoreFlag(value)", "StoreCheckedFlag(value)"),
            "changed_observed_candidate_bytes",
        ),
    ] {
        let packet = fixture.observe(&after)?;
        let state = &packet["instruction_manifest"]["instructions"][0];
        let change = &state["symbol_changes"][0];
        ensure!(change["status"] == expected, "{state}");
        ensure!(state["coverage_status"] == "incomplete_reference_coverage");
        for (version, raw) in [("before", SOURCE), ("after", after.as_str())] {
            let candidate = &change[format!("{version}_candidates")][0];
            let lo = candidate["byte_range"][0].as_u64().unwrap() as usize;
            let hi = candidate["byte_range"][1].as_u64().unwrap() as usize;
            let span = &raw[lo..hi];
            ensure!(candidate["kind"] == "property");
            ensure!(candidate["qualified_name"] == "Sales.OrderActions.CanSubmit");
            ensure!(candidate["span_kind"] == "regex_observed_property_line_range");
            ensure!(
                span.contains("End Property")
                    && span.contains("End Set")
                    && !span.contains("Caption")
            );
            ensure!(
                candidate["source_blake3"] == blake3::hash(raw.as_bytes()).to_hex().to_string()
            );
            ensure!(
                candidate["evidence_blake3"] == blake3::hash(span.as_bytes()).to_hex().to_string()
            );
            ensure!(candidate["absence_proof"] == false);
        }
        ensure!(
            state["status"]
                == if expected == "same_observed_candidate_bytes" {
                    "needs_reference_evidence"
                } else {
                    "review_changed_dependencies"
                }
        );
        ensure!(
            packet["instruction_manifest"]["symbol_references"]
                .as_array()
                .unwrap()
                .iter()
                .all(|reference| reference["status"] == "unverified_vb_declaration_candidate")
        );
    }
    fixture.close()
}

#[test]
fn auto_property_initializer_is_evidence_without_absorbing_the_next_member() -> Result<()> {
    let fixture = Fixture::new("Caption")?;
    for (after, expected) in [
        (
            SOURCE.replace("Return 2", "Return 3"),
            "same_observed_candidate_bytes",
        ),
        (
            SOURCE.replace("\"Orders\"", "\"Approved orders\""),
            "changed_observed_candidate_bytes",
        ),
    ] {
        let packet = fixture.observe(&after)?;
        let change = &packet["instruction_manifest"]["instructions"][0]["symbol_changes"][0];
        ensure!(change["status"] == expected, "{change}");
        let candidate = &change["after_candidates"][0];
        ensure!(candidate["span_kind"] == "regex_observed_auto_property_line_range");
        ensure!(candidate["start_line"] == candidate["end_line"]);
        let lo = candidate["byte_range"][0].as_u64().unwrap() as usize;
        let hi = candidate["byte_range"][1].as_u64().unwrap() as usize;
        let span = &after[lo..hi];
        ensure!(span.contains("Property Caption") && !span.contains("PreviewOrder"));
        ensure!(candidate["evidence_blake3"] == blake3::hash(span.as_bytes()).to_hex().to_string());
        ensure!(candidate["absence_proof"] == false);
    }
    fixture.close()
}

#[test]
fn changed_property_selects_referencing_guidance_without_expanding_its_scope() -> Result<()> {
    let mut fixture = Fixture::new("Sales.OrderActions.CanSubmit")?;
    fixture.request.instruction_paths.clear();
    fixture.request.evidence_paths.clear();
    fixture.request.changed_paths = vec![CODE.into()];
    let unrelated = fixture.observe(&SOURCE.replace("Return 2", "Return 3"))?;
    ensure!(
        unrelated["instructions"].as_array().unwrap().is_empty(),
        "{unrelated}"
    );
    let packet = fixture.observe(&SOURCE.replace("Return True", "Return False"))?;
    ensure!(
        packet["instructions"].as_array().unwrap().len() == 1,
        "{packet}"
    );
    ensure!(packet["instructions"][0]["path"] == GUIDE);
    ensure!(packet["instructions"][0]["current"]["content"] == fixture.guidance);
    let source = &packet["change_context"]["sources"][0];
    ensure!(source["selection_basis"] == "changed_symbol_reference");
    ensure!(
        source["reference_matches"][0]["symbol_change"]["status"]
            == "changed_observed_candidate_bytes"
    );
    ensure!(source["applicability"].to_string().contains("no authority"));
    fixture.close()
}
