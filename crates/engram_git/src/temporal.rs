//! Temporal coupling + revert analysis.
//!
//! v1 stored git commit + diff tables in SQLite and then ran expensive
//! self-joins. v2 streams commits once and updates weighted edges in the graph.

use engram_core::RelPath;
use std::collections::BTreeSet;

/// Return all unique unordered pairs from a set of file paths.
///
/// This is O(k^2) per commit, but k (files changed per commit) is typically small.
pub fn file_pairs(files: &[RelPath], hard_cap: usize) -> Vec<(RelPath, RelPath)> {
    let mut set: BTreeSet<(RelPath, RelPath)> = BTreeSet::new();
    let mut v: Vec<&RelPath> = files.iter().collect();
    v.sort();

    // Safety guard: a single giant refactor commit shouldn't explode work.
    let k = v.len().min(hard_cap);

    for i in 0..k {
        for j in (i + 1)..k {
            let a = v[i].clone();
            let b = v[j].clone();
            set.insert((a, b));
        }
    }

    set.into_iter().collect()
}
