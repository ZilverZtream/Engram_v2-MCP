//! Migration Order Planning Service — Ticket 10.
//!
//! Computes the optimal migration order using topological sorting (Kahn's
//! algorithm) on the dependency graph, grouping files into waves where each
//! wave's dependencies are fully satisfied by earlier waves.
//!
//! Edge kinds treated as "A depends on B" (B must be migrated first):
//!   `Dependency`, `Imports`, `RegistersControl`, `IncludesFile`
//!
//! Cycles are detected via DFS when Kahn's algorithm stalls, reported in
//! `circular_dependencies`, and broken by removing the back-edge node with
//! the lowest in-degree so that processing can continue.

use engram_graph::{EdgeKind, GraphStore, Node};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// ── Public output types ───────────────────────────────────────────────────────

/// Complete migration order plan for a project.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationOrderPlan {
    /// The project this plan was computed for.
    pub project_id: String,
    /// Total number of file nodes included in the plan.
    pub total_files: usize,
    /// Ordered migration waves; wave N's deps are all in waves 1..N-1.
    pub waves: Vec<MigrationWave>,
    /// File paths forming each detected dependency cycle.
    pub circular_dependencies: Vec<Vec<String>>,
    /// Files that block the most downstream work (top 5).
    pub bottleneck_files: Vec<BottleneckFile>,
    /// Human-readable Markdown summary.
    pub summary: String,
}

/// A single migration wave — all files in a wave can be migrated in parallel.
#[derive(Debug, Clone, Serialize)]
pub struct MigrationWave {
    /// 1-based wave number.
    pub wave_number: u32,
    /// Human-readable theme derived from the dominant file type in this wave.
    pub theme: String,
    /// Files belonging to this wave.
    pub files: Vec<WaveFile>,
    /// Paths that must be fully migrated before this wave starts.
    pub prerequisites: Vec<String>,
    /// Whether a strangler-fig integration checkpoint should follow this wave.
    pub strangler_fig_checkpoint: bool,
}

/// A single file entry within a wave.
#[derive(Debug, Clone, Serialize)]
pub struct WaveFile {
    /// Project-relative path of the file.
    pub path: String,
    /// Extension-derived type label, e.g. "aspx", "vb", "config".
    pub file_type: String,
    /// Graph node type, e.g. "file".
    pub node_type: String,
    /// Number of files *this* file depends on (within the project).
    pub dependency_count: usize,
    /// Number of files that depend on *this* file.
    pub dependent_count: usize,
    /// Explanation of why this file lands in this wave.
    pub reason: String,
    /// Rough complexity estimate based on total edge count.
    pub estimated_complexity: String,
}

/// A file that blocks many downstream files.
#[derive(Debug, Clone, Serialize)]
pub struct BottleneckFile {
    /// Project-relative path.
    pub path: String,
    /// Number of files that depend (directly or indirectly) on this file.
    pub blocks_count: usize,
    /// Actionable advice for reducing the bottleneck.
    pub suggestion: String,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// The four edge kinds that encode "source depends on target".
const DEP_KINDS: [EdgeKind; 4] = [
    EdgeKind::Dependency,
    EdgeKind::Imports,
    EdgeKind::RegistersControl,
    EdgeKind::IncludesFile,
];

/// Derive a short file-type label from a path extension.
fn file_type_from_path(path: &str) -> String {
    let lower = path.to_lowercase();
    // Match the last extension, supporting multi-part extensions like .aspx.vb
    if lower.ends_with(".aspx.vb") || lower.ends_with(".aspx.cs") {
        return "code-behind".to_string();
    }
    if lower.ends_with(".ascx.vb") || lower.ends_with(".ascx.cs") {
        return "control-code-behind".to_string();
    }
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "aspx" => "aspx",
        "ascx" => "ascx",
        "master" => "master",
        "vb" => "vb",
        "cs" => "cs",
        "asmx" => "asmx",
        "ashx" => "ashx",
        "svc" => "svc",
        "asax" => "asax",
        "config" => "config",
        "asp" => "asp",
        "rdlc" | "rdl" | "rpt" => "report",
        "js" => "js",
        "css" => "css",
        _ => "other",
    }
    .to_string()
}

