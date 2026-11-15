//! A caller starts maintenance with changed code paths, not known guide names.
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const GUIDE: &str = "---\napplyTo: Site/**\n---\nUse reviewed saves.\nException: read-only probes do not save.\nSee [shared](shared.md) and `Site/worker.ts`.\n";
const BEFORE: &str = "export function save() { return 1; }\n";
const AFTER: &str = "export function save() { return 2; }\n";
fn commit(root: &Path) -> String {
    let repo = git2::Repository::open(root).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
        .unwrap()
        .to_string()
}
struct Fixture {
    e: Engram,
    root: PathBuf,
    base: String,
    // State/database owners drop before the fixture directory, including unwind.
    _temp: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        // Keep libgit2's administrative paths short inside the recorded case.
        // The build runner supplies this root; ordinary runs use their temp dir.
        let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temp = tempfile::Builder::new().prefix("").rand_bytes(2)
            .tempdir_in(fixture_root).unwrap();
        let root = temp.path().join("r");
        std::fs::create_dir_all(root.join("Site")).unwrap();
        std::fs::create_dir_all(root.join(".github/instructions")).unwrap();
        drop(git2::Repository::init(&root).unwrap());
        std::fs::write(
            root.join(".github/instructions/saves.instructions.md"),
            GUIDE,
        )
        .unwrap();
        std::fs::write(
            root.join(".github/instructions/shared.md"),
            "Preserve authorization and accepted exceptions.\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".github/instructions/other.instructions.md"),
            "---\napplyTo: Elsewhere/**\n---\nUnrelated requirements.\n",
        )
        .unwrap();
        std::fs::write(root.join("Site/worker.ts"), BEFORE).unwrap();
        let base = commit(&root);
        std::fs::write(root.join("Site/worker.ts"), AFTER).unwrap();
        let (state, _) = AppState::new(Config {
            data_dir: temp.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })
        .unwrap();
        state
            .registry
            .put_project(&ProjectRecord {
                project_id: "changed-guidance".into(),
                project_name: "Changed guidance".into(),
                project_type: "general".into(),
                directory: root.to_string_lossy().into_owned(),
                created_at_ms: 1,
                updated_at_ms: 1,
                reindex_required_since_ms: None,
            })
            .unwrap();
        Self {
            e: Engram::new(state),
            root,
            base,
            _temp: temp,
        }
    }
    fn maintenance(&self) -> Value {
        json!({"changed_paths":["Site/worker.ts"],"base_commit":self.base})
    }
    async fn audit(&self, maintenance: Value) -> Result<Value, rmcp::ErrorData> {
        let result = self
            .e
            .handle_pre_push_audit(
                serde_json::from_value(
                    json!({"project_id":"changed-guidance","instruction_maintenance":maintenance}),
                )
                .unwrap(),
            )
            .await?;
        Ok(serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap())
    }
}
fn find<'a>(rows: &'a Value, path: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == path)
        .unwrap()
}

#[tokio::test]
async fn changed_code_discovers_complete_scoped_guidance_without_opening_an_index() {
    let f = Fixture::new();
    let report = f.audit(f.maintenance()).await.unwrap();
    assert!(f.e.state.projects.is_empty());
    assert_eq!(report["packet"]["version"], 6);
    let guide = find(
        &report["packet"]["instructions"],
        ".github/instructions/saves.instructions.md",
    );
    assert_eq!(guide["current"]["content"], GUIDE);
    assert_eq!(guide["before"]["content"], GUIDE);
    assert!(
        !report["packet"]["instructions"]
            .to_string()
            .contains("other.instructions.md")
    );
    let code = find(&report["packet"]["evidence"], "Site/worker.ts");
    assert_eq!(code["before"]["content"], BEFORE);
    assert_eq!(code["after"]["content"], AFTER);
    assert!(
        report["packet"]["instruction_manifest"]
            .to_string()
            .contains("Preserve authorization and accepted exceptions.")
    );
    assert_eq!(
        report["packet"]["change_context"]["changed_paths"],
        json!(["Site/worker.ts"])
    );
    assert!(
        report["packet"]["change_context"]["interpretation"]
            .as_str()
            .unwrap()
            .contains("not an exhaustive Git diff")
    );
    assert_eq!(report["authored_files_written"], 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join(".github/instructions/saves.instructions.md")).unwrap(),
        GUIDE
    );
    assert_eq!(
        report["packet_blake3"],
        f.audit(f.maintenance()).await.unwrap()["packet_blake3"]
    );
}

