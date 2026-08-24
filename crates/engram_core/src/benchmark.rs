//! Gold-standard benchmark schemas for retrieval, ADP, and trace evaluation.
//!
//! Defines the canonical data structures for:
//! - Benchmark query packs (retrieval quality)
//! - ADP decision corpora (calibration)
//! - UI-event trace scenario libraries (WebForms correctness)
//!
//! All schemas are versioned via `schema_version` fields for forward compatibility.

use serde::{Deserialize, Serialize};

// ── Benchmark Pack (Retrieval Quality) ──────────────────────────────────────

/// A versioned benchmark pack containing queries with known-good results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkPack {
    /// Semantic version tag for this benchmark pack (e.g., "1.0.0").
    pub schema_version: String,
    /// Human-readable name (e.g., "legacy-webforms-v1").
    pub name: String,
    /// Description of what this pack covers.
    pub description: String,
    /// Query class tags (e.g., "state_reader", "sql_writer", "blast_radius").
    pub tags: Vec<String>,
    /// The benchmark queries with ground-truth results.
    pub queries: Vec<BenchmarkQueryEntry>,
    /// Minimum thresholds that must be met per query class.
    pub thresholds: BenchmarkThresholds,
}

/// A single benchmark query with expected results and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkQueryEntry {
    /// Unique identifier for this query (e.g., "q001_find_control_writers").
    pub query_id: String,
    /// The natural-language query string.
    pub query: String,
    /// Query class for per-class threshold evaluation.
    pub query_class: String,
    /// Expected relevant file paths, ordered by decreasing relevance.
    pub relevant_paths: Vec<String>,
    /// Rationale for why these paths are relevant (for human review).
    pub rationale: String,
    /// Language/project type this query targets.
    pub language: Option<String>,
}

/// Minimum quality thresholds per query class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkThresholds {
    /// Global NDCG@10 floor.
    pub min_ndcg_at_10: f64,
    /// Global Recall@10 floor.
    pub min_recall_at_10: f64,
    /// Global MRR floor.
    pub min_mrr: f64,
    /// Per-class overrides: class name → (min_ndcg, min_recall).
    #[serde(default)]
    pub per_class: std::collections::HashMap<String, ClassThreshold>,
}

/// Per-class quality threshold override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassThreshold {
    pub min_ndcg: f64,
    pub min_recall: f64,
}

// ── ADP Decision Corpus (Calibration) ───────────────────────────────────────

/// A labeled corpus of ADP decision scenarios for calibration and replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdpCorpus {
    pub schema_version: String,
    pub name: String,
    pub description: String,
    pub scenarios: Vec<AdpScenario>,
}

/// A single labeled ADP scenario with input and expected verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdpScenario {
    /// Unique scenario identifier (e.g., "s001_safe_rename").
    pub scenario_id: String,
    /// Human-readable description of what this scenario tests.
    pub description: String,
    /// Risk class tag: "low", "medium", "high".
    pub risk_class: String,
    /// Source of this scenario: "synthetic", "test", "incident_retro".
    pub source: String,
    /// Input parameters for the ADP gate pipeline.
    pub input: AdpScenarioInput,
    /// Expected verdict: "allow", "deny", "abstain".
    pub expected_verdict: String,
    /// Expected failed gates (if deny/abstain).
    pub expected_failed_gates: Vec<String>,
    /// Rationale for expected verdict (for human adjudication).
    pub rationale: String,
}

/// Serializable ADP input for replay scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdpScenarioInput {
    pub extraction_confidence: Option<f64>,
    pub extraction_band: Option<String>,
    pub trace_used_fallback: bool,
    pub trace_candidate_count: usize,
    pub safety_allowed: Option<bool>,
    pub safety_confidence: Option<f64>,
    pub retrieval_production_ready: Option<bool>,
    pub retrieval_ndcg: Option<f64>,
    pub retrieval_recall: Option<f64>,
    pub blast_radius_risk: Option<u8>,
    pub blast_radius_band: Option<String>,
    pub blast_radius_downstream: Option<usize>,
    pub immune_verdict: Option<String>,
    pub immune_confidence: Option<f32>,
    pub require_runtime_evidence: bool,
    pub has_runtime_evidence: bool,
    pub risk_profile: String,
    pub min_extraction_confidence: f64,
    pub min_safety_confidence: f64,
    pub max_blast_radius_for_auto: u8,

    // ── vNext fields (all serde(default) for backward compat) ──
    /// Reconciliation confirmed ratio (0.0–1.0). If present, used instead of has_runtime_evidence.
    #[serde(default)]
    pub reconciliation_confirmed_ratio: Option<f64>,
    /// Reconciliation contradicted ratio (0.0–1.0).
    #[serde(default)]
    pub reconciliation_contradicted_ratio: Option<f64>,
    /// Reconciliation confidence delta.
    #[serde(default)]
    pub reconciliation_confidence_delta: Option<f64>,
    /// Number of static paths in reconciliation.
    #[serde(default)]
    pub reconciliation_static_paths: Option<usize>,

    /// Retrieval evaluation mode: "skipped", "cached", "live".
    #[serde(default)]
    pub retrieval_mode: Option<String>,

    /// Migration class for calibrated thresholds (e.g., "data_access", "webforms_page").
    #[serde(default)]
    pub migration_class: Option<String>,

    /// True when the blast report's CAUSAL coverage was truncated (fetch cap
    /// hit) — the risk score was computed from a subset of the causal callers.
    /// The gate must treat incomplete causal evidence as a failure (abstain),
    /// so the corpus must be able to express it; `serde(default)` keeps every
    /// existing scenario file valid (absent = None = coverage not recorded).
    #[serde(default)]
    pub blast_causal_truncated: Option<bool>,
}

