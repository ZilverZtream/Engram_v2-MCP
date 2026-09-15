//! Temporal coupling + revert analysis.
//!
//! v1 stored git commit + diff tables in SQLite and then ran expensive
//! self-joins. v2 streams commits once and updates weighted edges in the graph.

use engram_core::RelPath;

/// Return all unique unordered pairs from a set of file paths.
///
/// A change set with more than `hard_cap` distinct files is a bulk change
/// (reformat, dependency upgrade, mass rename) and yields no pairs: it says
/// nothing about which files belong together. Pairing only the first
/// `hard_cap` sorted paths instead gave alphabetically early files co-change
/// weight that the rest of the same commit never got. Input is sorted +
/// deduped so `(v[i], v[j])` with `i < j` is already unique — no set needed.
pub fn file_pairs(files: &[RelPath], hard_cap: usize) -> Vec<(RelPath, RelPath)> {
    let mut v: Vec<&RelPath> = files.iter().collect();
    v.sort();
    v.dedup();
    if v.len() > hard_cap {
        return Vec::new();
    }

    let k = v.len();
    let pair_count = k * k.saturating_sub(1) / 2;
    let mut pairs = Vec::with_capacity(pair_count);

    for i in 0..k {
        for j in (i + 1)..k {
            pairs.push((v[i].clone(), v[j].clone()));
        }
    }

    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(n: usize) -> Vec<RelPath> {
        (0..n).map(|i| RelPath::new(&format!("dir/f{i:03}.vb"))).collect()
    }

    #[test]
    fn a_commit_within_the_cap_pairs_every_file() {
        assert_eq!(file_pairs(&paths(4), 80).len(), 6);
    }

    /// A commit above the cap is a bulk change (reformat, upgrade, mass rename)
    /// whose files are not coupled by it. Pairing only the first `cap` sorted
    /// paths gave alphabetically early files co-change weight that later files
    /// in the same commit never got.
    #[test]
    fn a_commit_above_the_cap_yields_no_pairs() {
        assert!(file_pairs(&paths(81), 80).is_empty());
    }
}
