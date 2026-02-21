use crate::store::{EdgeKind, GraphStore};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Cluster {
    pub node_ids: Vec<String>,
    pub score: u64,
}

impl Cluster {
    pub fn fingerprint(&self) -> String {
        // Sort in-place on a clone for deterministic hashing.
        // Use with_capacity to hint the join buffer size.
        let mut sorted = self.node_ids.clone();
        sorted.sort_unstable();
        let estimated_len: usize = sorted.iter().map(|s| s.len() + 1).sum();
        let mut data = String::with_capacity(estimated_len);
        for (i, s) in sorted.iter().enumerate() {
            if i > 0 {
                data.push('\0');
            }
            data.push_str(s);
        }
        blake3::hash(data.as_bytes()).to_string()
    }
}

/// Robust cluster finder for "REM-style dreaming" using Label Propagation.
///
/// Label Propagation is a solid approximation for community detection (like Louvain/Leiden)
/// that works well on large sparse graphs. It is O(E) and naturally finds dense regions.
pub fn find_cooccurrence_clusters(
    store: &GraphStore,
    project_id: &str,
    min_edge_weight: u32,
    min_size: usize,
    max_clusters: usize,
) -> anyhow::Result<Vec<Cluster>> {
    let nodes = store.list_node_ids(project_id, Some("chunk"))?;
    if nodes.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Initialize each node with a unique label
    let mut labels: HashMap<String, String> =
        nodes.iter().map(|id| (id.clone(), id.clone())).collect();

    // 2. Iterate label propagation (limited rounds for performance/convergence)
    let iterations = 5;
    for _ in 0..iterations {
        let mut changed = false;

        // We'll shuffle nodes or just iterate.
        // In a real implementation we'd shuffle to avoid bias.
        for node_id in &nodes {
            let neighbors = store.neighbors(project_id, EdgeKind::CoOccurrence, node_id, 128)?;
            if neighbors.is_empty() {
                continue;
            }

            // Count frequencies of neighbor labels, weighted by edge weight
            let mut label_weights: HashMap<String, u64> = HashMap::new();
            for (neigh_id, weight) in neighbors {
                if weight < min_edge_weight {
                    continue;
                }
                if let Some(l) = labels.get(&neigh_id) {
                    *label_weights.entry(l.clone()).or_insert(0) += weight as u64;
                }
            }

            // Find most frequent/heaviest label
            if let Some(best_label) = label_weights
                .into_iter()
                .max_by_key(|&(_, w)| w)
                .map(|(l, _)| l)
                && let Some(current) = labels.get(node_id)
                && *current != best_label
            {
                labels.insert(node_id.clone(), best_label);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    // 3. Group nodes by labels
    let mut community_map: HashMap<String, Vec<String>> = HashMap::new();
    for (node_id, label) in labels {
        community_map.entry(label).or_default().push(node_id);
    }

    // 4. Convert to Cluster objects and score.
    // Build a HashSet per cluster for O(1) membership checks during scoring
    // (was O(N) Vec::contains, quadratic for large clusters).
    let mut clusters = Vec::new();
    for members in community_map.into_values() {
        if members.len() < min_size {
            continue;
        }

        // Skip clusters that already have an insight connected.
        if store.cluster_has_insight(project_id, &members, 16)? {
            continue;
        }

        // Score = internal density or sum of weights.
        // HashSet lookup is O(1) vs O(N) for Vec::contains.
        let member_set: HashSet<&str> = members.iter().map(|s| s.as_str()).collect();
        let mut score: u64 = 0;
        for m in &members {
            if let Ok(neigh) = store.neighbors(project_id, EdgeKind::CoOccurrence, m, 128) {
                for (target, w) in neigh {
                    if member_set.contains(target.as_str()) {
                        score += w as u64;
                    }
                }
            }
        }

        clusters.push(Cluster {
            node_ids: members,
            score,
        });
    }

    clusters.sort_by(|a, b| b.score.cmp(&a.score));
    if clusters.len() > max_clusters {
        clusters.truncate(max_clusters);
    }
    Ok(clusters)
}
