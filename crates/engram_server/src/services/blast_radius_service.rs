/// Migration Blast Radius Analysis.
///
/// Computes a composite migration complexity score for a file or symbol node,
/// factoring in event wiring density, SQL concatenation risk, PageRank centrality,
/// state coupling, GIS dependencies, and script injection coupling.
///
/// Returns a `BlastRadiusReport` with a 1-10 risk score, complexity breakdown,
/// seam candidates (logical refactoring cut-points), and agentic guidance.
use engram_graph::store::{EdgeKind, GraphStore};
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
    pub seam_candidates: Vec<SeamCandidate>,
    pub guidance: Vec<GuidanceItem>,
    pub total_downstream: usize,
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

    // 5. Composite risk score
    let raw_score = handles_clause_score * WEIGHT_HANDLES
        + sql_concat_score * WEIGHT_SQL
        + pagerank_score * WEIGHT_PAGERANK
        + state_coupling_score * WEIGHT_STATE
        + gis_coupling_score * WEIGHT_GIS
        + script_injection_score * WEIGHT_SCRIPT;
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

#[cfg(test)]
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
}
