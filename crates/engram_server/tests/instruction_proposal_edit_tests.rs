//! Edit explanations and offsets must survive the complete read-only proposal flow.
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
const GUIDE: &str = "---\napplyTo: Site/**\n---\n# Behörighet\nUse OldHelper for authorized saves.\nKeep the prior lookup name.\nException: previews do not save.\n";
const BEFORE: &str = "export function OldHelper() { return true; }\n";
const AFTER: &str = "export function NewReviewedHelper() { return true; }\n";
struct Fixture {
    root: PathBuf,
    request: MaintenanceRequest,
    _temp: tempfile::TempDir,
}
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
fn fixture() -> Fixture {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("")
        .rand_bytes(2)
        .tempdir_in(fixture_root)
        .unwrap();
    let root = temp.path().join("r");
    std::fs::create_dir_all(root.join("rules")).unwrap();
    std::fs::create_dir(root.join("Site")).unwrap();
    drop(git2::Repository::init(&root).unwrap());
    std::fs::write(root.join("rules/area.md"), GUIDE).unwrap();
    std::fs::write(root.join("Site/helper.ts"), BEFORE).unwrap();
    let base = commit(&root);
    std::fs::write(root.join("Site/helper.ts"), AFTER).unwrap();
    let request=serde_json::from_value(json!({"instruction_paths":["rules/area.md"],"evidence_paths":["Site/helper.ts"],"base_commit":base})).unwrap();
    Fixture {
        root,
        request,
        _temp: temp,
    }
}
fn draft(report: &maintenance::Report) -> Value {
    let guide = &report.packet.instructions[0];
    let code = &report.packet.evidence[0];
    json!({"expected_packet_blake3":report.packet_blake3,"decisions":[{"instruction_path":"rules/area.md","outcome":"change","rationale":"Review helper references while preserving scope and exceptions.","edits":[
 {"old_text":"prior lookup name","new_text":"documented helper label","reason":"Explain the later label independently."},
 {"old_text":"OldHelper","new_text":"NewReviewedHelper","reason":"Explain the earlier helper rename independently."}],
 "evidence":[{"path":"rules/area.md","version":"before","blake3":guide.before.as_ref().unwrap().blake3,"quote":GUIDE},{"path":"rules/area.md","version":"after","blake3":guide.current.blake3,"quote":GUIDE},{"path":"Site/helper.ts","version":"before","blake3":code.before.as_ref().unwrap().blake3,"quote":BEFORE},{"path":"Site/helper.ts","version":"after","blake3":code.after.as_ref().unwrap().blake3,"quote":AFTER}],
 "preserved_fragments":["applyTo: Site/**","Exception: previews do not save."]}]})
}
#[test]
fn proposal_retains_each_reason_with_original_input_identity_and_exact_utf8_ranges() {
    let mut f = fixture();
    let collected = maintenance::review(&f.root, "edit-reasons", &f.request).unwrap();
    let input = draft(&collected);
    f.request.draft = Some(serde_json::from_value(input.clone()).unwrap());
    let reviewed = maintenance::review(&f.root, "edit-reasons", &f.request).unwrap();
    let proposal = &reviewed.proposals[0];
    assert_eq!(proposal.edit_previews.len(), 2);
    assert_eq!(
        proposal
            .edit_previews
            .iter()
            .map(|edit| edit.edit_index)
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
    for edit in &proposal.edit_previews {
        let original = &input["decisions"][0]["edits"][edit.edit_index - 1];
        assert_eq!(edit.reason, original["reason"].as_str().unwrap());
        assert_eq!(
            &GUIDE[edit.original_byte_range[0]..edit.original_byte_range[1]],
            original["old_text"].as_str().unwrap()
        );
        assert_eq!(
            &proposal.proposed_content[edit.proposed_byte_range[0]..edit.proposed_byte_range[1]],
            original["new_text"].as_str().unwrap()
        );
    }
    assert_eq!(
        proposal.expected_source_blake3,
        collected.packet.instructions[0].current.blake3
    );
    assert_eq!(
        proposal.proposed_blake3,
        blake3::hash(proposal.proposed_content.as_bytes())
            .to_hex()
            .to_string()
    );
    let again = maintenance::review(&f.root, "edit-reasons", &f.request).unwrap();
    assert_eq!(
        serde_json::to_value(&reviewed.proposals).unwrap(),
        serde_json::to_value(&again.proposals).unwrap()
    );
    assert_eq!(reviewed.authored_files_written, 0);
    assert_eq!(
        std::fs::read_to_string(f.root.join("rules/area.md")).unwrap(),
        GUIDE
    );
}
#[test]
fn empty_edits_and_deletion_previews_keep_existing_stale_and_exception_checks() {
    let mut f = fixture();
    let collected = maintenance::review(&f.root, "edit-reasons", &f.request).unwrap();
    let mut input = draft(&collected);
    for outcome in ["no_change", "conflict", "needs_evidence"] {
        let mut unchanged = input.clone();
        unchanged["decisions"][0]["outcome"] = json!(outcome);
        unchanged["decisions"][0]["edits"] = json!([]);
        f.request.draft = Some(serde_json::from_value(unchanged).unwrap());
        let report = maintenance::review(&f.root, "edit-reasons", &f.request).unwrap();
        assert!(report.proposals[0].edit_previews.is_empty());
        assert!(report.proposals[0].unified_diff.is_empty());
    }
    input["decisions"][0]["edits"][0]["new_text"] = json!("");
    f.request.draft = Some(serde_json::from_value(input.clone()).unwrap());
    let removed = maintenance::review(&f.root, "edit-reasons", &f.request).unwrap();
    let deletion = removed.proposals[0]
        .edit_previews
        .iter()
        .find(|edit| edit.edit_index == 1)
        .unwrap();
    assert_eq!(
        deletion.proposed_byte_range[0],
        deletion.proposed_byte_range[1]
    );
    let mut invalid = input.clone();
    invalid["decisions"][0]["edits"] = json!([{"old_text":"Exception: previews do not save.","new_text":"","reason":"This must still be refused."}]);
    f.request.draft = Some(serde_json::from_value(invalid).unwrap());
    assert!(
        maintenance::review(&f.root, "edit-reasons", &f.request)
            .unwrap_err()
            .to_string()
            .contains("exception/condition")
    );
    f.request.draft = Some(serde_json::from_value(input).unwrap());
    std::fs::write(
        f.root.join("Site/helper.ts"),
        format!("{AFTER}// changed source\n"),
    )
    .unwrap();
    assert!(
        maintenance::review(&f.root, "edit-reasons", &f.request)
            .unwrap_err()
            .to_string()
            .contains("Stale maintenance proposal")
    );
}
