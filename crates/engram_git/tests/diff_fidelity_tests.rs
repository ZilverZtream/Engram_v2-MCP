#![allow(clippy::unwrap_used)]
use engram_git::history::GitWalker;
use git2::{Repository, Signature};

fn commit(repo: &Repository, files: &[(&str, &str)]) -> git2::Oid {
    let mut index = repo.index().unwrap();
    for (name, content) in files {
        std::fs::write(repo.workdir().unwrap().join(name), content).unwrap();
        index.add_path(std::path::Path::new(name)).unwrap();
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    let sig = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &parents)
        .unwrap()
}

#[test]
fn patch_preserves_added_removed_lines_and_emits_hunk_header_once() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    commit(&repo, &[("sample.txt", "before\nkept\n")]);
    let oid = commit(&repo, &[("sample.txt", "after\nkept\n")]);
    let diff = GitWalker::diff_text_for_commit(&repo, oid, 4096).unwrap();
    let text = &diff[0].1;
    assert!(text.lines().any(|l| l == "-before"), "{text}");
    assert!(text.lines().any(|l| l == "+after"), "{text}");
    assert!(text.lines().any(|l| l == " kept"), "{text}");
    assert_eq!(text.matches("@@ -").count(), 1, "{text}");
}

#[test]
fn oversized_unicode_line_respects_byte_budget_and_discloses_truncation() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let text = "???".repeat(2000);
    let oid = commit(&repo, &[("large.txt", &text)]);
    let diff = GitWalker::diff_text_for_commit(&repo, oid, 256).unwrap();
    assert!(diff[0].1.len() <= 256, "bytes={}", diff[0].1.len());
    assert!(diff[0].1.contains("[diff truncated]"));
}

#[test]
fn each_file_has_its_own_budget_and_zero_is_respected() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init(temp.path()).unwrap();
    let large = "x".repeat(4000);
    let oid = commit(&repo, &[("a.txt", &large), ("z.txt", "small\n")]);
    let diff = GitWalker::diff_text_for_commit(&repo, oid, 256).unwrap();
    assert_eq!(diff.len(), 2);
    assert!(diff.iter().all(|(_, s)| s.len() <= 256));
    assert!(diff.iter().any(|(_, s)| s.lines().any(|l| l == "+small")));
    let zero = GitWalker::diff_text_for_commit(&repo, oid, 0).unwrap();
    assert!(zero.iter().all(|(_, s)| s.is_empty()));
}
