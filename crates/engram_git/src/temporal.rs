//! Temporal coupling + revert analysis.
//!
//! v1 stored git commit + diff tables in SQLite and then ran expensive
//! self-joins. v2 streams commits once and updates weighted edges in the graph.

use engram_core::RelPath;

/// Return all unique unordered pairs from a set of file paths.
///
/// O(k^2) per commit, but k (files changed per commit) is typically small
/// and hard-capped. Input is sorted + deduped so `(v[i], v[j])` with `i < j`
/// is already unique — no set needed.
pub fn file_pairs(files: &[RelPath], hard_cap: usize) -> Vec<(RelPath, RelPath)> {
    let mut v: Vec<&RelPath> = files.iter().collect();
    v.sort();
    v.dedup();

    let k = v.len().min(hard_cap);
    let pair_count = k * k.saturating_sub(1) / 2;
    let mut pairs = Vec::with_capacity(pair_count);

    for i in 0..k {
        for j in (i + 1)..k {
            pairs.push((v[i].clone(), v[j].clone()));
        }
    }

    pairs
}