/// Derive a wave theme from the set of file types present in the wave.
fn theme_for_wave(files: &[WaveFile]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for f in files {
        *counts.entry(f.file_type.as_str()).or_default() += 1;
    }

    let dominant = counts
        .iter()
        .max_by_key(|&(_, c)| c)
        .map(|(&t, _)| t)
        .unwrap_or("other");

    match dominant {
        "config" => "Configuration: app settings and connection strings".to_string(),
        "ascx" | "control-code-behind" => "Components: shared user controls".to_string(),
        "master" => "Layout: master pages and templates".to_string(),
        "asmx" | "ashx" | "svc" => "Services: web services and handlers".to_string(),
        "aspx" | "code-behind" => "Pages: application pages".to_string(),
        "asp" => "Classic ASP: legacy script pages".to_string(),
        "report" | "rdlc" | "rdl" | "rpt" => "Reports: reporting components".to_string(),
        "asax" => "Application: global infrastructure".to_string(),
        "vb" | "cs" => {
            // Distinguish shared lib files from code-behind
            let aspx_cb = files
                .iter()
                .filter(|f| f.file_type == "code-behind")
                .count();
            let total_vb_cs = files
                .iter()
                .filter(|f| f.file_type == "vb" || f.file_type == "cs")
                .count();
            if aspx_cb > 0 || total_vb_cs == 0 {
                "Mixed: application code".to_string()
            } else {
                "Foundation: shared libraries and utilities".to_string()
            }
        }
        _ => "Mixed: application code".to_string(),
    }
}

/// Estimate per-file complexity from total edge count (in + out).
fn estimate_complexity(total_edges: usize) -> String {
    match total_edges {
        0..=3 => "Low".to_string(),
        4..=8 => "Medium".to_string(),
        _ => "High".to_string(),
    }
}

// ── Cycle detection ───────────────────────────────────────────────────────────

