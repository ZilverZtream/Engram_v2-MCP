use engram_core::RelPath;
use git2::{DiffFormat, Oid, Repository, Sort};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct GitUpdateResult {
    pub commits_processed: usize,
    pub temporal_edges_added: u64,
    pub reverted_commits: usize,
    pub anti_patterns: usize,
    pub last_oid: Option<Oid>,
}

#[derive(Debug, Clone)]
pub struct AntiPatternDoc {
    pub original_commit: String,
    pub file_path: RelPath,
    pub diff_text: String,
}

#[derive(Debug, Clone)]
pub enum FileChange {
    Modified(RelPath),
    Added(RelPath),
    Deleted(RelPath),
    Renamed { old: RelPath, new: RelPath },
}

impl FileChange {
    pub fn path(&self) -> &RelPath {
        match self {
            FileChange::Modified(p) | FileChange::Added(p) | FileChange::Deleted(p) => p,
            FileChange::Renamed { new, .. } => new,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeCommitPolicy {
    /// Follow all parents (default).
    AllParents,
    /// Follow only the first parent (linear history).
    FirstParentOnly,
}

#[derive(Clone)]
pub struct GitWalker;

impl GitWalker {
    pub fn open_repo(path: &Path) -> anyhow::Result<Repository> {
        Ok(Repository::discover(path)?)
    }

    /// Walk commits from HEAD back until `stop_oid` is encountered (exclusive).
    /// Returns commits in oldest->newest order (so you can apply streaming updates).
    pub fn walk_new_commits(
        repo: &Repository,
        stop_oid: Option<Oid>,
        max: usize,
        policy: MergeCommitPolicy,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Vec<Oid>> {
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;
        revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        if policy == MergeCommitPolicy::FirstParentOnly {
            revwalk.simplify_first_parent()?;
        }

        let mut oids: Vec<Oid> = Vec::new();
        for oid_res in revwalk {
            if cancel.is_cancelled() {
                break;
            }
            let oid = oid_res?;
            if let Some(stop) = stop_oid
                && oid == stop
            {
                break;
            }
            oids.push(oid);
            if oids.len() >= max {
                break;
            }
        }

        // revwalk yields newest->oldest with the above sorting; reverse for incremental application.
        oids.reverse();
        Ok(oids)
    }
    /// Walk commits from HEAD back and stream them to a callback.
    pub fn walk_commits_streaming<F>(
        repo: &Repository,
        stop_oid: Option<Oid>,
        max: usize,
        policy: MergeCommitPolicy,
        cancel: &tokio_util::sync::CancellationToken,
        mut callback: F,
    ) -> anyhow::Result<usize>
    where
        F: FnMut(Oid, usize, usize) -> anyhow::Result<()>,
    {
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;
        revwalk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        if policy == MergeCommitPolicy::FirstParentOnly {
            revwalk.simplify_first_parent()?;
        }

        let mut oids: Vec<Oid> = Vec::new();
        for oid_res in revwalk {
            if cancel.is_cancelled() {
                break;
            }
            let oid = oid_res?;
            if let Some(stop) = stop_oid
                && oid == stop
            {
                break;
            }
            oids.push(oid);
            if oids.len() >= max {
                break;
            }
        }

        // Still need to reverse to process oldest -> newest for correct state progression
        oids.reverse();

        let count = oids.len();
        for (i, oid) in oids.into_iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            callback(oid, i + 1, count)?;
        }

        Ok(count)
    }

    pub fn files_changed_in_commit(repo: &Repository, oid: Oid) -> anyhow::Result<Vec<FileChange>> {
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;
        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };

        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(false);
        opts.recurse_untracked_dirs(false);
        // Enable rename detection
        let mut find_opts = git2::DiffFindOptions::new();
        find_opts.renames(true);

        let mut diff =
            repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut opts))?;
        diff.find_similar(Some(&mut find_opts))?;

        let mut out = Vec::new();
        for d in diff.deltas() {
            let status = d.status();
            match status {
                git2::Delta::Added => {
                    if let Some(p) = d.new_file().path() {
                        out.push(FileChange::Added(RelPath::new(&p.to_string_lossy())));
                    }
                }
                git2::Delta::Deleted => {
                    if let Some(p) = d.old_file().path() {
                        out.push(FileChange::Deleted(RelPath::new(&p.to_string_lossy())));
                    }
                }
                git2::Delta::Renamed => {
                    if let (Some(op), Some(np)) = (d.old_file().path(), d.new_file().path()) {
                        out.push(FileChange::Renamed {
                            old: RelPath::new(&op.to_string_lossy()),
                            new: RelPath::new(&np.to_string_lossy()),
                        });
                    }
                }
                _ => {
                    if let Some(p) = d.new_file().path() {
                        out.push(FileChange::Modified(RelPath::new(&p.to_string_lossy())));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Best-effort revert detection:
    /// - If message contains "This reverts commit <hash>" we parse the hash.
    /// - Otherwise returns None.
    pub fn reverted_oid_from_message(message: &str) -> Option<Oid> {
        let needle = "This reverts commit ";
        let idx = message.find(needle)?;
        let rest = &message[idx + needle.len()..];
        let hash = rest
            .split(|c: char| !c.is_ascii_hexdigit())
            .next()
            .unwrap_or("")
            .trim();
        if hash.len() < 7 {
            return None;
        }
        Oid::from_str(hash).ok()
    }

    pub fn diff_text_for_commit(
        repo: &Repository,
        oid: Oid,
        max_bytes: usize,
    ) -> anyhow::Result<Vec<(RelPath, String)>> {
        let commit = repo.find_commit(oid)?;
        let tree = commit.tree()?;
        let parent_tree = if commit.parent_count() > 0 {
            Some(commit.parent(0)?.tree()?)
        } else {
            None
        };
        let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;

        let mut current_path: Option<RelPath> = None;
        // Pre-allocate buffer to avoid repeated reallocations as diff lines
        // are appended. Cap at 256 KiB to avoid over-reserving for huge limits.
        let mut buf = String::with_capacity(max_bytes.min(256 * 1024));
        let mut out: Vec<(RelPath, String)> = Vec::new();

        diff.print(DiffFormat::Patch, |delta, hunk, line| {
            let p = delta.new_file().path().or_else(|| delta.old_file().path());
            let p = p.map(|x| RelPath::new(&x.to_string_lossy()));
            if p != current_path {
                if let Some(cp) = current_path.take() {
                    let done = std::mem::take(&mut buf);
                    out.push((cp, done));
                    // Reserve for the next file's diff output.
                    buf.reserve(max_bytes.min(256 * 1024));
                }
                current_path = p;
            }

            if let Some(h) = hunk {
                // Write hunk header directly into buf (avoids intermediate
                // String allocation from format!()).
                use std::fmt::Write;
                let _ = write!(
                    buf,
                    "@@ -{},{} +{},{} @@\n",
                    h.old_start(),
                    h.old_lines(),
                    h.new_start(),
                    h.new_lines()
                );
            }

            if buf.len() < max_bytes
                && let Ok(s) = std::str::from_utf8(line.content())
            {
                buf.push_str(s);
            }

            true
        })?;

        if let Some(cp) = current_path.take() {
            out.push((cp, buf));
        }
        Ok(out)
    }

    pub fn extract_antipatterns_from_reverts(
        repo: &Repository,
        commit_oid: Oid,
        max_bytes_per_file: usize,
    ) -> anyhow::Result<Vec<AntiPatternDoc>> {
        let commit = repo.find_commit(commit_oid)?;
        let msg = commit.message().unwrap_or("");
        let Some(reverted) = Self::reverted_oid_from_message(msg) else {
            return Ok(Vec::new());
        };

        // Index the *original* commit's diff (the change that got reverted).
        let per_file = Self::diff_text_for_commit(repo, reverted, max_bytes_per_file)?;
        let mut out = Vec::new();
        for (path, text) in per_file {
            if text.trim().is_empty() {
                continue;
            }
            out.push(AntiPatternDoc {
                original_commit: reverted.to_string(),
                file_path: path,
                diff_text: text,
            });
        }
        Ok(out)
    }

    /// Check if commit B is a structural revert of commit A.
    ///
    /// This is true if the diff of B is the exact inverse of the diff of A.
    pub fn is_structural_revert(repo: &Repository, oid_a: Oid, oid_b: Oid) -> anyhow::Result<bool> {
        let diff_a = Self::diff_text_for_commit(repo, oid_a, 1_000_000)?;
        let diff_b = Self::diff_text_for_commit(repo, oid_b, 1_000_000)?;

        if diff_a.len() != diff_b.len() || diff_a.is_empty() {
            return Ok(false);
        }

        // Commits must touch same files
        for ((p_a, _), (p_b, _)) in diff_a.iter().zip(diff_b.iter()) {
            if p_a != p_b {
                return Ok(false);
            }

            // In a revert, additions in A become deletions in B and vice-versa.
            // A simple check: are they mirror images?
            // In git diff format, a line starting with '+' in A should be '-' in B at same location.
            // This is complex to parse perfectly, so we use a heuristic:
            // Does applying B's diff to A's result state return to A's start state?
            // Simpler: is the number of '+' in A equal to '-' in B, and vice-versa?

            // Let's use a more robust check:
            // A revert of A means B's tree is identical to A's parent's tree (for the files touched).
            // But commit B might touch more files than A.
        }

        let commit_a = repo.find_commit(oid_a)?;
        if commit_a.parent_count() == 0 {
            return Ok(false);
        }
        let parent_a_tree = commit_a.parent(0)?.tree()?;

        let commit_b = repo.find_commit(oid_b)?;
        let tree_b = commit_b.tree()?;

        // If commit B perfectly reverts A, then tree_b should be identical to parent_a_tree
        // for the files touched by A.
        // Even simpler: If HEAD^ is OID_A, and we do a hard revert, HEAD tree == HEAD^^ tree.

        Ok(tree_b.id() == parent_a_tree.id())
    }
}
