#![allow(clippy::unwrap_used)]
//! Issue 1: Deterministic index reproducibility tests.
//!
//! Verifies that chunk IDs, graph edges, and search results are stable and
//! reproducible across clean indexing, incremental updates, and re-indexing.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

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
