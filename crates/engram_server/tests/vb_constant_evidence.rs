//! Instruction maintenance follows authored VB property evidence, not whole files.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const CODE: &str = "Site/KmlGeoJsonConversionService.vb";
const GUIDE: &str = ".github/instructions/orders.instructions.md";
const OTHER_GUIDE: &str = ".github/instructions/conversion-errors.instructions.md";
const SOURCE: &str = r#"Namespace _vs
Public Class KmlGeoJsonConversionService
 Public Const CurrentConversionVersion As Integer = 1
 Private Const GenericConversionErrorMessage As String = "Conversion failed"
 Public Function PreviewOrder() As Integer
  Const LocalVersion As Integer = 99
  Return 2
 End Function
End Class
End Namespace
"#;
const OTHER_GUIDANCE: &str = "---\napplyTo: Elsewhere/**\n---\nPreserve `_vs.KmlGeoJsonConversionService.GenericConversionErrorMessage`.\n";

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
            .prefix("vb-constants-")
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
        std::fs::write(root.join(OTHER_GUIDE), OTHER_GUIDANCE)?;
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
        ensure!(std::fs::read_to_string(self.root.join(OTHER_GUIDE))? == OTHER_GUIDANCE);
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn conversion_version_initializer_is_exact_dependency_evidence_not_whole_file_invalidation()
-> Result<()> {
    let fixture = Fixture::new("_vs.KmlGeoJsonConversionService.CurrentConversionVersion")?;
    for (after, expected) in [
        (
            SOURCE.replace("Return 2", "Return 3"),
            "same_observed_candidate_bytes",
        ),
        (
            SOURCE.replace("As Integer = 1", "As Integer = 2"),
            "changed_observed_candidate_bytes",
        ),
    ] {
        let packet = fixture.observe(&after)?;
        let state = &packet["instruction_manifest"]["instructions"][0];
        let change = &state["symbol_changes"][0];
        ensure!(change["status"] == expected, "{change}");
        for (version, raw) in [("before", SOURCE), ("after", after.as_str())] {
            let candidate = &change[format!("{version}_candidates")][0];
            let lo = candidate["byte_range"][0].as_u64().unwrap() as usize;
            let hi = candidate["byte_range"][1].as_u64().unwrap() as usize;
            let evidence = &raw[lo..hi];
            ensure!(candidate["kind"] == "constant");
            ensure!(
                candidate["qualified_name"]
                    == "_vs.KmlGeoJsonConversionService.CurrentConversionVersion"
            );
            ensure!(candidate["span_kind"] == "regex_observed_constant_line_range");
            ensure!(candidate["start_line"] == candidate["end_line"]);
            ensure!(
                evidence.contains("Public Const CurrentConversionVersion")
                    && !evidence.contains("GenericConversionErrorMessage")
            );
            ensure!(
                candidate["evidence_blake3"]
                    == blake3::hash(evidence.as_bytes()).to_hex().to_string()
            );
            ensure!(
                candidate["source_blake3"] == blake3::hash(raw.as_bytes()).to_hex().to_string()
            );
            ensure!(candidate["absence_proof"] == false);
        }
        ensure!(state["coverage_status"] == "incomplete_reference_coverage");
    }
    let observed = engram_index::vb_extractor::snapshot_declaration_candidates(SOURCE)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        !observed
            .iter()
            .any(|candidate| candidate.kind == "constant"
                && candidate.name.ends_with("LocalVersion")),
        "Method-local constants must not become type-level dependencies"
    );
    fixture.close()
}

#[test]
fn changed_constant_selects_only_its_guide_preserving_authored_scope_and_exceptions() -> Result<()>
{
    let mut fixture = Fixture::new("_vs.KmlGeoJsonConversionService.CurrentConversionVersion")?;
    fixture.request.instruction_paths.clear();
    fixture.request.evidence_paths.clear();
    fixture.request.changed_paths = vec![CODE.into()];
    for (after, selected) in [
        (SOURCE.replace("Return 2", "Return 3"), None),
        (
            SOURCE.replace("As Integer = 1", "As Integer = 2"),
            Some(GUIDE),
        ),
        (
            SOURCE.replace("Conversion failed", "Unable to convert"),
            Some(OTHER_GUIDE),
        ),
    ] {
        let packet = fixture.observe(&after)?;
        let instructions = packet["instructions"].as_array().unwrap();
        if let Some(selected) = selected {
            ensure!(
                instructions.len() == 1 && instructions[0]["path"] == selected,
                "{packet}"
            );
            ensure!(
                instructions[0]["current"]["content"]
                    .as_str()
                    .unwrap()
                    .contains("Elsewhere/**")
            );
            let context = &packet["change_context"]["sources"][0];
            ensure!(
                context["applicability"]
                    .to_string()
                    .contains("no authority")
            );
        } else {
            ensure!(
                instructions.is_empty(),
                "Unrelated method edit selected guidance: {packet}"
            );
        }
    }
    fixture.close()
}
