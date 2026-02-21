//! Design anti-pattern detection via graph heuristics.
//!
//! Runs 5 deterministic checks against the project graph:
//! 1. God Object — class/file with too many Contains edges
//! 2. Spaghetti Events — function with Dependency in-edges from many files
//! 3. Session Soup — Session key accessed from many files
//! 4. SqlDataSource Coupling — node with both SqlCalls + DataBinding
//! 5. Tight GIS Coupling — node with both SpatialCall + DataBinding

use engram_graph::store::{EdgeKind, GraphStore};
use std::collections::{HashMap, HashSet};

// ── Public types ──────────────────────────────────────────────────────────

/// Severity of a detected design anti-pattern.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
pub enum AntiPatternSeverity {
    Minor,
    Moderate,
    Severe,
}

impl std::fmt::Display for AntiPatternSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AntiPatternSeverity::Minor => f.write_str("Minor"),
            AntiPatternSeverity::Moderate => f.write_str("Moderate"),
            AntiPatternSeverity::Severe => f.write_str("Severe"),
        }
    }
}

/// A detected design anti-pattern in the codebase.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DesignAntiPattern {
    pub pattern_name: String,
    pub description: String,
    pub severity: AntiPatternSeverity,
    pub affected_nodes: Vec<String>,
    pub evidence: Vec<String>,
    pub modern_target: String,
    pub refactoring_steps: Vec<String>,
}

// ── Detection engine ──────────────────────────────────────────────────────

