//! Wanted behavior: a reviewable, dependency-bound maintenance proposal never
//! turns an implementation change or mechanically valid quote into policy.
use engram_core::{Config, ProjectRecord};
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};
use std::path::Path;

#[path = "instruction_maintenance_tests/reference_behavior.rs"]
mod reference_behavior;
#[path = "instruction_maintenance_tests/symbol_behavior.rs"]
mod symbol_behavior;

const POLICY: &str = "---\napplyTo: Site/**\n---\nUse OldHelper for authorized saves.\nException: read-only health probes need no permission prompt.\nKeep the [shared rule](shared.md).\n";
const BEFORE: &str = "Function OldHelper()\n    CheckPermission()\n    Save()\nEnd Function\n";
const AFTER: &str = "Function NewHelper()\n    CheckPermission()\n    Save()\nEnd Function\n";

fn commit(root: &Path) -> String {
    let repo = git2::Repository::open(root).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature =
        git2::Signature::now("Engram behavioral fixture", "fixture@example.invalid").unwrap();
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "fixture",
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
    .to_string()
}

fn fixture() -> (tempfile::TempDir, Engram, Value) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(root.join("rules")).unwrap();
    std::fs::create_dir_all(root.join("Site")).unwrap();
    drop(git2::Repository::init(&root).unwrap());
    std::fs::write(root.join("rules/area.md"), POLICY).unwrap();
    std::fs::write(
        root.join("rules/shared.md"),
        "Retain per-action authorization.\n",
    )
    .unwrap();
    std::fs::write(root.join("Site/Helper.vb"), BEFORE).unwrap();
    let base = commit(&root);
    std::fs::write(root.join("Site/Helper.vb"), AFTER).unwrap();
    let cfg = Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: "maintenance".into(),
            project_name: "Maintenance behavior".into(),
            project_type: "general".into(),
            directory: root.to_str().unwrap().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    (
        temp,
        Engram::new(state),
        json!({"project_id":"maintenance", "instruction_maintenance":{
            "instruction_paths":["rules/area.md"], "evidence_paths":["Site/Helper.vb","rules/shared.md"], "base_commit":base
        }}),
    )
}

async fn call(engram: &Engram, request: Value) -> Result<Value, rmcp::ErrorData> {
    let response = engram
        .handle_pre_push_audit(serde_json::from_value(request).unwrap())
        .await?;
    Ok(serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap())
}

const NEW_AREA: &str = ".github/instructions/saves.instructions.md";
const NEW_PROVENANCE: &str = ".github/instructions/saves.provenance.json";

fn area_fixture() -> (tempfile::TempDir, Engram, Value) {
    let (temporary, engram, mut request) = fixture();
    request["instruction_maintenance"]["instruction_paths"] = json!([]);
    request["instruction_maintenance"]["evidence_paths"] =
        json!(["Site/Helper.vb", "rules/area.md", "rules/shared.md"]);
    request["instruction_maintenance"]["new_instruction_targets"] = json!([{
        "instruction_path":NEW_AREA, "provenance_path":NEW_PROVENANCE, "area_scope":["Site/**"]
    }]);
    (temporary, engram, request)
}

