//! Match ranked source citations to one immutable, locally fetched default-branch tree.
use super::story_sites::Site;
use engram_git::GitWalker;
use engram_graph::GraphStore;
use git2::{FileMode, Repository};
use similar::{Algorithm, DiffTag, TextDiff};
use std::{collections::HashMap, path::Path, time::Duration};

fn matching_lines(base: &str, checkout: &str) -> Vec<Option<usize>> {
    let old: Vec<_> = base.trim_start_matches('\u{feff}').lines().collect();
    let new: Vec<_> = checkout.trim_start_matches('\u{feff}').lines().collect();
    if old == new {
        return (1..=new.len()).map(Some).collect();
    }
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .timeout(Duration::from_millis(150))
        .diff_slices(&old, &new);
    let mut mapping = vec![None; new.len()];
    for op in diff.ops().iter().filter(|op| op.tag() == DiffTag::Equal) {
        for (i, j) in op.old_range().zip(op.new_range()) {
            mapping[j] = Some(i + 1);
        }
    }
    mapping
}

pub(super) fn annotate(
    graph: &GraphStore,
    project: &str,
    root: &Path,
    sites: &mut [Site],
) -> Option<String> {
    with_base(graph, project, root, |note| {
    // Normal rows begin with the indexed path, including the declaration and followed callees.
    // Route witnesses additionally contain registration and return locations inside the explanation.
    static ROUTE_LOCATION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?: matches |; )([^;\r\n]+?):([0-9]+)(?: \(| returns )").unwrap()
    });
    for site in sites {
        for row in &mut site.lines {
            if row.starts_with("Location evidence:") {
                let notes: Vec<_> = ROUTE_LOCATION
                    .captures_iter(row)
                    .filter_map(|m| {
                        let pos = m.get(2)?;
                        let suffix = note(&m[1], pos.as_str().parse().ok()?);
                        Some((pos.end(), suffix))
                    })
                    .collect();
                for (i, s) in notes.into_iter().rev() {
                    row.insert_str(i, &s)
                }
                continue;
            }
            let location = row.strip_prefix("Declaration: ").unwrap_or(row);
            if let Some((path, n)) = site.paths.iter().find_map(|path| {
                let tail = location.strip_prefix(path.as_str())?.strip_prefix(':')?;
                let digits = tail.split(|c: char| !c.is_ascii_digit()).next()?;
                Some((path, digits.parse().ok()?))
            }) {
                row.push_str(&note(path, n));
            }
        }
    }
    })
}

/// Map observations from the indexed checkout to equal lines in the fetched base.
pub(in crate::handlers) fn with_base(
    graph: &GraphStore, project: &str, root: &Path,
    apply: impl FnOnce(&mut dyn FnMut(&str, usize) -> String),
) -> Option<String> {
    let repo = Repository::discover(root).ok()?;
    let (name, oid) = GitWalker::approved_history_target(&repo)?;
    let tree = repo.find_commit(oid).ok()?.tree().ok()?;
    let workdir = repo.workdir()?.canonicalize().ok()?;
    let root_path = root.canonicalize().ok()?;
    let prefix = root_path.strip_prefix(&workdir).ok()?;
    let mut files = HashMap::new();
    let mut note = |path: &str, line: usize| -> String {
        let mapping = files.entry(path.to_owned()).or_insert_with(|| {
            let entry = tree.get_path(&prefix.join(path)).ok()?;
            if ![
                i32::from(FileMode::Blob),
                i32::from(FileMode::BlobExecutable),
            ]
            .contains(&entry.filemode())
            {
                return None;
            }
            let blob = repo.find_blob(entry.id()).ok()?;
            if blob.size() > 4 * 1024 * 1024 {
                return None;
            }
            let base = std::str::from_utf8(blob.content()).ok()?;
            let checkout = crate::planner_source::read(graph, project, root, path).ok()?;
            Some(matching_lines(base, &checkout))
        });
        match line
            .checked_sub(1)
            .and_then(|idx| mapping.as_ref()?.get(idx).copied().flatten())
        {
            Some(n) if n == line => String::new(),
            Some(n) => format!(" [base L{n}]"),
            None => " [base line unverified]".into(),
        }
    };
    apply(&mut note);
    Some(format!(
        "Source base: {name}@{oid} (locally fetched). Checkout lines match the base unless annotated."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_changes_do_not_acquire_an_upstream_coordinate() {
        assert_eq!(
            matching_lines(
                "declaration\nold value\ncommit\n",
                "\u{feff}inserted\r\ndeclaration\r\nnew value\r\ncommit\r\n"
            ),
            vec![None, Some(1), None, Some(3)]
        );
    }
}
