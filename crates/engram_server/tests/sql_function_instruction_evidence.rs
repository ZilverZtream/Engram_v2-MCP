//! SQL function evidence selects affected guidance without widening policy authority.
use anyhow::{Result, ensure};
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::PathBuf;

const SQL: &str = "CREATE FUNCTION dbo.ApprovalLimit(@Tier int)\nRETURNS int\nAS\nBEGIN\n RETURN @Tier + 2;\nEND\nGO\nCREATE FUNCTION dbo.ApprovalRows(@Tenant int)\nRETURNS TABLE\nAS\nRETURN (SELECT Id FROM dbo.Approvals WHERE TenantId = @Tenant);\nGO\nCREATE TABLE dbo.Approvals (Id int, TenantId int);\nGO\nCREATE PROCEDURE dbo.GetApprovals AS SELECT Id FROM dbo.Approvals;\nGO\n";
const FUNCTIONS: &str = ".github/instructions/functions.instructions.md";
const REPORTS: &str = ".github/instructions/reports.instructions.md";
const FUNCTION_GUIDE: &str = "---\napplyTo: Site/Approval/**\n---\nPreserve `dbo.ApprovalLimit`, `dbo.ApprovalRows`, `dbo.Approvals` and `GetApprovals`.\nException: approved exports remain read-only.\n";
const REPORT_GUIDE: &str =
    "---\napplyTo: Site/Reports/**\n---\nUse `GetApprovals` for approved reports.\n";

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
        let owned = tempfile::Builder::new().prefix("sfm-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        for (path, text) in [
            (FUNCTIONS, FUNCTION_GUIDE),
            (REPORTS, REPORT_GUIDE),
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
            request["instruction_paths"] = json!([FUNCTIONS, REPORTS]);
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
        ensure!(std::fs::read_to_string(self.root.join(FUNCTIONS))? == FUNCTION_GUIDE);
        ensure!(std::fs::read_to_string(self.root.join(REPORTS))? == REPORT_GUIDE);
        self.owned.close()?;
        Ok(())
    }
}

#[test]
fn scalar_and_table_valued_functions_preserve_complete_declared_evidence() -> Result<()> {
    let fixture = Fixture::new()?;
    let after = SQL.replace("RETURN @Tier + 2", "RETURN @Tier + 7");
    std::fs::write(fixture.root.join("db/schema.sql"), &after)?;
    let packet = fixture.capture(true)?;
    let instructions = packet["instruction_manifest"]["instructions"]
        .as_array()
        .unwrap();
    for (guide, literal, kind, changed) in [
        (FUNCTIONS, "dbo.ApprovalLimit", "function", true),
        (FUNCTIONS, "dbo.ApprovalRows", "function", false),
        (FUNCTIONS, "dbo.Approvals", "table", false),
        (REPORTS, "GetApprovals", "stored_procedure", false),
    ] {
        let state = instructions
            .iter()
            .find(|x| x["instruction_path"] == guide)
            .unwrap();
        let change = state["symbol_changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["literal"] == literal)
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
            ensure!(candidate["kind"] == kind && candidate["qualified_name"] == literal);
            ensure!(candidate["absence_proof"] == false);
            ensure!(
                candidate["source_blake3"] == blake3::hash(content.as_bytes()).to_hex().to_string()
            );
            let lo = candidate["byte_range"][0].as_u64().unwrap() as usize;
            let hi = candidate["byte_range"][1].as_u64().unwrap() as usize;
            let span = &content[lo..hi];
            ensure!(
                candidate["evidence_blake3"] == blake3::hash(span.as_bytes()).to_hex().to_string()
            );
            ensure!(span.contains(literal));
            if kind == "function" {
                ensure!(candidate["span_kind"] == "sql_catalog_declaration_line_range");
                if literal.ends_with("ApprovalLimit") {
                    ensure!(span.contains(if version == "before_candidates" {
                        "RETURN @Tier + 2"
                    } else {
                        "RETURN @Tier + 7"
                    }));
                } else {
                    ensure!(span.contains("SELECT Id FROM dbo.Approvals"));
                }
            }
        }
    }
    fixture.close()
}

#[test]
fn changed_function_selects_its_consumer_without_selecting_unchanged_procedure_guidance()
-> Result<()> {
    let fixture = Fixture::new()?;
    std::fs::write(
        fixture.root.join("db/schema.sql"),
        SQL.replace("RETURN @Tier + 2", "RETURN @Tier + 7"),
    )?;
    let packet = fixture.capture(false)?;
    let instructions = packet["instructions"].as_array().unwrap();
    ensure!(
        instructions.len() == 1 && instructions[0]["path"] == FUNCTIONS,
        "{packet}"
    );
    ensure!(instructions[0]["current"]["content"] == FUNCTION_GUIDE);
    let source = &packet["change_context"]["sources"][0];
    ensure!(source["selection_basis"] == "changed_symbol_reference");
    ensure!(
        source["reference_matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["symbol_change"]["literal"] == "dbo.ApprovalLimit")
    );
    ensure!(
        source["applicability"][0]
            .as_str()
            .unwrap()
            .contains("no authority")
    );
    fixture.close()
}