fn area_citation(report: &Value, path: &str, version: &str, quote: &str) -> Value {
    let snapshot = &report["packet"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap()[version];
    json!({"path":path,"version":version,"blake3":snapshot["blake3"],"quote":quote})
}

fn area_proposal(report: &Value) -> Value {
    let code = report["packet"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "Site/Helper.vb")
        .unwrap()["after"]["content"]
        .as_str()
        .unwrap();
    let policy = area_citation(report, "rules/area.md", "after", POLICY);
    let example = area_citation(report, "Site/Helper.vb", "after", code);
    let shared = area_citation(
        report,
        "rules/shared.md",
        "after",
        "Retain per-action authorization.\n",
    );
    let exception = "Exception: read-only health probes need no permission prompt.";
    json!({"expected_packet_blake3":report["packet_blake3"],"new_area_files":[{
        "instruction_path":NEW_AREA,
        "claims":[
            {"id":"purpose","section":"purpose","kind":"authored_intent","text":"Authorized saves in the Site area.",
                "rationale":"The complete area policy names this responsibility.","evidence":[policy.clone()]},
            {"id":"authorization","section":"invariant","kind":"authored_intent","text":"Retain per-action authorization.",
                "rationale":"Keep the shared requirement and its complete area exception.","evidence":[shared.clone(),policy.clone()]},
            {"id":"example","section":"house_example","kind":"observed_implementation","text":code,
                "rationale":"This is the exact current helper, supplied as an example rather than an inferred mandate.","evidence":[example.clone()]},
            {"id":"companion","section":"companion_file","kind":"observed_implementation","text":"The area policy links the shared authorization guidance.",
                "rationale":"Use the captured link and complete companion source.","evidence":[example,policy.clone(),shared.clone()]},
            {"id":"health-exception","section":"accepted_exception","kind":"authored_intent","text":exception,
                "rationale":"Retain the exact authored condition; semantic authority still requires review.","evidence":[policy.clone()],
                "explicit_ruling":area_citation(report,"rules/area.md","after",exception)}
        ],"shared_rules":[policy,shared],"unresolved_decisions":[]
    }]})
}

fn proposed_area(report: &Value) -> (&Value, Value) {
    let area = &report["new_area_proposals"][0];
    let provenance: Value =
        serde_json::from_str(area["provenance"]["proposed_content"].as_str().unwrap()).unwrap();
    (area, provenance)
}

#[tokio::test]
async fn new_area_draft_returns_scoped_guidance_with_resolvable_provenance_without_writing_files() {
    let (temporary, engram, mut request) = area_fixture();
    let packet = call(&engram, request.clone()).await.unwrap();
    request["instruction_maintenance"]["draft"] = area_proposal(&packet);
    let report = call(&engram, request).await.unwrap();
    let (area, provenance) = proposed_area(&report);
    assert_eq!(area["status"], "needs_policy_review");
    assert_eq!(area["proposed_scope"], json!(["Site/**"]));
    let content = area["instruction"]["proposed_content"].as_str().unwrap();
    assert!(content.contains("applyTo: \"Site/**\"") && content.contains("Function NewHelper()"));
    assert!(
        content.contains("Exceptions and cited rulings")
            && content.contains("read-only health probes need no permission prompt")
    );
    assert!(
        content.contains("../../rules/shared.md"),
        "Reference shared policy instead of copying its complete body"
    );
    assert_eq!(provenance["packet_blake3"], packet["packet_blake3"]);
    assert_eq!(provenance["policy_acceptance"], "not_established");
    for claim in provenance["claims"].as_array().unwrap() {
        let id = claim["claim"]["id"].as_str().unwrap();
        assert!(content.contains(&format!("Provenance {id}")));
        assert!(!claim["sources"].as_array().unwrap().is_empty());
        assert_eq!(claim["acceptance"], "not_established");
    }
    assert_eq!(
        provenance["claims"][4]["explicit_ruling"]["source_scope"]["raw_frontmatter"],
        "---\napplyTo: Site/**\n---\n"
    );
    assert_eq!(report["authored_files_written"], 0);
    for path in [NEW_AREA, NEW_PROVENANCE] {
        assert!(!temporary.path().join("repo").join(path).exists());
    }
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn new_area_draft_rejects_scope_or_selected_evidence_drift_but_is_stable_after_unrelated_work()
 {
    let (temporary, engram, mut request) = area_fixture();
    let packet = call(&engram, request.clone()).await.unwrap();
    request["instruction_maintenance"]["draft"] = area_proposal(&packet);
    let first = call(&engram, request.clone()).await.unwrap();
    std::fs::write(
        temporary.path().join("repo/unrelated.txt"),
        "Unrelated work",
    )
    .unwrap();
    commit(&temporary.path().join("repo"));
    let repeated = call(&engram, request.clone()).await.unwrap();
    assert_eq!(first["new_area_proposals"], repeated["new_area_proposals"]);
    let mut wider = request.clone();
    wider["instruction_maintenance"]["new_instruction_targets"][0]["area_scope"] = json!(["**"]);
    assert!(
        call(&engram, wider)
            .await
            .unwrap_err()
            .message
            .contains("Stale maintenance proposal")
    );
    std::fs::write(
        temporary.path().join("repo/rules/shared.md"),
        "Human changes the shared requirement.\n",
    )
    .unwrap();
    assert!(
        call(&engram, request)
            .await
            .unwrap_err()
            .message
            .contains("Stale maintenance proposal")
    );
    assert!(!temporary.path().join("repo").join(NEW_AREA).exists());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn new_area_draft_preserves_human_created_outputs_and_refuses_previously_authored_targets() {
    for target in [NEW_AREA, NEW_PROVENANCE] {
        let (temporary, engram, mut request) = area_fixture();
        let packet = call(&engram, request.clone()).await.unwrap();
        request["instruction_maintenance"]["draft"] = area_proposal(&packet);
        let path = temporary.path().join("repo").join(target);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "Human-owned file.\n").unwrap();
        assert!(call(&engram, request.clone()).await.is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "Human-owned file.\n"
        );
        let base = commit(&temporary.path().join("repo"));
        std::fs::remove_file(&path).unwrap();
        request["instruction_maintenance"]["base_commit"] = json!(base);
        request["instruction_maintenance"]
            .as_object_mut()
            .unwrap()
            .remove("draft");
        let error = call(&engram, request).await.unwrap_err();
        assert!(error.message.contains("existed at base"), "{error:?}");
        assert!(!path.exists());
        drop(engram);
        temporary.close().unwrap();
    }
}

#[tokio::test]
async fn area_claim_citations_cannot_be_fabricated_and_workflow_status_confers_no_accepted_authority()
 {
    let (temporary, engram, mut request) = area_fixture();
    std::fs::write(temporary.path().join("repo/decision.txt"), "fixed\n").unwrap();
    request["instruction_maintenance"]["evidence_paths"]
        .as_array_mut()
        .unwrap()
        .push(json!("decision.txt"));
    let packet = call(&engram, request.clone()).await.unwrap();
    let mut draft = area_proposal(&packet);
    draft["new_area_files"][0]["claims"][4]["explicit_ruling"] =
        area_citation(&packet, "decision.txt", "after", "fixed");
    request["instruction_maintenance"]["draft"] = draft;
    let report = call(&engram, request.clone()).await.unwrap();
    let (area, provenance) = proposed_area(&report);
    assert_eq!(area["status"], "needs_policy_review");
    let exception = &provenance["claims"][4];
    assert_eq!(exception["ruling_evidence"], "citation_verified");
    assert_eq!(exception["acceptance"], "not_established");
    assert!(
        area["instruction"]["proposed_content"]
            .as_str()
            .unwrap()
            .contains("review required")
    );
    request["instruction_maintenance"]["draft"]["new_area_files"][0]["claims"][0]["evidence"][0]
        ["quote"] = json!("Fabricated permission to remove authorization.");
    assert!(
        call(&engram, request)
            .await
            .unwrap_err()
            .message
            .contains("Evidence quote")
    );
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn observed_policy_violations_and_missing_area_evidence_remain_explicit_proposals_with_gaps()
{
    let (temporary, engram, mut request) = area_fixture();
    std::fs::write(
        temporary.path().join("repo/Site/Helper.vb"),
        "Function NewHelper()\n    Save()\nEnd Function\n",
    )
    .unwrap();
    let packet = call(&engram, request.clone()).await.unwrap();
    let mut draft = area_proposal(&packet);
    draft["new_area_files"][0]["claims"][1]["kind"] = json!("proposed_policy");
    draft["new_area_files"][0]["claims"][1]["text"] =
        json!("Consider omitting the permission check.");
    draft["new_area_files"][0]["unresolved_decisions"] = json!([
        "Observed code conflicts with authored authorization; repair code or obtain an explicit policy decision."
    ]);
    request["instruction_maintenance"]["draft"] = draft;
    let report = call(&engram, request.clone()).await.unwrap();
    let (area, provenance) = proposed_area(&report);
    assert_eq!(area["status"], "needs_evidence");
    assert_eq!(provenance["policy_correctness"], "not_established");
    assert!(
        area["instruction"]["proposed_content"]
            .as_str()
            .unwrap()
            .contains("Proposed policy; unaccepted")
    );
    assert!(area["gaps"].to_string().contains("repair code"));
    request["instruction_maintenance"]["draft"]["new_area_files"][0]["claims"] = json!([
        request["instruction_maintenance"]["draft"]["new_area_files"][0]["claims"][0].clone()
    ]);
    let sparse = call(&engram, request).await.unwrap();
    assert_eq!(sparse["new_area_proposals"][0]["status"], "needs_evidence");
    assert!(
        sparse["new_area_proposals"][0]["gaps"]
            .to_string()
            .contains("House examples")
    );
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    assert!(!temporary.path().join("repo").join(NEW_AREA).exists());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn new_area_outputs_reject_unavailable_parents_and_cross_role_aliases() {
    let (temporary, engram, request) = area_fixture();
    for provenance in [
        NEW_AREA,
        ".github/instructions/SAVES.instructions.md",
        ".github/instructions/saves.instructions.md/sidecar.json",
    ] {
        let mut invalid = request.clone();
        invalid["instruction_maintenance"]["new_instruction_targets"][0]["provenance_path"] =
            json!(provenance);
        assert!(call(&engram, invalid).await.is_err());
    }
    if cfg!(windows) {
        for duplicate_role in ["instruction_path", "provenance_path"] {
            let mut invalid = request.clone();
            let mut other = json!({"instruction_path":".github/instructions/other.instructions.md",
                "provenance_path":".github/instructions/other.provenance.json","area_scope":["Other/**"]});
            other[duplicate_role] = json!(if duplicate_role == "instruction_path" {
                NEW_AREA.to_uppercase()
            } else {
                NEW_PROVENANCE.to_uppercase()
            });
            invalid["instruction_maintenance"]["new_instruction_targets"]
                .as_array_mut()
                .unwrap()
                .push(other);
            assert!(
                call(&engram, invalid)
                    .await
                    .unwrap_err()
                    .message
                    .contains("collide")
            );
        }
    }
    std::fs::write(
        temporary.path().join("repo/.github"),
        "A human file, not a directory",
    )
    .unwrap();
    assert!(call(&engram, request).await.is_err());
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("repo/.github")).unwrap(),
        "A human file, not a directory"
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn deleted_historical_case_aliases_cannot_be_treated_as_new_area_files() {
    for (historical, requested) in [
        (".github/instructions/SAVES.instructions.md", NEW_AREA),
        (".GITHUB/INSTRUCTIONS/saves.instructions.md", NEW_AREA),
        (
            ".github/instructions/SÄVES.instructions.md",
            ".github/instructions/säves.instructions.md",
        ),
    ] {
        let (temporary, engram, mut request) = area_fixture();
        let root = temporary.path().join("repo");
        let old = root.join(historical);
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "Previously authored guidance.\n").unwrap();
        request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
        std::fs::remove_file(&old).unwrap();
        request["instruction_maintenance"]["new_instruction_targets"][0]["instruction_path"] =
            json!(requested);
        let error = call(&engram, request).await.unwrap_err();
        assert!(error.message.contains("existed at base"), "{error:?}");
        assert!(!old.exists() && !root.join(requested).exists());
        assert!(engram.state.projects.is_empty());
        drop(engram);
        temporary.close().unwrap();
    }
}

/// Create a real historical output subtree independently of the worktree. This
/// lets corrupt-object and ambiguous-name cases use ordinary public requests.
fn historical_area_tree(root: &Path, ambiguous: bool) -> (String, std::path::PathBuf) {
    let repo = git2::Repository::open(root).unwrap();
    let previous = repo.head().unwrap().peel_to_commit().unwrap();
    let original = previous.tree().unwrap();
    let blob = repo
        .blob(b"Previously authored instruction evidence.\n")
        .unwrap();
    let mut instructions = repo.treebuilder(None).unwrap();
    instructions
        .insert(
            if ambiguous {
                "saves.instructions.md"
            } else {
                "previous-area.md"
            },
            blob,
            0o100644,
        )
        .unwrap();
    if ambiguous {
        instructions
            .insert("SAVES.instructions.md", blob, 0o100644)
            .unwrap();
    }
    let instructions_id = instructions.write().unwrap();
    let mut github = repo.treebuilder(None).unwrap();
    github
        .insert("instructions", instructions_id, 0o040000)
        .unwrap();
    let mut root_builder = repo.treebuilder(Some(&original)).unwrap();
    root_builder
        .insert(".github", github.write().unwrap(), 0o040000)
        .unwrap();
    let tree = repo.find_tree(root_builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let base = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "Historical area evidence",
            &tree,
            &[&previous],
        )
        .unwrap();
    let hex = instructions_id.to_string();
    let object = repo.path().join("objects").join(&hex[..2]).join(&hex[2..]);
    (base.to_string(), object)
}

#[cfg(windows)]
#[tokio::test]
async fn ambiguous_historical_windows_names_refuse_creation_instead_of_selecting_one_owner() {
    let (temporary, engram, mut request) = area_fixture();
    let (base, _) = historical_area_tree(&temporary.path().join("repo"), true);
    request["instruction_maintenance"]["base_commit"] = json!(base);
    let error = call(&engram, request).await.unwrap_err();
    assert!(
        error.message.contains("Ambiguous historical output path"),
        "{error:?}"
    );
    assert!(!temporary.path().join("repo").join(NEW_AREA).exists());
    drop(engram);
    temporary.close().unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn case_sensitive_windows_directory_cannot_hide_an_untracked_human_output_alias()
-> anyhow::Result<()> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    let (temporary, engram, request) = area_fixture();
    let result = async {
        let root = temporary.path().join("repo");
        let parent = root.join(".github/instructions");
        std::fs::create_dir_all(&parent)?;
        {
            // Change only this empty, newly owned fixture directory. No global
            // filesystem setting, privilege or production folder is modified.
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn SetFileInformationByHandle(
                    file: *mut std::ffi::c_void,
                    class: i32,
                    information: *const std::ffi::c_void,
                    bytes: u32,
                ) -> i32;
            }
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .access_mode(0x100)
                .custom_flags(0x02000000)
                .open(&parent)?;
            let flags: u32 = 1; // FILE_CS_FLAG_CASE_SENSITIVE_DIR
            let enabled = unsafe {
                SetFileInformationByHandle(
                    directory.as_raw_handle(),
                    23,
                    (&flags as *const u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                )
            };
            anyhow::ensure!(
                enabled != 0,
                "Could not enable case sensitivity in owned fixture: {}",
                std::io::Error::last_os_error()
            );
        }
        let human = parent.join("SAVES.instructions.md");
        std::fs::write(&human, "Human-owned untracked guidance.\n")?;
        let response = call(&engram, request).await;
        anyhow::ensure!(
            response.is_err(),
            "A case alias must not enter the new-file proposal flow"
        );
        anyhow::ensure!(std::fs::read_to_string(&human)? == "Human-owned untracked guidance.\n");
        anyhow::ensure!(!root.join(NEW_PROVENANCE).exists());
        anyhow::ensure!(engram.state.projects.is_empty());
        Ok(())
    }
    .await;
    drop(engram);
    let closed = temporary.close();
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
async fn missing_historical_subtree_is_unavailable_evidence_and_restoration_recovers_collection() {
    let (temporary, engram, mut request) = area_fixture();
    let root = temporary.path().join("repo");
    let (base, object) = historical_area_tree(&root, false);
    request["instruction_maintenance"]["base_commit"] = json!(base);
    let bytes = std::fs::read(&object).unwrap();
    let permissions = std::fs::metadata(&object).unwrap().permissions();
    #[cfg(windows)]
    if permissions.readonly() {
        let mut writable = permissions.clone();
        writable.set_readonly(false);
        std::fs::set_permissions(&object, writable).unwrap();
    }
    // This is a newly-created object in this test's own repository. No
    // production or policy-held fixture tree is addressed.
    std::fs::remove_file(&object).unwrap();
    let unavailable = call(&engram, request.clone()).await;
    let restored = std::fs::write(&object, bytes);
    restored.unwrap();
    std::fs::set_permissions(&object, permissions).unwrap();
    let error = unavailable.unwrap_err();
    assert!(
        error
            .message
            .contains("Historical output parent unavailable"),
        "{error:?}"
    );
    let recovered = call(&engram, request).await.unwrap();
    assert_eq!(recovered["status"], "draft_required");
    assert_eq!(
        recovered["packet"]["new_instruction_targets"][0]["before"],
        "absent_at_requested_commit"
    );
    assert!(!root.join(NEW_AREA).exists() && !root.join(NEW_PROVENANCE).exists());
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn missing_historical_input_subtree_cannot_fabricate_before_absence_for_current_guidance() {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    let (base, object) = historical_area_tree(&root, false);
    let evidence = ".github/instructions/previous-area.md";
    std::fs::create_dir_all(root.join(".github/instructions")).unwrap();
    std::fs::write(root.join(evidence), "Current human guidance.\n").unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(base);
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb", evidence]);
    let bytes = std::fs::read(&object).unwrap();
    let permissions = std::fs::metadata(&object).unwrap().permissions();
    #[cfg(windows)]
    if permissions.readonly() {
        let mut writable = permissions.clone();
        writable.set_readonly(false);
        std::fs::set_permissions(&object, writable).unwrap();
    }
    std::fs::remove_file(&object).unwrap();
    let unavailable = call(&engram, request.clone()).await;
    std::fs::write(&object, bytes).unwrap();
    std::fs::set_permissions(&object, permissions).unwrap();
    let error = unavailable.unwrap_err();
    assert!(
        error
            .message
            .contains("Historical input parent unavailable"),
        "{error:?}"
    );
    let recovered = call(&engram, request).await.unwrap();
    let input = recovered["packet"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == evidence)
        .unwrap();
    assert_eq!(
        input["before"]["content"],
        "Previously authored instruction evidence.\n"
    );
    assert_eq!(input["after"]["content"], "Current human guidance.\n");
    assert_eq!(
        std::fs::read_to_string(root.join(evidence)).unwrap(),
        "Current human guidance.\n"
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temporary.close().unwrap();
}

fn update_fixture() -> (tempfile::TempDir, Engram, Value) {
    let (temporary, engram, mut request) = fixture();
    let root = temporary.path().join("repo");
    std::fs::write(
        root.join("Site/helper.rs"),
        "pub fn OldHelper() -> usize { 1 }\n",
    )
    .unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["evidence_paths"] =
        json!(["Site/helper.rs", "rules/shared.md"]);
    std::fs::write(
        root.join("Site/helper.rs"),
        "pub fn NewHelper() -> usize { 2 }\n",
    )
    .unwrap();
    let mut project = engram
        .state
        .registry
        .get_project("maintenance")
        .unwrap()
        .unwrap();
    project.project_type = "rust".into();
    engram.state.registry.put_project(&project).unwrap();
    request["wait"] = json!(true);
    request["max_commits"] = json!(1);
    (temporary, engram, request)
}

async fn updated_source_is_visible(engram: &Engram) {
    let result = engram
        .handle_search_memory(
            serde_json::from_value(json!({
                "project_id":"maintenance","query":"NewHelper","max_results":3
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let result = result
        .content
        .iter()
        .filter_map(|item| item.as_text())
        .map(|item| item.text.as_str())
        .collect::<String>();
    assert!(
        result.contains("Site/helper.rs") && result.contains("pub fn NewHelper()"),
        "The source update must actually publish the edited declaration"
    );
}

#[tokio::test]
async fn instruction_observation_after_update_and_noop_preserves_source_and_authored_policy() {
    let (temporary, engram, request) = update_fixture();
    let policy_path = temporary.path().join("repo/rules/area.md");
    let policy_before = std::fs::read(&policy_path).unwrap();
    for expected_noop in [false, true] {
        let response = engram
            .handle_update_project(serde_json::from_value(request.clone()).unwrap())
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true));
        let source = &response.content[0].as_text().unwrap().text;
        assert!(source.contains("status: done"));
        assert_eq!(
            source.contains("source_update: no_changes_detected"),
            expected_noop
        );
        let report: Value =
            serde_json::from_str(&response.content.last().unwrap().as_text().unwrap().text)
                .unwrap();
        let observation = &report["instruction_reference_observation"];
        assert_eq!(observation["status"], "observed");
        assert_eq!(
            observation["basis"],
            "separate_worktree_observation_collected_after_source_update"
        );
        assert_eq!(observation["atomic_index_and_policy_snapshot"], false);
        assert_eq!(observation["authored_files_written"], 0);
        assert_eq!(
            observation["manifest"]["instructions"][0]["instruction_path"],
            "rules/area.md"
        );
        updated_source_is_visible(&engram).await;
        assert_eq!(std::fs::read(&policy_path).unwrap(), policy_before);
    }
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn instruction_observation_failure_keeps_completed_source_update_successful() {
    let (temporary, engram, mut request) = update_fixture();
    request["instruction_maintenance"]["instruction_paths"] = json!(["rules/not-present.md"]);
    let response = engram
        .handle_update_project(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    assert_ne!(response.is_error, Some(true));
    assert!(
        response.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("status: done")
    );
    let report: Value =
        serde_json::from_str(&response.content.last().unwrap().as_text().unwrap().text).unwrap();
    let observation = &report["instruction_reference_observation"];
    assert_eq!(observation["status"], "unavailable");
    assert!(
        observation["reason"]
            .as_str()
            .unwrap()
            .contains("not-present.md")
    );
    assert_eq!(observation["authored_files_written"], 0);
    assert!(observation.get("manifest").is_none());
    updated_source_is_visible(&engram).await;
    assert_eq!(
        std::fs::read_to_string(temporary.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    drop(engram);
    temporary.close().unwrap();
}

#[tokio::test]
async fn instruction_observation_invalid_requests_refuse_before_update_or_job_creation() {
    for case in ["async", "draft", "path", "commit"] {
        let (temporary, engram, mut request) = update_fixture();
        let epoch = engram
            .state
            .registry
            .source_identity_epoch("maintenance")
            .unwrap();
        match case {
            "async" => request["wait"] = json!(false),
            "draft" => {
                request["instruction_maintenance"]["draft"] =
                    json!({"expected_packet_blake3":"unused","decisions":[]})
            }
            "path" => {
                request["instruction_maintenance"]["instruction_paths"] = json!(["../outside.md"])
            }
            _ => request["instruction_maintenance"]["base_commit"] = json!("not-a-commit"),
        }
        let result = engram
            .handle_update_project(serde_json::from_value(request).unwrap())
            .await;
        assert!(
            result.is_err(),
            "Invalid optional observation must refuse before source work: {case}"
        );
        assert_eq!(
            engram
                .state
                .registry
                .source_identity_epoch("maintenance")
                .unwrap(),
            epoch
        );
        assert!(
            !temporary.path().join("data/projects/maintenance").exists(),
            "Rejected requests must not initialize the project runtime"
        );
        assert_eq!(
            std::fs::read_to_string(temporary.path().join("repo/rules/area.md")).unwrap(),
            POLICY
        );
        drop(engram);
        temporary.close().unwrap();
    }
}

fn draft(packet: &Value, outcome: &str) -> Value {
    let evidence = &packet["packet"]["evidence"][0];
    json!({"expected_packet_blake3":packet["packet_blake3"],"decisions":[{
        "instruction_path":"rules/area.md","outcome":outcome,
        "rationale":"The helper was renamed while its permission check and save order stayed the same; update only its reference.",
        "evidence":[
            {"path":"Site/Helper.vb","version":"before","blake3":evidence["before"]["blake3"],"quote":BEFORE},
            {"path":"Site/Helper.vb","version":"after","blake3":evidence["after"]["blake3"],"quote":AFTER}
        ],
        "edits": if outcome=="change" {json!([{"old_text":"OldHelper","new_text":"NewHelper","reason":"Exact behavior-preserving helper rename."}])} else {json!([])},
        "preserved_fragments":["Exception: read-only health probes need no permission prompt.","Keep the [shared rule](shared.md)."],
        "unresolved_decisions":[]
    }]})
}

#[tokio::test]
async fn helper_rename_returns_grounded_diff_preserving_scope_exception_and_shared_rule_without_writes()
 {
    let (temp, engram, mut request) = fixture();
    let packet = call(&engram, request.clone()).await.unwrap();
    assert_eq!(
        packet["packet"]["references"][0]["status"],
        "selected_complete_current_content"
    );
    request["instruction_maintenance"]["draft"] = draft(&packet, "change");
    let reviewed = call(&engram, request).await.unwrap();
    assert_eq!(reviewed["status"], "proposal_requires_semantic_review");
    assert_eq!(reviewed["authored_files_written"], 0);
    assert_eq!(
        reviewed["proposals"][0]["proposed_content"],
        POLICY.replace("OldHelper", "NewHelper")
    );
    let diff = reviewed["proposals"][0]["unified_diff"].as_str().unwrap();
    assert!(diff.contains("-Use OldHelper") && diff.contains("+Use NewHelper"));
    assert_eq!(
        std::fs::read_to_string(temp.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    assert!(
        engram.state.projects.is_empty(),
        "maintenance must not open native index runtimes"
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn changed_guidance_or_selected_evidence_rejects_the_old_proposal_and_preserves_human_text() {
    for path in ["rules/area.md", "Site/Helper.vb", "rules/shared.md"] {
        let (temp, engram, mut request) = fixture();
        let packet = call(&engram, request.clone()).await.unwrap();
        request["instruction_maintenance"]["draft"] = draft(&packet, "change");
        let file = temp.path().join("repo").join(path);
        let human = format!(
            "{}Human amendment.\n",
            std::fs::read_to_string(&file).unwrap()
        );
        std::fs::write(&file, &human).unwrap();
        let error = call(&engram, request).await.unwrap_err();
        assert!(error.message.contains("Stale maintenance proposal"));
        assert_eq!(std::fs::read_to_string(file).unwrap(), human);
        drop(engram);
        temp.close().unwrap();
    }
}

#[tokio::test]
async fn unrelated_commit_does_not_invalidate_an_unchanged_dependency_set() {
    let (temp, engram, mut request) = fixture();
    let packet = call(&engram, request.clone()).await.unwrap();
    request["instruction_maintenance"]["draft"] = draft(&packet, "change");
    std::fs::write(temp.path().join("repo/unrelated.txt"), "Unrelated change").unwrap();
    commit(&temp.path().join("repo"));
    assert!(call(&engram, request).await.is_ok());
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn no_change_and_code_policy_conflict_are_explicit_review_outcomes_without_policy_edits() {
    let (temp, engram, request) = fixture();
    let packet = call(&engram, request.clone()).await.unwrap();
    for outcome in ["no_change", "conflict", "needs_evidence"] {
        let mut req = request.clone();
        let mut proposal = draft(&packet, outcome);
        proposal["decisions"][0]["rationale"] = json!(match outcome {
            "no_change" =>
                "The rename does not alter the per-action authorization invariant or its read-only exception.",
            "conflict" =>
                "A proposed bypass would violate accepted authorization; repair the implementation rather than weakening the rule.",
            _ =>
                "Policy intent is not established; obtain the accepted decision before drafting new mandates.",
        });
        req["instruction_maintenance"]["draft"] = proposal;
        let reviewed = call(&engram, req).await.unwrap();
        assert_eq!(reviewed["proposals"][0]["outcome"], outcome);
        assert_eq!(reviewed["proposals"][0]["unified_diff"], "");
        assert_eq!(reviewed["proposals"][0]["proposed_content"], POLICY);
    }
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn fabricated_quotes_removed_exceptions_and_overlapping_or_missing_decisions_are_refused() {
    let (temp, engram, request) = fixture();
    let packet = call(&engram, request.clone()).await.unwrap();
    for kind in [
        "quote",
        "hash",
        "exception",
        "overlap",
        "duplicate",
        "missing",
    ] {
        let mut req = request.clone();
        let mut proposal = draft(&packet, "change");
        match kind {
            "quote" => proposal["decisions"][0]["evidence"][0]["quote"] = json!("InventedPermissionCheck()"),
            "hash" => proposal["decisions"][0]["evidence"][0]["blake3"] = json!("0".repeat(64)),
            "exception" => proposal["decisions"][0]["edits"] = json!([{"old_text":"Exception: read-only health probes need no permission prompt.","new_text":"Always prompt.","reason":"Invalid policy weakening."}]),
            "overlap" => proposal["decisions"][0]["edits"].as_array_mut().unwrap().push(json!({"old_text":"Use OldHelper","new_text":"Use OtherHelper","reason":"Overlapping edit."})),
            "duplicate" => { let first=proposal["decisions"][0].clone(); proposal["decisions"].as_array_mut().unwrap().push(first); },
            _ => proposal["decisions"] = json!([]),
        }
        req["instruction_maintenance"]["draft"] = proposal;
        assert!(call(&engram, req).await.is_err(), "invalid {kind} accepted");
    }
    assert_eq!(
        std::fs::read_to_string(temp.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn linked_shared_guidance_is_supplied_and_oversized_authored_guidance_is_never_truncated() {
    let (temp, engram, mut request) = fixture();
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let packet = call(&engram, request.clone()).await.unwrap();
    assert_eq!(packet["packet"]["references"][0]["target"], "shared.md");
    assert_eq!(
        packet["packet"]["references"][0]["status"],
        "selected_complete_current_content"
    );
    assert_eq!(
        packet["packet"]["instruction_manifest"]["dependencies"][0]["path"],
        "rules/shared.md"
    );
    assert_eq!(
        packet["packet"]["instruction_manifest"]["dependencies"][0]["after"]["snapshot"]["content"],
        "Retain per-action authorization.\n"
    );
    std::fs::write(
        temp.path().join("repo/rules/area.md"),
        "x".repeat(128 * 1024 + 1),
    )
    .unwrap();
    assert!(
        call(&engram, request)
            .await
            .unwrap_err()
            .message
            .contains("byte budget")
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn overlapping_anchor_occurrences_cannot_select_an_arbitrary_instruction_edit() {
    let (temp, engram, mut request) = fixture();
    std::fs::write(temp.path().join("repo/rules/area.md"), "aaa\n").unwrap();
    let packet = call(&engram, request.clone()).await.unwrap();
    let mut proposal = draft(&packet, "change");
    proposal["decisions"][0]["preserved_fragments"] = json!([]);
    proposal["decisions"][0]["edits"] = json!([
        {"old_text":"aa","new_text":"bb","reason":"Ambiguous repeated original anchor."}
    ]);
    request["instruction_maintenance"]["draft"] = proposal;
    assert!(
        call(&engram, request)
            .await
            .unwrap_err()
            .message
            .contains("exactly once")
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("repo/rules/area.md")).unwrap(),
        "aaa\n"
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn oversized_historical_evidence_is_refused_even_when_current_code_is_small() {
    let (temp, engram, mut request) = fixture();
    let root = temp.path().join("repo");
    std::fs::write(root.join("Site/Helper.vb"), "x".repeat(128 * 1024 + 1)).unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    std::fs::write(root.join("Site/Helper.vb"), AFTER).unwrap();
    let error = call(&engram, request).await.unwrap_err();
    assert!(
        error
            .message
            .contains("Complete committed evidence exceeds byte budget")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("Site/Helper.vb")).unwrap(),
        AFTER
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
        POLICY
    );
    assert!(engram.state.projects.is_empty());
    drop(engram);
    temp.close().unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn project_junction_cannot_expose_outside_instruction_content() {
    use std::os::windows::process::CommandExt;
    let (temp, engram, mut request) = fixture();
    // Both sides belong to this fixture, but only repo is the allowed project.
    let outside = temp.path().join("private");
    let junction = temp.path().join("repo").join("escape");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("secret.md"),
        "Outside content must not be returned",
    )
    .unwrap();
    let output = std::process::Command::new("C:\\Windows\\System32\\cmd.exe")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Could not create owned junction fixture: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    request["instruction_maintenance"]["instruction_paths"] = json!(["escape/secret.md"]);
    let result = call(&engram, request.clone()).await;
    request["instruction_maintenance"]["instruction_paths"] = json!([]);
    request["instruction_maintenance"]["new_instruction_targets"] = json!([{
        "instruction_path":"escape/new.md", "provenance_path":"escape/new.json", "area_scope":["Site/**"]
    }]);
    let creation_result = call(&engram, request).await;
    let outside_unchanged = !outside.join("new.md").exists() && !outside.join("new.json").exists();
    // Remove just this empty junction link, never its target tree.
    let removed = std::fs::remove_dir(&junction);
    drop(engram);
    let closed = temp.close();
    let error = result.unwrap_err();
    assert!(
        error.message.contains("symlink") || error.message.contains("reparse"),
        "{error}"
    );
    assert!(
        !error
            .message
            .contains("Outside content must not be returned")
    );
    removed.unwrap();
    closed.unwrap();
    let creation_error = creation_result.unwrap_err();
    assert!(
        creation_error.message.contains("symlink") || creation_error.message.contains("reparse"),
        "{creation_error}"
    );
    assert!(outside_unchanged);
}

#[tokio::test]
async fn historical_links_use_their_own_tree_and_cycles_do_not_expand_edit_targets() {
    let (temp, engram, mut request) = fixture();
    let root = temp.path().join("repo");
    std::fs::write(
        root.join("rules/shared.md"),
        "Historical [detail](old.md).\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rules/old.md"),
        "Historical-only rule. [cycle](shared.md)\n",
    )
    .unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    std::fs::write(
        root.join("rules/shared.md"),
        "Current [detail](current.md).\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rules/current.md"),
        "Current rule. [cycle](shared.md)\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("rules/old.md")).unwrap();
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let report = call(&engram, request).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    assert_eq!(
        report["packet"]["instructions"].as_array().unwrap().len(),
        1,
        "Linked guidance is context, never an implicit edit target"
    );
    assert_eq!(manifest["dependencies"].as_array().unwrap().len(), 3);
    let old = manifest["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == "rules/old.md")
        .unwrap();
    assert!(
        old["before"]["snapshot"]["content"]
            .as_str()
            .unwrap()
            .contains("Historical-only")
    );
    assert_eq!(old["after"]["status"], "absent");
    let edges = manifest["edges"].as_array().unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e["version"] == "before" && e["normalized_path"] == "rules/old.md")
    );
    assert!(
        !edges
            .iter()
            .any(|e| e["version"] == "after" && e["normalized_path"] == "rules/old.md")
    );
    assert!(
        edges
            .iter()
            .any(|e| e["version"] == "after" && e["normalized_path"] == "rules/current.md")
    );
    assert!(
        !edges
            .iter()
            .any(|e| e["version"] == "before" && e["normalized_path"] == "rules/current.md")
    );
    assert!(
        edges.len() < 10,
        "Cycles must terminate by version and path"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
        POLICY
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn only_roots_reaching_changed_guidance_need_review_and_scope_bytes_survive() {
    let (temp, engram, mut request) = fixture();
    let root = temp.path().join("repo");
    let scoped = "\u{feff}---\r\napplyTo: Site/Alpha/**\r\n---\r\nÅ rule [shared](x.md).\r\n";
    // Exercise Git's Windows clean conversion without changing host settings.
    git2::Repository::open(&root)
        .unwrap()
        .config()
        .unwrap()
        .set_bool("core.autocrlf", true)
        .unwrap();
    std::fs::write(root.join("rules/area.md"), scoped).unwrap();
    std::fs::write(root.join("rules/other.md"), "[shared](y.md)\n").unwrap();
    std::fs::write(root.join("rules/x.md"), "Stable policy.\n").unwrap();
    std::fs::write(root.join("rules/y.md"), "Old policy evidence.\n").unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    request["instruction_maintenance"]["instruction_paths"] =
        json!(["rules/area.md", "rules/other.md"]);
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    std::fs::write(
        root.join("rules/y.md"),
        "Changed policy evidence, still requires review.\n",
    )
    .unwrap();
    std::fs::write(root.join("unrelated.txt"), "Unrelated body change.\n").unwrap();
    let report = call(&engram, request.clone()).await.unwrap();
    let manifest = &report["packet"]["instruction_manifest"];
    let states = manifest["instructions"].as_array().unwrap();
    let alpha = states
        .iter()
        .find(|s| s["instruction_path"] == "rules/area.md")
        .unwrap();
    let other = states
        .iter()
        .find(|s| s["instruction_path"] == "rules/other.md")
        .unwrap();
    assert!(alpha["dependent_changes"].as_array().unwrap().is_empty());
    assert_eq!(alpha["status"], "line_ending_differences_only");
    assert_eq!(alpha["line_ending_differences"][0]["path"], "rules/area.md");
    assert_ne!(
        alpha["line_ending_differences"][0]["before_blake3"],
        alpha["line_ending_differences"][0]["after_blake3"],
        "Raw evidence hashes must remain exact through Git clean conversion"
    );
    assert_eq!(other["dependent_changes"][0]["path"], "rules/y.md");
    assert_eq!(other["status"], "review_changed_dependencies");
    assert_eq!(
        alpha["scope"]["raw_frontmatter"],
        "\u{feff}---\r\napplyTo: Site/Alpha/**\r\n---\r\n"
    );
    let link = manifest["edges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["source_path"] == "rules/area.md" && e["version"] == "after")
        .unwrap();
    assert_eq!(
        link["byte_offset"].as_u64().unwrap() as usize,
        scoped.find("[shared]").unwrap()
    );
    commit(&root);
    assert_eq!(
        call(&engram, request).await.unwrap()["packet_blake3"],
        report["packet_blake3"],
        "An incidental HEAD advance must not invalidate the same requested dependency basis"
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn automatically_supplied_dependency_changes_reject_stale_drafts_without_editing_policy() {
    let (temp, engram, mut request) = fixture();
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let report = call(&engram, request.clone()).await.unwrap();
    request["instruction_maintenance"]["draft"] = draft(&report, "change");
    std::fs::write(
        temp.path().join("repo/rules/shared.md"),
        "Human changed the linked authorization rule.\n",
    )
    .unwrap();
    assert!(
        call(&engram, request)
            .await
            .unwrap_err()
            .message
            .contains("Stale maintenance proposal")
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn move_candidates_disclose_duplicate_sources_destinations_copies_and_changed_bytes() {
    for case in [
        "unique",
        "two_sources",
        "two_destinations",
        "copy",
        "changed_bytes",
    ] {
        let (temp, engram, mut request) = fixture();
        let root = temp.path().join("repo");
        let body = std::fs::read(root.join("rules/shared.md")).unwrap();
        if case == "two_sources" {
            std::fs::write(root.join("rules/old-copy.md"), &body).unwrap();
        }
        request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
        std::fs::write(
            root.join("rules/new.md"),
            if case == "changed_bytes" {
                b"Changed policy bytes.\n".as_slice()
            } else {
                body.as_slice()
            },
        )
        .unwrap();
        if case != "copy" {
            std::fs::remove_file(root.join("rules/shared.md")).unwrap();
        }
        let mut selected = vec!["Site/Helper.vb", "rules/new.md"];
        if case == "two_sources" {
            std::fs::remove_file(root.join("rules/old-copy.md")).unwrap();
            selected.push("rules/old-copy.md");
        }
        if case == "two_destinations" {
            std::fs::write(root.join("rules/another.md"), &body).unwrap();
            selected.push("rules/another.md");
        }
        request["instruction_maintenance"]["evidence_paths"] = json!(selected);
        let report = call(&engram, request).await.unwrap();
        let manifest = &report["packet"]["instruction_manifest"];
        let moves = manifest["move_candidates"].as_array().unwrap();
        if matches!(case, "copy" | "changed_bytes") {
            assert!(moves.is_empty(), "False move: {case}");
        } else {
            let candidate = moves
                .iter()
                .find(|m| m["old_path"] == "rules/shared.md")
                .unwrap();
            assert_eq!(
                candidate["status"],
                if case == "unique" {
                    "exact_content_move_candidate"
                } else {
                    "ambiguous_exact_content_matches"
                }
            );
            assert!(
                manifest["edges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["version"] == "after"
                        && e["normalized_path"] == "rules/shared.md"
                        && e["status"] == "absent"),
                "A candidate must not silently repair the missing original reference"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
            POLICY
        );
        drop(engram);
        temp.close().unwrap();
    }
}

#[tokio::test]
async fn reference_limits_and_unsupported_syntax_are_named_without_truncating_guidance() {
    for case in ["depth", "documents", "edges", "syntax"] {
        let (temp, engram, mut request) = fixture();
        let root = temp.path().join("repo");
        request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
        let addition = match case {
            "depth" => {
                for n in 0..5 { std::fs::write(root.join(format!("rules/depth{n}.md")), format!("[next](depth{}.md)\n", n+1)).unwrap(); }
                "[chain](depth0.md)\n".to_owned()
            },
            "documents" => (0..20).map(|n| format!("[missing{n}](missing{n}.md)\n")).collect::<String>(),
            "edges" => "[same](shared.md)\n".repeat(300),
            _ => "[shared][rule]\n\n[rule]: shared.md\n\n[missing][no-definition]\n[[wiki]]\n[spaced](shared.md \"title\")\n[remote](https://example.invalid)\n[outside](../../outside.md)\n[symbol](shared.md#not-a-verified-symbol)\n".into(),
        };
        let policy = format!("{POLICY}{addition}");
        std::fs::write(root.join("rules/area.md"), &policy).unwrap();
        let report = call(&engram, request).await.unwrap();
        assert_eq!(
            report["packet"]["instructions"][0]["current"]["content"],
            policy
        );
        let manifest = &report["packet"]["instruction_manifest"];
        assert!(manifest["dependencies"].as_array().unwrap().len() <= 16);
        assert!(manifest["edges"].as_array().unwrap().len() <= 256);
        assert_eq!(
            manifest["instructions"][0]["coverage_status"],
            "incomplete_reference_coverage"
        );
        assert_eq!(
            manifest["instructions"][0]["status"], "review_changed_dependencies",
            "Known changes and incomplete coverage must both remain visible"
        );
        let encoded = serde_json::to_string(manifest).unwrap();
        assert!(
            encoded.contains(match case {
                "depth" => "depth_budget_omitted",
                "documents" => "dependency_budget_omitted",
                "edges" => "edge_budget_omitted",
                _ => "unresolved_reference_label",
            }),
            "Gap not disclosed: {case}"
        );
        if case == "syntax" {
            assert!(encoded.contains("wiki_reference_syntax_unverified"));
            assert!(encoded.contains("external_reference_unverified"));
            assert!(encoded.contains("outside_project_or_unsupported"));
            assert!(encoded.contains("content_complete_fragment_unverified"));
        }
        assert_eq!(
            std::fs::read_to_string(root.join("rules/area.md")).unwrap(),
            policy
        );
        drop(engram);
        temp.close().unwrap();
    }
}

#[tokio::test]
async fn oversized_auto_dependency_is_omitted_explicitly_and_cannot_be_cited_as_absence() {
    let (temp, engram, mut request) = fixture();
    let root = temp.path().join("repo");
    std::fs::write(
        root.join("rules/area.md"),
        format!("{POLICY}[large](large.md)\n"),
    )
    .unwrap();
    std::fs::write(root.join("rules/large.md"), "x".repeat(128 * 1024 + 1)).unwrap();
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let report = call(&engram, request.clone()).await.unwrap();
    let dependency = report["packet"]["instruction_manifest"]["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == "rules/large.md")
        .unwrap();
    assert_eq!(dependency["before"]["status"], "absent");
    assert_eq!(dependency["after"]["status"], "byte_budget_omitted");
    assert!(dependency["after"]["snapshot"].is_null());
    let mut proposal = draft(&report, "needs_evidence");
    proposal["decisions"][0]["evidence"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path":"rules/large.md", "version":"after", "blake3":null,"quote":null}));
    request["instruction_maintenance"]["draft"] = proposal;
    assert!(
        call(&engram, request)
            .await
            .unwrap_err()
            .message
            .contains("not proof of absence")
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn project_health_reports_optional_reference_manifest_separately_from_storage_verdict() {
    let (temp, engram, request) = fixture();
    let report = call(&engram, request.clone()).await.unwrap();
    let health = engram
        .handle_project_health_with_instructions(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    let observation: Value =
        serde_json::from_str(&health.content.last().unwrap().as_text().unwrap().text).unwrap();
    assert_eq!(
        observation["instruction_reference_observation"]["packet_blake3"],
        report["packet_blake3"]
    );
    assert_eq!(
        observation["instruction_reference_observation"]["policy_correctness"],
        "not_established_by_reference_observation"
    );
    assert_eq!(
        observation["instruction_reference_observation"]["authored_files_written"],
        0
    );
    assert_eq!(
        observation["instruction_reference_observation"]["basis"],
        "separate_worktree_observation_collected_after_storage_health"
    );
    assert_eq!(
        observation["instruction_reference_observation"]["atomic_index_and_policy_snapshot"],
        false
    );
    assert_eq!(
        observation["instruction_reference_observation"]["manifest"]["instructions"][0]["instruction_path"],
        "rules/area.md"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("repo/rules/area.md")).unwrap(),
        POLICY
    );
    drop(engram);
    temp.close().unwrap();
}

#[tokio::test]
async fn one_roots_edge_budget_exhaustion_cannot_report_another_root_as_verified_unchanged() {
    let (temp, engram, mut request) = fixture();
    let root = temp.path().join("repo");
    std::fs::write(
        root.join("rules/area.md"),
        "[same](shared.md)\n".repeat(300),
    )
    .unwrap();
    std::fs::write(root.join("rules/z.md"), "[changing](changed.md)\n").unwrap();
    std::fs::write(root.join("rules/changed.md"), "Before.\n").unwrap();
    request["instruction_maintenance"]["base_commit"] = json!(commit(&root));
    std::fs::write(root.join("rules/changed.md"), "After.\n").unwrap();
    request["instruction_maintenance"]["instruction_paths"] =
        json!(["rules/area.md", "rules/z.md"]);
    request["instruction_maintenance"]["evidence_paths"] = json!(["Site/Helper.vb"]);
    let report = call(&engram, request).await.unwrap();
    let state = report["packet"]["instruction_manifest"]["instructions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["instruction_path"] == "rules/z.md")
        .unwrap();
    assert_eq!(state["coverage_status"], "incomplete_reference_coverage");
    assert_eq!(state["status"], "needs_reference_evidence");
    assert!(
        !state["coverage_issue_indices"]
            .as_array()
            .unwrap()
            .is_empty(),
        "Unvisited references after exhaustion must leave a root-scoped coverage gap"
    );
    drop(engram);
    temp.close().unwrap();
}
