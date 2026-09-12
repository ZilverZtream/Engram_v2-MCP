#![allow(clippy::unwrap_used)]

use engram_server::services::pre_commit_review_service::{parse_unified_diff, resolve_diff_source};

#[test]
fn inline_diff_ending_in_patch_filename_reaches_the_review_parser() {
    let tmp = tempfile::tempdir().unwrap();
    for suffix in ["patch", "diff"] {
        let patch = format!(
            "diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-old\n+review.{suffix}\n"
        );
        for text in [patch.clone(), patch.replace('\n', "\r\n")] {
            let resolved = resolve_diff_source(tmp.path(), &text).unwrap();
            assert_eq!(resolved, text.trim());
            let files = parse_unified_diff(&resolved);
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, "notes.txt");
            assert!(files[0].added_content.contains(&format!("review.{suffix}")));
        }
    }
}

#[test]
fn project_relative_patch_file_preserves_complete_legacy_content() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("review inputs")).unwrap();
    let bytes = b"diff --git a/notes.txt b/notes.txt\n--- a/notes.txt\n+++ b/notes.txt\n@@ -1 +1 @@\n-old\n+don\x92t drop this\n";
    std::fs::write(tmp.path().join("review inputs/change.patch"), bytes).unwrap();
    let resolved = resolve_diff_source(tmp.path(), "review inputs/change.patch").unwrap();
    assert_eq!(resolved, String::from_utf8_lossy(bytes));
    let files = parse_unified_diff(&resolved);
    assert_eq!(files.len(), 1);
    assert!(files[0].added_content.contains("don\u{fffd}t drop this"));
}

#[test]
fn patch_file_cannot_escape_the_selected_project() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let outside = tmp.path().join("outside.patch");
    std::fs::write(&outside, "private fixture outside the selected project").unwrap();
    for input in [
        "../outside.patch".to_owned(),
        outside.to_string_lossy().into_owned(),
        "invalid\0.patch".to_owned(),
        format!("{}\0.diff", "界".repeat(50)),
    ] {
        assert!(resolve_diff_source(&project, &input).is_err(), "accepted {input}");
    }
}

#[test]
fn patch_paths_preserve_spaces_git_escapes_and_change_kinds() {
    use engram_server::services::pre_commit_review_service::ChangeType;
    let diff = concat!(
        "diff --git a/src/my file.vb b/src/my file.vb\n--- a/src/my file.vb\n+++ b/src/my file.vb\n@@ -1 +1 @@\n-old\n+new\n",
        "diff --git \"a/src/\\303\\245\\t\\\"name.vb\" \"b/src/\\303\\245\\t\\\"name.vb\"\n--- \"a/src/\\303\\245\\t\\\"name.vb\"\n+++ /dev/null\n@@ -1 +0,0 @@\n-removed\n",
        "diff --git a/old name.vb \"b/new\\tname.vb\"\nsimilarity index 100%\nrename from old name.vb\nrename to \"new\\tname.vb\"\n",
        "diff --git a/image b/icon.png b/image b/icon.png\nBinary files a/image b/icon.png and b/image b/icon.png differ\n",
        "diff --git \"a/\\303\\245.png\" \"b/\\303\\245.png\"\nGIT binary patch\nliteral 0\n"
    );
    let files = parse_unified_diff(diff);
    assert_eq!(files.len(), 5);
    assert_eq!(files[0].path, "src/my file.vb");
    assert_eq!(files[1].path, "src/å\t\"name.vb");
    assert!(matches!(files[1].change_type, ChangeType::Deleted));
    assert_eq!(files[2].path, "new\tname.vb");
    assert!(matches!(&files[2].change_type, ChangeType::Renamed(old) if old == "old name.vb"));
    assert_eq!(files[3].path, "image b/icon.png");
    assert!(files[3].is_binary);
    assert_eq!(files[4].path, "å.png");
    assert!(files[4].is_binary);
}

#[test]
fn leading_diff_markers_in_source_are_content_and_keep_line_positions() {
    let diff = "diff --git a/a.js b/a.js\n--- a/a.js\n+++ b/a.js\n@@ -10,3 +10,3 @@\n--- old source\n--counter;\n-old();\n+++ new source\n++counter;\n+danger();\n";
    let files = parse_unified_diff(diff);
    assert_eq!(files[0].path, "a.js");
    assert_eq!(
        files[0].removed_lines,
        vec![
            (10, "-- old source".into()),
            (11, "-counter;".into()),
            (12, "old();".into())
        ]
    );
    assert_eq!(
        files[0].added_lines,
        vec![
            (10, "++ new source".into()),
            (11, "+counter;".into()),
            (12, "danger();".into())
        ]
    );
}

#[test]
fn unstaged_review_contains_new_files_in_nested_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(tmp.path()).unwrap();
    let mut index = repo.index().unwrap();
    std::fs::write(tmp.path().join("tracked.txt"), "before\n").unwrap();
    index.add_path(std::path::Path::new("tracked.txt")).unwrap();
    index.write().unwrap();
    std::fs::write(tmp.path().join("tracked.txt"), "after\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("new/nested")).unwrap();
    std::fs::write(
        tmp.path().join("new/nested/action.vb"),
        "db.SubmitChanges()\n",
    )
    .unwrap();

    let diff = resolve_diff_source(tmp.path(), "unstaged").unwrap();
    let files = parse_unified_diff(&diff);
    assert_eq!(files.len(), 2, "{diff}");
    let added = files
        .iter()
        .find(|f| f.path == "new/nested/action.vb")
        .unwrap();
    assert!(added.added_content.contains("db.SubmitChanges()"), "{diff}");
    assert!(
        !added.hunks.is_empty(),
        "the gates must receive new-file content"
    );

    // An entirely untracked candidate must also reach the gates, not NO_CHANGES.
    std::fs::write(tmp.path().join("tracked.txt"), "before\n").unwrap();
    let diff = resolve_diff_source(tmp.path(), "unstaged").unwrap();
    assert_eq!(parse_unified_diff(&diff).len(), 1, "{diff}");
    assert!(diff.contains("+db.SubmitChanges()"), "{diff}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn immune_empty_corpus_is_insufficient_but_known_rule_still_warns() {
    use engram_core::{ProjectRecord, config::Config, registry::RepoRule};
    use engram_server::{state::AppState, tools::Engram};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (state, _rx) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    let pid = "workflow-immune-test";
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: pid.into(),
            project_name: pid.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(pid, "active_generation", "1")
        .unwrap();
    let engram = Engram::new(state.clone());
    let request = || {
        serde_json::from_value(serde_json::json!({
        "project_id": pid, "code": "DELETE FROM records", "file_path": "data.vb", "use_vector": false
    })).unwrap()
    };
    let result = engram.handle_immune_check(request()).await.unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("Final Status: INSUFFICIENT"), "{text}");
    assert!(!text.contains("CLEAN"), "{text}");
    state
        .registry
        .put_repo_rule(
            pid,
            &RepoRule {
                rule_id: "immune_data".into(),
                file_pattern: "data.vb".into(),
                rule_text: "Previously reverted data mutation".into(),
                priority: 1,
                updated_at_ms: 0,
            },
        )
        .unwrap();
    let result = engram.handle_immune_check(request()).await.unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("WARNING"), "{text}");
    assert!(text.contains("Incomplete evidence"), "{text}");
    let empty = serde_json::from_value(serde_json::json!({"project_id":pid,"code":"  "})).unwrap();
    assert!(engram.handle_immune_check(empty).await.is_err());
}
