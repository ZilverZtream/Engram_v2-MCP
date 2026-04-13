//! Retrieval production gates — benchmark harness for search quality.
//!
//! Evaluates vector_search and hybrid search quality using NDCG@k and Recall@k
//! metrics on representative query sets. Search is only promoted to "production"
//! status when benchmark thresholds are met.

use serde::{Deserialize, Serialize};

/// Results from a benchmark evaluation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub project_id: String,
    pub timestamp_ms: u64,
    pub query_count: usize,
    pub ndcg_at_10: f64,
    pub recall_at_10: f64,
    pub mean_reciprocal_rank: f64,
    pub mean_latency_ms: f64,
    pub p95_latency_ms: f64,
    pub passed_ndcg_gate: bool,
    pub passed_recall_gate: bool,
    pub production_ready: bool,
    pub per_query_results: Vec<QueryBenchmarkResult>,
}

/// Per-query benchmark result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryBenchmarkResult {
    pub query: String,
    pub expected_top_paths: Vec<String>,
    pub actual_top_paths: Vec<String>,
    pub ndcg: f64,
    pub recall: f64,
    pub reciprocal_rank: f64,
    pub latency_ms: u64,
}

/// A benchmark query with known relevant results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkQuery {
    pub query: String,
    /// Expected relevant file paths (ordered by relevance).
    pub relevant_paths: Vec<String>,
}

/// Compute NDCG@k for a single query.
///
/// `retrieved` is the list of paths returned by search.
/// `relevant` is the ground-truth ordered list (position implies graded relevance).
pub fn compute_ndcg(retrieved: &[String], relevant: &[String], k: usize) -> f64 {
    let k = k.min(retrieved.len());
    if k == 0 || relevant.is_empty() {
        return 0.0;
    }

    // DCG: sum of relevance/log2(rank+1) for retrieved items
    let dcg: f64 = (0..k)
        .map(|i| {
            let relevance = relevance_score(&retrieved[i], relevant);
            relevance / (i as f64 + 2.0).log2()
        })
        .sum();

    // IDCG: ideal DCG (first k items from relevant list)
    let idcg: f64 = (0..k.min(relevant.len()))
        .map(|i| {
            let relevance = (relevant.len() - i) as f64; // Graded: top item gets highest score
            relevance / (i as f64 + 2.0).log2()
        })
        .sum();

    if idcg == 0.0 {
        0.0
    } else {
        (dcg / idcg).min(1.0)
    }
}

/// Compute Recall@k for a single query.
pub fn compute_recall(retrieved: &[String], relevant: &[String], k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0; // No relevant items means perfect recall by definition
    }
    let k = k.min(retrieved.len());
    let retrieved_set: std::collections::HashSet<&str> =
        retrieved[..k].iter().map(|s| s.as_str()).collect();
    let found = relevant
        .iter()
        .filter(|r| retrieved_set.contains(r.as_str()))
        .count();
    found as f64 / relevant.len() as f64
}