#[tokio::test]
async fn discovered_guidance_uses_existing_proposal_and_stale_packet_checks() {
    let f = Fixture::new();
    let report = f.audit(f.maintenance()).await.unwrap();
    let mut request = f.maintenance();
    let guide = find(
        &report["packet"]["instructions"],
        ".github/instructions/saves.instructions.md",
    );
    let code = find(&report["packet"]["evidence"], "Site/worker.ts");
    request["draft"] = json!({"expected_packet_blake3":report["packet_blake3"],"decisions":[{"instruction_path":guide["path"],"outcome":"no_change","rationale":"The changed return value does not alter the accepted save/authorization requirements.","evidence":[{"path":guide["path"],"version":"after","blake3":guide["current"]["blake3"],"quote":"Exception: read-only probes do not save."},{"path":"Site/worker.ts","version":"before","blake3":code["before"]["blake3"],"quote":BEFORE},{"path":"Site/worker.ts","version":"after","blake3":code["after"]["blake3"],"quote":AFTER}],"preserved_fragments":["Exception: read-only probes do not save."]}]});
    let proposed = f.audit(request.clone()).await.unwrap();
    assert_eq!(proposed["status"], "proposal_requires_semantic_review");
    assert_eq!(proposed["proposals"][0]["proposed_content"], GUIDE);
    std::fs::write(
        f.root.join(".github/instructions/added.instructions.md"),
        "---\napplyTo: Site/**\n---\nNew reviewed instruction.\n",
    )
    .unwrap();
    assert!(
        f.audit(request).await.is_err(),
        "New applicable guidance must invalidate the old proposal packet"
    );
    assert_eq!(
        std::fs::read_to_string(f.root.join(".github/instructions/saves.instructions.md")).unwrap(),
        GUIDE
    );
}

#[tokio::test]
async fn health_and_completed_update_report_changed_path_selection_and_preserve_guidance() {
    let f = Fixture::new();
    let repaired=f.e.handle_repair_project(serde_json::from_value(json!({"project_id":"changed-guidance","scope":"full","max_commits":0,"index_antipatterns":false})).unwrap()).await.unwrap();
    assert_ne!(repaired.is_error, Some(true));
    let audit = f.audit(f.maintenance()).await.unwrap();
    let health =
        f.e.handle_project_health_with_instructions(
            serde_json::from_value(
                json!({"project_id":"changed-guidance","instruction_maintenance":f.maintenance()}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let health: Value =
        serde_json::from_str(&health.content.last().unwrap().as_text().unwrap().text).unwrap();
    let observed = &health["instruction_reference_observation"];
    assert_eq!(observed["status"], "observed");
    assert_eq!(observed["packet_blake3"], audit["packet_blake3"]);
    assert_eq!(
        observed["change_context"],
        audit["packet"]["change_context"]
    );
    assert_eq!(
        observed["routing_coverage"]["basis"],
        "explicit_changed_paths"
    );
    let generation =
        f.e.state
            .registry
            .get_meta("changed-guidance", "active_generation")
            .unwrap();
    std::fs::write(
        f.root.join("Site/worker.ts"),
        "export function save() { return 3; }\n",
    )
    .unwrap();
    let updated=f.e.handle_update_project(serde_json::from_value(json!({"project_id":"changed-guidance","wait":true,"max_commits":0,"index_antipatterns":false,"instruction_maintenance":f.maintenance()})).unwrap()).await.unwrap();
    assert_ne!(updated.is_error, Some(true));
    assert!(
        updated.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("status: done")
    );
    assert_ne!(
        generation,
        f.e.state
            .registry
            .get_meta("changed-guidance", "active_generation")
            .unwrap()
    );
    let updated: Value =
        serde_json::from_str(&updated.content.last().unwrap().as_text().unwrap().text).unwrap();
    let observed = &updated["instruction_reference_observation"];
    assert_eq!(observed["status"], "observed");
    assert_eq!(
        observed["change_context"]["changed_paths"],
        json!(["Site/worker.ts"])
    );
    assert_eq!(observed["authored_files_written"], 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join(".github/instructions/saves.instructions.md")).unwrap(),
        GUIDE
    );
}

#[tokio::test]
async fn missing_candidates_and_invalid_scopes_do_not_claim_policy_coverage() {
    let f = Fixture::new();
    std::fs::create_dir(f.root.join("Elsewhere")).unwrap();
    std::fs::write(f.root.join("Elsewhere/new.sql"), "SELECT 1;\n").unwrap();
    let mut request = f.maintenance();
    request["changed_paths"] = json!(["Elsewhere/new.sql"]);
    let report = f.audit(request).await.unwrap();
    // Its own explicit guide is selected; Site's guide must not widen to SQL.
    assert!(
        !report["packet"]["instructions"]
            .to_string()
            .contains("saves.instructions.md")
    );
    let mut request = f.maintenance();
    request["changed_paths"] = json!(["../outside.ts"]);
    assert!(f.audit(request).await.is_err());
    let mut request = f.maintenance();
    request["instruction_paths"] = json!([".github/instructions/saves.instructions.md"]);
    assert!(f.audit(request).await.is_err());
    std::fs::create_dir(f.root.join("Uncovered")).unwrap();
    std::fs::write(
        f.root.join("Uncovered/new.vb"),
        "Public Class NewClass\nEnd Class\n",
    )
    .unwrap();
    let mut request = f.maintenance();
    request["changed_paths"] = json!(["Uncovered/new.vb"]);
    let report = f.audit(request).await.unwrap();
    assert!(
        report["packet"]["instructions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        report["packet"]["change_context"]["status"],
        "no_current_instruction_candidates_observed"
    );
    assert!(
        report["packet"]["change_context"]["interpretation"]
            .as_str()
            .unwrap()
            .contains("no guidance change is warranted")
    );
}
