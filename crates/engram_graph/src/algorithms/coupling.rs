use crate::store::{EdgeKind, GraphStore};

#[derive(Debug, Clone)]
pub struct Coupling {
    pub file_node_id: String,
    pub neighbor_node_id: String,
    pub weight: u32,
}

pub fn top_project_couplings(
    store: &GraphStore,
    project_id: &str,
    limit: usize,
) -> anyhow::Result<Vec<Coupling>> {
    let all_ids = store.list_node_ids(project_id, Some("file"))?;
    let mut all_couplings = Vec::new();

    for id in all_ids {
        let neigh = store.neighbors(project_id, EdgeKind::TemporalCoupling, &id, limit)?;
        for (n, w) in neigh {
            // To avoid double counting (a<->b and b<->a), we can enforce an order.
            if id < n {
                all_couplings.push(Coupling {
                    file_node_id: id.clone(),
                    neighbor_node_id: n,
                    weight: w,
                });
            }
        }
    }

    all_couplings.sort_by(|a, b| b.weight.cmp(&a.weight));
    if all_couplings.len() > limit {
        all_couplings.truncate(limit);
    }
    Ok(all_couplings)
}

pub fn file_temporal_couplings(
    store: &GraphStore,
    project_id: &str,
    file_node_id: &str,
    min_frequency: u32,
    limit: usize,
) -> anyhow::Result<Vec<Coupling>> {
    let neigh = store.neighbors(project_id, EdgeKind::TemporalCoupling, file_node_id, limit)?;
    let mut couplings = Vec::new();
    for (n, w) in neigh {
        if w >= min_frequency {
            couplings.push(Coupling {
                file_node_id: file_node_id.to_string(),
                neighbor_node_id: n,
                weight: w,
            });
        }
    }
    Ok(couplings)
}
