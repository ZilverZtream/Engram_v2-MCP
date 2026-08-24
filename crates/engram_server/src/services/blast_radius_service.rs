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
    /// Dependency density — saturation curve over the count of incoming
    /// edges, i.e. how many other nodes break if this target changes. 0-10.
    #[serde(default)]
    pub dependency_density_score: f32,
    /// Polymorphism fan-out - direct subclasses / implementors. 0-10.
    #[serde(default)]
    pub polymorphism_score: f32,
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

/// Honest coverage of the counts in a report. Every edge fetch in this
/// service is capped (500 outgoing per kind, 1000 incoming, 1000 touching per
/// contained symbol); on an OciusX-sized graph those caps are HIT (resource
/// files report exactly 1000/500, a designer file ~9.6k). Presenting a capped
/// count as exact turned a lower bound into a "fact" that agents then trusted
/// as a risk oracle. This struct says which counts are lower bounds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CountCoverage {
    /// True when ANY fetch hit its cap — every count is then a LOWER BOUND.
    pub truncated: bool,
    /// True only when a CAUSAL-kind incoming fetch hit its cap — the causal
    /// count specifically is a lower bound. A truncated companion/structural
    /// fetch does not set this (per-metric coverage, not one global flag).
    #[serde(default)]
    pub causal_truncated: bool,
    /// Which fetches hit their cap ("incoming", "outgoing:<kind>", "contained:<sym>").
    pub truncated_fetches: Vec<String>,
    /// The caps that applied, so a reader can interpret a count equal to one.
    pub cap_incoming: usize,
    pub cap_outgoing_per_kind: usize,
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
    pub total_incoming: usize,
    pub total_outgoing: usize,
    /// DEPRECATED semantics: this is `total_incoming + total_outgoing`, a
    /// 1-hop degree, NOT a transitive blast radius. Outgoing edges are things
    /// the target depends on; they do not break when the target changes.
    /// Prefer `causal_dependents` for "what may break".
    pub total_downstream: usize,
    /// Incoming edges of CAUSAL kinds only (calls, dependency, imports,
    /// inheritance, setting/state/table reads, API calls, data binding). This
    /// excludes temporal coupling, search co-occurrence, insight, anti-pattern
    /// and containment edges, which are historical/associative/structural and
    /// were previously counted as if they were things that break.
    #[serde(default)]
    pub causal_dependents: usize,
    /// UNIQUE sources with a TemporalCoupling edge — "usually changed
    /// together". Companion evidence for planning, NEVER causal impact.
    #[serde(default)]
    pub historical_companions: usize,
    /// Unique sources whose only evidence is runtime observation
    /// (ObservedRuntimeControl/Sql): an interaction occurred, which is weaker
    /// than a compiler-resolved call — never folded into causal counts/score.
    #[serde(default)]
    pub possible_dependents: usize,
    /// Causal edges whose SOURCE has no node record (dangling). Quarantined:
    /// excluded from causal_dependents, the named list, and the score.
    #[serde(default)]
    pub unresolved_endpoints: usize,
    /// File targets only: edges with BOTH endpoints inside the file. Internal
    /// complexity/cohesion — excluded from every external count and from the
    /// score (previously each one inflated incoming AND outgoing AND risk).
    #[serde(default)]
    pub internal_edges: usize,
    /// Named causal dependents from the SAME unique-source population the
    /// `causal_dependents` count uses (direct + file-aggregated), ranked
    /// directness → confidence → node id. Renderers must use this rather than
    /// re-querying the graph, so count and examples describe one population.
    /// Each: (source node id, most direct edge kind, best confidence).
    #[serde(default)]
    pub top_causal_dependents: Vec<(String, String, f32)>,
    #[serde(default)]
    pub coverage: CountCoverage,
}

/// Does an INCOMING edge of this kind mean "the source may BREAK when the
/// target changes"? Only these kinds count toward `causal_dependents`.
/// Historical (TemporalCoupling), associative (CoOccurrence, Insight,
/// AntiPattern, UiLayoutNeighbor, StateAffinity), structural (Contains,
/// ContainsUi, HasColumn) and unresolved kinds are NOT causal: "usually
/// changed together" and "often searched together" are not "will break".
pub fn is_causal_dependency(kind: &EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls
            | EdgeKind::Dependency
            | EdgeKind::Imports
            | EdgeKind::IncludesFile
            | EdgeKind::ApiCall
            | EdgeKind::DataBinding
            | EdgeKind::ParameterBinding
            | EdgeKind::ReadsState
            | EdgeKind::WritesState
            | EdgeKind::QueriesTable
            | EdgeKind::ReadsColumn
            | EdgeKind::ForeignKey
            | EdgeKind::SqlCalls
            | EdgeKind::CallsStoredProcedure
            | EdgeKind::StoredProcReadsTable
            | EdgeKind::StoredProcWritesTable
            | EdgeKind::SpatialCall
            | EdgeKind::InjectsScript
            | EdgeKind::ManipulatesDom
            | EdgeKind::RegistersHandler
            | EdgeKind::RegistersControl
            | EdgeKind::RegistersModule
            | EdgeKind::TriggersPostback
            | EdgeKind::FillsRegion
            | EdgeKind::ExposesHttpHandler
            | EdgeKind::ExposesWcfService
            | EdgeKind::ExposesWebService
            // Settings/inheritance: a setting change reaches its readers, a
            // base/interface change reaches derived types. These were missing
            // and contradicted the polymorphism score, which already knew
            // about derived types.
            | EdgeKind::ReadsSetting
            | EdgeKind::InheritsFrom
            | EdgeKind::Implements
    )
    // Deliberately NOT causal-incoming:
    // - TestOracle runs source-program → oracle file. Incoming-to-target means
    //   the target IS the oracle; changing an oracle does not break the
    //   program. Following it needs the OUTGOING direction (a change-aware
    //   engine concern), so it is excluded from this incoming-only model.
    // - ObservedRuntimeControl/Sql prove an interaction OCCURRED, not that a
    //   change breaks the source — see `is_possible_dependency`.
}

/// Runtime-observed evidence: an interaction was seen to happen, which is
/// weaker than a compiler-resolved call. Reported as a separate "possible"
/// tier, never folded into confirmed causal counts or the score.
pub fn is_possible_dependency(kind: &EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::ObservedRuntimeControl | EdgeKind::ObservedRuntimeSql
    )
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

// Weights sum to 1.0. Dependency density dominates ("what breaks when I touch
// this"), SQL/state follow as the two highest-signal risk factors, and
// handles/pagerank/gis/script fill in the rest. The previous 0.30 weight for
// dependency turned out to be too modest once combined with the 0.80
// base-weighted dilution below — a target with 170 incoming deps and nothing
// else scored 2/10, which under-reports the risk of touching such a node.
const WEIGHT_DEPENDENCY: f32 = 0.35;
const WEIGHT_SQL: f32 = 0.15;
const WEIGHT_STATE: f32 = 0.15;
const WEIGHT_HANDLES: f32 = 0.05;
/// Polymorphism fan-out: classes inheriting from / implementing this node.
/// Editing a base class or interface ripples into every derived type.
const WEIGHT_POLYMORPHISM: f32 = 0.10;
const WEIGHT_PAGERANK: f32 = 0.10;
const WEIGHT_GIS: f32 = 0.05;
const WEIGHT_SCRIPT: f32 = 0.05;

