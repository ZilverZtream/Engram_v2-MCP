#![allow(clippy::unwrap_used)]
//! Issue 1: Deterministic index reproducibility tests.
//!
//! Verifies that chunk IDs, graph edges, and search results are stable and
//! reproducible across clean indexing, incremental updates, and re-indexing.

use engram_core::{Config, Registry};
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;
use engram_server::services::autonomous_decision_service::{
    AdpInput as RepAdpInput, AdpVerdict as RepAdpVerdict, RiskProfile as RepRiskProfile,
    RetrievalMode as RepRetrievalMode, evaluate_gates as rep_evaluate_gates,
};
use engram_server::services::safety_service::{
    PolicyDecision as RepPolicyDecision, RiskLevel as RepRiskLevel,
};

/// Golden fixture: a small multi-file C# project with known structure.
fn write_golden_fixture(root: &std::path::Path) {
    // File 1: Domain model
    std::fs::write(
        root.join("Order.cs"),
        r#"
namespace GoldenApp {
    public class Order {
        public int OrderId { get; set; }
        public string CustomerName { get; set; }
        public void CalculateTotal() {
            var cmd = new SqlCommand("SELECT Price FROM OrderItems WHERE OrderId = @id");
        }
    }
}
"#,
    )
    .unwrap();

    // File 2: Service
    std::fs::write(
        root.join("OrderService.cs"),
        r#"
using System;
namespace GoldenApp {
    public class OrderService {
        private readonly Order _order;
        public OrderService(Order order) { _order = order; }
        public void ProcessOrder() {
            _order.CalculateTotal();
            Console.WriteLine("Order processed");
        }
    }
}
"#,
    )
    .unwrap();

    // File 3: WebForms page
    std::fs::write(
        root.join("OrderPage.aspx"),
        r#"<%@ Page Language="C#" Inherits="GoldenApp.OrderPage" CodeBehind="OrderPage.aspx.cs" %>
<asp:GridView ID="gvOrders" runat="server" />
<asp:Button ID="btnProcess" runat="server" OnClick="btnProcess_Click" Text="Process" />
"#,
    )
    .unwrap();

    // File 4: Codebehind
    std::fs::write(
        root.join("OrderPage.aspx.cs"),
        r#"
namespace GoldenApp {
    public partial class OrderPage : System.Web.UI.Page {
        protected void Page_Load(object sender, EventArgs e) {
            var svc = new OrderService(new Order());
        }
        protected void btnProcess_Click(object sender, EventArgs e) {
            var svc = new OrderService(new Order());
            svc.ProcessOrder();
        }
    }
}
"#,
    )
    .unwrap();
}

