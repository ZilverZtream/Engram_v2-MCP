//! SQL-qualified guidance must retain its schema when selecting maintenance.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const SQL: &str = "CREATE TABLE dbo.Orders (Id int, Amount decimal(10,2));\nGO\nCREATE TABLE audit.Orders (Id int, Message varchar(20));\nGO\n";
const ORDERS: &str = ".github/instructions/orders.instructions.md";
const AUDIT: &str = ".github/instructions/audit.instructions.md";
const ORDER_GUIDE: &str = "---\napplyTo: Site/Orders/**\n---\nUse `[dbo].[Orders]`; `dbo.Orders` is the same written name. Preserve approved amount exceptions.\n";
const AUDIT_GUIDE: &str =
    "---\napplyTo: Site/Audit/**\n---\nUse `[audit].[Orders]` for audit records.\n";

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
        let owned = tempfile::Builder::new().prefix("sq-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        for (path, text) in [
            (ORDERS, ORDER_GUIDE),
            (AUDIT, AUDIT_GUIDE),
            ("db/schema.sql", SQL),
        ] {
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
            let signature = git2::Signature::now("Owned SQL fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned qualified SQL base",
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
            request["instruction_paths"] = json!([ORDERS, AUDIT]);
            request["evidence_paths"] = json!(["db/schema.sql"]);
        } else {
            request["changed_paths"] = json!(["db/schema.sql"]);
        }
        let request: MaintenanceRequest = serde_json::from_value(request)?;
        Ok(serde_json::to_value(maintenance::capture(
            &self.root,
            "owned-sql-qualified",
            &request,
        )?)?)
    }

    fn close(self) -> Result<()> {
        ensure!(std::fs::read_to_string(self.root.join(ORDERS))? == ORDER_GUIDE);
        ensure!(std::fs::read_to_string(self.root.join(AUDIT))? == AUDIT_GUIDE);
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn bracketed_qualified_names_keep_separate_schema_evidence() -> Result<()> {
    let fixture = Fixture::new()?;
    let after = SQL.replace("decimal(10,2)", "decimal(12,2)");
    std::fs::write(fixture.root.join("db/schema.sql"), &after)?;
    let packet = fixture.capture(true)?;
    let manifest = &packet["instruction_manifest"];
    for (guide, literal, name, changed) in [
        (ORDERS, "[dbo].[Orders]", "dbo.Orders", true),
        (ORDERS, "dbo.Orders", "dbo.Orders", true),
        (AUDIT, "[audit].[Orders]", "audit.Orders", false),
    ] {
        let state = manifest["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["instruction_path"] == guide)
            .unwrap();
        let change = state["symbol_changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["literal"] == literal)
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
        for (version, content) in [
            ("before_candidates", SQL),
            ("after_candidates", after.as_str()),
        ] {
            let candidates = change[version].as_array().unwrap();
            ensure!(candidates.len() == 1, "{change}");
            let candidate = &candidates[0];
            ensure!(candidate["qualified_name"] == name && candidate["absence_proof"] == false);
            ensure!(
                candidate["source_blake3"] == blake3::hash(content.as_bytes()).to_hex().to_string()
            );
            let lo = candidate["byte_range"][0].as_u64().unwrap() as usize;
            let hi = candidate["byte_range"][1].as_u64().unwrap() as usize;
            ensure!(
                candidate["evidence_blake3"]
                    == blake3::hash(&content.as_bytes()[lo..hi])
                        .to_hex()
                        .to_string()
            );
            ensure!(content[lo..hi].contains(name));
        }
    }
    fixture.close()
}

#[test]
fn shared_schema_change_selects_only_the_referencing_consumer_guide() -> Result<()> {
    let fixture = Fixture::new()?;
    std::fs::write(
        fixture.root.join("db/schema.sql"),
        SQL.replace("decimal(10,2)", "decimal(12,2)"),
    )?;
    let packet = fixture.capture(false)?;
    let instructions = packet["instructions"].as_array().unwrap();
    ensure!(
        instructions.len() == 1 && instructions[0]["path"] == ORDERS,
        "{packet}"
    );
    ensure!(instructions[0]["current"]["content"] == ORDER_GUIDE);
    let selection = &packet["change_context"]["sources"][0];
    ensure!(selection["selection_basis"] == "changed_symbol_reference");
    ensure!(
        selection["applicability"][0]
            .as_str()
            .unwrap()
            .contains("no authority")
    );
    ensure!(
        selection["reference_matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["symbol_change"]["literal"] == "[dbo].[Orders]"),
        "{selection}"
    );
    fixture.close()
}
