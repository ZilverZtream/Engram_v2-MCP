//! A normal multi-file feature remains a complete, stale-checked review packet.
use engram_server::services::instruction_maintenance::{self as maintenance, MaintenanceRequest};
use serde_json::json;
use std::path::Path;

const GUIDE_PATH: &str = ".github/instructions/volume.instructions.md";
const GUIDE: &str = "---\napplyTo: Site/**\n---\nPreserve reviewed authorization on saves.\nException: read-only previews do not submit.\n";

fn source(index: usize, value: usize) -> (String, String) {
    match index % 3 {
        0 => (
            format!("Site/rule{index:02}.vb"),
            format!(
                "Public Module Rule{index:02}\nPublic Function Apply() As Integer\nReturn {value}\nEnd Function\nEnd Module\n"
            ),
        ),
        1 => (
            format!("Site/rule{index:02}.ts"),
            format!("export function rule{index:02}() {{ return {value}; }}\n"),
        ),
        _ => (
            format!("Site/rule{index:02}.sql"),
            format!("CREATE PROCEDURE dbo.Rule{index:02} AS SELECT {value} AS value;\n"),
        ),
    }
}

fn commit(root: &Path) -> String {
    let repo = git2::Repository::open(root).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Owned fixture", "fixture@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "Owned multi-file base",
        &tree,
        &[],
    )
    .unwrap()
    .to_string()
}

#[test]
fn multi_file_feature_retains_every_before_after_source_and_late_file_stale_guard() {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temporary = tempfile::Builder::new()
        .prefix("m")
        .rand_bytes(2)
        .tempdir_in(fixture_root)
        .unwrap();
    let root = temporary.path().join("r");
    std::fs::create_dir_all(root.join(".github/instructions")).unwrap();
    std::fs::create_dir(root.join("Site")).unwrap();
    drop(git2::Repository::init(&root).unwrap());
    std::fs::write(root.join(GUIDE_PATH), GUIDE).unwrap();
    for i in 0..35 {
        let (path, body) = source(i, 1);
        std::fs::write(root.join(path), body).unwrap();
    }
    let base = commit(&root);
    for i in 0..35 {
        let (path, body) = source(i, 2);
        std::fs::write(root.join(path), body).unwrap();
    }
    let mut request: MaintenanceRequest = serde_json::from_value(json!({
        "base_commit":base,"use_git_changes":true
    }))
    .unwrap();
    std::fs::create_dir(root.join("SiteArchive")).unwrap();
    for index in 0..130 {
        std::fs::write(root.join(format!("SiteArchive/record{index:03}.ts")), "export const archived = true;\n").unwrap();
    }
    let unscoped = maintenance::review(&root, "owned-volume", &request).unwrap_err();
    assert!(unscoped.to_string().contains("Git comparison exceeds"));
    request.git_path_prefixes = vec!["Site".into()];
    let collected = maintenance::review(&root, "owned-volume", &request).unwrap();
    let context = collected.packet.change_context.as_ref().unwrap();
    assert_eq!(context["git_path_prefixes"], json!(["Site"]));
    assert_eq!(context["basis"], "git_base_to_current_worktree_within_path_scope");
    assert_eq!(collected.packet.evidence.len(), 35);
    for i in 0..35 {
        let (path, before) = source(i, 1);
        let after = source(i, 2).1;
        let evidence = collected
            .packet
            .evidence
            .iter()
            .find(|e| e.path == path)
            .unwrap();
        for (snapshot, expected) in [(&evidence.before, before), (&evidence.after, after)] {
            let snapshot = snapshot.as_ref().unwrap();
            assert_eq!(snapshot.content, expected);
            assert_eq!(
                snapshot.blake3,
                blake3::hash(expected.as_bytes()).to_hex().to_string()
            );
        }
    }
    assert_eq!(collected.packet.instructions.len(), 1);
    assert_eq!(collected.packet.instructions[0].current.content, GUIDE);
    let guide = &collected.packet.instructions[0];
    request.draft = Some(serde_json::from_value(json!({
        "expected_packet_blake3":collected.packet_blake3,
        "decisions":[{"instruction_path":GUIDE_PATH,"outcome":"needs_evidence",
            "rationale":"Changed implementation does not establish policy intent; preserve the accepted exception.",
            "evidence":[{"path":GUIDE_PATH,"version":"after","blake3":guide.current.blake3,"quote":GUIDE}],
            "preserved_fragments":["Exception: read-only previews do not submit."]}]
    })).unwrap());
    let proposal = maintenance::review(&root, "owned-volume", &request).unwrap();
    assert_eq!(proposal.proposals[0].proposed_content, GUIDE);
    assert_eq!(proposal.authored_files_written, 0);
    let (last_path, last_body) = source(34, 3);
    std::fs::write(root.join(last_path), last_body).unwrap();
    let stale = maintenance::review(&root, "owned-volume", &request)
        .err()
        .expect("A changed late-file snapshot must refuse the captured proposal");
    assert!(stale.to_string().contains("Stale maintenance proposal"));
    assert_eq!(
        std::fs::read_to_string(root.join(GUIDE_PATH)).unwrap(),
        GUIDE
    );
    // All Git handles are scoped to commit(); no database/process owns this fixture.
    temporary.close().unwrap();
}