// Saturation for dependency density: at 50+ incoming dependencies the score
// maxes out at 10/10. The previous 100-dep saturation was too forgiving for
// real projects — a shared utility with 50 callers is already a migration
// hub and should read as such.
const DEPENDENCY_SATURATION: usize = 50;

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
    if node.is_none() {
        anyhow::bail!("Target node not found: {target_id}");
    }
    let node_type = node
        .as_ref()
        .map(|n| n.node_type.clone())
        .unwrap_or_else(|| "unknown".into());

    // Every fetch below is capped. Record which caps were HIT so the report
    // can say "lower bound" instead of presenting a capped count as exact.
    const CAP_OUT_PER_KIND: usize = 500;
    const CAP_INCOMING: usize = 1000;
    const CAP_TOUCHING: usize = 1000;
    let mut coverage = CountCoverage {
        truncated: false,
        causal_truncated: false,
        truncated_fetches: Vec::new(),
        cap_incoming: CAP_INCOMING,
        cap_outgoing_per_kind: CAP_OUT_PER_KIND,
    };

    // 2+3. One substrate sweep: outgoing, direct incoming, and file-level
    // aggregation all come from `component_adjacency`, which applies the
    // component boundary BEFORE its accepted-result caps, per (kind,
    // endpoint), in a single read transaction. This replaces three separate
    // traversals that each applied raw caps first (so >cap higher-weight
    // internal or historical edges could hide external causal callers and
    // produce a falsely LOW score from incomplete evidence), plus the
    // per-symbol transaction storm on file targets.
    let is_file_target = target_id.starts_with("file:");
    let file_rel_path: String = node
        .as_ref()
        .map(|n| n.file_path.as_str().to_string())
        .unwrap_or_else(|| {
            target_id
                .strip_prefix("file:")
                .unwrap_or(target_id)
                .to_string()
        });
    const CAP_CONTAINED: usize = 50_000;
    let contained_symbols: Vec<String> = if is_file_target {
        // Exact-equality membership BEFORE the cap, cap+1 detection, sorted —
        // in the store (`query_nodes`' substring pre-filter let suffix
        // collisions consume the budget, and exactly-at-cap read as truncated).
        let (members, trunc) =
            graph.file_component_members(project_id, &file_rel_path, target_id, CAP_CONTAINED)?;
        if trunc {
            coverage.truncated = true;
            coverage
                .truncated_fetches
                .push("contained-symbol scan".to_string());
        }
        members
    } else {
        Vec::new()
    };
    let component: HashSet<String> = std::iter::once(target_id.to_string())
        .chain(contained_symbols.iter().cloned())
        .collect();
    let endpoints: Vec<String> = std::iter::once(target_id.to_string())
        .chain(contained_symbols.iter().cloned())
        .collect();

    let adjacency = graph.component_adjacency(
        project_id,
        &endpoints,
        &component,
        &|_| CAP_INCOMING,
        &|_| CAP_OUT_PER_KIND,
    )?;
    let internal_edges: usize = adjacency.internal_skipped;
    for (kind, ep) in &adjacency.truncated_in {
        coverage.truncated = true;
        if is_causal_dependency(kind) {
            coverage.causal_truncated = true;
        }
        if coverage.truncated_fetches.len() < 20 {
            coverage
                .truncated_fetches
                .push(format!("incoming:{}:{}", kind.as_str(), ep));
        }
    }
    for (kind, ep) in &adjacency.truncated_out {
        coverage.truncated = true;
        if coverage.truncated_fetches.len() < 20 {
            coverage
                .truncated_fetches
                .push(format!("outgoing:{}:{}", kind.as_str(), ep));
        }
    }

    let mut out_counts: HashMap<EdgeKind, usize> = HashMap::new();
    for (_tgt, kind, _w, _ep) in &adjacency.outgoing {
        *out_counts.entry(kind.clone()).or_default() += 1;
    }
    let mut in_counts: HashMap<EdgeKind, usize> = HashMap::new();
    let mut in_sources_by_kind: HashMap<EdgeKind, HashSet<String>> = HashMap::new();
    for (src, kind, _w, _ep) in &adjacency.incoming {
        *in_counts.entry(kind.clone()).or_default() += 1;
        in_sources_by_kind
            .entry(kind.clone())
            .or_default()
            .insert(src.clone());
    }

    // The SCORE input: CAUSAL kinds only, UNIQUE RESOLVED source nodes, each
    // at its best edge confidence. Unknown confidence contributes a
    // conservative 0.5, never a laundered 1.0. Dangling sources (edge with no
    // node record) are quarantined: they appear in `unresolved_endpoints`,
    // never in the causal count, the named list, or the score.
    let causal_edges: Vec<&(String, EdgeKind, u32, String)> = adjacency
        .incoming
        .iter()
        .filter(|(_, kind, _, _)| is_causal_dependency(kind))
        .collect();
    let conf_queries: Vec<(EdgeKind, String, String)> = causal_edges
        .iter()
        .map(|(src, kind, _, ep)| (kind.clone(), src.clone(), ep.clone()))
        .collect();
    let confs: Vec<Option<f32>> = match graph.get_edge_confidences(project_id, &conf_queries) {
        Ok(c) => c,
        Err(_) => vec![None; causal_edges.len()],
    };
    // Batch-resolve causal sources once to quarantine dangling ones.
    let mut causal_ids: Vec<String> = causal_edges.iter().map(|(s, ..)| s.clone()).collect();
    causal_ids.sort();
    causal_ids.dedup();
    let causal_nodes = graph.get_nodes(project_id, &causal_ids)?;
    let mut low_confidence_incoming: usize = 0;
    let mut causal_source_conf: HashMap<&str, f32> = HashMap::new();
    let mut unresolved_endpoints_set: HashSet<&str> = HashSet::new();
    for ((src, kind, _w, _ep), conf) in causal_edges.iter().zip(confs.iter()) {
        if causal_nodes
            .get(src.as_str())
            .map(|n| n.is_none())
            .unwrap_or(true)
        {
            unresolved_endpoints_set.insert(src.as_str());
            continue;
        }
        let c = conf.unwrap_or(0.5).clamp(0.0, 1.0);
        if c < 0.6 {
            low_confidence_incoming += 1;
        }
        let e = causal_source_conf.entry(src.as_str()).or_insert(0.0);
        if c > *e {
            *e = c;
        }
        let _ = kind;
    }
    let unresolved_endpoints = unresolved_endpoints_set.len();
    let discounted_incoming: f32 = causal_source_conf.values().sum();
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
    let pagerank_score = match graph.get_or_compute_centrality(project_id, generation) {
        Ok(pagerank) => {
            let pr = pagerank.get(target_id).copied().unwrap_or(0.0);
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

    // Polymorphism fan-out: incoming inherits_from / implements_interface
    // edges = direct subclasses and implementors whose behavior changes
    // when this node changes. Saturates at 8 - one base page class with
    // 19 derived pages is already maximal ripple.
    let polymorphism_count = in_counts.get(&EdgeKind::InheritsFrom).copied().unwrap_or(0)
        + in_counts.get(&EdgeKind::Implements).copied().unwrap_or(0);
    let polymorphism_score = normalize_score(polymorphism_count, 8);

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
    // O(degree) adjacency lookup — this used to be a full structural-edge
    // table scan on EVERY call (the dominant cost of check_edit_safety and
    // the per-file loop in pre_commit_review on large graphs).
    let (touching_edges, touching_edges_trunc) =
        graph.edges_touching_with_coverage(project_id, target_id, CAP_TOUCHING)?;
    if touching_edges_trunc {
        coverage.truncated = true;
        coverage.truncated_fetches.push("touching".to_string());
    }

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

    // 5. Dependency density — compute before the composite so it can feed in.
    let total_incoming: usize = in_counts.values().sum();
    let total_outgoing: usize = out_counts.values().sum();
    let total_downstream: usize = total_incoming + total_outgoing;
    // Split incoming into what may BREAK (causal) vs what merely correlates
    // (historical companions) — previously both fed the same "dependents"
    // number, so a resource file that co-changed with everything looked like
    // a hub that everything depended on.
    // UNIQUE external source NODES with a causal edge to the target (or to a
    // contained symbol), not an edge sum: one caller reaching the target via
    // three edge kinds, or three contained symbols, is ONE dependent.
    // Unique causal sources with their most-direct kind and best confidence —
    // ONE population for both the count and the named examples.
    let directness = |k: &EdgeKind| -> u8 {
        match k {
            EdgeKind::Calls | EdgeKind::Dependency | EdgeKind::SqlCalls => 0,
            EdgeKind::InheritsFrom | EdgeKind::Implements | EdgeKind::Imports => 1,
            _ => 2,
        }
    };
    // Confirmed causal source ⇔ it survived the dangling quarantine, i.e. it
    // has a `causal_source_conf` entry. Dangling sources are counted in
    // `unresolved_endpoints`, never here.
    let mut causal_sources: HashMap<&str, (u8, &EdgeKind)> = HashMap::new();
    for (kind, sources) in &in_sources_by_kind {
        if !is_causal_dependency(kind) {
            continue;
        }
        let d = directness(kind);
        for s in sources {
            if !causal_source_conf.contains_key(s.as_str()) {
                continue; // dangling — quarantined
            }
            let e = causal_sources.entry(s.as_str()).or_insert((d, kind));
            // Deterministic selection: most direct kind wins; equal directness
            // breaks on the kind's stable string (HashMap iteration order must
            // never decide what a report says).
            if d < e.0 || (d == e.0 && kind.as_str() < e.1.as_str()) {
                *e = (d, kind);
            }
        }
    }
    let causal_dependents: usize = causal_sources.len();
    let mut top_causal_dependents: Vec<(String, String, f32)> = causal_sources
        .iter()
        .map(|(src, (_, kind))| {
            let conf = causal_source_conf.get(src).copied().unwrap_or(0.5);
            (src.to_string(), kind.as_str().to_string(), conf)
        })
        .collect();
    top_causal_dependents.sort_by(|a, b| {
        let da = causal_sources.get(a.0.as_str()).map(|e| e.0).unwrap_or(9);
        let db = causal_sources.get(b.0.as_str()).map(|e| e.0).unwrap_or(9);
        da.cmp(&db)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.cmp(&b.0))
    });
    top_causal_dependents.truncate(10);
    // UNIQUE co-change partners, consistent with causal_dependents (unique
    // nodes) and with impact_analysis — an edge count triple-counted a file
    // that co-changed with three contained symbols.
    let historical_companions: usize = in_sources_by_kind
        .get(&EdgeKind::TemporalCoupling)
        .map(|s| s.len())
        .unwrap_or(0);
    // Runtime-observed interaction: evidence an interaction occurred, weaker
    // than a compiler-resolved call — a separate "possible" population.
    let possible_dependents: usize = {
        let mut uniq: HashSet<&str> = HashSet::new();
        for (kind, sources) in &in_sources_by_kind {
            if is_possible_dependency(kind) {
                for s in sources {
                    uniq.insert(s.as_str());
                }
            }
        }
        uniq.len()
    };
    let dependency_density_score =
        normalize_score(discounted_incoming.round() as usize, DEPENDENCY_SATURATION);

    // 6. Composite risk score
    //
    // Use the full weighted score directly — the previous implementation
    // diluted `base_weighted_score` by 0.80 and blended in 0.20 of the
    // uncertainty composite. That meant even a target with dependency
    // density saturated (10/10, weight 0.40) could only contribute
    // 10 * 0.40 * 0.80 = 3.2 to the final score, yielding migration_risk 3
    // (still Low). Callers interpret migration_risk as a direct 1-10 band,
    // so the weighted sum is the score — no extra dilution. Uncertainty is
    // added as a small uplift (capped at +1.5) so dynamic/probabilistic
    // nodes still rise above their pure-static peers.
    let base_weighted_score = dependency_density_score * WEIGHT_DEPENDENCY
        + sql_concat_score * WEIGHT_SQL
        + state_coupling_score * WEIGHT_STATE
        + handles_clause_score * WEIGHT_HANDLES
        + pagerank_score * WEIGHT_PAGERANK
        + gis_coupling_score * WEIGHT_GIS
        + script_injection_score * WEIGHT_SCRIPT
        + polymorphism_score * WEIGHT_POLYMORPHISM;
    let uncertainty_uplift = (uncertainty_composite * 0.15).min(1.5);
    let raw_score = (base_weighted_score + uncertainty_uplift).min(10.0);
    let migration_risk = (raw_score.round() as u8).clamp(1, 10);

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
                concern: "Data Access Coupling (NOT an injection analysis)".into(),
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
        if low_confidence_incoming * 2 > total_incoming.max(1) {
            guidance.push(GuidanceItem {
                concern: "Phantom Caller Inflation".into(),
                severity: "info".into(),
                recommendation: format!(
                    "{low_confidence_incoming} of {total_incoming} incoming edges are \
                     bare-name bindings (confidence < 0.6) — likely name collisions, \
                     not real callers. The density score already discounts them; \
                     verify hot callers with find_symbol_references before trusting \
                     raw counts."
                ),
                modern_pattern: None,
            });
        }
        if polymorphism_score > 5.0 {
            guidance.push(GuidanceItem {
                concern: "Polymorphism Fan-Out".into(),
                severity: "high".into(),
                recommendation: "Multiple classes inherit from or implement this type. \
                    Any behavioral change here silently changes every derived class - \
                    enumerate them (find_symbol_references / inherits_from edges) and \
                    test each derived page before shipping."
                    .into(),
                modern_pattern: Some("Prefer composition or explicit interface versioning".into()),
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
            dependency_density_score,
            polymorphism_score,
        },
        uncertainty_breakdown: UncertaintyBreakdown {
            dynamic_ui_uncertainty_score,
            late_binding_uncertainty_score,
            dynamic_sql_uncertainty_score,
        },
        seam_candidates,
        guidance,
        total_incoming,
        total_outgoing,
        total_downstream,
        causal_dependents,
        historical_companions,
        possible_dependents,
        unresolved_endpoints,
        internal_edges,
        top_causal_dependents,
        coverage,
    })
}