/// DFS-based cycle finder. Returns the first cycle found as a Vec of node IDs,
/// or `None` if the remaining subgraph is acyclic.
fn find_cycle(
    remaining: &HashSet<String>,
    adj: &HashMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    let mut on_stack: HashSet<String> = HashSet::new();

    // Only consider nodes still in `remaining`
    for start in remaining {
        if visited.contains(start) {
            continue;
        }
        // Iterative DFS with explicit call stack to avoid recursion overflow
        let mut call_stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
        while let Some(top) = call_stack.last() {
            let node = top.0.clone();
            let idx = top.1;
            if idx == 0 {
                if visited.contains(&node) {
                    call_stack.pop();
                    continue;
                }
                visited.insert(node.clone());
                on_stack.insert(node.clone());
                stack.push(node.clone());
            }
            let neighbors = adj.get(&node).map(|v| v.as_slice()).unwrap_or(&[]);
            if idx < neighbors.len() {
                let next = neighbors[idx].clone();
                // Advance the child index before we potentially push a new frame
                call_stack.last_mut().expect("stack non-empty in loop").1 = idx + 1;
                if !remaining.contains(&next) {
                    // Neighbor not in remaining subgraph, skip
                    continue;
                }
                if on_stack.contains(&next) {
                    // Found a back-edge — extract cycle
                    let start_pos = stack.iter().position(|n| n == &next).unwrap_or(0);
                    return Some(stack[start_pos..].to_vec());
                }
                if !visited.contains(&next) {
                    call_stack.push((next, 0));
                }
            } else {
                on_stack.remove(&node);
                stack.pop();
                call_stack.pop();
            }
        }
    }
    None
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Compute the optimal migration order for `project_id` using Kahn's
/// topological sort, broken into parallel waves.
pub fn suggest_migration_order(
    graph: &Arc<GraphStore>,
    project_id: &str,
) -> anyhow::Result<MigrationOrderPlan> {
    // ── 1. Collect all file nodes ─────────────────────────────────────────────
    let file_nodes: Vec<Node> = graph.query_nodes(project_id, Some("file"), None, None, 50_000)?;

    if file_nodes.is_empty() {
        return Ok(MigrationOrderPlan {
            project_id: project_id.to_string(),
            total_files: 0,
            waves: vec![],
            circular_dependencies: vec![],
            bottleneck_files: vec![],
            summary: format!(
                "# Migration Order: {project_id}\n\nNo file nodes found in the graph."
            ),
        });
    }

    // Build a path → node_id lookup and a set of project-internal node IDs.
    let node_by_id: HashMap<String, Node> = file_nodes
        .iter()
        .map(|n| (n.node_id.clone(), n.clone()))
        .collect();
    let all_ids: HashSet<String> = node_by_id.keys().cloned().collect();

    // ── 2. Build adjacency lists from dependency edges ────────────────────────
    // `deps_of[A]` = list of file node IDs that A depends on (A → dep).
    // Only edges where both source and target are project-internal files are
    // included (cross-project or non-file targets are skipped).
    let mut deps_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut dependents_of: HashMap<String, Vec<String>> = HashMap::new();
    for id in &all_ids {
        deps_of.entry(id.clone()).or_default();
        dependents_of.entry(id.clone()).or_default();
    }

    for kind in &DEP_KINDS {
        let edges = graph.list_edges_by_kind(project_id, kind.clone(), 200_000)?;
        for edge in &edges {
            // Only track edges where both endpoints are known file nodes.
            if !all_ids.contains(&edge.source_id) || !all_ids.contains(&edge.target_id) {
                continue;
            }
            // source depends on target → target must migrate first
            deps_of
                .entry(edge.source_id.clone())
                .or_default()
                .push(edge.target_id.clone());
            dependents_of
                .entry(edge.target_id.clone())
                .or_default()
                .push(edge.source_id.clone());
        }
    }

    // De-duplicate adjacency lists
    for list in deps_of.values_mut() {
        list.sort();
        list.dedup();
    }
    for list in dependents_of.values_mut() {
        list.sort();
        list.dedup();
    }

    // Per-file total edge count (in + out across ALL kinds) for complexity.
    let mut total_edges_for: HashMap<String, usize> = HashMap::new();
    for kind in EdgeKind::ALL {
        let edges = graph.list_edges_by_kind(project_id, kind.clone(), 200_000)?;
        for edge in &edges {
            if all_ids.contains(&edge.source_id) {
                *total_edges_for.entry(edge.source_id.clone()).or_default() += 1;
            }
            if all_ids.contains(&edge.target_id) {
                *total_edges_for.entry(edge.target_id.clone()).or_default() += 1;
            }
        }
    }

    // ── 3. Kahn's algorithm with cycle-breaking ───────────────────────────────
    // in_degree[id] = number of project-internal deps not yet placed in a wave.
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    for id in &all_ids {
        in_degree.insert(id.clone(), deps_of[id].len());
    }

    let mut remaining: HashSet<String> = all_ids.clone();
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut placed: HashSet<String> = HashSet::new();
    let mut circular_dependencies: Vec<Vec<String>> = Vec::new();

    loop {
        // Collect nodes with zero in-degree (no unresolved deps).
        let mut zero: Vec<String> = remaining
            .iter()
            .filter(|id| in_degree.get(*id).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();

        if zero.is_empty() {
            if remaining.is_empty() {
                break;
            }
            // Stall — cycle detected. Find it, record it, break it.
            if let Some(cycle) = find_cycle(&remaining, &deps_of) {
                // Convert node IDs to paths for the report
                let cycle_paths: Vec<String> = cycle
                    .iter()
                    .map(|id| {
                        node_by_id
                            .get(id)
                            .map(|n| n.file_path.as_str().to_string())
                            .unwrap_or_else(|| id.clone())
                    })
                    .collect();
                circular_dependencies.push(cycle_paths);

                // Break the cycle: force-release the node in the cycle with
                // the lowest in-degree (least dependencies).
                // When in-degrees are tied, pick lexicographically smallest ID
                // for determinism. [ENG-AUD-2026-N10-0004]
                let break_node = cycle
                    .iter()
                    .filter(|id| remaining.contains(*id))
                    .min_by(|a, b| {
                        let da = in_degree.get(*a).copied().unwrap_or(0);
                        let db = in_degree.get(*b).copied().unwrap_or(0);
                        da.cmp(&db).then_with(|| a.cmp(b))
                    })
                    .cloned();

                if let Some(bn) = break_node {
                    zero.push(bn.clone());
                } else {
                    // Fallback: take the lexicographically smallest remaining
                    // node to avoid an infinite loop while remaining deterministic.
                    // [ENG-AUD-2026-N10-0004]
                    if let Some(first) = remaining.iter().min().cloned() {
                        zero.push(first);
                    } else {
                        break;
                    }
                }
            } else {
                // No cycle found but nodes remain (shouldn't happen); bail out.
                // Drain in sorted order for deterministic output.
                // [ENG-AUD-2026-N10-0004]
                let mut leftover: Vec<String> = remaining.drain().collect();
                leftover.sort();
                for id in leftover {
                    zero.push(id);
                }
            }
        }

        if zero.is_empty() {
            break;
        }

        // [ENG-AUD-2026-N10-0004] Sort at the top of every wave iteration so
        // that the wave order is fully deterministic regardless of which path
        // populated `zero` (initial seed, cycle-break, or fallback drain).
        // The initial seed comes from a HashSet iterator whose order is
        // hash-table-layout-dependent; the cycle-break paths may also yield
        // non-deterministic orderings without this sort.
        zero.sort();

        // Remove placed nodes and reduce in-degrees.
        for id in &zero {
            remaining.remove(id);
            placed.insert(id.clone());
        }
        for id in &zero {
            if let Some(dependents) = dependents_of.get(id) {
                for dep in dependents {
                    if remaining.contains(dep) {
                        let entry = in_degree.entry(dep.clone()).or_default();
                        *entry = entry.saturating_sub(1);
                    }
                }
            }
        }

        waves.push(zero);
    }

    // ── 4. Build WaveFile entries and compute per-file metadata ───────────────
    let mut wave_structs: Vec<MigrationWave> = Vec::new();
    let mut wave_theme_history: Vec<String> = Vec::new();

    // Collect prerequisites: all files in waves 0..wave_idx
    let mut cumulative_paths: Vec<String> = Vec::new();

    for (wave_idx, wave_ids) in waves.iter().enumerate() {
        let wave_number = (wave_idx + 1) as u32;

        let mut wave_files: Vec<WaveFile> = Vec::new();

        for id in wave_ids {
            let node = match node_by_id.get(id) {
                Some(n) => n,
                None => continue,
            };
            let path = node.file_path.as_str().to_string();
            let dep_count = deps_of.get(id).map(|v| v.len()).unwrap_or(0);
            let dependent_count = dependents_of.get(id).map(|v| v.len()).unwrap_or(0);
            let total_edges = total_edges_for.get(id).copied().unwrap_or(0);

            let reason = if wave_number == 1 {
                "No unmigrated dependencies — leaf node".to_string()
            } else if dependent_count > 5 {
                format!("Blocks {dependent_count} downstream files")
            } else {
                format!("Depends on {dep_count} files from earlier waves")
            };

            wave_files.push(WaveFile {
                path: path.clone(),
                file_type: file_type_from_path(&path),
                node_type: node.node_type.clone(),
                dependency_count: dep_count,
                dependent_count,
                reason,
                estimated_complexity: estimate_complexity(total_edges),
            });
        }

        // Sort files within a wave for stable output.
        wave_files.sort_by(|a, b| a.path.cmp(&b.path));

        let theme = theme_for_wave(&wave_files);

        // Compute prerequisites (flat list of all paths in earlier waves).
        let prerequisites: Vec<String> = cumulative_paths.clone();

        // Append this wave's paths to cumulative set.
        for f in &wave_files {
            cumulative_paths.push(f.path.clone());
        }

        // Strangler-fig checkpoint: after every 3rd wave or on theme transition.
        let theme_changed = wave_theme_history
            .last()
            .map(|prev| prev != &theme)
            .unwrap_or(false);
        let strangler_fig_checkpoint = wave_number.is_multiple_of(3) || theme_changed;

        wave_theme_history.push(theme.clone());

        wave_structs.push(MigrationWave {
            wave_number,
            theme,
            files: wave_files,
            prerequisites,
            strangler_fig_checkpoint,
        });
    }

    // ── 5. Bottleneck files (top 5 by dependent_count) ───────────────────────
    let mut all_wave_files: Vec<&WaveFile> =
        wave_structs.iter().flat_map(|w| w.files.iter()).collect();
    all_wave_files.sort_by(|a, b| b.dependent_count.cmp(&a.dependent_count));
    all_wave_files.dedup_by_key(|f| f.path.as_str());

    let bottleneck_files: Vec<BottleneckFile> = all_wave_files
        .iter()
        .take(5)
        .filter(|f| f.dependent_count > 0)
        .map(|f| BottleneckFile {
            path: f.path.clone(),
            blocks_count: f.dependent_count,
            suggestion: format!(
                "Extract shared logic from '{}' into a separate library module \
                 to reduce coupling and allow parallel migration of the {} \
                 dependent file(s).",
                f.path, f.dependent_count
            ),
        })
        .collect();

    // ── 6. Generate summary Markdown ─────────────────────────────────────────
    let summary = build_summary(
        project_id,
        &wave_structs,
        &bottleneck_files,
        &circular_dependencies,
    );

    Ok(MigrationOrderPlan {
        project_id: project_id.to_string(),
        total_files: file_nodes.len(),
        waves: wave_structs,
        circular_dependencies,
        bottleneck_files,
        summary,
    })
}

// ── Markdown summary builder ──────────────────────────────────────────────────

fn build_summary(
    project_id: &str,
    waves: &[MigrationWave],
    bottlenecks: &[BottleneckFile],
    cycles: &[Vec<String>],
) -> String {
    let mut md = String::new();
    md.push_str(&format!("# Migration Order Plan: {project_id}\n\n"));

    let total_files: usize = waves.iter().map(|w| w.files.len()).sum();
    md.push_str(&format!(
        "**{total_files} files** across **{} waves**\n\n",
        waves.len()
    ));

    // Visual dependency flow
    if !waves.is_empty() {
        md.push_str("## Dependency Flow\n\n```\n");
        for w in waves {
            let checkpoint = if w.strangler_fig_checkpoint {
                " ← Strangler-Fig Checkpoint"
            } else {
                ""
            };
            md.push_str(&format!(
                "Wave {:>2}: [{}] ({} files){}\n",
                w.wave_number,
                w.theme,
                w.files.len(),
                checkpoint
            ));
        }
        md.push_str("```\n\n");
    }

    // Wave details
    md.push_str("## Waves\n\n");
    for w in waves {
        md.push_str(&format!("### Wave {}: {}\n\n", w.wave_number, w.theme));
        if !w.prerequisites.is_empty() {
            md.push_str(&format!(
                "_Prerequisites: {} earlier files_\n\n",
                w.prerequisites.len()
            ));
        }
        md.push_str("| File | Type | Complexity | Reason |\n");
        md.push_str("|------|------|------------|--------|\n");
        for f in &w.files {
            md.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                f.path, f.file_type, f.estimated_complexity, f.reason
            ));
        }
        if w.strangler_fig_checkpoint {
            md.push_str(
                "\n> **Strangler-Fig Checkpoint** — validate integration before proceeding.\n",
            );
        }
        md.push('\n');
    }

    // Bottleneck files
    if !bottlenecks.is_empty() {
        md.push_str("## Bottleneck Files\n\n");
        md.push_str("| File | Blocks | Suggestion |\n");
        md.push_str("|------|--------|------------|\n");
        for b in bottlenecks {
            md.push_str(&format!(
                "| `{}` | {} | {} |\n",
                b.path, b.blocks_count, b.suggestion
            ));
        }
        md.push('\n');
    }

    // Circular dependencies
    if !cycles.is_empty() {
        md.push_str("## Circular Dependencies Detected\n\n");
        md.push_str(
            "> Cycles were automatically broken by removing the weakest back-edge,\n\
             > but manual refactoring is recommended.\n\n",
        );
        for (i, cycle) in cycles.iter().enumerate() {
            let cycle_str = cycle.join(" → ");
            md.push_str(&format!("{i}. `{cycle_str}`\n"));
        }
        md.push('\n');
    }

    md
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use engram_core::paths::RelPath;
    use engram_graph::store::{Edge, GraphStore, Node};
    use std::sync::Arc;
    use tempfile::TempDir;

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn make_store(dir: &TempDir) -> Arc<GraphStore> {
        let path = dir.path().join("graph.redb");
        Arc::new(GraphStore::open(&path).expect("open graph store"))
    }

    fn add_file_node(graph: &Arc<GraphStore>, project: &str, node_id: &str, path: &str) {
        let node = Node {
            node_id: node_id.to_string(),
            node_type: "file".to_string(),
            name: path.split('/').next_back().unwrap_or(path).to_string(),
            namespace: project.to_string(),
            language: "vb".to_string(),
            file_path: RelPath::new(path),
            start_line: 1,
            end_line: 100,
            generation: 1,
            metadata: None,
        };
        graph.upsert_nodes(project, &[node]).expect("upsert node");
    }

    fn add_dep_edge(graph: &Arc<GraphStore>, project: &str, from: &str, to: &str) {
        let edge = Edge {
            source_id: from.to_string(),
            target_id: to.to_string(),
            namespace: project.to_string(),
            language: "vb".to_string(),
            edge_kind: EdgeKind::Dependency,
            weight: 1,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        };
        graph.upsert_edges(project, &[edge]).expect("upsert edge");
    }

    // ── Test 1: Empty project ─────────────────────────────────────────────────

    #[test]
    fn test_empty_project() {
        let dir = TempDir::new().unwrap();
        let graph = make_store(&dir);
        let plan = suggest_migration_order(&graph, "empty_proj").unwrap();
        assert_eq!(plan.total_files, 0);
        assert!(plan.waves.is_empty());
        assert!(plan.circular_dependencies.is_empty());
        assert!(plan.bottleneck_files.is_empty());
        assert!(plan.summary.contains("No file nodes found"));
    }

    // ── Test 2: No dependencies — all files in wave 1 ────────────────────────

    #[test]
    fn test_no_dependencies_all_wave_1() {
        let dir = TempDir::new().unwrap();
        let graph = make_store(&dir);
        let proj = "nodeps";
        add_file_node(&graph, proj, "file:a.vb", "a.vb");
        add_file_node(&graph, proj, "file:b.vb", "b.vb");
        add_file_node(&graph, proj, "file:c.vb", "c.vb");

        let plan = suggest_migration_order(&graph, proj).unwrap();
        assert_eq!(plan.total_files, 3);
        assert_eq!(plan.waves.len(), 1);
        assert_eq!(plan.waves[0].wave_number, 1);
        assert_eq!(plan.waves[0].files.len(), 3);
        assert!(plan.circular_dependencies.is_empty());
    }

    // ── Test 3: Linear chain A→B→C — 3 waves ─────────────────────────────────

    #[test]
    fn test_linear_chain_three_waves() {
        let dir = TempDir::new().unwrap();
        let graph = make_store(&dir);
        let proj = "linear";
        // A depends on B; B depends on C → migrate order: C (w1), B (w2), A (w3)
        add_file_node(&graph, proj, "file:a.vb", "a.vb");
        add_file_node(&graph, proj, "file:b.vb", "b.vb");
        add_file_node(&graph, proj, "file:c.vb", "c.vb");
        add_dep_edge(&graph, proj, "file:a.vb", "file:b.vb");
        add_dep_edge(&graph, proj, "file:b.vb", "file:c.vb");

        let plan = suggest_migration_order(&graph, proj).unwrap();
        assert_eq!(plan.waves.len(), 3, "expected 3 waves for A→B→C chain");
        assert_eq!(plan.waves[0].wave_number, 1);
        assert_eq!(plan.waves[1].wave_number, 2);
        assert_eq!(plan.waves[2].wave_number, 3);

        // c.vb must be in wave 1, b.vb in wave 2, a.vb in wave 3
        let w1_paths: Vec<&str> = plan.waves[0]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let w2_paths: Vec<&str> = plan.waves[1]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let w3_paths: Vec<&str> = plan.waves[2]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(w1_paths.contains(&"c.vb"), "c.vb should be in wave 1");
        assert!(w2_paths.contains(&"b.vb"), "b.vb should be in wave 2");
        assert!(w3_paths.contains(&"a.vb"), "a.vb should be in wave 3");
    }

    // ── Test 4: Diamond A→B, A→C, B→D, C→D — 3 waves ────────────────────────

    #[test]
    fn test_diamond_three_waves() {
        let dir = TempDir::new().unwrap();
        let graph = make_store(&dir);
        let proj = "diamond";
        // D has no deps, B and C depend on D, A depends on B and C
        add_file_node(&graph, proj, "file:a.vb", "a.vb");
        add_file_node(&graph, proj, "file:b.vb", "b.vb");
        add_file_node(&graph, proj, "file:c.vb", "c.vb");
        add_file_node(&graph, proj, "file:d.vb", "d.vb");
        add_dep_edge(&graph, proj, "file:a.vb", "file:b.vb");
        add_dep_edge(&graph, proj, "file:a.vb", "file:c.vb");
        add_dep_edge(&graph, proj, "file:b.vb", "file:d.vb");
        add_dep_edge(&graph, proj, "file:c.vb", "file:d.vb");

        let plan = suggest_migration_order(&graph, proj).unwrap();
        assert_eq!(plan.waves.len(), 3, "diamond should produce 3 waves");

        let w1_paths: Vec<&str> = plan.waves[0]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let w2_paths: Vec<&str> = plan.waves[1]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let w3_paths: Vec<&str> = plan.waves[2]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert!(w1_paths.contains(&"d.vb"), "d.vb should be in wave 1");
        assert!(
            w2_paths.contains(&"b.vb") && w2_paths.contains(&"c.vb"),
            "b.vb and c.vb should both be in wave 2; got {:?}",
            w2_paths
        );
        assert!(w3_paths.contains(&"a.vb"), "a.vb should be in wave 3");
    }

    // ── Test 5: Circular dependency detection and breaking ────────────────────

    #[test]
    fn test_circular_dependency_detected_and_broken() {
        let dir = TempDir::new().unwrap();
        let graph = make_store(&dir);
        let proj = "cyclic";
        // A → B → C → A (cycle)
        add_file_node(&graph, proj, "file:a.vb", "a.vb");
        add_file_node(&graph, proj, "file:b.vb", "b.vb");
        add_file_node(&graph, proj, "file:c.vb", "c.vb");
        add_dep_edge(&graph, proj, "file:a.vb", "file:b.vb");
        add_dep_edge(&graph, proj, "file:b.vb", "file:c.vb");
        add_dep_edge(&graph, proj, "file:c.vb", "file:a.vb");

        let plan = suggest_migration_order(&graph, proj).unwrap();
        // All files must be placed despite the cycle
        let all_placed: usize = plan.waves.iter().map(|w| w.files.len()).sum();
        assert_eq!(all_placed, 3, "all 3 files must be placed even with cycle");
        // At least one cycle must be reported
        assert!(
            !plan.circular_dependencies.is_empty(),
            "cycle should be detected"
        );
        // The reported cycle must have at least 2 entries
        assert!(plan.circular_dependencies[0].len() >= 2);
    }

    // ── Test 6: Bottleneck identification ─────────────────────────────────────

    #[test]
    fn test_bottleneck_identification() {
        let dir = TempDir::new().unwrap();
        let graph = make_store(&dir);
        let proj = "bottleneck";
        // shared.vb has many dependents
        add_file_node(&graph, proj, "file:shared.vb", "shared.vb");
        for i in 0..6u32 {
            let id = format!("file:page{i}.aspx");
            let path = format!("page{i}.aspx");
            add_file_node(&graph, proj, &id, &path);
            add_dep_edge(&graph, proj, &id, "file:shared.vb");
        }

        let plan = suggest_migration_order(&graph, proj).unwrap();
        // shared.vb should be in wave 1 (no deps)
        assert!(!plan.waves.is_empty());
        assert!(
            plan.waves[0].files.iter().any(|f| f.path == "shared.vb"),
            "shared.vb should be in wave 1"
        );
        // shared.vb should appear as a bottleneck
        assert!(
            !plan.bottleneck_files.is_empty(),
            "bottleneck_files should not be empty"
        );
        assert_eq!(
            plan.bottleneck_files[0].path, "shared.vb",
            "shared.vb should be the top bottleneck"
        );
        assert_eq!(plan.bottleneck_files[0].blocks_count, 6);
    }

    // ── Test 7: Wave theme assignment ─────────────────────────────────────────

    #[test]
    fn test_wave_theme_assignment() {
        // Verify theme_for_wave returns correct themes for different compositions.
        let make_wave_files = |types: &[&str]| -> Vec<WaveFile> {
            types
                .iter()
                .map(|t| WaveFile {
                    path: format!("test.{t}"),
                    file_type: t.to_string(),
                    node_type: "file".to_string(),
                    dependency_count: 0,
                    dependent_count: 0,
                    reason: String::new(),
                    estimated_complexity: "Low".to_string(),
                })
                .collect()
        };

        let config_files = make_wave_files(&["config", "config"]);
        assert!(theme_for_wave(&config_files).contains("Configuration"));

        let ascx_files = make_wave_files(&["ascx", "ascx", "ascx"]);
        assert!(theme_for_wave(&ascx_files).contains("Components"));

        let master_files = make_wave_files(&["master"]);
        assert!(theme_for_wave(&master_files).contains("Layout"));

        let svc_files = make_wave_files(&["asmx", "ashx", "svc"]);
        assert!(theme_for_wave(&svc_files).contains("Services"));

        let page_files = make_wave_files(&["aspx", "aspx", "code-behind"]);
        assert!(theme_for_wave(&page_files).contains("Pages"));

        let lib_files = make_wave_files(&["vb", "vb", "cs"]);
        assert!(theme_for_wave(&lib_files).contains("Foundation"));
    }

    // ── Test 8: Strangler-fig checkpoints ────────────────────────────────────

    #[test]
    fn test_strangler_fig_checkpoints() {
        let dir = TempDir::new().unwrap();
        let graph = make_store(&dir);
        let proj = "sfig";
        // Create a 4-wave linear chain: a→b→c→d
        add_file_node(&graph, proj, "file:a.vb", "a.vb");
        add_file_node(&graph, proj, "file:b.vb", "b.vb");
        add_file_node(&graph, proj, "file:c.vb", "c.vb");
        add_file_node(&graph, proj, "file:d.vb", "d.vb");
        add_dep_edge(&graph, proj, "file:a.vb", "file:b.vb");
        add_dep_edge(&graph, proj, "file:b.vb", "file:c.vb");
        add_dep_edge(&graph, proj, "file:c.vb", "file:d.vb");

        let plan = suggest_migration_order(&graph, proj).unwrap();
        assert_eq!(plan.waves.len(), 4);

        // Wave 3 must have a checkpoint (wave_number % 3 == 0)
        assert!(
            plan.waves[2].strangler_fig_checkpoint,
            "wave 3 should have a strangler-fig checkpoint"
        );
    }

    // ── Test 9: Complexity estimation ────────────────────────────────────────

    #[test]
    fn test_complexity_estimation() {
        assert_eq!(estimate_complexity(0), "Low");
        assert_eq!(estimate_complexity(3), "Low");
        assert_eq!(estimate_complexity(4), "Medium");
        assert_eq!(estimate_complexity(8), "Medium");
        assert_eq!(estimate_complexity(9), "High");
        assert_eq!(estimate_complexity(100), "High");
    }

    // ── Test 10: file_type_from_path ─────────────────────────────────────────

    #[test]
    fn test_file_type_from_path() {
        assert_eq!(file_type_from_path("Default.aspx"), "aspx");
        assert_eq!(file_type_from_path("Default.aspx.vb"), "code-behind");
        assert_eq!(file_type_from_path("Ctrl.ascx"), "ascx");
        assert_eq!(file_type_from_path("Ctrl.ascx.cs"), "control-code-behind");
        assert_eq!(file_type_from_path("Site.master"), "master");
        assert_eq!(file_type_from_path("Service.asmx"), "asmx");
        assert_eq!(file_type_from_path("Handler.ashx"), "ashx");
        assert_eq!(file_type_from_path("Web.config"), "config");
        assert_eq!(file_type_from_path("Global.asax"), "asax");
        assert_eq!(file_type_from_path("Report.rdlc"), "report");
        assert_eq!(file_type_from_path("legacy.asp"), "asp");
        assert_eq!(file_type_from_path("unknown.xyz"), "other");
    }

    // ── Test 11: ENG-AUD-2026-N10-0004 — topological sort is deterministic ───
    //
    // Builds a graph with 5 nodes and multiple dependency edges, runs the sort
    // twice with the same input, and asserts the resulting wave sequences are
    // identical. This guards against nondeterminism from HashSet iteration.

    #[test]
    fn topo_sort_is_deterministic_across_repeated_calls() {
        // Graph layout (A depends on E, B depends on E, C depends on A and B,
        // D depends on C — produces waves: [E] → [A, B] → [C] → [D]):
        //   E  (no deps)
        //   A → E
        //   B → E
        //   C → A, B
        //   D → C

        let run_once = |proj: &str| {
            let dir = TempDir::new().unwrap();
            let graph = make_store(&dir);
            add_file_node(&graph, proj, "node:e.vb", "e.vb");
            add_file_node(&graph, proj, "node:a.vb", "a.vb");
            add_file_node(&graph, proj, "node:b.vb", "b.vb");
            add_file_node(&graph, proj, "node:c.vb", "c.vb");
            add_file_node(&graph, proj, "node:d.vb", "d.vb");
            add_dep_edge(&graph, proj, "node:a.vb", "node:e.vb");
            add_dep_edge(&graph, proj, "node:b.vb", "node:e.vb");
            add_dep_edge(&graph, proj, "node:c.vb", "node:a.vb");
            add_dep_edge(&graph, proj, "node:c.vb", "node:b.vb");
            add_dep_edge(&graph, proj, "node:d.vb", "node:c.vb");
            suggest_migration_order(&graph, proj).unwrap()
        };

        // Run twice with independent stores (same logical graph, different
        // in-memory hash states to expose any HashSet ordering sensitivity).
        let plan1 = run_once("det_test_run1");
        let plan2 = run_once("det_test_run2");

        // Both plans must have the same number of waves.
        assert_eq!(
            plan1.waves.len(),
            plan2.waves.len(),
            "wave count must be identical across runs"
        );

        // Each wave must list files in the same order.
        for (w1, w2) in plan1.waves.iter().zip(plan2.waves.iter()) {
            let paths1: Vec<&str> = w1.files.iter().map(|f| f.path.as_str()).collect();
            let paths2: Vec<&str> = w2.files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(
                paths1, paths2,
                "wave {} file order must be identical: run1={:?} run2={:?}",
                w1.wave_number, paths1, paths2
            );
        }

        // Validate the expected wave structure (regression guard).
        assert_eq!(
            plan1.waves.len(),
            4,
            "expected 4 waves for 5-node fan-in chain"
        );

        let wave1_paths: Vec<&str> = plan1.waves[0]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let wave2_paths: Vec<&str> = plan1.waves[1]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let wave3_paths: Vec<&str> = plan1.waves[2]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let wave4_paths: Vec<&str> = plan1.waves[3]
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();

        assert_eq!(wave1_paths, vec!["e.vb"], "wave 1 must contain only e.vb");
        assert_eq!(
            wave2_paths,
            vec!["a.vb", "b.vb"],
            "wave 2 must be [a.vb, b.vb] sorted"
        );
        assert_eq!(wave3_paths, vec!["c.vb"], "wave 3 must contain only c.vb");
        assert_eq!(wave4_paths, vec!["d.vb"], "wave 4 must contain only d.vb");
    }
}
