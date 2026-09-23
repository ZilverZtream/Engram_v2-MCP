#![allow(clippy::unwrap_used)]

use engram_server::services::pre_commit_review_service::resolve_bounded_worktree_diff;

fn committed_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("Changed.vb"), "Class Changed\nEnd Class\n").unwrap();
    let repo = git2::Repository::init(temp.path()).unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_path(std::path::Path::new("Changed.vb"))
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .unwrap();
    drop(tree);
    drop(repo);
    temp
}

#[test]
fn no_vb_request_is_silent_even_outside_git() {
    let temp = tempfile::tempdir().unwrap();
    let result = resolve_bounded_worktree_diff(
        temp.path(),
        &["Notes.txt".into()],
        10,
        100,
        4096,
    )
    .unwrap();
    assert!(result.text.is_empty());
    assert!(result.notes.is_empty());
    assert!(resolve_bounded_worktree_diff(
        temp.path(),
        &["Changed.vb".into()],
        10,
        100,
        4096,
    )
    .is_err());
}

#[test]
fn nested_project_root_is_rejected_instead_of_using_wrong_coordinates() {
    let temp = committed_repo();
    let nested = temp.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    assert!(resolve_bounded_worktree_diff(
        &nested,
        &["Changed.vb".into()],
        10,
        100,
        4096,
    )
    .unwrap_err()
    .to_string()
    .contains("must equal"));
}

#[test]
fn requested_pathspec_excludes_unrelated_untracked_content() {
    let temp = committed_repo();
    std::fs::write(
        temp.path().join("Changed.vb"),
        "Class Changed\n  Sub Save()\n  End Sub\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("Unrelated.vb"),
        "Class Unrelated\nEnd Class\n",
    )
    .unwrap();
    let result = resolve_bounded_worktree_diff(
        temp.path(),
        &["Changed.vb".into()],
        10,
        100,
        4096,
    )
    .unwrap();
    assert!(result.text.contains("Changed.vb"), "{}", result.text);
    assert!(!result.text.contains("Unrelated"), "{}", result.text);
}

#[test]
fn requested_nested_untracked_file_is_included() {
    let temp = committed_repo();
    std::fs::create_dir(temp.path().join("feature")).unwrap();
    std::fs::write(
        temp.path().join("feature/NewFile.vb"),
        "Class NewFile\nEnd Class\n",
    )
    .unwrap();
    let result = resolve_bounded_worktree_diff(
        temp.path(),
        &["feature/NewFile.vb".into()],
        10,
        100,
        4096,
    )
    .unwrap();
    assert!(result.text.contains("feature/NewFile.vb"), "{}", result.text);
    assert!(result.text.contains("Class NewFile"), "{}", result.text);
}

#[test]
fn render_caps_return_partial_diff_with_incomplete_notes() {
    let temp = committed_repo();
    let source = (0..30)
        .map(|index| format!("' changed line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(temp.path().join("Changed.vb"), source).unwrap();

    let line_capped = resolve_bounded_worktree_diff(
        temp.path(),
        &["Changed.vb".into()],
        10,
        4,
        4096,
    )
    .unwrap();
    assert!(!line_capped.text.is_empty());
    assert!(line_capped.notes.iter().any(|note| note.contains("4 rendered line")));

    let byte_capped = resolve_bounded_worktree_diff(
        temp.path(),
        &["Changed.vb".into()],
        10,
        100,
        300,
    )
    .unwrap();
    assert!(!byte_capped.text.is_empty());
    assert!(byte_capped.text.len() <= 300);
    assert!(byte_capped.notes.iter().any(|note| note.contains("300 bytes")));
}

#[test]
fn requested_pathspec_is_literal_and_unsafe_paths_are_rejected() {
    let temp = committed_repo();
    std::fs::write(
        temp.path().join("Changed.vb"),
        "Class Changed\n  Sub Save()\n  End Sub\nEnd Class\n",
    )
    .unwrap();
    let wildcard = resolve_bounded_worktree_diff(
        temp.path(),
        &["*.vb".into()],
        10,
        100,
        4096,
    )
    .unwrap();
    assert!(wildcard.text.is_empty(), "{}", wildcard.text);

    for unsafe_path in [
        "../Changed.vb",
        "./Changed.vb",
        "feature/./Changed.vb",
        "feature//Changed.vb",
        "/Changed.vb",
    ] {
        assert!(
            resolve_bounded_worktree_diff(
                temp.path(),
                &[unsafe_path.into()],
                10,
                100,
                4096,
            )
            .is_err(),
            "accepted {unsafe_path}"
        );
    }
}