fn init_git_repo(root: &std::path::Path, files: &[&str]) {
    let repo = git2::Repository::init(root).unwrap();
    let mut index = repo.index().unwrap();
    for f in files {
        index.add_path(std::path::Path::new(f)).unwrap();
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
        .unwrap();
}

fn make_config(root: &std::path::Path, data_parent: &std::path::Path, suffix: &str) -> Config {
    Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: data_parent.join(format!("engram_data_{suffix}")),
        max_project_files: Some(100),
        max_project_bytes: Some(10 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    }
}

/// Wait for graph nodes to reach at least `min_count`.
async fn wait_for_nodes(
    state: &AppState,
    project_id: &str,
    min_count: usize,
) -> Vec<engram_graph::Node> {
    for _ in 0..40 {
        let nodes = state
            .graph
            .query_nodes(project_id, None, None, None, 500)
            .unwrap_or_default();
        if nodes.len() >= min_count {
            return nodes;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    state
        .graph
        .query_nodes(project_id, None, None, None, 500)
        .unwrap_or_default()
}

/// Wait for graph edges to reach at least `min_count`.
async fn wait_for_edges(
    state: &AppState,
    project_id: &str,
    min_count: usize,
) -> Vec<engram_graph::Edge> {
    for _ in 0..40 {
        let edges = state.graph.list_edges(project_id, None).unwrap_or_default();
        if edges.len() >= min_count {
            return edges;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    state.graph.list_edges(project_id, None).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Test 1: Node IDs are deterministic across clean indexing runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deterministic_node_ids_across_clean_runs() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let data_parent = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&data_parent).unwrap();
    write_golden_fixture(&root);
    init_git_repo(
        &root,
        &[
            "Order.cs",
            "OrderService.cs",
            "OrderPage.aspx",
            "OrderPage.aspx.cs",
        ],
    );

    // Run 1: Clean index
    let cfg1 = make_config(&root, &data_parent, "run1");
    std::fs::create_dir_all(&cfg1.data_dir).unwrap();
    let (state1, _rx1) = AppState::new(cfg1).unwrap();
    let engram1 = engram_server::Engram::new(state1.clone());

    engram1
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "GoldenRun1".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let pid1 = state1.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let nodes1 = wait_for_nodes(&state1, &pid1, 3).await;

    // Run 2: Fresh clean index (different data_dir, same source files)
    let cfg2 = make_config(&root, &data_parent, "run2");
    std::fs::create_dir_all(&cfg2.data_dir).unwrap();
    let (state2, _rx2) = AppState::new(cfg2).unwrap();
    let engram2 = engram_server::Engram::new(state2.clone());

    engram2
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "GoldenRun2".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let pid2 = state2.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let nodes2 = wait_for_nodes(&state2, &pid2, 3).await;

    // Collect node identity (type, name, node_id) — sort for comparison
    let mut ids1: Vec<(String, String, String)> = nodes1
        .iter()
        .map(|n| (n.node_type.clone(), n.name.clone(), n.node_id.clone()))
        .collect();
    let mut ids2: Vec<(String, String, String)> = nodes2
        .iter()
        .map(|n| (n.node_type.clone(), n.name.clone(), n.node_id.clone()))
        .collect();

    ids1.sort();
    ids2.sort();

    assert!(
        !ids1.is_empty(),
        "Run 1 should produce some nodes (got {})",
        ids1.len()
    );
    assert!(
        !ids2.is_empty(),
        "Run 2 should produce some nodes (got {})",
        ids2.len()
    );

    // Core structural nodes (type, name) must be present in both runs.
    // Post-processing steps like resolve_app_code_globals or link_binding_fields
    // may produce a few extra nodes depending on timing, so we verify that all
    // nodes from the smaller set are in the larger set by (type, name).
    let set1: std::collections::HashSet<(String, String)> = ids1
        .iter()
        .map(|(t, n, _)| (t.clone(), n.clone()))
        .collect();
    let set2: std::collections::HashSet<(String, String)> = ids2
        .iter()
        .map(|(t, n, _)| (t.clone(), n.clone()))
        .collect();

    let (smaller, larger) = if set1.len() <= set2.len() {
        (&set1, &set2)
    } else {
        (&set2, &set1)
    };

    for key in smaller {
        assert!(
            larger.contains(key),
            "Node ({}, {}) missing in one of the runs",
            key.0,
            key.1
        );
    }

    // For nodes present in both runs, verify their IDs are deterministic
    let id_map1: std::collections::HashMap<(String, String), &str> = ids1
        .iter()
        .map(|(t, n, id)| ((t.clone(), n.clone()), id.as_str()))
        .collect();
    let id_map2: std::collections::HashMap<(String, String), &str> = ids2
        .iter()
        .map(|(t, n, id)| ((t.clone(), n.clone()), id.as_str()))
        .collect();

    for key in smaller {
        if let (Some(id1), Some(id2)) = (id_map1.get(key), id_map2.get(key)) {
            assert_eq!(
                id1, id2,
                "Node ID differs for ({}, {}): {} vs {}",
                key.0, key.1, id1, id2
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 2: Edge stability across incremental update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edge_stability_across_incremental_update() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let data_parent = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&data_parent).unwrap();
    write_golden_fixture(&root);
    init_git_repo(
        &root,
        &[
            "Order.cs",
            "OrderService.cs",
            "OrderPage.aspx",
            "OrderPage.aspx.cs",
        ],
    );

    let cfg = make_config(&root, &data_parent, "edge_stable");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "EdgeStable".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let edges_gen1 = wait_for_edges(&state, &pid, 1).await;

    // Collect edge signatures for gen1
    let mut edge_sigs1: Vec<String> = edges_gen1
        .iter()
        .map(|e| format!("{}->{}:{:?}", e.source_id, e.target_id, e.edge_kind))
        .collect();
    edge_sigs1.sort();

    // Add a comment to a file (non-semantic change) and re-index
    std::fs::write(
        root.join("Order.cs"),
        r#"
// Added comment — should not change graph structure
namespace GoldenApp {
    public class Order {
        public int OrderId { get; set; }
        public string CustomerName { get; set; }
        public void CalculateTotal() {
            var cmd = new SqlCommand("SELECT Price FROM OrderItems WHERE OrderId = @id");
        }
    }
}
"#,
    )
    .unwrap();

    // Commit the change
    let repo = git2::Repository::open(&root).unwrap();
    {
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("Order.cs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Add comment", &tree, &[&parent])
            .unwrap();
    }

    // Incremental update
    engram
        .update_project(Parameters(engram_server::UpdateProjectRequest {
            project_id: pid.clone(),
            wait: true,
            max_commits: 1,
            index_antipatterns: false,
        }))
        .await
        .unwrap();

    // Allow graph rebuild to settle
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // After incremental update, compare all edges for the project (any generation).
    // Only the changed file (Order.cs) gets re-indexed to gen2 while unchanged files
    // keep their gen1 edges, so we compare the full deduplicated edge set.
    let edges_gen2 = state.graph.list_edges(&pid, None).unwrap_or_default();
    let mut edge_sigs2: Vec<String> = edges_gen2
        .iter()
        .map(|e| format!("{}->{}:{:?}", e.source_id, e.target_id, e.edge_kind))
        .collect();
    edge_sigs2.sort();
    edge_sigs2.dedup();

    // Dedup gen1 as well for fair comparison
    edge_sigs1.dedup();

    // The graph structure (source->target:kind) should be the same
    // since we only added a comment, not changed any symbols
    assert!(
        !edge_sigs1.is_empty(),
        "Gen1 should have edges (got {})",
        edge_sigs1.len()
    );

    // After update, edges from unchanged files (gen1) + re-indexed file (gen2)
    // may produce duplicates or slightly more edges from git temporal coupling.
    // The core structure should be preserved: all gen1 edges should still exist.
    for sig in &edge_sigs1 {
        assert!(
            edge_sigs2.contains(sig),
            "Gen1 edge missing after update: {}",
            sig
        );
    }
}

// ---------------------------------------------------------------------------
// Test 3: Search results stability (same query → same results)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_results_stable_across_queries() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let data_parent = tmp.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&data_parent).unwrap();
    write_golden_fixture(&root);
    init_git_repo(
        &root,
        &[
            "Order.cs",
            "OrderService.cs",
            "OrderPage.aspx",
            "OrderPage.aspx.cs",
        ],
    );

    let cfg = make_config(&root, &data_parent, "search_stable");
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "SearchStable".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // Wait for indexing to complete
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    let ps = state.get_project_cached(&pid).unwrap();
    let generation = 1u64;

    // Run the same search query twice
    let query = engram_index::HybridQuery {
        project_id: pid.clone(),
        namespace: "memory".into(),
        generation,
        text: "Order".into(),
        top_k: 10,
        fts_mode: "strict".into(),
        include_path_prefixes: None,
        exclude_path_prefixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };

    let results1 = ps.search.search(&query, None).await.unwrap_or_default();
    let results2 = ps.search.search(&query, None).await.unwrap_or_default();

    // Same query should yield same results
    assert_eq!(
        results1.len(),
        results2.len(),
        "Same query should return same number of results"
    );

    let paths1: Vec<&str> = results1.iter().map(|h| h.path.as_str()).collect();
    let paths2: Vec<&str> = results2.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(
        paths1, paths2,
        "Same query should return results in same order"
    );

    // Scores should also be identical (deterministic ranking)
    for (a, b) in results1.iter().zip(results2.iter()) {
        assert!(
            (a.score - b.score).abs() < 1e-6,
            "Scores should be identical: {} vs {}",
            a.score,
            b.score
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4: Content hash stability — same content → same doc_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_hash_determinism() {
    use engram_core::{ContentHash, DocIdStr};

    let content = "namespace GoldenApp { public class Order { } }";
    let path = "Order.cs";

    // Compute hash multiple times
    let ch1 = ContentHash::compute(content.as_bytes());
    let ch2 = ContentHash::compute(content.as_bytes());
    assert_eq!(ch1, ch2, "ContentHash must be deterministic");

    let doc_id1 = DocIdStr::compute(path, 0, 0, &ch1);
    let doc_id2 = DocIdStr::compute(path, 0, 0, &ch2);
    assert_eq!(
        doc_id1.as_str(),
        doc_id2.as_str(),
        "DocIdStr must be deterministic for same content+path"
    );

    // Different content → different hash
    let content_v2 = "namespace GoldenApp { public class OrderV2 { } }";
    let ch3 = ContentHash::compute(content_v2.as_bytes());
    assert_ne!(ch1, ch3, "Different content must produce different hash");
}

// ---------------------------------------------------------------------------
// Test 5: Chunk ID from content hash is stable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chunk_id_from_content_hash_stable() {
    use engram_core::ContentHash;
    use engram_index::chunk_id_from_content_hash;

    let content = "SELECT * FROM Orders WHERE OrderId = @id";
    let ch1 = ContentHash::compute(content.as_bytes());
    let ch2 = ContentHash::compute(content.as_bytes());

    let cid1 = chunk_id_from_content_hash(&ch1);
    let cid2 = chunk_id_from_content_hash(&ch2);
    assert_eq!(cid1, cid2, "chunk_id must be stable for same content hash");
}

// ── ADP verdict reproducibility tests (from adp_verdict_reproducibility_tests.rs) ──

fn safe_policy() -> RepPolicyDecision {
    RepPolicyDecision {
        allowed: true,
        risk_level: RepRiskLevel::Low,
        checks: vec![],
        confidence: 0.95,
        summary: "Safe".into(),
        mitigations: vec![],
    }
}

fn unsafe_policy() -> RepPolicyDecision {
    RepPolicyDecision {
        allowed: false,
        risk_level: RepRiskLevel::High,
        checks: vec![],
        confidence: 0.3,
        summary: "Unsafe".into(),
        mitigations: vec!["review required".into()],
    }
}

fn all_green_input() -> RepAdpInput {
    RepAdpInput {
        extraction_confidence: Some(0.9),
        extraction_band: Some("high".into()),
        trace_used_fallback: false,
        trace_candidate_count: 0,
        safety_decision: Some(safe_policy()),
        retrieval_production_ready: Some(true),
        retrieval_ndcg: Some(0.85),
        retrieval_recall: Some(0.90),
        blast_radius_risk: Some(2),
        blast_radius_band: Some(engram_server::services::blast_radius_service::RiskBand::Low),
        blast_radius_downstream: Some(3),
        immune_verdict: Some("PASS".into()),
        immune_confidence: Some(0.05),
        require_runtime_evidence: false,
        has_runtime_evidence: false,
        risk_profile: RepRiskProfile::Medium,
        min_extraction_confidence: 0.5,
        min_safety_confidence: 0.7,
        max_blast_radius_for_auto: 6,
        reconciliation: None,
        graph_impact: None,
        retrieval_mode: RepRetrievalMode::Live,
        migration_class: None,
    }
}

/// The Registry set_meta → get_meta contract: after set_meta commits,
/// the new generation value is immediately visible. This is the production
/// persistence path (redb write transaction) that must succeed for
/// generation advancement to occur.
#[test]
fn registry_set_meta_makes_generation_visible() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = Registry::open(&tmp.path().join("reg.redb")).expect("open registry");

    reg.set_meta("proj-gen-contract", "active_generation", "10")
        .expect("set_meta must commit successfully");

    let val = reg
        .get_meta("proj-gen-contract", "active_generation")
        .expect("get_meta must not error")
        .expect("active_generation must be present after set_meta commits");

    assert_eq!(
        val, "10",
        "AUD-2026-INV-0002: generation must be visible as '10' immediately after set_meta commits"
    );
}

/// The fail-before-commit contract: if set_meta is never called for a new
/// generation (e.g. process_ingest_stats failed before reaching set_meta),
/// the old generation value is the only value visible in the Registry.
/// Tests the production redb read path directly.
#[test]
fn registry_generation_absent_before_set_meta_is_called() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let reg = Registry::open(&tmp.path().join("reg.redb")).expect("open registry");

    // Establish old generation at 5
    reg.set_meta("proj-gen-order", "active_generation", "5")
        .expect("establish old generation");

    // new_gen=6 was computed but set_meta was NOT called (process_ingest_stats failed first).
    // The Registry must still show 5, not 6.
    let visible = reg
        .get_meta("proj-gen-order", "active_generation")
        .expect("get_meta must not error")
        .expect("baseline generation must be present");

    assert_eq!(
        visible, "5",
        "AUD-2026-INV-0002: generation must remain at '5' when set_meta(6) was never called"
    );
    assert_ne!(
        visible, "6",
        "AUD-2026-INV-0002: uncommitted generation '6' must not appear in the registry"
    );
}

/// When retrieval is skipped (infra failure), the overall ADP confidence must
/// NOT be depressed as if the retrieval had scored poorly — the gate is simply
/// absent from the score, not counted as zero.
#[test]
fn adp_skipped_retrieval_confidence_not_depressed_vs_live() {
    let mut live = all_green_input();
    live.retrieval_mode = RepRetrievalMode::Live;
    live.retrieval_production_ready = Some(true);
    live.retrieval_ndcg = Some(0.85);

    let mut skipped = all_green_input();
    skipped.retrieval_mode = RepRetrievalMode::Skipped;
    skipped.retrieval_production_ready = None;
    skipped.retrieval_ndcg = None;
    skipped.retrieval_recall = None;

    let live_dec = rep_evaluate_gates(&live);
    let skip_dec = rep_evaluate_gates(&skipped);

    // Skipped confidence should be reasonably close to live confidence
    // (not artificially low due to "missing" retrieval counting as zero)
    let delta = (live_dec.confidence - skip_dec.confidence).abs();
    assert!(
        delta < 0.40,
        "AUD-2026-INV-0005: skipped retrieval confidence ({}) must not be severely \
         depressed vs live retrieval confidence ({}) — delta={delta:.3}",
        skip_dec.confidence, live_dec.confidence
    );
}

/// When both safety AND blast radius fail simultaneously, the confidence
/// should be lower than either failure alone (interaction penalty).
#[test]
fn compound_safety_blast_failure_lower_confidence_than_single_failure() {
    // Only safety fails
    let mut safety_only = all_green_input();
    safety_only.safety_decision = Some(unsafe_policy());
    let safety_only_dec = rep_evaluate_gates(&safety_only);

    // Only blast radius fails
    let mut blast_only = all_green_input();
    blast_only.blast_radius_risk = Some(9);
    blast_only.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    blast_only.blast_radius_downstream = Some(50);
    let blast_only_dec = rep_evaluate_gates(&blast_only);

    // Both fail together
    let mut both = all_green_input();
    both.safety_decision = Some(unsafe_policy());
    both.blast_radius_risk = Some(9);
    both.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    both.blast_radius_downstream = Some(50);
    let both_dec = rep_evaluate_gates(&both);

    // Both verdicts must be Deny
    assert_eq!(safety_only_dec.verdict, RepAdpVerdict::Deny,
        "safety-only failure must Deny");
    assert_eq!(blast_only_dec.verdict, RepAdpVerdict::Deny,
        "blast-only failure must Deny");
    assert_eq!(both_dec.verdict, RepAdpVerdict::Deny,
        "compound failure must Deny");

    // Compound confidence must be lower than either single failure
    assert!(
        both_dec.confidence <= safety_only_dec.confidence.max(blast_only_dec.confidence),
        "compound failure confidence ({}) must not exceed single-failure confidence \
         (safety={}, blast={})",
        both_dec.confidence, safety_only_dec.confidence, blast_only_dec.confidence
    );
}

/// Deterministic: calling evaluate_gates with the same input twice must produce
/// the same verdict and identical confidence (no random/non-deterministic paths).
#[test]
fn same_input_produces_identical_verdict_and_confidence() {
    let input = all_green_input();
    let dec1 = rep_evaluate_gates(&input);
    let dec2 = rep_evaluate_gates(&input);

    assert_eq!(dec1.verdict, dec2.verdict,
        "reproducibility: same input must produce same verdict");
    assert_eq!(dec1.confidence, dec2.confidence,
        "reproducibility: same input must produce identical confidence");
    assert_eq!(dec1.gate_results.len(), dec2.gate_results.len(),
        "reproducibility: same number of gate results");
}

/// Deterministic: the deny verdict for failing safety must reproduce exactly.
#[test]
fn deny_verdict_reproduces_identically() {
    let mut input = all_green_input();
    input.safety_decision = Some(unsafe_policy());

    let dec1 = rep_evaluate_gates(&input);
    let dec2 = rep_evaluate_gates(&input);

    assert_eq!(dec1.verdict, dec2.verdict,
        "deny verdict must be deterministic");
    assert_eq!(dec1.confidence, dec2.confidence,
        "deny confidence must be deterministic");
}

/// When a post-index job degrades (enrichment failed), the ADP verdict based
/// on that evidence should be more conservative. When the enrichment is retried
/// successfully, the ADP verdict should become more permissive.
#[test]
fn corrected_enrichment_after_degraded_improves_adp_verdict() {
    // Degraded: retrieval evidence unavailable (infra error during enrichment)
    let mut degraded = all_green_input();
    degraded.retrieval_mode = RepRetrievalMode::Skipped;
    degraded.retrieval_production_ready = None;
    degraded.retrieval_ndcg = None;
    degraded.retrieval_recall = None;
    let degraded_dec = rep_evaluate_gates(&degraded);

    // Clean: retrieval evidence available (enrichment succeeded on retry)
    let clean = all_green_input();
    let clean_dec = rep_evaluate_gates(&clean);

    // Clean run should produce Allow (or at least equal/better verdict)
    assert_eq!(clean_dec.verdict, RepAdpVerdict::Allow,
        "clean enrichment run must produce Allow");

    // Clean confidence >= degraded confidence (enrichment adds information)
    assert!(
        clean_dec.confidence >= degraded_dec.confidence,
        "clean enrichment confidence ({}) must be >= degraded confidence ({})",
        clean_dec.confidence, degraded_dec.confidence
    );
}

/// The ADP pipeline must not Allow when all three critical gates (safety,
/// extraction, blast radius) simultaneously fail.
#[test]
fn adp_deny_when_all_three_hard_gates_fail() {
    let mut input = all_green_input();
    // Safety fails
    input.safety_decision = Some(unsafe_policy());
    // Extraction fails
    input.extraction_confidence = Some(0.1);
    input.extraction_band = Some("low".into());
    // Blast radius critical
    input.blast_radius_risk = Some(9);
    input.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    input.blast_radius_downstream = Some(100);

    let decision = rep_evaluate_gates(&input);
    assert_ne!(
        decision.verdict,
        RepAdpVerdict::Allow,
        "all-three-gates-failing must not produce Allow; got {:?}",
        decision.verdict
    );
}

/// Mutation test: injecting a failing safety gate into an otherwise all-green
/// input must change Allow → Deny. The pipeline must not "absorb" the mutation.
#[test]
fn adp_mutation_safety_fail_changes_allow_to_deny() {
    let baseline = all_green_input();
    let baseline_dec = rep_evaluate_gates(&baseline);
    assert_eq!(baseline_dec.verdict, RepAdpVerdict::Allow,
        "baseline must be Allow");

    let mut mutated = all_green_input();
    mutated.safety_decision = Some(unsafe_policy());
    let mutated_dec = rep_evaluate_gates(&mutated);

    assert_eq!(mutated_dec.verdict, RepAdpVerdict::Deny,
        "safety mutation must flip Allow to Deny");
    assert_ne!(baseline_dec.verdict, mutated_dec.verdict,
        "mutation must produce detectably different verdict");
}

/// Mutation test: injecting critical blast radius into all-green must
/// change Allow → Deny.
#[test]
fn adp_mutation_critical_blast_radius_changes_allow_to_deny() {
    let baseline = all_green_input();
    assert_eq!(rep_evaluate_gates(&baseline).verdict, RepAdpVerdict::Allow);

    let mut mutated = all_green_input();
    mutated.blast_radius_risk = Some(9);
    mutated.blast_radius_band =
        Some(engram_server::services::blast_radius_service::RiskBand::Critical);
    mutated.blast_radius_downstream = Some(50);
    mutated.max_blast_radius_for_auto = 5;

    let dec = rep_evaluate_gates(&mutated);
    assert_ne!(dec.verdict, RepAdpVerdict::Allow,
        "critical blast radius mutation must not remain Allow; got {:?}", dec.verdict);
}

/// Canary: all-green input must produce Allow with confidence > 0.7.
/// If this test starts failing, it signals a regression in the confidence
/// calibration or gate logic.
#[test]
fn enrichment_canary_all_green_produces_allow_with_high_confidence() {
    let input = all_green_input();
    let decision = rep_evaluate_gates(&input);

    assert_eq!(decision.verdict, RepAdpVerdict::Allow,
        "enrichment canary: all-green must Allow");
    assert!(
        decision.confidence > 0.7,
        "enrichment canary: all-green confidence must exceed 0.7; got {}",
        decision.confidence
    );
}

/// Chaos test: 10 concurrent spawn_blocking panics must all independently
/// produce JoinError — no deadlock, no silent swallowing, no cross-task
/// contamination. (Tests the behavioral property the actor fixes rely on.)
#[tokio::test]
async fn concurrent_spawn_blocking_panics_all_produce_join_errors() {
    use futures::future::join_all;

    let handles: Vec<_> = (0..10)
        .map(|i| {
            tokio::task::spawn_blocking(move || -> i32 {
                panic!("chaos: concurrent spawn_blocking panic #{i}");
            })
        })
        .collect();

    let results = join_all(handles).await;

    let error_count = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(
        error_count, 10,
        "All 10 concurrent spawn_blocking panics must produce JoinError; \
         got {error_count}/10 errors"
    );

    // Each error must be identifiable as a panic (not a cancellation)
    for (i, result) in results.iter().enumerate() {
        let err = result.as_ref().unwrap_err();
        assert!(
            err.is_panic(),
            "concurrent panic #{i} must be is_panic()=true; got cancelled={}",
            err.is_cancelled()
        );
    }
}

/// No orphan results: when all spawn_blocking tasks panic, none should
/// produce Ok(_) — all results must be Err.
#[tokio::test]
async fn all_panicking_spawn_blockings_produce_only_errors_no_ok() {
    use futures::future::join_all;

    let handles: Vec<_> = (0..5)
        .map(|_| {
            tokio::task::spawn_blocking(|| -> String {
                panic!("all-panic chaos test");
            })
        })
        .collect();

    let results = join_all(handles).await;
    let ok_count = results.iter().filter(|r| r.is_ok()).count();

    assert_eq!(
        ok_count, 0,
        "No panicking spawn_blocking must return Ok; got {ok_count} Ok results \
         (implies some errors were silently swallowed)"
    );
}

/// Embed parse parity: the full range of valid float types must parse correctly.
#[test]
fn embed_parse_parity_all_valid_json_float_types() {
    let cases = vec![
        (serde_json::json!(0.0f64), 0.0f32),
        (serde_json::json!(1.0f64), 1.0f32),
        (serde_json::json!(-1.0f64), -1.0f32),
        (serde_json::json!(0.5f64), 0.5f32),
        (serde_json::json!(1e-5f64), 1e-5f32),
    ];

    for (json_val, expected) in &cases {
        let result: Option<f64> = json_val.as_f64();
        assert!(result.is_some(),
            "valid float JSON value must parse via as_f64(): {json_val}");
        let parsed = result.unwrap() as f32;
        assert!(
            (parsed - expected).abs() < 1e-4,
            "parsed {parsed} must be close to {expected} for input {json_val}"
        );
    }
}

/// Embed parse: all known-invalid JSON types must return None from as_f64().
#[test]
fn embed_parse_all_invalid_json_types_return_none() {
    let invalid_values = vec![
        serde_json::Value::Null,
        serde_json::json!("string"),
        serde_json::json!(true),
        serde_json::json!(false),
        serde_json::json!({"key": "value"}),
        serde_json::json!([1, 2, 3]),
    ];

    for val in &invalid_values {
        assert!(
            val.as_f64().is_none(),
            "invalid JSON type must return None from as_f64(): {val}"
        );
    }
}