// ── Trace Scenario Library (WebForms) ───────────────────────────────────────

/// A library of UI-event trace scenarios for WebForms correctness validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceScenarioLibrary {
    pub schema_version: String,
    pub name: String,
    pub description: String,
    pub scenarios: Vec<TraceScenario>,
}

/// A single trace scenario with fixture files and expected trace paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceScenario {
    /// Unique scenario identifier (e.g., "t001_onclick_to_sql").
    pub scenario_id: String,
    /// Human-readable description.
    pub description: String,
    /// Category: "onclick", "handles", "dynamic_control", "master_page", etc.
    pub category: String,
    /// Fixture files to create (path → content).
    pub fixtures: Vec<TraceFixtureFile>,
    /// Trace query parameters.
    pub trace_input: TraceInput,
    /// Expected trace path (ordered node types/names).
    pub expected_path: Vec<TracePathStep>,
    /// Whether this scenario is expected to produce ambiguous results.
    pub expect_ambiguous: bool,
    /// Expected confidence band if applicable.
    pub expected_confidence_band: Option<String>,
}

/// A fixture file for a trace scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceFixtureFile {
    /// Relative path within the test project.
    pub path: String,
    /// File content.
    pub content: String,
}

/// Input parameters for a trace scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceInput {
    pub page_path: String,
    pub control_id: Option<String>,
    pub handler_fqn: Option<String>,
    pub max_hops: u8,
    pub max_paths: usize,
}

/// An expected step in a trace path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracePathStep {
    /// Expected node type (e.g., "control", "function", "inline_sql").
    pub node_type: String,
    /// Expected node name substring (partial match).
    pub name_contains: String,
    /// Expected justification text substring.
    pub justification_contains: Option<String>,
}

// ── Benchmark Report (CI artifact) ──────────────────────────────────────────

/// Machine-readable benchmark report for CI artifact upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: String,
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// Git commit hash or build ID.
    pub build_id: String,
    /// Pack name used for this run.
    pub pack_name: String,
    /// Aggregate metrics.
    pub aggregate: AggregateMetrics,
    /// Per-class breakdowns.
    pub per_class: std::collections::HashMap<String, AggregateMetrics>,
    /// Per-query details.
    pub per_query: Vec<QueryResult>,
    /// Whether all thresholds were met.
    pub all_gates_passed: bool,
    /// Failing gate details (if any).
    pub failing_gates: Vec<String>,
}

/// Aggregate quality metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateMetrics {
    pub ndcg_at_10: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
    pub mean_latency_ms: f64,
    pub p95_latency_ms: f64,
    pub query_count: usize,
}

/// Per-query benchmark result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub query_id: String,
    pub query_class: String,
    pub ndcg: f64,
    pub recall: f64,
    pub reciprocal_rank: f64,
    pub latency_ms: u64,
    pub expected_paths: Vec<String>,
    pub actual_paths: Vec<String>,
}

// ── Drift Detection ─────────────────────────────────────────────────────────

/// Drift report comparing two benchmark runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftReport {
    pub schema_version: String,
    pub baseline_build_id: String,
    pub current_build_id: String,
    pub timestamp: String,
    /// Per-metric deltas (positive = improvement, negative = regression).
    pub ndcg_delta: f64,
    pub recall_delta: f64,
    pub mrr_delta: f64,
    /// Whether any metric regressed beyond the allowed threshold.
    pub has_regression: bool,
    /// Max allowed regression percentage (e.g., 0.03 = 3%).
    pub regression_threshold: f64,
    /// Per-class drift details.
    pub per_class_deltas: std::collections::HashMap<String, ClassDelta>,
}

/// Per-class quality delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassDelta {
    pub ndcg_delta: f64,
    pub recall_delta: f64,
    pub regressed: bool,
}

