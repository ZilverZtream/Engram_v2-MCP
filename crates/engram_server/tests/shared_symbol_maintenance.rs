//! Shared helper changes can require review of guidance scoped to its consumers.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const GUIDE: &str = ".github/instructions/prices.instructions.md";
const VB: &str = "Public Class SharedProjects\nPublic Function GetTopProjectID(id As Integer) As Integer\nReturn id\nEnd Function\nPublic Function ListProjects() As Integer\nReturn 2\nEnd Function\nEnd Class\n";
struct Fixture {
    root: PathBuf,
    base: String,
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new(guide: &str, sources: &[(&str, &str)]) -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("sm-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        for (path, text) in std::iter::once((GUIDE, guide)).chain(sources.iter().copied()) {
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap())?;
            std::fs::write(target, text)?;
        }
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned shared helper", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned shared helper base",
                &tree,
                &[],
            )?
            .to_string()
        };
        Ok(Self { root, base, owned })
    }
    fn capture(&self, paths: &[&str]) -> Result<Value> {
        let request: MaintenanceRequest =
            serde_json::from_value(json!({"base_commit":self.base,"changed_paths":paths}))?;
        Ok(serde_json::to_value(maintenance::capture(
            &self.root,
            "owned-shared-helper",
            &request,
        )?)?)
    }
    fn close(self) -> Result<()> {
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn shared_vb_method_selects_referencing_guidance_without_expanding_its_scope() -> Result<()> {
    let path = "Site/Shared/projects.vb";
    let guide = "---\napplyTo: Site/Prices/**\n---\nUse `projectId = GetTopProjectID()` for price-list inheritance.\nException: approved independent projects remain independent.\n";
    let fixture = Fixture::new(guide, &[(path, VB)])?;
    std::fs::write(fixture.root.join(path), VB.replace("Return 2", "Return 3"))?;
    let unrelated = fixture.capture(&[path])?;
    ensure!(
        unrelated["instructions"].as_array().unwrap().is_empty(),
        "{unrelated}"
    );
    let after = VB.replace("Return id", "Return -1");
    std::fs::write(fixture.root.join(path), &after)?;
    let packet = fixture.capture(&[path])?;
    ensure!(
        packet["instructions"].as_array().unwrap().len() == 1,
        "{packet}"
    );
    ensure!(
        packet["instructions"][0]["path"] == GUIDE
            && packet["instructions"][0]["current"]["content"] == guide
    );
    let source = &packet["change_context"]["sources"][0];
    ensure!(source["selection_basis"] == "changed_symbol_reference");
    ensure!(
        source["applicability"][0]
            .as_str()
            .unwrap()
            .contains("no authority")
    );
    let proof = &source["reference_matches"][0];
    ensure!(proof["symbol_change"]["literal"] == "GetTopProjectID()");
    for reference in proof["references"].as_array().unwrap() {
        let lo = reference["byte_range"][0].as_u64().unwrap() as usize;
        let hi = reference["byte_range"][1].as_u64().unwrap() as usize;
        ensure!(&guide[lo..hi] == "GetTopProjectID()");
        let lo = reference["container_byte_range"][0].as_u64().unwrap() as usize;
        let hi = reference["container_byte_range"][1].as_u64().unwrap() as usize;
        ensure!(&guide[lo..hi] == "`projectId = GetTopProjectID()`");
    }
    ensure!(proof["symbol_change"]["status"] == "changed_observed_candidate_bytes");
    let candidate = &proof["symbol_change"]["after_candidates"][0];
    ensure!(candidate["path"] == path && candidate["absence_proof"] == false);
    ensure!(candidate["source_blake3"] == blake3::hash(after.as_bytes()).to_hex().to_string());
    let lo = candidate["byte_range"][0].as_u64().unwrap() as usize;
    let hi = candidate["byte_range"][1].as_u64().unwrap() as usize;
    ensure!(
        candidate["evidence_blake3"] == blake3::hash(after[lo..hi].as_bytes()).to_hex().to_string()
    );
    ensure!(!after[lo..hi].contains("ListProjects"));
    ensure!(std::fs::read_to_string(fixture.root.join(GUIDE))? == guide);
    fixture.close()
}

#[test]
fn changed_typescript_and_sql_helpers_supply_their_own_source_evidence() -> Result<()> {
    let ts = "export function savePrices(id: number) { return id + 1; }\n";
    let sql = "CREATE PROCEDURE dbo.GetPrices AS SELECT 1 AS Price;\nGO\n";
    let ts_path = "Site/Shared/prices.ts";
    let sql_path = "db/prices.sql";
    let fixture = Fixture::new(
        "---\napplyTo: Site/Prices/**\n---\nUse `savePrices()` and `GetPrices()`.\n",
        &[(ts_path, ts), (sql_path, sql)],
    )?;
    let after_ts = ts.replace("id + 1", "id + 2");
    let after_sql = sql.replace("SELECT 1", "SELECT 2");
    std::fs::write(fixture.root.join(ts_path), &after_ts)?;
    std::fs::write(fixture.root.join(sql_path), &after_sql)?;
    let packet = fixture.capture(&[ts_path, sql_path])?;
    let matches = packet["change_context"]["sources"][0]["reference_matches"]
        .as_array()
        .unwrap();
    ensure!(matches.len() == 2, "{packet}");
    for (literal, path, source) in [
        ("savePrices()", ts_path, after_ts),
        ("GetPrices()", sql_path, after_sql),
    ] {
        let proof = matches
            .iter()
            .find(|proof| proof["symbol_change"]["literal"] == literal)
            .unwrap();
        ensure!(proof["symbol_change"]["status"] == "changed_observed_candidate_bytes");
        let candidate = &proof["symbol_change"]["after_candidates"][0];
        ensure!(
            candidate["path"] == path
                && candidate["source_blake3"]
                    == blake3::hash(source.as_bytes()).to_hex().to_string()
        );
        ensure!(proof["references"].as_array().unwrap().len() == 2);
    }
    fixture.close()
}
