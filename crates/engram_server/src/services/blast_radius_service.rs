/// Migration Blast Radius Analysis.
///
/// Computes a composite migration complexity score for a file or symbol node,
/// factoring in event wiring density, SQL concatenation risk, PageRank centrality,
/// state coupling, GIS dependencies, and script injection coupling.
///
/// Returns a `BlastRadiusReport` with a 1-10 risk score, complexity breakdown,
/// seam candidates (logical refactoring cut-points), and agentic guidance.
use engram_graph::store::{EdgeKind, GraphStore};
use engram_index::solution_parser::{self, SolutionStructure};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ── Output types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskBand {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskBand {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskBand::Low => "Low",
            RiskBand::Medium => "Medium",
            RiskBand::High => "High",
            RiskBand::Critical => "Critical",
        }
    }
}

impl std::fmt::Display for RiskBand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexityBreakdown {
    /// Event wiring complexity (Handles clauses, event bindings). 0-10.
    pub handles_clause_score: f32,
    /// SQL injection risk (inline SQL, concatenation). 0-10.
    pub sql_concat_score: f32,
    /// Architectural centrality (PageRank-based). 0-10.
    pub pagerank_score: f32,
    /// State dependency density (Session/ViewState/Application). 0-10.
    pub state_coupling_score: f32,
    /// GIS dependency weight (SpatialCall edges). 0-10.
    pub gis_coupling_score: f32,
    /// Server-to-client script injection coupling. 0-10.
    pub script_injection_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UncertaintyBreakdown {
    /// Dynamic control/event synthesis and runtime UI composition uncertainty. 0-10.
    pub dynamic_ui_uncertainty_score: f32,
    /// Late-bound/member-resolution uncertainty. 0-10.
    pub late_binding_uncertainty_score: f32,
    /// Dynamic SQL/table-inference uncertainty. 0-10.
    pub dynamic_sql_uncertainty_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeamCandidate {
    pub node_id: String,
    pub node_type: String,
    pub reason: String,
    pub edge_kinds_crossing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuidanceItem {
    pub concern: String,
    pub severity: String,
    pub recommendation: String,
    pub modern_pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlastRadiusReport {
    pub target: String,
    pub target_type: String,
    pub migration_risk: u8,
    pub risk_band: RiskBand,
    pub complexity_breakdown: ComplexityBreakdown,
    pub uncertainty_breakdown: UncertaintyBreakdown,
    pub seam_candidates: Vec<SeamCandidate>,
    pub guidance: Vec<GuidanceItem>,
    pub total_downstream: usize,
}

fn meta_bool(metadata: Option<&serde_json::Value>, key: &str) -> bool {
    metadata
        .and_then(|m| m.get(key))
        .and_then(|v| match v {
            serde_json::Value::Bool(b) => Some(*b),
            serde_json::Value::String(s) => Some(matches!(s.as_str(), "true" | "1" | "yes")),
            serde_json::Value::Number(n) => Some(n.as_i64().unwrap_or_default() > 0),
            _ => None,
        })
        .unwrap_or(false)
}

fn meta_str_eq(metadata: Option<&serde_json::Value>, key: &str, expected: &str) -> bool {
    metadata
        .and_then(|m| m.get(key))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn meta_f32(metadata: Option<&serde_json::Value>, key: &str) -> Option<f32> {
    let value = metadata?.get(key)?;
    match value {
        serde_json::Value::Number(n) => n.as_f64().map(|v| v as f32),
        serde_json::Value::String(s) => s.parse::<f32>().ok(),
        _ => None,
    }
}

// ── Weights ──────────────────────────────────────────────────────────────────

const WEIGHT_HANDLES: f32 = 0.20;
const WEIGHT_SQL: f32 = 0.25;
const WEIGHT_PAGERANK: f32 = 0.15;
const WEIGHT_STATE: f32 = 0.20;
const WEIGHT_GIS: f32 = 0.10;
const WEIGHT_SCRIPT: f32 = 0.10;

// ── Scoring helpers ──────────────────────────────────────────────────────────

/// Normalize a raw count into a 0-10 score using a saturation curve.
/// `saturation` is the count at which the score approaches 10.
fn normalize_score(count: usize, saturation: usize) -> f32 {
    if saturation == 0 {
        return 0.0;
    }
    let ratio = count as f32 / saturation as f32;
    (ratio * 10.0).min(10.0)
}

fn risk_band(score: u8) -> RiskBand {
    match score {
        1..=3 => RiskBand::Low,
        4..=6 => RiskBand::Medium,
        7..=8 => RiskBand::High,
        _ => RiskBand::Critical,
    }
}

// ── Core computation ─────────────────────────────────────────────────────────

/// Compute the migration blast radius for a node.
///
/// `target_id` should be a valid node ID (e.g., `file:path/to/File.aspx.vb`
/// or `sym:function:Namespace.Class.Method`).
pub fn compute_blast_radius(
    graph: &GraphStore,
    project_id: &str,
    target_id: &str,
    generation: u64,
    include_guidance: bool,
) -> anyhow::Result<BlastRadiusReport> {
    // 1. Fetch target node metadata
    let node = graph.get_node(project_id, target_id)?;
    let node_type = node
        .as_ref()
        .map(|n| n.node_type.clone())
        .unwrap_or_else(|| "unknown".into());

    // 2. Count outgoing edges by kind
    let mut out_counts: HashMap<EdgeKind, usize> = HashMap::new();
    for kind in EdgeKind::ALL {
        let neighbors = graph.neighbors(project_id, kind.clone(), target_id, 500)?;
        if !neighbors.is_empty() {
            out_counts.insert(kind.clone(), neighbors.len());
        }
    }

    // 3. Count incoming edges by kind
    let incoming = graph.find_incoming_edges_with_kind(project_id, None, target_id, 1000)?;
    let mut in_counts: HashMap<EdgeKind, usize> = HashMap::new();
    let mut in_sources_by_kind: HashMap<EdgeKind, HashSet<String>> = HashMap::new();
    for (source_id, kind, _weight) in &incoming {
        *in_counts.entry(kind.clone()).or_default() += 1;
        in_sources_by_kind
            .entry(kind.clone())
            .or_default()
            .insert(source_id.clone());
    }

    // 4. Compute sub-scores
    // Handles/event complexity: incoming Dependency + Contains edges → event wiring complexity
    let handles_count = in_counts.get(&EdgeKind::Dependency).copied().unwrap_or(0)
        + out_counts.get(&EdgeKind::Contains).copied().unwrap_or(0);
    let handles_clause_score = normalize_score(handles_count, 15);

    // SQL risk: SqlCalls edges (both in and out)
    let sql_count = out_counts.get(&EdgeKind::SqlCalls).copied().unwrap_or(0)
        + in_counts.get(&EdgeKind::SqlCalls).copied().unwrap_or(0)
        + out_counts
            .get(&EdgeKind::QueriesTable)
            .copied()
            .unwrap_or(0);
    let sql_concat_score = normalize_score(sql_count, 10);

    // PageRank centrality
    let pagerank_score =
        match engram_graph::analysis::compute_pagerank(graph, project_id, generation) {
            Ok(metrics) => {
                let pr = metrics.pagerank.get(target_id).copied().unwrap_or(0.0);
                // Normalize: typical high PageRank in a medium project is 0.01-0.05
                (pr * 200.0).min(10.0)
            }
            Err(_) => 0.0,
        };

    // State coupling: reads + writes + affinity
    let state_count = out_counts.get(&EdgeKind::ReadsState).copied().unwrap_or(0)
        + out_counts.get(&EdgeKind::WritesState).copied().unwrap_or(0)
        + out_counts
            .get(&EdgeKind::StateAffinity)
            .copied()
            .unwrap_or(0)
        + in_counts.get(&EdgeKind::ReadsState).copied().unwrap_or(0)
        + in_counts.get(&EdgeKind::WritesState).copied().unwrap_or(0);
    let state_coupling_score = normalize_score(state_count, 10);

    // GIS coupling
    let gis_count = out_counts.get(&EdgeKind::SpatialCall).copied().unwrap_or(0)
        + in_counts.get(&EdgeKind::SpatialCall).copied().unwrap_or(0);
    let gis_coupling_score = normalize_score(gis_count, 5);

    // Script injection
    let script_count = out_counts
        .get(&EdgeKind::InjectsScript)
        .copied()
        .unwrap_or(0)
        + in_counts
            .get(&EdgeKind::InjectsScript)
            .copied()
            .unwrap_or(0);
    let script_injection_score = normalize_score(script_count, 5);

    // Runtime uncertainty: dynamic UI, late binding, and probabilistic SQL/table inference.
    let touching_edges = graph
        .list_edges(project_id, None)?
        .into_iter()
        .filter(|edge| edge.source_id == target_id || edge.target_id == target_id)
        .collect::<Vec<_>>();

    let node_meta = node.as_ref().and_then(|n| n.metadata.as_ref());
    let mut dynamic_ui_signals = usize::from(meta_bool(node_meta, "dynamic_control"));
    let mut late_binding_signals = usize::from(meta_bool(node_meta, "has_late_binding"));
    let mut probabilistic_resolution_signals =
        usize::from(meta_str_eq(node_meta, "resolution", "probabilistic"));
    let mut table_inference_confidences = Vec::new();
    if let Some(conf) = meta_f32(node_meta, "table_inference_confidence") {
        table_inference_confidences.push(conf);
    }

    for edge in &touching_edges {
        let meta = edge.metadata.as_ref();
        if meta_bool(meta, "dynamic_control") {
            dynamic_ui_signals += 1;
        }
        if meta_bool(meta, "has_late_binding") {
            late_binding_signals += 1;
        }
        if meta_str_eq(meta, "resolution", "probabilistic") {
            probabilistic_resolution_signals += 1;
        }
        if let Some(conf) = meta_f32(meta, "table_inference_confidence") {
            table_inference_confidences.push(conf);
        }
    }

    let dynamic_ui_uncertainty_score = normalize_score(dynamic_ui_signals, 4);
    let late_binding_uncertainty_score =
        normalize_score(late_binding_signals + probabilistic_resolution_signals, 4);
    let low_confidence_table_signals = table_inference_confidences
        .iter()
        .copied()
        .filter(|confidence| *confidence < 0.80)
        .count();
    let avg_table_unknown = if table_inference_confidences.is_empty() {
        0.0
    } else {
        let avg_confidence = table_inference_confidences.iter().sum::<f32>()
            / table_inference_confidences.len() as f32;
        (1.0 - avg_confidence.clamp(0.0, 1.0)) * 10.0
    };
    let dynamic_sql_uncertainty_score = (normalize_score(
        low_confidence_table_signals + probabilistic_resolution_signals,
        4,
    ) * 0.7
        + avg_table_unknown * 0.3)
        .min(10.0);
    let uncertainty_composite = (dynamic_ui_uncertainty_score * 0.35
        + late_binding_uncertainty_score * 0.35
        + dynamic_sql_uncertainty_score * 0.30)
        .min(10.0);

    // 5. Composite risk score
    let base_weighted_score = handles_clause_score * WEIGHT_HANDLES
        + sql_concat_score * WEIGHT_SQL
        + pagerank_score * WEIGHT_PAGERANK
        + state_coupling_score * WEIGHT_STATE
        + gis_coupling_score * WEIGHT_GIS
        + script_injection_score * WEIGHT_SCRIPT;
    let blended_score = (base_weighted_score * 0.80) + (uncertainty_composite * 0.20);
    let unresolved_uplift = ((uncertainty_composite - 3.0).max(0.0) * 0.20).min(1.5);
    let raw_score = (blended_score + unresolved_uplift).min(10.0);
    let migration_risk = (raw_score.round() as u8).clamp(1, 10);

    // 6. Total downstream count
    let total_downstream: usize = out_counts.values().sum();

    // 7. Seam detection: boundary nodes where edge kinds change
    let mut seam_candidates = Vec::new();
    let in_kinds: HashSet<_> = in_counts.keys().collect();
    let out_kinds: HashSet<_> = out_counts.keys().collect();
    let boundary_kinds: Vec<_> = in_kinds.symmetric_difference(&out_kinds).collect();

    if !boundary_kinds.is_empty() {
        let crossing: Vec<String> = boundary_kinds
            .iter()
            .map(|k| k.as_str().to_string())
            .collect();
        seam_candidates.push(SeamCandidate {
            node_id: target_id.to_string(),
            node_type: node_type.clone(),
            reason: format!(
                "Edge kind boundary: {} incoming kinds vs {} outgoing kinds differ",
                in_kinds.len(),
                out_kinds.len()
            ),
            edge_kinds_crossing: crossing,
        });
    }

    // Also look for neighbor nodes that straddle different edge kinds
    for (kind, sources) in &in_sources_by_kind {
        for source in sources.iter().take(5) {
            // Check if this source also has outgoing edges to different kinds of targets
            let src_out = graph.neighbors(project_id, kind.clone(), source, 50)?;
            if src_out.len() > 10 {
                seam_candidates.push(SeamCandidate {
                    node_id: source.clone(),
                    node_type: "function".into(),
                    reason: format!(
                        "High fan-out ({} {} edges from single source)",
                        src_out.len(),
                        kind.as_str()
                    ),
                    edge_kinds_crossing: vec![kind.as_str().to_string()],
                });
            }
        }
    }

    // Limit seam candidates
    seam_candidates.truncate(10);

    // 8. Guidance generation
    let mut guidance = Vec::new();
    if include_guidance {
        if handles_clause_score > 5.0 {
            guidance.push(GuidanceItem {
                concern: "Event Wiring Complexity".into(),
                severity: if handles_clause_score > 7.0 {
                    "high"
                } else {
                    "medium"
                }
                .into(),
                recommendation: "Decouple event handlers using Mediator/Event Bus pattern. \
                    Extract handler logic into service classes."
                    .into(),
                modern_pattern: Some("MediatR IRequestHandler<TRequest, TResponse>".into()),
            });
        }
        if sql_concat_score > 5.0 {
            guidance.push(GuidanceItem {
                concern: "SQL Injection Risk / Data Access Coupling".into(),
                severity: if sql_concat_score > 7.0 {
                    "high"
                } else {
                    "medium"
                }
                .into(),
                recommendation: "Replace inline SQL with parameterized queries in a Repository. \
                    Use Entity Framework Core or Dapper with typed DTOs."
                    .into(),
                modern_pattern: Some("Repository<T> + IDbConnection (Dapper)".into()),
            });
        }
        if pagerank_score > 5.0 {
            guidance.push(GuidanceItem {
                concern: "Architectural Centrality".into(),
                severity: if pagerank_score > 7.0 {
                    "high"
                } else {
                    "medium"
                }
                .into(),
                recommendation: "This node is a hub — many other nodes depend on it. \
                    Migrate it early (Wave 0/1) and provide a facade adapter for consumers."
                    .into(),
                modern_pattern: Some("Strangler Fig Pattern with API Gateway".into()),
            });
        }
        if state_coupling_score > 5.0 {
            guidance.push(GuidanceItem {
                concern: "State Dependency Density".into(),
                severity: if state_coupling_score > 7.0 {
                    "high"
                } else {
                    "medium"
                }
                .into(),
                recommendation: "Extract session/viewstate into dedicated state service. \
                    Use JWT claims for auth state, Redis for shared session."
                    .into(),
                modern_pattern: Some("IDistributedCache + JWT ClaimsPrincipal".into()),
            });
        }
        if gis_coupling_score > 5.0 {
            guidance.push(GuidanceItem {
                concern: "GIS Library Coupling".into(),
                severity: "medium".into(),
                recommendation: "Isolate GIS logic into a dedicated service layer. \
                    Replace legacy map library with React-based component."
                    .into(),
                modern_pattern: Some("React: react-leaflet or @react-google-maps/api".into()),
            });
        }
        if script_injection_score > 5.0 {
            guidance.push(GuidanceItem {
                concern: "Server-to-Client Script Injection".into(),
                severity: "high".into(),
                recommendation:
                    "Replace RegisterStartupScript with SPA event-driven architecture. \
                    Use frontend state management instead of server-injected scripts."
                        .into(),
                modern_pattern: Some("React useEffect + fetch API / SignalR".into()),
            });
        }
        if uncertainty_composite > 5.0 {
            guidance.push(GuidanceItem {
                concern: "Runtime Uncertainty (Dynamic Behavior)".into(),
                severity: if uncertainty_composite > 7.0 {
                    "high"
                } else {
                    "medium"
                }
                .into(),
                recommendation: "Add runtime instrumentation and migration guards for dynamic controls, probabilistic resolution, and inferred SQL table targets before cutover.".into(),
                modern_pattern: Some("Feature flags + telemetry-backed rollout checkpoints".into()),
            });
        }
    }

    Ok(BlastRadiusReport {
        target: target_id.to_string(),
        target_type: node_type,
        migration_risk,
        risk_band: risk_band(migration_risk),
        complexity_breakdown: ComplexityBreakdown {
            handles_clause_score,
            sql_concat_score,
            pagerank_score,
            state_coupling_score,
            gis_coupling_score,
            script_injection_score,
        },
        uncertainty_breakdown: UncertaintyBreakdown {
            dynamic_ui_uncertainty_score,
            late_binding_uncertainty_score,
            dynamic_sql_uncertainty_score,
        },
        seam_candidates,
        guidance,
        total_downstream,
    })
}

/// Format a `BlastRadiusReport` as human-readable text.
pub fn format_report(report: &BlastRadiusReport) -> String {
    let mut out = String::with_capacity(2048);

    out.push_str(&format!(
        "Migration Blast Radius: {}\n\
         Type: {}\n\
         Risk Score: {}/10 ({})\n\
         Total Downstream Nodes: {}\n\n",
        report.target,
        report.target_type,
        report.migration_risk,
        report.risk_band,
        report.total_downstream,
    ));

    out.push_str("Complexity Breakdown:\n");
    let bd = &report.complexity_breakdown;
    out.push_str(&format!(
        "  Event Wiring:     {:.1}/10\n",
        bd.handles_clause_score
    ));
    out.push_str(&format!(
        "  SQL Risk:         {:.1}/10\n",
        bd.sql_concat_score
    ));
    out.push_str(&format!(
        "  PageRank:         {:.1}/10\n",
        bd.pagerank_score
    ));
    out.push_str(&format!(
        "  State Coupling:   {:.1}/10\n",
        bd.state_coupling_score
    ));
    out.push_str(&format!(
        "  GIS Coupling:     {:.1}/10\n",
        bd.gis_coupling_score
    ));
    out.push_str(&format!(
        "  Script Injection: {:.1}/10\n",
        bd.script_injection_score
    ));

    out.push_str("Uncertainty Breakdown:\n");
    let ud = &report.uncertainty_breakdown;
    out.push_str(&format!(
        "  Dynamic UI:       {:.1}/10\n",
        ud.dynamic_ui_uncertainty_score
    ));
    out.push_str(&format!(
        "  Late Binding:     {:.1}/10\n",
        ud.late_binding_uncertainty_score
    ));
    out.push_str(&format!(
        "  Dynamic SQL:      {:.1}/10\n",
        ud.dynamic_sql_uncertainty_score
    ));

    if !report.seam_candidates.is_empty() {
        out.push_str(&format!(
            "\nSeam Candidates ({}):\n",
            report.seam_candidates.len()
        ));
        for s in &report.seam_candidates {
            out.push_str(&format!(
                "  - {} ({}): {}\n    Crossing: {}\n",
                s.node_id,
                s.node_type,
                s.reason,
                s.edge_kinds_crossing.join(", ")
            ));
        }
    }

    if !report.guidance.is_empty() {
        out.push_str(&format!(
            "\nMigration Guidance ({}):\n",
            report.guidance.len()
        ));
        for g in &report.guidance {
            out.push_str(&format!(
                "  [{severity}] {concern}\n    {rec}\n",
                severity = g.severity.to_uppercase(),
                concern = g.concern,
                rec = g.recommendation,
            ));
            if let Some(ref pattern) = g.modern_pattern {
                out.push_str(&format!("    Modern: {pattern}\n"));
            }
        }
    }

    out
}

/// Compute blast radius with solution-aware cross-project multiplier.
///
/// When a `SolutionStructure` is provided and the target file belongs to a shared
/// library (referenced by 2+ other projects), the risk score is scaled upward
/// by `cross_project_multiplier()` (1.0x–3.0x).
///
/// Also injects a guidance item explaining the cross-project impact.
pub fn compute_blast_radius_with_solution(
    graph: &GraphStore,
    project_id: &str,
    target_id: &str,
    generation: u64,
    include_guidance: bool,
    solution: Option<&SolutionStructure>,
) -> anyhow::Result<BlastRadiusReport> {
    let mut report =
        compute_blast_radius(graph, project_id, target_id, generation, include_guidance)?;

    if let Some(sln) = solution {
        // Extract file path from target_id (strip "file:" prefix if present)
        let file_path = target_id.strip_prefix("file:").unwrap_or(target_id);
        if let Some(proj_name) = solution_parser::file_to_project(sln, file_path) {
            let multiplier = solution_parser::cross_project_multiplier(sln, proj_name);
            if multiplier > 1.0 {
                // Scale the risk score
                let scaled = (report.migration_risk as f32 * multiplier).round() as u8;
                report.migration_risk = scaled.clamp(1, 10);
                report.risk_band = risk_band(report.migration_risk);

                // Add guidance about cross-project impact
                let ref_count = sln
                    .dependency_graph
                    .values()
                    .filter(|deps| deps.contains(&proj_name.to_string()))
                    .count();
                report.guidance.push(GuidanceItem {
                    concern: "Cross-Project Shared Library".into(),
                    severity: if multiplier >= 2.0 { "high" } else { "medium" }.into(),
                    recommendation: format!(
                        "File is in shared library '{proj_name}' referenced by {ref_count} project(s). \
                         Risk multiplied by {multiplier:.1}x. Migrate this project in Wave 0 and \
                         provide a stable API facade before migrating dependents."
                    ),
                    modern_pattern: Some("Shared Project → NuGet package with semver".into()),
                });
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_score() {
        assert_eq!(normalize_score(0, 10), 0.0);
        assert_eq!(normalize_score(10, 10), 10.0);
        assert_eq!(normalize_score(20, 10), 10.0); // capped at 10
        assert_eq!(normalize_score(5, 10), 5.0);
        assert_eq!(normalize_score(0, 0), 0.0);
    }

    #[test]
    fn test_risk_band() {
        assert_eq!(risk_band(1), RiskBand::Low);
        assert_eq!(risk_band(3), RiskBand::Low);
        assert_eq!(risk_band(4), RiskBand::Medium);
        assert_eq!(risk_band(6), RiskBand::Medium);
        assert_eq!(risk_band(7), RiskBand::High);
        assert_eq!(risk_band(8), RiskBand::High);
        assert_eq!(risk_band(9), RiskBand::Critical);
        assert_eq!(risk_band(10), RiskBand::Critical);
    }

    #[test]
    fn test_format_report() {
        let report = BlastRadiusReport {
            target: "file:Default.aspx.vb".into(),
            target_type: "file".into(),
            migration_risk: 7,
            risk_band: RiskBand::High,
            complexity_breakdown: ComplexityBreakdown {
                handles_clause_score: 6.0,
                sql_concat_score: 8.0,
                pagerank_score: 5.0,
                state_coupling_score: 4.0,
                gis_coupling_score: 2.0,
                script_injection_score: 3.0,
            },
            uncertainty_breakdown: UncertaintyBreakdown {
                dynamic_ui_uncertainty_score: 2.0,
                late_binding_uncertainty_score: 1.0,
                dynamic_sql_uncertainty_score: 4.0,
            },
            seam_candidates: vec![],
            guidance: vec![GuidanceItem {
                concern: "SQL Risk".into(),
                severity: "high".into(),
                recommendation: "Use parameterized queries".into(),
                modern_pattern: Some("Dapper".into()),
            }],
            total_downstream: 15,
        };
        let text = format_report(&report);
        assert!(text.contains("7/10"));
        assert!(text.contains("High"));
        assert!(text.contains("SQL Risk"));
    }

    #[test]
    fn cross_project_multiplier_scales_risk() {
        use engram_index::solution_parser::build_solution_structure;

        let sln_content = r#"
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Common", "Common\Common.csproj", "{A}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Web1", "Web1\Web1.csproj", "{B}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Web2", "Web2\Web2.csproj", "{C}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Web3", "Web3\Web3.csproj", "{D}"
EndProject
"#;
        let mut proj_files = std::collections::HashMap::new();
        proj_files.insert(
            "Common".to_string(),
            "<Project><PropertyGroup><RootNamespace>Common</RootNamespace></PropertyGroup></Project>".to_string(),
        );
        // Web1, Web2, Web3 all reference Common
        for name in ["Web1", "Web2", "Web3"] {
            proj_files.insert(
                name.to_string(),
                format!(
                    r#"<Project><PropertyGroup><RootNamespace>{name}</RootNamespace></PropertyGroup>
                    <ItemGroup><ProjectReference Include="..\Common\Common.csproj" /></ItemGroup></Project>"#
                ),
            );
        }

        let structure = build_solution_structure(sln_content, &proj_files);

        // Verify the multiplier: Common is referenced by 3 projects → 2.0x
        let mult = engram_index::solution_parser::cross_project_multiplier(&structure, "Common");
        assert!(
            mult >= 2.0,
            "Common referenced by 3 projects should have multiplier >= 2.0, got {mult}"
        );

        // Test a report with the multiplier applied
        let mut report = BlastRadiusReport {
            target: "file:Common/Utils.cs".into(),
            target_type: "file".into(),
            migration_risk: 5,
            risk_band: RiskBand::Medium,
            complexity_breakdown: ComplexityBreakdown {
                handles_clause_score: 3.0,
                sql_concat_score: 4.0,
                pagerank_score: 2.0,
                state_coupling_score: 3.0,
                gis_coupling_score: 0.0,
                script_injection_score: 0.0,
            },
            uncertainty_breakdown: UncertaintyBreakdown {
                dynamic_ui_uncertainty_score: 0.0,
                late_binding_uncertainty_score: 0.0,
                dynamic_sql_uncertainty_score: 0.0,
            },
            seam_candidates: vec![],
            guidance: vec![],
            total_downstream: 10,
        };

        // Apply the multiplier manually (simulating what compute_blast_radius_with_solution does)
        let file_path = report
            .target
            .strip_prefix("file:")
            .unwrap_or(&report.target);
        if let Some(proj_name) = solution_parser::file_to_project(&structure, file_path) {
            let m = solution_parser::cross_project_multiplier(&structure, proj_name);
            if m > 1.0 {
                let scaled = (report.migration_risk as f32 * m).round() as u8;
                report.migration_risk = scaled.clamp(1, 10);
                report.risk_band = risk_band(report.migration_risk);
            }
        }

        assert_eq!(report.migration_risk, 10, "5 * 2.0 = 10 (clamped)");
        assert_eq!(report.risk_band, RiskBand::Critical);
    }
}