impl BenchmarkPack {
    /// Create a default legacy WebForms benchmark pack with representative queries.
    pub fn legacy_webforms_v1() -> Self {
        BenchmarkPack {
            schema_version: "1.0.0".into(),
            name: "legacy-webforms-v1".into(),
            description: "Benchmark pack for ASP.NET WebForms legacy modernization queries".into(),
            tags: vec!["webforms".into(), "legacy".into(), "aspnet".into()],
            queries: vec![
                BenchmarkQueryEntry {
                    query_id: "q001_control_writes_db".into(),
                    query: "find where this control writes to the database".into(),
                    query_class: "sql_writer".into(),
                    relevant_paths: vec![
                        "Orders.aspx.cs".into(),
                        "App_Code/OrderRepository.cs".into(),
                    ],
                    rationale: "Control click handler → repository → SQL INSERT/UPDATE".into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q002_files_changed_with_page".into(),
                    query: "files that changed with this page over time".into(),
                    query_class: "temporal_coupling".into(),
                    relevant_paths: vec![
                        "Orders.aspx".into(),
                        "Orders.aspx.cs".into(),
                        "Orders.aspx.designer.cs".into(),
                    ],
                    rationale: "Temporal coupling from git co-occurrence".into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q003_state_key_readers_writers".into(),
                    query: "session state key readers and writers for UserProfile".into(),
                    query_class: "state_reader".into(),
                    relevant_paths: vec![
                        "Login.aspx.cs".into(),
                        "Profile.aspx.cs".into(),
                        "App_Code/SessionHelper.cs".into(),
                    ],
                    rationale: "Session[\"UserProfile\"] accessed across login, profile, helper"
                        .into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q004_blast_radius_handler".into(),
                    query: "blast radius for btnSave_Click handler".into(),
                    query_class: "blast_radius".into(),
                    relevant_paths: vec![
                        "Orders.aspx.cs".into(),
                        "App_Code/OrderService.cs".into(),
                        "App_Code/DbHelper.cs".into(),
                    ],
                    rationale: "Handler → service → DB: full downstream chain".into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q005_gridview_datasource".into(),
                    query: "GridView DataSource binding and data access".into(),
                    query_class: "data_binding".into(),
                    relevant_paths: vec!["Orders.aspx".into(), "Orders.aspx.cs".into()],
                    rationale: "GridView bound via ObjectDataSource or code-behind DataBind()"
                        .into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q006_master_page_layout".into(),
                    query: "master page layout and content placeholders".into(),
                    query_class: "ui_layout".into(),
                    relevant_paths: vec!["Site.Master".into(), "Site.Master.cs".into()],
                    rationale: "Master page defines layout structure for all pages".into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q007_ajax_update_panel".into(),
                    query: "AJAX UpdatePanel and partial postback handlers".into(),
                    query_class: "async_postback".into(),
                    relevant_paths: vec!["Dashboard.aspx".into(), "Dashboard.aspx.cs".into()],
                    rationale: "UpdatePanel async postback with ScriptManager".into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q008_global_error_handling".into(),
                    query: "global error handling Application_Error".into(),
                    query_class: "error_handling".into(),
                    relevant_paths: vec![
                        "Global.asax".into(),
                        "Global.asax.cs".into(),
                        "web.config".into(),
                    ],
                    rationale: "Application-level error handling in Global.asax".into(),
                    language: Some("csharp".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q009_vb_handles_clause".into(),
                    query: "VB.NET Handles clause event wiring for button click".into(),
                    query_class: "event_wiring".into(),
                    relevant_paths: vec!["Order.aspx.vb".into()],
                    rationale: "VB uses Handles keyword instead of OnClick attribute".into(),
                    language: Some("vb".into()),
                },
                BenchmarkQueryEntry {
                    query_id: "q010_stored_proc_call".into(),
                    query: "stored procedure call from code-behind using SqlCommand".into(),
                    query_class: "sql_writer".into(),
                    relevant_paths: vec![
                        "Reports.aspx.cs".into(),
                        "App_Code/ReportRepository.cs".into(),
                    ],
                    rationale: "SqlCommand with CommandType.StoredProcedure".into(),
                    language: Some("csharp".into()),
                },
            ],
            thresholds: BenchmarkThresholds {
                min_ndcg_at_10: 0.5,
                min_recall_at_10: 0.6,
                min_mrr: 0.4,
                per_class: std::collections::HashMap::new(),
            },
        }
    }
}

impl DriftReport {
    /// Check whether any metric exceeds the allowed regression threshold.
    pub fn check_regression(&self) -> bool {
        self.ndcg_delta < -self.regression_threshold
            || self.recall_delta < -self.regression_threshold
            || self.mrr_delta < -self.regression_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_pack_has_10_queries() {
        let pack = BenchmarkPack::legacy_webforms_v1();
        assert_eq!(pack.queries.len(), 10);
        assert_eq!(pack.schema_version, "1.0.0");
    }

    #[test]
    fn all_queries_have_ids_and_classes() {
        let pack = BenchmarkPack::legacy_webforms_v1();
        for q in &pack.queries {
            assert!(!q.query_id.is_empty(), "query_id must not be empty");
            assert!(!q.query_class.is_empty(), "query_class must not be empty");
            assert!(
                !q.relevant_paths.is_empty(),
                "relevant_paths must not be empty"
            );
        }
    }

    #[test]
    fn drift_report_detects_regression() {
        let report = DriftReport {
            schema_version: "1.0.0".into(),
            baseline_build_id: "abc123".into(),
            current_build_id: "def456".into(),
            timestamp: "2026-02-21T00:00:00Z".into(),
            ndcg_delta: -0.05,
            recall_delta: 0.01,
            mrr_delta: 0.0,
            has_regression: true,
            regression_threshold: 0.03,
            per_class_deltas: std::collections::HashMap::new(),
        };
        assert!(report.check_regression());
    }

    #[test]
    fn drift_report_no_regression_within_threshold() {
        let report = DriftReport {
            schema_version: "1.0.0".into(),
            baseline_build_id: "abc123".into(),
            current_build_id: "def456".into(),
            timestamp: "2026-02-21T00:00:00Z".into(),
            ndcg_delta: -0.02,
            recall_delta: 0.01,
            mrr_delta: -0.01,
            has_regression: false,
            regression_threshold: 0.03,
            per_class_deltas: std::collections::HashMap::new(),
        };
        assert!(!report.check_regression());
    }

    #[test]
    fn adp_corpus_roundtrip() {
        let corpus = AdpCorpus {
            schema_version: "1.0.0".into(),
            name: "test-corpus".into(),
            description: "Test ADP corpus".into(),
            scenarios: vec![AdpScenario {
                scenario_id: "s001".into(),
                description: "Safe rename".into(),
                risk_class: "low".into(),
                source: "synthetic".into(),
                input: AdpScenarioInput {
                    extraction_confidence: Some(0.9),
                    extraction_band: Some("high".into()),
                    trace_used_fallback: false,
                    trace_candidate_count: 0,
                    safety_allowed: Some(true),
                    safety_confidence: Some(0.95),
                    retrieval_production_ready: Some(true),
                    retrieval_ndcg: Some(0.8),
                    retrieval_recall: Some(0.9),
                    blast_radius_risk: Some(2),
                    blast_radius_band: Some("Low".into()),
                    blast_radius_downstream: Some(3),
                    immune_verdict: Some("PASS".into()),
                    immune_confidence: Some(0.05),
                    require_runtime_evidence: false,
                    has_runtime_evidence: false,
                    risk_profile: "medium".into(),
                    min_extraction_confidence: 0.5,
                    min_safety_confidence: 0.7,
                    max_blast_radius_for_auto: 6,
                    reconciliation_confirmed_ratio: None,
                    reconciliation_contradicted_ratio: None,
                    reconciliation_confidence_delta: None,
                    reconciliation_static_paths: None,
                    retrieval_mode: None,
                    migration_class: None,
                    blast_causal_truncated: None,
                },
                expected_verdict: "allow".into(),
                expected_failed_gates: vec![],
                rationale: "All signals green, low risk, high confidence".into(),
            }],
        };
        let json = serde_json::to_string_pretty(&corpus).unwrap();
        let decoded: AdpCorpus = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.scenarios.len(), 1);
        assert_eq!(decoded.scenarios[0].expected_verdict, "allow");
    }

    #[test]
    fn trace_scenario_roundtrip() {
        let lib = TraceScenarioLibrary {
            schema_version: "1.0.0".into(),
            name: "webforms-onclick-v1".into(),
            description: "OnClick event trace scenarios".into(),
            scenarios: vec![TraceScenario {
                scenario_id: "t001".into(),
                description: "Button click to SQL insert".into(),
                category: "onclick".into(),
                fixtures: vec![TraceFixtureFile {
                    path: "Order.aspx".into(),
                    content:
                        "<asp:Button ID=\"btnSave\" runat=\"server\" OnClick=\"btnSave_Click\" />"
                            .into(),
                }],
                trace_input: TraceInput {
                    page_path: "Order.aspx".into(),
                    control_id: Some("btnSave".into()),
                    handler_fqn: None,
                    max_hops: 10,
                    max_paths: 5,
                },
                expected_path: vec![TracePathStep {
                    node_type: "control".into(),
                    name_contains: "btnSave".into(),
                    justification_contains: None,
                }],
                expect_ambiguous: false,
                expected_confidence_band: Some("high".into()),
            }],
        };
        let json = serde_json::to_string_pretty(&lib).unwrap();
        let decoded: TraceScenarioLibrary = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.scenarios.len(), 1);
    }
}
