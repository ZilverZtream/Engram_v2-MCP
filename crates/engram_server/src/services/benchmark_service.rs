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
}