/// Format a `BlastRadiusReport` as human-readable text.
pub fn format_report(report: &BlastRadiusReport) -> String {
    let mut out = String::with_capacity(2048);

    // Honest counts: say "lower bound" when a fetch cap was hit, and lead
    // with the CAUSAL dependents (what may break) rather than the raw
    // incoming+outgoing degree that mixed in co-change history.
    // Per-metric: the causal count is a lower bound only when a CAUSAL-kind
    // fetch was truncated; the raw degree when anything was.
    let ge_causal = if report.coverage.causal_truncated {
        ">="
    } else {
        ""
    };
    let ge = if report.coverage.truncated { ">=" } else { "" };
    out.push_str(&format!(
        "Migration Blast Radius: {}\n\
         Type: {}\n\
         Risk Score: {}/10 ({})\n\
         Causal dependents (1-hop, may break if this changes): {ge_causal}{}\n\
         Possible dependents (runtime-observed only): {}\n\
         Historical companions (usually changed together, NOT causal): {}\n\
         Unresolved endpoints (dangling causal edges, quarantined): {}\n\
         Raw 1-hop degree: incoming {ge}{}, outgoing {ge}{} (outgoing = things this depends on; they do not break)\n",
        report.target,
        report.target_type,
        report.migration_risk,
        report.risk_band,
        report.causal_dependents,
        report.possible_dependents,
        report.historical_companions,
        report.unresolved_endpoints,
        report.total_incoming,
        report.total_outgoing,
    ));
    if report.coverage.truncated {
        out.push_str(&format!(
            "⚠ COUNTS ARE LOWER BOUNDS: a fetch cap was hit ({}); the true numbers are higher. \
             Caps: incoming {}, outgoing {} per edge kind. Do not treat these as exact.\n",
            report.coverage.truncated_fetches.join(", "),
            report.coverage.cap_incoming,
            report.coverage.cap_outgoing_per_kind,
        ));
    }
    out.push('\n');

    out.push_str("Complexity Breakdown:\n");
    let bd = &report.complexity_breakdown;
    out.push_str(&format!(
        "  Dependency:       {:.1}/10\n",
        bd.dependency_density_score
    ));
    out.push_str(&format!(
        "  Data Access:      {:.1}/10 (coupling, not injection)\n",
        bd.sql_concat_score
    ));
    out.push_str(&format!(
        "  State Coupling:   {:.1}/10\n",
        bd.state_coupling_score
    ));
    out.push_str(&format!(
        "  Event Wiring:     {:.1}/10\n",
        bd.handles_clause_score
    ));
    out.push_str(&format!(
        "  PageRank:         {:.1}/10\n",
        bd.pagerank_score
    ));
    out.push_str(&format!(
        "  GIS Coupling:     {:.1}/10\n",
        bd.gis_coupling_score
    ));
    out.push_str(&format!(
        "  Script Injection: {:.1}/10\n",
        bd.script_injection_score
    ));
    out.push_str(&format!(
        "  Polymorphism:     {:.1}/10\n",
        bd.polymorphism_score
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

    out.push_str(
        "\nnext: check_edit_safety per method you will touch; \
         find_symbol_references(<symbol>) to enumerate callers; \
         begin_edit_session(planned_files=[...]) before editing.\n",
    );

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
                polymorphism_score: 0.0,
                script_injection_score: 3.0,
                dependency_density_score: 1.0,
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
            total_incoming: 10,
            total_outgoing: 5,
            total_downstream: 15,
            causal_dependents: 0,
            historical_companions: 0,
            possible_dependents: 0,
            unresolved_endpoints: 0,
            internal_edges: 0,
            top_causal_dependents: vec![],
            coverage: CountCoverage::default(),
        };
        let text = format_report(&report);
        assert!(text.contains("7/10"));
        assert!(text.contains("High"));
        assert!(text.contains("Data Access:"));
    }

    // ── normalize_score boundary cases ──────────────────────────────────────

    #[test]
    fn normalize_score_partial_saturation() {
        // 3/10 = 0.3 → score = 3.0
        let s = normalize_score(3, 10);
        assert!((s - 3.0).abs() < 0.001, "expected 3.0, got {s}");
    }

    #[test]
    fn normalize_score_one_count() {
        let s = normalize_score(1, 10);
        assert!((s - 1.0).abs() < 0.001);
    }

    #[test]
    fn normalize_score_exactly_at_saturation() {
        let s = normalize_score(15, 15);
        assert!((s - 10.0).abs() < 0.001);
    }

    #[test]
    fn normalize_score_exceeds_saturation_capped() {
        let s = normalize_score(100, 5);
        assert!((s - 10.0).abs() < 0.001, "should be capped at 10.0");
    }

    // ── risk_band boundary values ────────────────────────────────────────────

    #[test]
    fn risk_band_score_zero_is_critical() {
        // score 0 does not match any arm, falls through to _ => Critical
        assert_eq!(risk_band(0), RiskBand::Critical);
    }

    #[test]
    fn risk_band_boundary_between_low_and_medium() {
        assert_eq!(risk_band(3), RiskBand::Low);
        assert_eq!(risk_band(4), RiskBand::Medium);
    }

    #[test]
    fn risk_band_boundary_between_medium_and_high() {
        assert_eq!(risk_band(6), RiskBand::Medium);
        assert_eq!(risk_band(7), RiskBand::High);
    }

    #[test]
    fn risk_band_boundary_between_high_and_critical() {
        assert_eq!(risk_band(8), RiskBand::High);
        assert_eq!(risk_band(9), RiskBand::Critical);
    }

    // ── RiskBand display ─────────────────────────────────────────────────────

    #[test]
    fn risk_band_as_str() {
        assert_eq!(RiskBand::Low.as_str(), "Low");
        assert_eq!(RiskBand::Medium.as_str(), "Medium");
        assert_eq!(RiskBand::High.as_str(), "High");
        assert_eq!(RiskBand::Critical.as_str(), "Critical");
    }

    #[test]
    fn risk_band_display_matches_as_str() {
        for band in [
            RiskBand::Low,
            RiskBand::Medium,
            RiskBand::High,
            RiskBand::Critical,
        ] {
            assert_eq!(band.to_string(), band.as_str());
        }
    }

    // ── meta_bool helper ─────────────────────────────────────────────────────

    #[test]
    fn meta_bool_true_value() {
        let meta = serde_json::json!({"flag": true});
        assert!(meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_false_value() {
        let meta = serde_json::json!({"flag": false});
        assert!(!meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_string_true() {
        let meta = serde_json::json!({"flag": "true"});
        assert!(meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_string_yes() {
        let meta = serde_json::json!({"flag": "yes"});
        assert!(meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_string_one() {
        let meta = serde_json::json!({"flag": "1"});
        assert!(meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_number_nonzero() {
        let meta = serde_json::json!({"flag": 3});
        assert!(meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_number_zero() {
        let meta = serde_json::json!({"flag": 0});
        assert!(!meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_missing_key() {
        let meta = serde_json::json!({"other": true});
        assert!(!meta_bool(Some(&meta), "flag"));
    }

    #[test]
    fn meta_bool_none_metadata() {
        assert!(!meta_bool(None, "flag"));
    }

    // ── meta_str_eq helper ───────────────────────────────────────────────────

    #[test]
    fn meta_str_eq_matches_case_insensitive() {
        let meta = serde_json::json!({"resolution": "Probabilistic"});
        assert!(meta_str_eq(Some(&meta), "resolution", "probabilistic"));
    }

    #[test]
    fn meta_str_eq_no_match() {
        let meta = serde_json::json!({"resolution": "exact"});
        assert!(!meta_str_eq(Some(&meta), "resolution", "probabilistic"));
    }

    // ── meta_f32 helper ──────────────────────────────────────────────────────

    #[test]
    fn meta_f32_number_value() {
        let meta = serde_json::json!({"confidence": 0.75});
        let v = meta_f32(Some(&meta), "confidence");
        assert!(v.is_some());
        assert!((v.unwrap() - 0.75_f32).abs() < 0.001);
    }

    #[test]
    fn meta_f32_string_value() {
        let meta = serde_json::json!({"confidence": "0.9"});
        let v = meta_f32(Some(&meta), "confidence");
        assert!(v.is_some());
        assert!((v.unwrap() - 0.9_f32).abs() < 0.001);
    }

    #[test]
    fn meta_f32_missing_key_returns_none() {
        let meta = serde_json::json!({"other": 1.0});
        assert!(meta_f32(Some(&meta), "confidence").is_none());
    }

    // ── format_report content checks ────────────────────────────────────────

    #[test]
    fn format_report_contains_all_sections() {
        let report = BlastRadiusReport {
            target: "file:Test.aspx.vb".into(),
            target_type: "file".into(),
            migration_risk: 5,
            risk_band: RiskBand::Medium,
            complexity_breakdown: ComplexityBreakdown {
                handles_clause_score: 3.0,
                sql_concat_score: 5.0,
                pagerank_score: 2.0,
                state_coupling_score: 4.0,
                gis_coupling_score: 0.0,
                polymorphism_score: 0.0,
                script_injection_score: 1.0,
                dependency_density_score: 2.0,
            },
            uncertainty_breakdown: UncertaintyBreakdown {
                dynamic_ui_uncertainty_score: 1.0,
                late_binding_uncertainty_score: 2.0,
                dynamic_sql_uncertainty_score: 3.0,
            },
            seam_candidates: vec![SeamCandidate {
                node_id: "node1".into(),
                node_type: "function".into(),
                reason: "boundary crossing".into(),
                edge_kinds_crossing: vec!["Dependency".into()],
            }],
            guidance: vec![GuidanceItem {
                concern: "SQL Risk".into(),
                severity: "high".into(),
                recommendation: "Use parameterized queries".into(),
                modern_pattern: Some("Dapper".into()),
            }],
            total_incoming: 3,
            total_outgoing: 5,
            total_downstream: 8,
            causal_dependents: 0,
            historical_companions: 0,
            possible_dependents: 0,
            unresolved_endpoints: 0,
            internal_edges: 0,
            top_causal_dependents: vec![],
            coverage: CountCoverage::default(),
        };
        let text = format_report(&report);
        assert!(text.contains("5/10"), "should contain risk score");
        assert!(text.contains("Medium"), "should contain risk band");
        assert!(
            text.contains("Complexity Breakdown"),
            "should have complexity section"
        );
        assert!(
            text.contains("Uncertainty Breakdown"),
            "should have uncertainty section"
        );
        assert!(
            text.contains("Seam Candidates"),
            "should list seam candidates"
        );
        assert!(text.contains("Migration Guidance"), "should list guidance");
        assert!(text.contains("Data Access"), "should show concern");
        assert!(text.contains("Dapper"), "should show modern pattern");
        // The header no longer presents incoming+outgoing as "downstream
        // nodes": it leads with causal dependents and labels the raw degree.
        assert!(
            text.contains("Causal dependents (1-hop, may break if this changes)"),
            "should lead with causal dependents"
        );
        assert!(
            text.contains("Raw 1-hop degree: incoming"),
            "should label the raw degree honestly"
        );
    }

    #[test]
    fn truncated_counts_are_flagged_as_lower_bounds() {
        let report = BlastRadiusReport {
            target: "file:Resources.resx".into(),
            target_type: "file".into(),
            migration_risk: 4,
            risk_band: RiskBand::Medium,
            complexity_breakdown: ComplexityBreakdown {
                handles_clause_score: 0.0,
                sql_concat_score: 0.0,
                pagerank_score: 1.0,
                state_coupling_score: 0.0,
                gis_coupling_score: 0.0,
                polymorphism_score: 0.0,
                script_injection_score: 0.0,
                dependency_density_score: 10.0,
            },
            uncertainty_breakdown: UncertaintyBreakdown {
                dynamic_ui_uncertainty_score: 0.0,
                late_binding_uncertainty_score: 0.0,
                dynamic_sql_uncertainty_score: 0.0,
            },
            seam_candidates: vec![],
            guidance: vec![],
            total_incoming: 1000,
            total_outgoing: 500,
            total_downstream: 1500,
            causal_dependents: 12,
            historical_companions: 988,
            possible_dependents: 0,
            unresolved_endpoints: 0,
            internal_edges: 0,
            top_causal_dependents: vec![],
            coverage: CountCoverage {
                truncated: true,
                causal_truncated: true,
                truncated_fetches: vec!["incoming".into()],
                cap_incoming: 1000,
                cap_outgoing_per_kind: 500,
            },
        };
        let text = format_report(&report);
        assert!(
            text.contains(">=1000"),
            "a capped count must render as a lower bound: {text}"
        );
        assert!(
            text.contains("COUNTS ARE LOWER BOUNDS"),
            "must warn that counts are truncated"
        );
    }

    #[test]
    fn temporal_coupling_is_not_a_causal_dependency() {
        assert!(!is_causal_dependency(&EdgeKind::TemporalCoupling));
        assert!(!is_causal_dependency(&EdgeKind::CoOccurrence));
        assert!(!is_causal_dependency(&EdgeKind::Insight));
        assert!(!is_causal_dependency(&EdgeKind::AntiPattern));
        assert!(!is_causal_dependency(&EdgeKind::Contains));
        assert!(is_causal_dependency(&EdgeKind::Calls));
        assert!(is_causal_dependency(&EdgeKind::Dependency));
        assert!(is_causal_dependency(&EdgeKind::ReadsState));
        assert!(is_causal_dependency(&EdgeKind::QueriesTable));
    }

    #[test]
    fn format_report_no_seams_no_guidance_omits_sections() {
        let report = BlastRadiusReport {
            target: "file:Simple.aspx.vb".into(),
            target_type: "file".into(),
            migration_risk: 2,
            risk_band: RiskBand::Low,
            complexity_breakdown: ComplexityBreakdown {
                handles_clause_score: 0.0,
                sql_concat_score: 0.0,
                pagerank_score: 0.0,
                state_coupling_score: 0.0,
                gis_coupling_score: 0.0,
                polymorphism_score: 0.0,
                script_injection_score: 0.0,
                dependency_density_score: 0.0,
            },
            uncertainty_breakdown: UncertaintyBreakdown {
                dynamic_ui_uncertainty_score: 0.0,
                late_binding_uncertainty_score: 0.0,
                dynamic_sql_uncertainty_score: 0.0,
            },
            seam_candidates: vec![],
            guidance: vec![],
            total_incoming: 0,
            total_outgoing: 0,
            total_downstream: 0,
            causal_dependents: 0,
            historical_companions: 0,
            possible_dependents: 0,
            unresolved_endpoints: 0,
            internal_edges: 0,
            top_causal_dependents: vec![],
            coverage: CountCoverage::default(),
        };
        let text = format_report(&report);
        assert!(
            !text.contains("Seam Candidates"),
            "no seams so section absent"
        );
        assert!(
            !text.contains("Migration Guidance"),
            "no guidance so section absent"
        );
    }

    #[test]
    fn format_report_scores_formatted_to_one_decimal() {
        let report = BlastRadiusReport {
            target: "file:X.vb".into(),
            target_type: "file".into(),
            migration_risk: 4,
            risk_band: RiskBand::Medium,
            complexity_breakdown: ComplexityBreakdown {
                handles_clause_score: 3.333,
                sql_concat_score: 7.777,
                pagerank_score: 0.0,
                state_coupling_score: 0.0,
                gis_coupling_score: 0.0,
                polymorphism_score: 0.0,
                script_injection_score: 0.0,
                dependency_density_score: 5.555,
            },
            uncertainty_breakdown: UncertaintyBreakdown {
                dynamic_ui_uncertainty_score: 0.0,
                late_binding_uncertainty_score: 0.0,
                dynamic_sql_uncertainty_score: 0.0,
            },
            seam_candidates: vec![],
            guidance: vec![],
            total_incoming: 1,
            total_outgoing: 1,
            total_downstream: 2,
            causal_dependents: 0,
            historical_companions: 0,
            possible_dependents: 0,
            unresolved_endpoints: 0,
            internal_edges: 0,
            top_causal_dependents: vec![],
            coverage: CountCoverage::default(),
        };
        let text = format_report(&report);
        // {:.1} formatting means 3.333 → "3.3" and 7.777 → "7.8"
        assert!(text.contains("3.3"), "handles score formatted to 1 decimal");
        assert!(text.contains("7.8"), "sql score formatted to 1 decimal");
    }

    // ── Weights sum to 1.0 ───────────────────────────────────────────────────

    #[test]
    fn weights_sum_to_one() {
        let sum = WEIGHT_DEPENDENCY
            + WEIGHT_HANDLES
            + WEIGHT_SQL
            + WEIGHT_PAGERANK
            + WEIGHT_STATE
            + WEIGHT_GIS
            + WEIGHT_SCRIPT
            + WEIGHT_POLYMORPHISM;
        assert!(
            (sum - 1.0).abs() < 0.001,
            "weights must sum to 1.0, got {sum}"
        );
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
                polymorphism_score: 0.0,
                script_injection_score: 0.0,
                dependency_density_score: 1.0,
            },
            uncertainty_breakdown: UncertaintyBreakdown {
                dynamic_ui_uncertainty_score: 0.0,
                late_binding_uncertainty_score: 0.0,
                dynamic_sql_uncertainty_score: 0.0,
            },
            seam_candidates: vec![],
            guidance: vec![],
            total_incoming: 4,
            total_outgoing: 6,
            total_downstream: 10,
            causal_dependents: 0,
            historical_companions: 0,
            possible_dependents: 0,
            unresolved_endpoints: 0,
            internal_edges: 0,
            top_causal_dependents: vec![],
            coverage: CountCoverage::default(),
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

    // ── Dependency density factor ────────────────────────────────────────────
    //
    // These tests build a tiny graph with a single target and N distinct
    // sources pointing at it via Dependency edges, then call
    // `compute_blast_radius` end-to-end. They verify that the dependency
    // density factor actually moves the composite score — before this was
    // added, every target on projects like the pilot corpus scored 1/10 regardless of
    // how many files depended on it.

    fn tmp_graph() -> (tempfile::TempDir, engram_graph::GraphStore) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let store = engram_graph::GraphStore::open(&tmp.path().join("graph.redb"))
            .expect("GraphStore::open must succeed");
        (tmp, store)
    }

    fn make_file_node(node_id: &str) -> engram_graph::Node {
        engram_graph::Node {
            node_id: node_id.to_string(),
            node_type: "file".to_string(),
            name: node_id.trim_start_matches("file:").to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            file_path: engram_core::RelPath::new(node_id.trim_start_matches("file:")),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        }
    }

    fn make_dep_edge(source: &str, target: &str) -> engram_graph::Edge {
        engram_graph::Edge {
            source_id: source.to_string(),
            target_id: target.to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            edge_kind: EdgeKind::Dependency,
            weight: 1,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        }
    }

    /// Seed a graph: target node + `n_sources` source nodes, each with a
    /// Dependency edge pointing at the target.
    fn seed_incoming_deps(
        store: &engram_graph::GraphStore,
        project_id: &str,
        target_id: &str,
        n_sources: usize,
    ) {
        let target = make_file_node(target_id);
        store
            .upsert_nodes(project_id, std::slice::from_ref(&target))
            .expect("upsert target");

        let mut sources = Vec::with_capacity(n_sources);
        let mut edges = Vec::with_capacity(n_sources);
        for i in 0..n_sources {
            let sid = format!("file:Src{i}.vb");
            sources.push(make_file_node(&sid));
            edges.push(make_dep_edge(&sid, target_id));
        }
        store
            .upsert_nodes(project_id, &sources)
            .expect("upsert sources");
        store
            .upsert_edges(project_id, &edges)
            .expect("upsert edges");
    }

    /// A target with 200 incoming Dependency edges and nothing else should
    /// score at least Medium (4+) — it is the entire point of this change.
    #[test]
    fn compute_blast_radius_high_incoming_alone_is_medium_or_higher() {
        let (_tmp, store) = tmp_graph();
        let target_id = "file:Hub.vb";
        seed_incoming_deps(&store, "proj", target_id, 200);

        let report = compute_blast_radius(&store, "proj", target_id, 1, false)
            .expect("compute_blast_radius must succeed");

        assert_eq!(report.total_incoming, 200);
        assert!(
            (report.complexity_breakdown.dependency_density_score - 10.0).abs() < 0.01,
            "200 incoming deps saturates at 10/10; got {}",
            report.complexity_breakdown.dependency_density_score
        );
        assert!(
            report.migration_risk >= 4,
            "200 incoming Dependency edges alone must score Medium (4+); got {} ({:?})",
            report.migration_risk,
            report.risk_band
        );
        assert!(matches!(
            report.risk_band,
            RiskBand::Medium | RiskBand::High | RiskBand::Critical
        ));
    }

    /// A target with zero incoming dependencies and nothing else should
    /// score Low (1-3). Otherwise the rebalanced scoring has inflated the
    /// floor.
    #[test]
    fn compute_blast_radius_no_signals_is_low() {
        let (_tmp, store) = tmp_graph();
        let target_id = "file:Leaf.vb";
        seed_incoming_deps(&store, "proj", target_id, 0);

        let report = compute_blast_radius(&store, "proj", target_id, 1, false)
            .expect("compute_blast_radius must succeed");

        assert_eq!(report.total_incoming, 0);
        assert!(
            (report.complexity_breakdown.dependency_density_score - 0.0).abs() < 0.01,
            "0 incoming deps is 0/10; got {}",
            report.complexity_breakdown.dependency_density_score
        );
        assert!(
            (1..=3).contains(&report.migration_risk),
            "a target with no signals must score Low (1-3); got {} ({:?})",
            report.migration_risk,
            report.risk_band
        );
        assert_eq!(report.risk_band, RiskBand::Low);
    }

    /// TODO-12: bare-name bindings must not inflate dependency density.
    /// 60 incoming calls at confidence 0.35 ≈ 21 effective — Low/Medium,
    /// not the High that 60 real callers would earn.
    #[test]
    fn low_confidence_callers_are_discounted() {
        let (_tmp, store) = tmp_graph();
        let target_id = "sym:class:Cfg.vb:ConfigSettings.Map:10";
        let target = engram_graph::Node {
            node_id: target_id.to_string(),
            node_type: "class".to_string(),
            name: "ConfigSettings.Map".to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            file_path: engram_core::RelPath::new("Cfg.vb"),
            start_line: 10,
            end_line: 20,
            generation: 1,
            metadata: None,
        };
        store
            .upsert_nodes("proj", std::slice::from_ref(&target))
            .expect("upsert target");

        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for i in 0..60 {
            let sid = format!("sym:function:js{i}.js:helper{i}:1");
            nodes.push(engram_graph::Node {
                node_id: sid.clone(),
                node_type: "function".to_string(),
                name: format!("helper{i}"),
                namespace: "memory".to_string(),
                language: "javascript".to_string(),
                file_path: engram_core::RelPath::new(&format!("js{i}.js")),
                start_line: 1,
                end_line: 2,
                generation: 1,
                metadata: None,
            });
            edges.push(engram_graph::Edge {
                source_id: sid,
                target_id: target_id.to_string(),
                namespace: "memory".to_string(),
                language: "javascript".to_string(),
                edge_kind: EdgeKind::Dependency,
                weight: 1,
                generation: 1,
                metadata: Some(serde_json::json!({
                    "resolution": "batch_unique_any_terminal",
                    "confidence": "0.35"
                })),
                updated_at_ms: 0,
            });
        }
        store.upsert_nodes("proj", &nodes).expect("upsert callers");
        store.upsert_edges("proj", &edges).expect("upsert edges");

        let report = compute_blast_radius(&store, "proj", target_id, 1, true).expect("compute");

        assert_eq!(report.total_incoming, 60, "raw count still reported");
        // 60 * 0.35 = 21 effective vs DEPENDENCY_SATURATION 50 → ~4.2/10,
        // where 60 full-confidence callers saturate at 10/10.
        assert!(
            report.complexity_breakdown.dependency_density_score < 6.0,
            "discounted density must be well below saturation; got {}",
            report.complexity_breakdown.dependency_density_score
        );
        assert!(
            report
                .guidance
                .iter()
                .any(|g| g.concern.contains("Phantom Caller")),
            "phantom-inflation guidance must fire when most callers are bare-name"
        );
    }

    /// TODO-16 guard: synthesized file->symbol Contains edges (metadata
    /// containment=file) are membership, not dependency. They must not
    /// inflate a file's incoming density or its event-wiring score.
    #[test]
    fn file_membership_edges_do_not_inflate_blast_scores() {
        let (_tmp, store) = tmp_graph();
        let file_id = "file:Big.vb";
        let file_node = make_file_node(file_id);
        store
            .upsert_nodes("proj", std::slice::from_ref(&file_node))
            .expect("upsert file");

        // Three symbols inside the file, each linked by a synthesized
        // containment edge exactly as ingest emits it.
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for i in 0..3 {
            let sid = format!("sym:function:Big.vb:Fn{i}:{}", i + 1);
            nodes.push(engram_graph::Node {
                node_id: sid.clone(),
                node_type: "function".to_string(),
                name: format!("Fn{i}"),
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                file_path: engram_core::RelPath::new("Big.vb"),
                start_line: (i + 1) as u32,
                end_line: (i + 2) as u32,
                generation: 1,
                metadata: None,
            });
            edges.push(engram_graph::Edge {
                source_id: file_id.to_string(),
                target_id: sid,
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                edge_kind: EdgeKind::Contains,
                weight: 1,
                generation: 1,
                metadata: Some(serde_json::json!({"containment": "file"})),
                updated_at_ms: 0,
            });
        }
        store.upsert_nodes("proj", &nodes).expect("upsert symbols");
        store
            .upsert_edges("proj", &edges)
            .expect("upsert membership");

        let report = compute_blast_radius(&store, "proj", file_id, 1, false)
            .expect("compute_blast_radius must succeed");

        // Membership edges must not count as incoming dependency mass:
        // nothing actually calls into this file.
        assert!(
            (report.complexity_breakdown.dependency_density_score - 0.0).abs() < 0.01,
            "membership edges must not create incoming density; got {}",
            report.complexity_breakdown.dependency_density_score
        );
        assert_eq!(
            report.risk_band,
            RiskBand::Low,
            "an uncalled file with 3 symbols must stay Low; got {} ({:?})",
            report.migration_risk,
            report.risk_band
        );
    }

    /// A base class with many subclasses must register polymorphism
    /// fan-out: 16 incoming InheritsFrom edges saturate the sub-score
    /// and trigger the Polymorphism Fan-Out guidance item.
    #[test]
    fn compute_blast_radius_base_class_fanout_scores_polymorphism() {
        let (_tmp, store) = tmp_graph();
        let target_id = "sym:class:Base.vb:BasePage:1";
        let target = engram_graph::Node {
            node_id: target_id.to_string(),
            node_type: "class".to_string(),
            name: "BasePage".to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            file_path: engram_core::RelPath::new("Base.vb"),
            start_line: 1,
            end_line: 10,
            generation: 1,
            metadata: None,
        };
        store
            .upsert_nodes("proj", std::slice::from_ref(&target))
            .expect("upsert target");

        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for i in 0..16 {
            let sid = format!("sym:class:Page{i}.vb:Page{i}:1");
            nodes.push(engram_graph::Node {
                node_id: sid.clone(),
                node_type: "class".to_string(),
                name: format!("Page{i}"),
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                file_path: engram_core::RelPath::new(&format!("Page{i}.vb")),
                start_line: 1,
                end_line: 10,
                generation: 1,
                metadata: None,
            });
            edges.push(engram_graph::Edge {
                source_id: sid,
                target_id: target_id.to_string(),
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                edge_kind: EdgeKind::InheritsFrom,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            });
        }
        store
            .upsert_nodes("proj", &nodes)
            .expect("upsert subclasses");
        store.upsert_edges("proj", &edges).expect("upsert inherits");

        let report = compute_blast_radius(&store, "proj", target_id, 1, true)
            .expect("compute_blast_radius must succeed");

        assert!(
            (report.complexity_breakdown.polymorphism_score - 10.0).abs() < 0.01,
            "16 subclasses saturates polymorphism at 10/10; got {}",
            report.complexity_breakdown.polymorphism_score
        );
        assert!(
            report
                .guidance
                .iter()
                .any(|g| g.concern.contains("Polymorphism")),
            "polymorphism guidance must fire"
        );
        assert!(
            report.migration_risk >= 2,
            "base-class fan-out must lift risk above the floor; got {}",
            report.migration_risk
        );
    }

    /// A target with 100 incoming dependencies AND SQL edges AND state
    /// coupling edges should score High (7-8). Under the rebalanced
    /// weights (dep=0.40, sql=0.15, state=0.15) with no base/uncertainty
    /// dilution, the three saturated factors land at 7.0 → migration_risk 7.
    #[test]
    fn compute_blast_radius_combined_signals_is_high() {
        let (_tmp, store) = tmp_graph();
        let target_id = "file:Risky.vb";
        seed_incoming_deps(&store, "proj", target_id, 100);

        // Add 12 outgoing SqlCalls edges (saturates sql_concat_score at 10).
        // Each call targets a distinct fake SQL endpoint node.
        let mut sql_nodes = Vec::new();
        let mut sql_edges = Vec::new();
        for i in 0..12 {
            let sid = format!("sql:Query{i}");
            sql_nodes.push(engram_graph::Node {
                node_id: sid.clone(),
                node_type: "sql".to_string(),
                name: sid.clone(),
                namespace: "memory".to_string(),
                language: "sql".to_string(),
                file_path: engram_core::RelPath::new("sql"),
                start_line: 0,
                end_line: 0,
                generation: 1,
                metadata: None,
            });
            sql_edges.push(engram_graph::Edge {
                source_id: target_id.to_string(),
                target_id: sid,
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                edge_kind: EdgeKind::SqlCalls,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            });
        }

        // Add 12 outgoing ReadsState edges (saturates state_coupling_score at 10).
        let mut state_nodes = Vec::new();
        let mut state_edges = Vec::new();
        for i in 0..12 {
            let sid = format!("state:Key{i}");
            state_nodes.push(engram_graph::Node {
                node_id: sid.clone(),
                node_type: "global_state".to_string(),
                name: sid.clone(),
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                file_path: engram_core::RelPath::new("state"),
                start_line: 0,
                end_line: 0,
                generation: 1,
                metadata: None,
            });
            state_edges.push(engram_graph::Edge {
                source_id: target_id.to_string(),
                target_id: sid,
                namespace: "memory".to_string(),
                language: "vb".to_string(),
                edge_kind: EdgeKind::ReadsState,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 0,
            });
        }

        store
            .upsert_nodes("proj", &sql_nodes)
            .expect("upsert sql nodes");
        store
            .upsert_nodes("proj", &state_nodes)
            .expect("upsert state nodes");
        store
            .upsert_edges("proj", &sql_edges)
            .expect("upsert sql edges");
        store
            .upsert_edges("proj", &state_edges)
            .expect("upsert state edges");

        let report = compute_blast_radius(&store, "proj", target_id, 1, false)
            .expect("compute_blast_radius must succeed");

        // Three factors saturated at 10/10: dep, sql, state.
        // base_weighted = 10*0.40 + 10*0.15 + 10*0.15 = 7.0.
        // No base/uncertainty dilution → raw_score = 7.0 → migration_risk 7.
        assert!(
            report.migration_risk >= 7,
            "combined signals (deps + sql + state) must score High (≥7); got {} ({:?})",
            report.migration_risk,
            report.risk_band
        );
        assert!(matches!(
            report.risk_band,
            RiskBand::High | RiskBand::Critical
        ));
        // All three sub-scores must be populated and saturated.
        assert!((report.complexity_breakdown.dependency_density_score - 10.0).abs() < 0.01);
        assert!((report.complexity_breakdown.sql_concat_score - 10.0).abs() < 0.01);
        assert!((report.complexity_breakdown.state_coupling_score - 10.0).abs() < 0.01);
    }

    /// Dependency density dominates the weighted score — a target with a
    /// huge incoming fan-in should outscore a target with everything else
    /// maxed out except dependency density.
    #[test]
    fn dependency_density_factor_dominates_composite() {
        let (_tmp, hub) = tmp_graph();
        seed_incoming_deps(&hub, "proj", "file:Hub.vb", 150);
        let hub_report = compute_blast_radius(&hub, "proj", "file:Hub.vb", 1, false).unwrap();

        // "Isolated" target with zero incoming deps → density_score 0 →
        // it contributes nothing through the 0.30-weighted factor.
        let (_tmp2, leaf) = tmp_graph();
        seed_incoming_deps(&leaf, "proj", "file:Leaf.vb", 0);
        let leaf_report = compute_blast_radius(&leaf, "proj", "file:Leaf.vb", 1, false).unwrap();

        assert!(
            hub_report.migration_risk > leaf_report.migration_risk,
            "hub with 150 incoming must out-score isolated leaf; hub={} leaf={}",
            hub_report.migration_risk,
            leaf_report.migration_risk
        );
    }

    // ── File-level transitive aggregation ───────────────────────────────────
    //
    // Regression guard. File nodes themselves hold only a few direct edges
    // (Contains to their symbols). The real blast radius of touching a file
    // comes from the functions/classes INSIDE the file. Before this was
    // added, `compute_blast_radius("file:foo.vb")` returned 0 incoming and
    // 1 outgoing (the Contains edge) even when 732 other files' symbols
    // depended on foo.vb's contents. The function must now follow Contains
    // edges from the file to its symbols and aggregate every edge touching
    // any contained symbol.

    fn make_sym_node_in_file(node_id: &str, file_path: &str) -> engram_graph::Node {
        engram_graph::Node {
            node_id: node_id.to_string(),
            node_type: "function".to_string(),
            name: node_id.to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            file_path: engram_core::RelPath::new(file_path),
            start_line: 0,
            end_line: 0,
            generation: 1,
            metadata: None,
        }
    }

    fn make_typed_edge(source: &str, target: &str, kind: EdgeKind) -> engram_graph::Edge {
        engram_graph::Edge {
            source_id: source.to_string(),
            target_id: target.to_string(),
            namespace: "memory".to_string(),
            language: "vb".to_string(),
            edge_kind: kind,
            weight: 1,
            generation: 1,
            metadata: None,
            updated_at_ms: 0,
        }
    }

    /// File target must aggregate edges into the symbols it contains.
    /// Containment is discovered by `Node.file_path` equality: the real
    /// indexer never emits `file → symbol` Contains edges, and before
    /// this fix a Contains-based lookup returned zero on real projects
    /// even when hundreds of other files depended on the inner symbols.
    #[test]
    fn compute_blast_radius_file_target_aggregates_contained_symbol_edges() {
        let (_tmp, store) = tmp_graph();
        let project = "proj";
        let file_id = "file:test.vb";

        // File + 5 functions that live inside it (same file_path).
        let mut nodes = vec![make_file_node(file_id)];
        let contained: Vec<String> = (0..5)
            .map(|i| format!("sym:function:test.vb:Inner{i}:{}", i * 10))
            .collect();
        for sid in &contained {
            nodes.push(make_sym_node_in_file(sid, "test.vb"));
        }

        // 10 functions in OTHER files (distinct `file_path`), each calling
        // one of the inner symbols round-robin. These are the incoming
        // callers that file-level blast radius should surface.
        let callers: Vec<String> = (0..10)
            .map(|i| format!("sym:function:other{i}.vb:Caller:0"))
            .collect();
        for (i, cid) in callers.iter().enumerate() {
            nodes.push(make_sym_node_in_file(cid, &format!("other{i}.vb")));
        }
        store.upsert_nodes(project, &nodes).expect("upsert nodes");

        // No `file → symbol` Contains edges are needed — containment is
        // `file_path` equality, not an edge shape.
        let mut edges = Vec::new();
        for (i, caller) in callers.iter().enumerate() {
            let callee = &contained[i % contained.len()];
            edges.push(make_typed_edge(caller, callee, EdgeKind::Dependency));
        }
        store.upsert_edges(project, &edges).expect("upsert edges");

        let report = compute_blast_radius(&store, project, file_id, 1, false)
            .expect("compute_blast_radius must succeed");

        assert!(
            report.total_incoming >= 10,
            "file target must aggregate 10 incoming Dependency edges through its \
             contained symbols (file_path-based containment); got total_incoming={}",
            report.total_incoming
        );
        // Dependency density is now non-trivial for a file hub.
        assert!(
            report.complexity_breakdown.dependency_density_score > 0.0,
            "aggregated incoming → dependency density > 0; got {}",
            report.complexity_breakdown.dependency_density_score
        );
    }

    /// ACCEPTANCE: adding temporal-coupling edges must not change causal
    /// impact or the risk score. "Usually changed together" is companion
    /// evidence, never "will break".
    #[test]
    fn temporal_edges_cannot_change_causal_impact_or_risk() {
        let (_tmp, store) = tmp_graph();
        let project = "proj";
        let file_id = "file:test.vb";
        let mut nodes = vec![make_file_node(file_id)];
        let inner = "sym:function:test.vb:Inner:10".to_string();
        nodes.push(make_sym_node_in_file(&inner, "test.vb"));
        let callers: Vec<String> = (0..3)
            .map(|i| format!("sym:function:other{i}.vb:Caller:0"))
            .collect();
        for (i, cid) in callers.iter().enumerate() {
            nodes.push(make_sym_node_in_file(cid, &format!("other{i}.vb")));
        }
        // 50 "companion" files that only co-change with the target.
        let companions: Vec<String> = (0..50).map(|i| format!("file:companion{i}.resx")).collect();
        for c in &companions {
            nodes.push(make_file_node(c));
        }
        store.upsert_nodes(project, &nodes).expect("upsert nodes");

        let causal: Vec<_> = callers
            .iter()
            .map(|c| make_typed_edge(c, &inner, EdgeKind::Calls))
            .collect();
        store.upsert_edges(project, &causal).expect("upsert causal");
        let before = compute_blast_radius(&store, project, file_id, 1, false).expect("before");

        let temporal: Vec<_> = companions
            .iter()
            .map(|c| make_typed_edge(c, file_id, EdgeKind::TemporalCoupling))
            .collect();
        store
            .upsert_edges(project, &temporal)
            .expect("upsert temporal");
        let after = compute_blast_radius(&store, project, file_id, 1, false).expect("after");

        assert_eq!(
            after.causal_dependents, before.causal_dependents,
            "temporal edges must not change causal dependents"
        );
        assert_eq!(
            after.complexity_breakdown.dependency_density_score,
            before.complexity_breakdown.dependency_density_score,
            "temporal edges must not change the dependency-density score"
        );
        assert_eq!(
            after.migration_risk, before.migration_risk,
            "temporal edges must not change migration_risk"
        );
        assert_eq!(
            after.historical_companions, 50,
            "companions reported separately"
        );
        assert_eq!(before.causal_dependents, 3, "3 unique callers");
    }

    /// ACCEPTANCE: intra-file calls (both endpoints inside the file) are
    /// internal complexity and must not change external dependent counts or
    /// the score.
    #[test]
    fn intra_file_calls_cannot_change_external_dependents() {
        let (_tmp, store) = tmp_graph();
        let project = "proj";
        let file_id = "file:test.vb";
        let mut nodes = vec![make_file_node(file_id)];
        let inner: Vec<String> = (0..6)
            .map(|i| format!("sym:function:test.vb:Inner{i}:{}", i * 10))
            .collect();
        for s in &inner {
            nodes.push(make_sym_node_in_file(s, "test.vb"));
        }
        let ext = "sym:function:other.vb:Caller:0".to_string();
        nodes.push(make_sym_node_in_file(&ext, "other.vb"));
        store.upsert_nodes(project, &nodes).expect("upsert nodes");

        store
            .upsert_edges(
                project,
                &[make_typed_edge(&ext, &inner[0], EdgeKind::Calls)],
            )
            .expect("upsert ext");
        let before = compute_blast_radius(&store, project, file_id, 1, false).expect("before");

        // 5 internal calls Inner0 -> Inner1 -> ... inside the same file.
        let internal: Vec<_> = (0..5)
            .map(|i| make_typed_edge(&inner[i], &inner[i + 1], EdgeKind::Calls))
            .collect();
        store
            .upsert_edges(project, &internal)
            .expect("upsert internal");
        let after = compute_blast_radius(&store, project, file_id, 1, false).expect("after");

        assert_eq!(
            after.causal_dependents, before.causal_dependents,
            "internal wiring must not add external dependents"
        );
        assert_eq!(
            after.total_incoming, before.total_incoming,
            "internal wiring must not inflate incoming"
        );
        assert_eq!(
            after.migration_risk, before.migration_risk,
            "internal wiring must not change risk"
        );
        assert_eq!(
            after.internal_edges, 5,
            "internal edges reported as complexity"
        );
        assert_eq!(before.causal_dependents, 1);
    }

    /// AUDITOR'S FIXTURE: 1001 high-weight internal Calls (over the 1000/kind
    /// incoming cap) + 1 low-weight external Calls whose id sorts after every
    /// internal id. Under a raw first-N fetch the internal edges consume the
    /// budget and the external caller vanishes, producing a falsely LOW score.
    #[test]
    fn internal_flood_cannot_hide_external_caller_in_blast() {
        let (_tmp, store) = tmp_graph();
        let project = "proj";
        let file_id = "file:test.vb";
        let mut nodes = vec![make_file_node(file_id)];
        let hub = "sym:function:test.vb:Hub:1".to_string();
        nodes.push(make_sym_node_in_file(&hub, "test.vb"));
        let mut edges = Vec::new();
        for i in 0..1001 {
            let s = format!("sym:function:test.vb:Inner{i:04}:{}", i + 10);
            nodes.push(make_sym_node_in_file(&s, "test.vb"));
            let mut e = make_typed_edge(&s, &hub, EdgeKind::Calls);
            e.weight = 9999;
            edges.push(e);
        }
        let ext = "sym:function:zz_other.vb:ZzExternal:1";
        nodes.push(make_sym_node_in_file(ext, "zz_other.vb"));
        edges.push(make_typed_edge(ext, &hub, EdgeKind::Calls));
        store.upsert_nodes(project, &nodes).expect("nodes");
        store.upsert_edges(project, &edges).expect("edges");

        let report = compute_blast_radius(&store, project, file_id, 1, false).expect("report");
        assert_eq!(
            report.causal_dependents, 1,
            "the single external caller must survive 1001 internal ones"
        );
        assert!(
            !report.coverage.causal_truncated,
            "internal edges never consume the causal cap: {:?}",
            report.coverage.truncated_fetches
        );
        assert!(report.internal_edges >= 1001, "internal wiring counted");
    }

    /// Classification regression: oracle and runtime kinds are never
    /// incoming-causal; runtime kinds are "possible".
    #[test]
    fn oracle_and_runtime_kinds_are_not_causal() {
        assert!(!is_causal_dependency(&EdgeKind::TestOracle));
        assert!(!is_causal_dependency(&EdgeKind::ObservedRuntimeControl));
        assert!(!is_causal_dependency(&EdgeKind::ObservedRuntimeSql));
        assert!(is_possible_dependency(&EdgeKind::ObservedRuntimeControl));
        assert!(is_possible_dependency(&EdgeKind::ObservedRuntimeSql));
        assert!(!is_possible_dependency(&EdgeKind::Calls));
        assert!(is_causal_dependency(&EdgeKind::ReadsSetting));
        assert!(is_causal_dependency(&EdgeKind::InheritsFrom));
        assert!(is_causal_dependency(&EdgeKind::Implements));
    }

    /// ACCEPTANCE: exactly-at-cap is complete; only cap+1 is truncated.
    #[test]
    fn cap_boundary_is_exact() {
        let (_tmp, store) = tmp_graph();
        let project = "proj";
        let target = "sym:function:t.vb:Target:0";
        let mut nodes = vec![make_sym_node_in_file(target, "t.vb")];
        // Exactly CAP_INCOMING (1000) distinct callers: must NOT be truncated.
        let callers: Vec<String> = (0..1000)
            .map(|i| format!("sym:function:c{i}.vb:C:0"))
            .collect();
        for (i, c) in callers.iter().enumerate() {
            nodes.push(make_sym_node_in_file(c, &format!("c{i}.vb")));
        }
        store.upsert_nodes(project, &nodes).expect("nodes");
        let edges: Vec<_> = callers
            .iter()
            .map(|c| make_typed_edge(c, target, EdgeKind::Calls))
            .collect();
        store.upsert_edges(project, &edges).expect("edges");
        let r = compute_blast_radius(&store, project, target, 1, false).expect("report");
        assert!(
            !r.coverage
                .truncated_fetches
                .contains(&"incoming".to_string()),
            "exactly 1000 incoming must be COMPLETE, not truncated: {:?}",
            r.coverage.truncated_fetches
        );
        assert_eq!(r.causal_dependents, 1000);
    }

    /// Symbol-level queries must be unaffected by the transitive pass —
    /// the `file:` prefix guard is what distinguishes the two paths.
    #[test]
    fn compute_blast_radius_symbol_target_does_not_aggregate() {
        let (_tmp, store) = tmp_graph();
        let project = "proj";
        let sym_id = "sym:function:test.vb:Target:10";
        let sibling_id = "sym:function:test.vb:Sibling:20";

        // Seed: target with 3 direct Dependency incoming + a sibling in
        // the same file that *also* receives a Dependency. A `sym:` query
        // must count only direct edges and must NOT pull in the sibling
        // edge, which would happen if the transitive branch accidentally
        // fired for a symbol target.
        store
            .upsert_nodes(
                project,
                &[
                    make_sym_node_in_file(sym_id, "test.vb"),
                    make_sym_node_in_file(sibling_id, "test.vb"),
                    make_file_node("file:test.vb"),
                    make_sym_node_in_file("sym:function:other.vb:A:0", "other.vb"),
                    make_sym_node_in_file("sym:function:other.vb:B:0", "other.vb"),
                    make_sym_node_in_file("sym:function:other.vb:C:0", "other.vb"),
                ],
            )
            .expect("upsert nodes");
        store
            .upsert_edges(
                project,
                &[
                    make_typed_edge("sym:function:other.vb:A:0", sym_id, EdgeKind::Dependency),
                    make_typed_edge("sym:function:other.vb:B:0", sym_id, EdgeKind::Dependency),
                    make_typed_edge("sym:function:other.vb:C:0", sym_id, EdgeKind::Dependency),
                    // This edge targets the sibling — must NOT leak into
                    // the target's count.
                    make_typed_edge(
                        "sym:function:other.vb:A:0",
                        sibling_id,
                        EdgeKind::Dependency,
                    ),
                ],
            )
            .expect("upsert edges");

        let report = compute_blast_radius(&store, project, sym_id, 1, false)
            .expect("compute_blast_radius must succeed");

        // Exactly 3 Dependency edges point at the target. The sibling's
        // incoming edge must not leak in.
        assert_eq!(
            report.total_incoming, 3,
            "symbol target must count only its direct Dependency edges"
        );
    }

    /// Regression guard for the pilot-corpus shape: a file node with *zero*
    /// `Contains` outgoing edges (that is how the real VB/C# extractors
    /// emit — they attach Contains to the namespace/class, not the file)
    /// must still surface incoming edges on its contained symbols through
    /// the `file_path`-equality containment pass.
    ///
    /// Before the switch from Contains-based to file_path-based
    /// containment this test returned `total_incoming == 0`.
    #[test]
    fn compute_blast_radius_file_target_with_no_contains_edges_still_aggregates() {
        let (_tmp, store) = tmp_graph();
        let project = "proj";
        let file_id = "file:ConfigSettings.vb";

        // 3 inner symbols live in ConfigSettings.vb. NO `file → sym`
        // Contains edges — matching what the real extractors produce.
        let mut nodes = vec![make_file_node(file_id)];
        let inner = [
            "sym:function:ConfigSettings.vb:GetSetting:10",
            "sym:function:ConfigSettings.vb:SaveSetting:40",
            "sym:function:ConfigSettings.vb:LoadAll:70",
        ];
        for sid in &inner {
            nodes.push(make_sym_node_in_file(sid, "ConfigSettings.vb"));
        }
        // 7 callers in OTHER files, each hitting one of the inner syms.
        let callers: Vec<String> = (0..7)
            .map(|i| format!("sym:function:Consumer{i}.vb:Use:0"))
            .collect();
        for (i, cid) in callers.iter().enumerate() {
            nodes.push(make_sym_node_in_file(cid, &format!("Consumer{i}.vb")));
        }
        store.upsert_nodes(project, &nodes).expect("upsert nodes");

        let mut edges = Vec::new();
        for (i, caller) in callers.iter().enumerate() {
            edges.push(make_typed_edge(
                caller,
                inner[i % inner.len()],
                EdgeKind::Dependency,
            ));
        }
        store.upsert_edges(project, &edges).expect("upsert edges");

        // Sanity: traversing Contains outgoing from the file returns
        // nothing — exactly the condition that tripped up the previous
        // Contains-based aggregation.
        let contains_children = store
            .neighbors(project, EdgeKind::Contains, file_id, 1_000)
            .expect("neighbors");
        assert!(
            contains_children.is_empty(),
            "precondition: file has no Contains outgoing (matches real indexer output)"
        );

        let report = compute_blast_radius(&store, project, file_id, 1, false)
            .expect("compute_blast_radius must succeed");

        assert_eq!(
            report.total_incoming, 7,
            "file target with no Contains edges must still aggregate the 7 \
             incoming Dependency edges into its inner symbols via file_path"
        );
    }
}
