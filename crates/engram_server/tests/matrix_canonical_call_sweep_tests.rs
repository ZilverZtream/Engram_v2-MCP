#![allow(clippy::unwrap_used)]

use engram_core::Config;
use engram_server::{state::AppState, tools::Engram};
use serde_json::json;

fn configure_sidecar() -> bool {
    static CONFIGURED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CONFIGURED.get_or_init(|| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tools/vb_roslyn_sidecar/publish_out")
            .join(if cfg!(windows) {
                "vb_roslyn_sidecar.exe"
            } else {
                "vb_roslyn_sidecar"
            });
        if !path.exists() {
            return false;
        }
        unsafe { std::env::set_var("ENGRAM_VB_SIDECAR_PATH", path) };
        true
    })
}

async fn fixture() -> (tempfile::TempDir, Engram, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Changed.vb"),
        "Class Changed\n Sub Save()\n  AuditTrail.Record(\"same-file\", 7)\n  AuditTrail.Record(EventPrefix.Preexisting, 7)\n End Sub\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Legacy.vb"),
        "Class Legacy\n Sub Save()\n  AuditTrail.Record(\"other-file\", 8)\n End Sub\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Unrelated.vb"),
        "Class Unrelated\n Sub Save()\n  OtherTrail.Record(\"other-file\", 8)\n End Sub\nEnd Class\n",
    )
    .unwrap();

    let repo = git2::Repository::init(&root).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .unwrap();
    drop(tree);
    drop(repo);

    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    let server = Engram::new(state.clone());
    server
        .handle_index_project(
            serde_json::from_value(json!({
                "directory": root,
                "project_name": "canonical-call-matrix",
                "project_type": "general",
                "wait": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let project_id = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (temp, server, project_id)
}

#[tokio::test]
async fn preexisting_member_without_diff_does_not_activate_matrix_sweep() {
    if !configure_sidecar() {
        return;
    }
    let (_temp, server, project_id) = fixture().await;
    let response = server
        .handle_derive_test_matrix(
            serde_json::from_value(json!({
                "project_id": project_id,
                "files": ["Changed.vb"],
                "change_intent": "Update audit behavior"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(!text.contains("Canonical-call residual sweep"), "{text}");
    assert!(!text.contains("canonical-call"), "{text}");
}

#[tokio::test]
async fn added_diff_member_reports_same_callee_literals_including_changed_file() {
    if !configure_sidecar() {
        return;
    }
    let (temp, server, project_id) = fixture().await;
    let changed_path = temp.path().join("repo/Changed.vb");
    std::fs::write(
        &changed_path,
        "Class Changed\n Sub Save()\n  AuditTrail.Record(\"same-file\", 7)\n  AuditTrail.Record(EventPrefix.Preexisting, 7)\n  AuditTrail.Record(EventToken.Widget, 7)\n End Sub\nEnd Class\n",
    )
    .unwrap();
    let repo = git2::Repository::open(temp.path().join("repo")).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("Changed.vb")).unwrap();
    index.write().unwrap();
    std::fs::write(
        &changed_path,
        "' inserted after staging\nClass Changed\n Sub Save()\n  AuditTrail.Record(\"same-file\", 7)\n  AuditTrail.Record(EventPrefix.Preexisting, 7)\n  AuditTrail.Record(EventToken.Widget, 7)\n End Sub\nEnd Class\n",
    )
    .unwrap();
    let response = server
        .handle_derive_test_matrix(
            serde_json::from_value(json!({
                "project_id": project_id,
                "files": ["Changed.vb"],
                "change_intent": "Update audit behavior"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(text.contains("Canonical-call residual sweep — 2"), "{text}");
    assert!(text.contains("Changed.vb:4"), "{text}");
    assert!(text.contains("Legacy.vb:3"), "{text}");
    assert!(text.contains("`AuditTrail.Record` argument 1"), "{text}");
    assert!(text.contains("`EventToken.Widget` at Changed.vb:6"), "{text}");
    assert!(!text.contains("Unrelated.vb:3"), "{text}");
    assert!(text.contains("Requested seed files are prioritized"), "{text}");
}
