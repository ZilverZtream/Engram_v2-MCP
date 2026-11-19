//! Git supplies candidate paths; the existing collector owns complete snapshots.
use super::*;

pub(super) fn paths_since(root: &Path, base_commit: &str, scopes: &[String]) -> Result<Vec<String>> {
    let root = std::fs::canonicalize(root)?;
    let repo = git2::Repository::open(&root)
        .context("Git-driven maintenance needs this project's Git repository")?;
    let workdir = repo
        .workdir()
        .context("Git-driven maintenance needs a worktree")?;
    ensure!(
        std::fs::canonicalize(workdir)? == root,
        "Git-driven maintenance requires the project root to equal the Git worktree root"
    );
    let tree = repo
        .find_commit(git2::Oid::from_str(base_commit)?)?
        .tree()?;
    let mut options = git2::DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_typechange(true);
    if !scopes.is_empty() {
        options.disable_pathspec_match(true);
        for scope in scopes {
            options.pathspec(scope);
        }
    }
    let diff = repo.diff_tree_to_workdir_with_index(Some(&tree), Some(&mut options))?;
    let mut selected = BTreeSet::new();
    for delta in diff.deltas() {
        // Keep both sides if Git identifies a rename; ordinarily it reports two
        // paths. Never infer a surviving symbol or widen the old policy scope.
        for path in [delta.old_file().path(), delta.new_file().path()]
            .into_iter()
            .flatten()
        {
            let path = path.to_str().context(
                "Git changed path is not UTF-8; provide a supported explicit review slice",
            )?;
            selected.insert(path.replace('\\', "/"));
            ensure!(
                selected.len() <= GIT_EVIDENCE_PATH_LIMIT,
                "Git comparison exceeds{GIT_EVIDENCE_PATH_LIMIT} changed files; narrow git_path_prefixes or use explicit changed_paths for a bounded review slice. No files were silently omitted."
            );
        }
    }
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    paths(
        &selected.into_iter().collect::<Vec<_>>(),
        GIT_EVIDENCE_PATH_LIMIT,
    )
}
