//! Config changes should select dependent guidance without invalidating every key.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const CONFIG: &str = "<configuration>\n<appSettings>\n<add key=\"ExportEnabled\" value=\"true\"/><add key=\"OtherSetting\"\n value=\"4\"/>\n</appSettings>\n</configuration>\n";
const EXPORT_PATH: &str = ".github/instructions/export.instructions.md";
const OTHER_PATH: &str = ".github/instructions/other.instructions.md";
const EXPORT: &str = "---\napplyTo: Site/Exports/**\n---\nPreserve `ExportEnabled`. Exception: approved exports remain read-only.\n";
const OTHER: &str = "---\napplyTo: Site/Other/**\n---\nPreserve `OtherSetting`.\n";

struct Fixture {
    root: PathBuf,
    base: String,
    owned: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("cim-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir_all(root.join(".github/instructions"))?;
        for (path, text) in [
            (EXPORT_PATH, EXPORT),
            (OTHER_PATH, OTHER),
            ("Web.config", CONFIG),
        ] {
            std::fs::write(root.join(path), text)?;
        }
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature =
                git2::Signature::now("Owned config fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned configuration base",
                &tree,
                &[],
            )?
            .to_string()
        };
        Ok(Self { root, base, owned })
    }

    fn capture(&self, explicit: bool) -> Result<Value> {
        let mut request = json!({"base_commit":self.base});
        if explicit {
            request["instruction_paths"] = json!([EXPORT_PATH, OTHER_PATH]);
            request["evidence_paths"] = json!(["Web.config"]);
        } else {
            request["changed_paths"] = json!(["Web.config"]);
        }
        let request: MaintenanceRequest = serde_json::from_value(request)?;
        Ok(serde_json::to_value(maintenance::capture(
            &self.root,
            "owned-config-guidance",
            &request,
        )?)?)
    }

    fn close(self) -> Result<()> {
        ensure!(std::fs::read_to_string(self.root.join(EXPORT_PATH))? == EXPORT);
        ensure!(std::fs::read_to_string(self.root.join(OTHER_PATH))? == OTHER);
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn app_setting_candidates_keep_exact_elements_values_lines_and_source_hashes() -> Result<()> {
    let fixture = Fixture::new()?;
    let after = CONFIG.replace("value=\"true\"", "value=\"false\"");
    std::fs::write(fixture.root.join("Web.config"), &after)?;
    let packet = fixture.capture(true)?;
    let states = packet["instruction_manifest"]["instructions"]
        .as_array()
        .unwrap();
    for (guide, name, changed) in [
        (EXPORT_PATH, "ExportEnabled", true),
        (OTHER_PATH, "OtherSetting", false),
    ] {
        let state = states
            .iter()
            .find(|row| row["instruction_path"] == guide)
            .unwrap();
        let change = state["symbol_changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["literal"] == name)
            .unwrap();
        ensure!(
            change["status"]
                == if changed {
                    "changed_observed_candidate_bytes"
                } else {
                    "same_observed_candidate_bytes"
                },
            "{change}"
        );
        for (version, source) in [
            ("before_candidates", CONFIG),
            ("after_candidates", after.as_str()),
        ] {
            let candidates = change[version].as_array().unwrap();
            ensure!(candidates.len() == 1, "{change}");
            let candidate = &candidates[0];
            ensure!(candidate["kind"] == "app_setting" && candidate["qualified_name"] == name);
            ensure!(
                candidate["span_kind"] == "xml_app_setting_element"
                    && candidate["absence_proof"] == false
            );
            let start = candidate["byte_range"][0].as_u64().unwrap() as usize;
            let end = candidate["byte_range"][1].as_u64().unwrap() as usize;
            let span = &source[start..end];
            ensure!(
                span.starts_with("<add ")
                    && span.ends_with("/>")
                    && span.matches("<add ").count() == 1
            );
            ensure!(span.contains(&format!("key=\"{name}\"")));
            ensure!(
                candidate["source_blake3"] == blake3::hash(source.as_bytes()).to_hex().to_string()
            );
            ensure!(
                candidate["evidence_blake3"] == blake3::hash(span.as_bytes()).to_hex().to_string()
            );
            ensure!(candidate["start_line"] == 3);
            ensure!(candidate["end_line"] == if name == "OtherSetting" { 4 } else { 3 });
        }
    }
    fixture.close()
}

#[test]
fn a_config_key_edit_selects_only_its_referencing_guide_without_policy_adoption() -> Result<()> {
    let fixture = Fixture::new()?;
    for (from, to, expected) in [
        ("value=\"true\"", "value=\"false\"", EXPORT_PATH),
        ("value=\"4\"", "value=\"8\"", OTHER_PATH),
    ] {
        std::fs::write(fixture.root.join("Web.config"), CONFIG.replace(from, to))?;
        let packet = fixture.capture(false)?;
        let instructions = packet["instructions"].as_array().unwrap();
        ensure!(
            instructions.len() == 1 && instructions[0]["path"] == expected,
            "{packet}"
        );
        let selection = &packet["change_context"]["sources"][0];
        ensure!(selection["selection_basis"] == "changed_symbol_reference");
        ensure!(
            selection["applicability"][0]
                .as_str()
                .unwrap()
                .contains("no authority")
        );
    }
    fixture.close()
}