/// Compute reciprocal rank: 1/rank of first relevant result.
pub fn compute_reciprocal_rank(retrieved: &[String], relevant: &[String]) -> f64 {
    for (i, item) in retrieved.iter().enumerate() {
        if relevant.contains(item) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Graded relevance score for a retrieved item.
fn relevance_score(item: &str, relevant: &[String]) -> f64 {
    // Position in the relevant list determines relevance grade.
    // First item in relevant = highest relevance.
    for (i, r) in relevant.iter().enumerate() {
        if r == item {
            return (relevant.len() - i) as f64;
        }
    }
    0.0
}

/// Evaluate benchmark thresholds and return gate decision.
pub fn evaluate_gates(
    ndcg_at_10: f64,
    recall_at_10: f64,
    min_ndcg: f64,
    min_recall: f64,
) -> (bool, bool, bool) {
    let passed_ndcg = ndcg_at_10 >= min_ndcg;
    let passed_recall = recall_at_10 >= min_recall;
    let production_ready = passed_ndcg && passed_recall;
    (passed_ndcg, passed_recall, production_ready)
}

/// Generate a representative set of benchmark queries for common legacy patterns.
pub fn generate_legacy_benchmark_queries() -> Vec<BenchmarkQuery> {
    vec![
        BenchmarkQuery {
            query: "user authentication login page session".into(),
            relevant_paths: vec![
                "Login.aspx".into(),
                "Login.aspx.cs".into(),
                "Global.asax".into(),
                "web.config".into(),
            ],
        },
        BenchmarkQuery {
            query: "database connection string SQL query".into(),
            relevant_paths: vec![
                "web.config".into(),
                "App_Code/DbHelper.cs".into(),
                "App_Code/DataAccess.cs".into(),
            ],
        },
        BenchmarkQuery {
            query: "GridView DataSource binding events".into(),
            relevant_paths: vec![
                "Orders.aspx".into(),
                "Orders.aspx.cs".into(),
                "Orders.aspx.designer.cs".into(),
            ],
        },
        BenchmarkQuery {
            query: "error handling global exception Application_Error".into(),
            relevant_paths: vec![
                "Global.asax".into(),
                "Global.asax.cs".into(),
                "web.config".into(),
            ],
        },
        BenchmarkQuery {
            query: "session state management cookies ViewState".into(),
            relevant_paths: vec!["web.config".into(), "App_Code/SessionHelper.cs".into()],
        },
        BenchmarkQuery {
            query: "master page layout header footer navigation".into(),
            relevant_paths: vec![
                "Site.Master".into(),
                "Site.Master.cs".into(),
                "App_Themes/Default.css".into(),
            ],
        },
        BenchmarkQuery {
            query: "AJAX UpdatePanel ScriptManager async postback".into(),
            relevant_paths: vec![
                "Default.aspx".into(),
                "Default.aspx.cs".into(),
                "Scripts/site.js".into(),
            ],
        },
        BenchmarkQuery {
            query: "user registration form validation RequiredFieldValidator".into(),
            relevant_paths: vec!["Register.aspx".into(), "Register.aspx.cs".into()],
        },
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ndcg_perfect_retrieval() {
        let relevant = vec!["a".into(), "b".into(), "c".into()];
        let retrieved = vec!["a".into(), "b".into(), "c".into()];
        let ndcg = compute_ndcg(&retrieved, &relevant, 10);
        assert!(
            (ndcg - 1.0).abs() < 0.001,
            "Perfect retrieval should have NDCG ~1.0, got {ndcg}"
        );
    }

    #[test]
    fn ndcg_reversed_order() {
        let relevant = vec!["a".into(), "b".into(), "c".into()];
        let retrieved = vec!["c".into(), "b".into(), "a".into()];
        let ndcg = compute_ndcg(&retrieved, &relevant, 10);
        assert!(
            ndcg > 0.0 && ndcg < 1.0,
            "Reversed order should have 0 < NDCG < 1, got {ndcg}"
        );
    }

    #[test]
    fn ndcg_no_relevant_retrieved() {
        let relevant = vec!["a".into(), "b".into()];
        let retrieved = vec!["x".into(), "y".into()];
        let ndcg = compute_ndcg(&retrieved, &relevant, 10);
        assert_eq!(ndcg, 0.0);
    }

    #[test]
    fn recall_at_k() {
        let relevant = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let retrieved = vec!["a".into(), "x".into(), "b".into(), "y".into()];
        let recall = compute_recall(&retrieved, &relevant, 4);
        assert!((recall - 0.5).abs() < 0.001);
    }

    #[test]
    fn reciprocal_rank_first_hit() {
        let relevant = vec!["a".into(), "b".into()];
        let retrieved = vec!["a".into(), "x".into()];
        assert!((compute_reciprocal_rank(&retrieved, &relevant) - 1.0).abs() < 0.001);
    }

    #[test]
    fn reciprocal_rank_second_hit() {
        let relevant = vec!["a".into()];
        let retrieved = vec!["x".into(), "a".into()];
        assert!((compute_reciprocal_rank(&retrieved, &relevant) - 0.5).abs() < 0.001);
    }

    #[test]
    fn gates_pass() {
        let (ndcg_ok, recall_ok, prod) = evaluate_gates(0.7, 0.8, 0.5, 0.6);
        assert!(ndcg_ok);
        assert!(recall_ok);
        assert!(prod);
    }

    #[test]
    fn gates_fail_ndcg() {
        let (ndcg_ok, recall_ok, prod) = evaluate_gates(0.3, 0.8, 0.5, 0.6);
        assert!(!ndcg_ok);
        assert!(recall_ok);
        assert!(!prod);
    }

    // ── compute_ndcg edge cases ──────────────────────────────────────────────

    #[test]
    fn ndcg_empty_retrieved_returns_zero() {
        let relevant = vec!["a".into(), "b".into()];
        let retrieved: Vec<String> = vec![];
        let ndcg = compute_ndcg(&retrieved, &relevant, 10);
        assert_eq!(ndcg, 0.0);
    }

    #[test]
    fn ndcg_empty_relevant_returns_zero() {
        let retrieved = vec!["a".into(), "b".into()];
        let relevant: Vec<String> = vec![];
        let ndcg = compute_ndcg(&retrieved, &relevant, 10);
        assert_eq!(ndcg, 0.0);
    }

    #[test]
    fn ndcg_k_limits_retrieved_set() {
        // Only top-1 is evaluated. Retrieved has relevant at position 2 (0-indexed).
        let relevant = vec!["b".into()];
        let retrieved = vec!["a".into(), "b".into(), "b".into()];
        let ndcg_k1 = compute_ndcg(&retrieved, &relevant, 1);
        let ndcg_k2 = compute_ndcg(&retrieved, &relevant, 2);
        // k=1: only "a" evaluated → 0; k=2: "b" found at pos 1 → > 0
        assert_eq!(ndcg_k1, 0.0, "k=1 misses relevant item at pos 1");
        assert!(ndcg_k2 > 0.0, "k=2 should find relevant item");
    }

    #[test]
    fn ndcg_capped_at_one() {
        // Perfect retrieval must not exceed 1.0
        let relevant: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        let retrieved = relevant.clone();
        let ndcg = compute_ndcg(&retrieved, &relevant, 10);
        assert!(ndcg <= 1.0, "NDCG must be <= 1.0, got {ndcg}");
    }

    #[test]
    fn ndcg_single_item_perfect() {
        let relevant = vec!["a".into()];
        let retrieved = vec!["a".into()];
        let ndcg = compute_ndcg(&retrieved, &relevant, 1);
        assert!(
            (ndcg - 1.0).abs() < 0.001,
            "single perfect result should have NDCG=1.0, got {ndcg}"
        );
    }

    #[test]
    fn ndcg_partial_match_is_between_0_and_1() {
        let relevant = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let retrieved = vec!["x".into(), "b".into(), "y".into(), "d".into()];
        let ndcg = compute_ndcg(&retrieved, &relevant, 4);
        assert!(
            ndcg > 0.0 && ndcg < 1.0,
            "partial match should be between 0 and 1, got {ndcg}"
        );
    }

    // ── compute_recall edge cases ────────────────────────────────────────────

    #[test]
    fn recall_empty_relevant_returns_one() {
        let retrieved = vec!["a".into()];
        let relevant: Vec<String> = vec![];
        let recall = compute_recall(&retrieved, &relevant, 10);
        assert_eq!(recall, 1.0, "no relevant items means perfect recall");
    }

    #[test]
    fn recall_zero_when_no_match() {
        let relevant = vec!["a".into(), "b".into()];
        let retrieved = vec!["x".into(), "y".into(), "z".into()];
        let recall = compute_recall(&retrieved, &relevant, 3);
        assert_eq!(recall, 0.0);
    }

    #[test]
    fn recall_one_when_all_found() {
        let relevant = vec!["a".into(), "b".into(), "c".into()];
        let retrieved = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let recall = compute_recall(&retrieved, &relevant, 4);
        assert!(
            (recall - 1.0).abs() < 0.001,
            "all relevant items found → recall = 1.0"
        );
    }

    #[test]
    fn recall_k_limits_window() {
        // Relevant items only appear beyond k
        let relevant = vec!["d".into()];
        let retrieved = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let recall_k3 = compute_recall(&retrieved, &relevant, 3);
        let recall_k4 = compute_recall(&retrieved, &relevant, 4);
        assert_eq!(recall_k3, 0.0, "relevant not in top 3");
        assert!((recall_k4 - 1.0).abs() < 0.001, "relevant found in top 4");
    }

    // ── compute_reciprocal_rank edge cases ───────────────────────────────────

    #[test]
    fn reciprocal_rank_no_relevant_returns_zero() {
        let retrieved = vec!["a".into(), "b".into()];
        let relevant = vec!["x".into()];
        assert_eq!(compute_reciprocal_rank(&retrieved, &relevant), 0.0);
    }

    #[test]
    fn reciprocal_rank_empty_retrieved_returns_zero() {
        let retrieved: Vec<String> = vec![];
        let relevant = vec!["a".into()];
        assert_eq!(compute_reciprocal_rank(&retrieved, &relevant), 0.0);
    }

    #[test]
    fn reciprocal_rank_third_hit_is_one_third() {
        let relevant = vec!["c".into()];
        let retrieved = vec!["a".into(), "b".into(), "c".into()];
        let rr = compute_reciprocal_rank(&retrieved, &relevant);
        assert!(
            (rr - (1.0 / 3.0)).abs() < 0.001,
            "3rd position → RR=1/3, got {rr}"
        );
    }

    // ── evaluate_gates: boundary conditions ─────────────────────────────────

    #[test]
    fn gates_both_fail_not_production_ready() {
        let (ndcg_ok, recall_ok, prod) = evaluate_gates(0.3, 0.4, 0.5, 0.6);
        assert!(!ndcg_ok);
        assert!(!recall_ok);
        assert!(!prod);
    }

    #[test]
    fn gates_fail_recall_only() {
        let (ndcg_ok, recall_ok, prod) = evaluate_gates(0.8, 0.3, 0.5, 0.6);
        assert!(ndcg_ok);
        assert!(!recall_ok);
        assert!(!prod, "must pass both gates to be production ready");
    }

    #[test]
    fn gates_exactly_at_threshold_passes() {
        let (ndcg_ok, recall_ok, prod) = evaluate_gates(0.5, 0.6, 0.5, 0.6);
        assert!(ndcg_ok, "NDCG exactly at threshold should pass");
        assert!(recall_ok, "Recall exactly at threshold should pass");
        assert!(prod);
    }

    // ── generate_legacy_benchmark_queries ────────────────────────────────────

    #[test]
    fn legacy_benchmark_queries_not_empty() {
        let queries = generate_legacy_benchmark_queries();
        assert!(
            !queries.is_empty(),
            "should have at least one benchmark query"
        );
    }

    #[test]
    fn legacy_benchmark_queries_all_have_relevant_paths() {
        for q in generate_legacy_benchmark_queries() {
            assert!(!q.query.is_empty(), "query text must not be empty");
            assert!(
                !q.relevant_paths.is_empty(),
                "query '{}' must have at least one relevant path",
                q.query
            );
        }
    }

    #[test]
    fn legacy_benchmark_queries_cover_key_webforms_concepts() {
        let queries = generate_legacy_benchmark_queries();
        let all_queries: String = queries
            .iter()
            .map(|q| q.query.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all_queries.contains("session") || all_queries.contains("Session"));
        assert!(all_queries.contains("GridView") || all_queries.contains("database"));
        assert!(all_queries.contains("AJAX") || all_queries.contains("UpdatePanel"));
    }

    // ── relevance_score ──────────────────────────────────────────────────────

    #[test]
    fn relevance_score_first_item_gets_highest_score() {
        let relevant = vec!["a".into(), "b".into(), "c".into()];
        // First item relevance = len - 0 = 3, second = 2, third = 1
        // We test this via NDCG: perfect order should give 1.0
        let retrieved = relevant.clone();
        let ndcg = compute_ndcg(&retrieved, &relevant, 3);
        assert!(
            (ndcg - 1.0).abs() < 0.001,
            "perfect retrieval must give 1.0"
        );
    }

    #[test]
    fn relevance_score_item_not_in_relevant_gets_zero() {
        // Item not in relevant has relevance 0 → no contribution to DCG
        let relevant = vec!["a".into()];
        let retrieved = vec!["z".into()]; // z not in relevant
        let ndcg = compute_ndcg(&retrieved, &relevant, 1);
        assert_eq!(ndcg, 0.0, "irrelevant item has zero relevance");
    }
}
