//! Git-to-card extraction and its persistent-cache identity inputs.
//! Keep ranking, provider, response and cache-maintenance code outside this
//! module: changing those callers must not rebuild unchanged Git metadata.
use super::ChangeCard;
use git2::{Oid, Repository};
use regex::Regex;
use std::sync::LazyLock;

pub(super) const MAX_DESCRIPTION: usize = 1500;
pub(super) const MAX_CARD_FILES: usize = 40;

/// Build output and dependency files that say nothing about a change's intent.
pub(super) fn is_generated(path: &str) -> bool {
    static GENERATED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(\.designer\.(vb|cs)$|\.resx$|\.min\.(js|css)$|\.(js|css)\.map$|(^|/)(bin|obj|node_modules|dist|packages)/|\.(dll|exe|pdb)$|\.refresh$|\.dbml\.layout$|\.(csproj|vbproj|sqlproj|sln)$|packages\.config$|package-lock\.json$|yarn\.lock$|\.lock$)",
        )
        .expect("generated-file pattern")
    });
    GENERATED.is_match(path)
}

/// Parse a PR identity from a first-parent commit summary.
/// Returns (pr_id, title). Falls back to the short oid + full summary for
/// direct pushes so the corpus still covers repos without PR discipline.
pub(crate) fn parse_pr_identity(summary: &str, short_oid: &str) -> (String, String) {
    static ADO_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)^merged pr (\d+)\s*:?\s*(.*)$").expect("ADO_RE"));
    static GH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)^merge pull request #(\d+)(?:\s+from\s+\S+)?\s*(.*)$")
            .expect("GH_RE")
    });
    if let Some(c) = ADO_RE.captures(summary) {
        let title = c[2].trim().to_string();
        return (
            format!("PR-{}", &c[1]),
            if title.is_empty() {
                summary.to_string()
            } else {
                title
            },
        );
    }
    if let Some(c) = GH_RE.captures(summary) {
        let title = c[2].trim().to_string();
        return (
            format!("PR-{}", &c[1]),
            if title.is_empty() {
                summary.to_string()
            } else {
                title
            },
        );
    }
    (format!("commit-{short_oid}"), summary.to_string())
}

/// A card's fields, from git (no vector yet).
pub(super) fn describe(repo: &Repository, oid: Oid) -> anyhow::Result<ChangeCard> {
    let commit = repo.find_commit(oid)?;
    let message = commit.message().unwrap_or("").trim().to_string();
    let summary = commit.summary().unwrap_or("").to_string();
    let hex = oid.to_string();
    let (_, title) = parse_pr_identity(&summary, &hex[..10]);
    let description: String = message
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .chars()
        .take(MAX_DESCRIPTION)
        .collect();
    let tree = commit.tree()?;
    let parent_tree = if commit.parent_count() == 0 {
        None
    } else {
        Some(commit.parent(0)?.tree()?)
    };
    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
    let mut files: Vec<String> = diff
        .deltas()
        .filter_map(|d| d.new_file().path().or_else(|| d.old_file().path()))
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .filter(|p| !is_generated(p))
        .collect();
    files.dedup();
    let file_count = files.len();
    files.truncate(MAX_CARD_FILES);
    Ok(ChangeCard {
        oid: hex,
        title,
        description,
        author: commit.author().name().unwrap_or("").to_string(),
        timestamp: commit.time().seconds().max(0) as u64,
        files,
        file_count,
        vector: Vec::new(),
    })
}