/// Detect design anti-patterns by analyzing graph structure.
///
/// Runs 5 deterministic heuristics (no LLM required):
/// 1. **God Object** — classes/files with > `god_threshold` Contains edges
/// 2. **Spaghetti Events** — functions targeted by > `spaghetti_threshold` Dependency edges from different files
/// 3. **Session Soup** — Session state keys accessed from > `soup_threshold` different files
/// 4. **SqlDataSource Coupling** — nodes with both SqlCalls + DataBinding edges
/// 5. **Tight GIS Coupling** — files with both SpatialCall + DataBinding edges
pub fn detect_design_antipatterns(
    graph: &GraphStore,
    project_id: &str,
    god_threshold: usize,
    spaghetti_threshold: usize,
    soup_threshold: usize,
) -> anyhow::Result<Vec<DesignAntiPattern>> {
    let mut patterns = Vec::new();

    // 1. God Object: class/file nodes with too many Contains edges
    let contains_edges = graph.list_edges_by_kind(project_id, EdgeKind::Contains, 10_000)?;
    let mut contains_out: HashMap<String, Vec<String>> = HashMap::new();
    for e in &contains_edges {
        contains_out
            .entry(e.source_id.clone())
            .or_default()
            .push(e.target_id.clone());
    }
    for (node_id, children) in &contains_out {
        if children.len() > god_threshold {
            let node = graph.get_node(project_id, node_id)?;
            let node_type = node
                .as_ref()
                .map(|n| n.node_type.as_str())
                .unwrap_or("unknown");
            if matches!(node_type, "class" | "file") {
                let node_name = node
                    .as_ref()
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| node_id.clone());
                patterns.push(DesignAntiPattern {
                    pattern_name: "God Object".into(),
                    description: format!(
                        "{} contains {} members (threshold: {}). \
                         This violates the Single Responsibility Principle.",
                        node_name,
                        children.len(),
                        god_threshold
                    ),
                    severity: if children.len() > god_threshold * 2 {
                        AntiPatternSeverity::Severe
                    } else {
                        AntiPatternSeverity::Moderate
                    },
                    affected_nodes: vec![node_id.clone()],
                    evidence: children.iter().take(5).cloned().collect(),
                    modern_target:
                        "Split into focused service classes (Single Responsibility Principle). \
                         Each class should have one reason to change."
                            .into(),
                    refactoring_steps: vec![
                        "Identify cohesive groups of methods by data affinity".into(),
                        "Extract each group into a dedicated service class".into(),
                        "Create interfaces for dependency injection".into(),
                        "Route callers through the new services".into(),
                    ],
                });
            }
        }
    }

    // 2. Spaghetti Events: functions with many incoming Dependency edges from different files
    let dep_edges = graph.list_edges_by_kind(project_id, EdgeKind::Dependency, 10_000)?;
    let mut dep_in: HashMap<String, HashSet<String>> = HashMap::new();
    for e in &dep_edges {
        let source_file = if e.source_id.starts_with("file:") {
            e.source_id.clone()
        } else {
            // Extract file from sym:kind:path:name:line
            let parts: Vec<&str> = e.source_id.splitn(4, ':').collect();
            if parts.len() >= 3 {
                format!("file:{}", parts[2])
            } else {
                e.source_id.clone()
            }
        };
        dep_in
            .entry(e.target_id.clone())
            .or_default()
            .insert(source_file);
    }
    for (node_id, source_files) in &dep_in {
        if source_files.len() > spaghetti_threshold {
            let node = graph.get_node(project_id, node_id)?;
            let node_type = node
                .as_ref()
                .map(|n| n.node_type.as_str())
                .unwrap_or("unknown");
            if matches!(node_type, "function" | "class") {
                let node_name = node
                    .as_ref()
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| node_id.clone());
                patterns.push(DesignAntiPattern {
                    pattern_name: "Spaghetti Events".into(),
                    description: format!(
                        "{} is called from {} different files (threshold: {}). \
                         Indicates tight coupling and fragile event chains.",
                        node_name,
                        source_files.len(),
                        spaghetti_threshold
                    ),
                    severity: if source_files.len() > spaghetti_threshold * 2 {
                        AntiPatternSeverity::Severe
                    } else {
                        AntiPatternSeverity::Moderate
                    },
                    affected_nodes: vec![node_id.clone()],
                    evidence: source_files.iter().take(5).cloned().collect(),
                    modern_target: "Mediator Pattern (MediatR) or Event Bus. \
                                    Decouple callers from the handler."
                        .into(),
                    refactoring_steps: vec![
                        "Define IRequest/INotification for each event type".into(),
                        "Move handler logic into IRequestHandler implementations".into(),
                        "Replace direct calls with mediator.Send(request)".into(),
                        "Register handlers in DI container".into(),
                    ],
                });
            }
        }
    }

    // 3. Session Soup: Session state keys accessed from many different files
    let reads_edges = graph.list_edges_by_kind(project_id, EdgeKind::ReadsState, 5_000)?;
    let writes_edges = graph.list_edges_by_kind(project_id, EdgeKind::WritesState, 5_000)?;
    let mut state_files: HashMap<String, HashSet<String>> = HashMap::new();
    for e in reads_edges.iter().chain(writes_edges.iter()) {
        if e.target_id.starts_with("state:Session:") {
            let source_file = if e.source_id.starts_with("file:") {
                e.source_id.clone()
            } else {
                let parts: Vec<&str> = e.source_id.splitn(4, ':').collect();
                if parts.len() >= 3 {
                    format!("file:{}", parts[2])
                } else {
                    e.source_id.clone()
                }
            };
            state_files
                .entry(e.target_id.clone())
                .or_default()
                .insert(source_file);
        }
    }
    for (state_key, files) in &state_files {
        if files.len() > soup_threshold {
            let key_name = state_key
                .strip_prefix("state:Session:")
                .unwrap_or(state_key);
            patterns.push(DesignAntiPattern {
                pattern_name: "Session Soup".into(),
                description: format!(
                    "Session key '{}' is accessed from {} different files (threshold: {}). \
                     Session overuse makes stateless migration difficult.",
                    key_name,
                    files.len(),
                    soup_threshold
                ),
                severity: if files.len() > soup_threshold * 2 {
                    AntiPatternSeverity::Severe
                } else {
                    AntiPatternSeverity::Moderate
                },
                affected_nodes: vec![state_key.clone()],
                evidence: files.iter().take(5).cloned().collect(),
                modern_target: "Repository Pattern + REST API with JWT claims. \
                                Replace Session with proper state management."
                    .into(),
                refactoring_steps: vec![
                    "Identify the data lifecycle for this session key".into(),
                    "Create a typed DTO/ViewModel to replace the session slot".into(),
                    "Expose via REST API endpoint with proper auth".into(),
                    "Use JWT claims for auth-related state, Redis for shared session".into(),
                ],
            });
        }
    }

    // 4. SqlDataSource Coupling: nodes with both SqlCalls + DataBinding in the same file
    let sql_edges = graph.list_edges_by_kind(project_id, EdgeKind::SqlCalls, 5_000)?;
    let binding_edges = graph.list_edges_by_kind(project_id, EdgeKind::DataBinding, 5_000)?;
    let sql_sources: HashSet<_> = sql_edges.iter().map(|e| &e.source_id).collect();
    let binding_sources: HashSet<_> = binding_edges.iter().map(|e| &e.source_id).collect();
    let coupled: Vec<_> = sql_sources
        .intersection(&binding_sources)
        .cloned()
        .collect();
    for node_id in &coupled {
        let node = graph.get_node(project_id, node_id)?;
        let node_name = node
            .as_ref()
            .map(|n| n.name.clone())
            .unwrap_or_else(|| (*node_id).clone());
        patterns.push(DesignAntiPattern {
            pattern_name: "SqlDataSource Coupling".into(),
            description: format!(
                "{} has both SQL data access and UI data binding in the same scope. \
                 This violates separation of concerns.",
                node_name
            ),
            severity: AntiPatternSeverity::Moderate,
            affected_nodes: vec![(*node_id).clone()],
            evidence: vec![
                format!("SQL edges from {}", node_id),
                format!("DataBinding edges from {}", node_id),
            ],
            modern_target: "Repository Pattern + Typed DTOs + Blazor/Razor component binding. \
                            Separate data access from UI."
                .into(),
            refactoring_steps: vec![
                "Extract SQL queries into a Repository class".into(),
                "Define DTOs for data transfer between layers".into(),
                "Bind UI to DTOs via ViewModel/Controller".into(),
                "Remove SqlDataSource controls from markup".into(),
            ],
        });
    }

    // 5. Tight GIS Coupling: files with both SpatialCall + DataBinding
    let gis_edges = graph.list_edges_by_kind(project_id, EdgeKind::SpatialCall, 5_000)?;
    let gis_sources: HashSet<_> = gis_edges.iter().map(|e| &e.source_id).collect();
    let gis_coupled: Vec<_> = gis_sources
        .intersection(&binding_sources)
        .cloned()
        .collect();
    for node_id in &gis_coupled {
        let node = graph.get_node(project_id, node_id)?;
        let node_name = node
            .as_ref()
            .map(|n| n.name.clone())
            .unwrap_or_else(|| (*node_id).clone());
        patterns.push(DesignAntiPattern {
            pattern_name: "Tight GIS Coupling".into(),
            description: format!(
                "{} mixes GIS map logic with UI data binding. \
                 Isolate spatial logic for independent migration.",
                node_name
            ),
            severity: AntiPatternSeverity::Minor,
            affected_nodes: vec![(*node_id).clone()],
            evidence: vec![
                format!("SpatialCall edges from {}", node_id),
                format!("DataBinding edges from {}", node_id),
            ],
            modern_target: "Dedicated GIS service layer + React map component (react-leaflet or \
                            @react-google-maps/api)."
                .into(),
            refactoring_steps: vec![
                "Extract GIS logic into a standalone service class".into(),
                "Create a REST API for coordinate/polygon data".into(),
                "Replace legacy map widget with React map component".into(),
                "Pass GIS data via props/state, not server-rendered scripts".into(),
            ],
        });
    }

    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_display() {
        assert_eq!(AntiPatternSeverity::Minor.to_string(), "Minor");
        assert_eq!(AntiPatternSeverity::Moderate.to_string(), "Moderate");
        assert_eq!(AntiPatternSeverity::Severe.to_string(), "Severe");
    }

    #[test]
    fn test_design_antipattern_struct() {
        let p = DesignAntiPattern {
            pattern_name: "God Object".into(),
            description: "Too many members".into(),
            severity: AntiPatternSeverity::Severe,
            affected_nodes: vec!["file:test.vb".into()],
            evidence: vec!["child1".into(), "child2".into()],
            modern_target: "SRP".into(),
            refactoring_steps: vec!["Step 1".into()],
        };
        assert_eq!(p.pattern_name, "God Object");
        assert_eq!(p.severity, AntiPatternSeverity::Severe);
        assert_eq!(p.affected_nodes.len(), 1);
        assert_eq!(p.refactoring_steps.len(), 1);
    }

    #[test]
    fn test_detect_on_empty_graph() {
        let dir = tempfile::tempdir().unwrap();
        let graph = GraphStore::open(&dir.path().join("graph.redb")).unwrap();
        let result = detect_design_antipatterns(&graph, "test_proj", 20, 10, 5).unwrap();
        assert!(
            result.is_empty(),
            "Empty graph should yield no anti-patterns"
        );
    }
}
