#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

#[tokio::test]
async fn test_impact_analysis_smoke() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    // 1. Setup dependent code
    let aspx = r#"<%@ Page Inherits="App.Page" CodeBehind="Page.aspx.cs" %>
<asp:Button ID="btn" runat="server" OnClick="btn_Click" />"#;
    std::fs::write(root.join("Page.aspx"), aspx).unwrap();

    let cb = r#"
namespace App {
    public partial class Page {
        protected void btn_Click(object sender, System.EventArgs e) {
            Utility.Helper();
        }
    }
    public class Utility {
        public static void Helper() {}
    }
}"#;
    std::fs::write(root.join("Page.aspx.cs"), cb).unwrap();

    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();

    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());

    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "ImpactTest".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsCs,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();

    let projects = state.registry.list_projects().unwrap();
    let project_id = &projects[0].project_id;

    // Resolve
    state.graph.resolve_symbol_edges(project_id).unwrap();

    // 2. Analyze impact of Utility.Helper
    let res = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: project_id.clone(),
            file_path: None,
            symbol_fqn: Some("App.Utility.Helper".to_string()),
            limit: 10,
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    println!(
        "IMPACT OUTPUT:
{}",
        text
    );

    assert!(
        text.contains("App.Page.btn_Click"),
        "Impact should include the caller"
    );
    // Raw `calls` edges map to EdgeKind::Calls (not Dependency) since the
    // calls edge kind was restored through the ingest pipeline.
    assert!(text.contains("calls this"), "Should include reason");
    assert!(
        text.contains("## Causal dependents"),
        "callers must be reported in the causal tier"
    );

    // 3. Analyze impact of file
    let res_file = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: project_id.clone(),
            file_path: Some("Page.aspx.cs".to_string()),
            symbol_fqn: None,
            limit: 10,
        }))
        .await
        .unwrap();

    let text_file = &res_file.content[0].as_text().unwrap().text;
    println!(
        "FILE IMPACT OUTPUT:
{}",
        text_file
    );
    assert!(
        text_file.contains("Page.aspx"),
        "Impact should include the markup page"
    );
}

/// Build a bare graph fixture: nodes + edges written straight into the graph
/// store (no parsing) so each test controls every edge.
fn graph_fixture() -> (
    tempfile::TempDir,
    engram_server::Engram,
    engram_server::AppState,
    String,
) {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("t.vb"), "Class T\nEnd Class\n").unwrap();
    let cfg = Config {
        allowed_roots: vec![root.to_path_buf()],
        data_dir: root.join("engram_data"),
        max_project_files: Some(100),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    let pid = "impact-fixture".to_string();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: pid.clone(),
            project_name: "fixture".into(),
            project_type: "dotnet_webforms_cs".into(),
            directory: root.to_string_lossy().to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    (tmp, engram, state, pid)
}

fn node(id: &str, name: &str, file: &str, ty: &str) -> engram_graph::Node {
    engram_graph::Node {
        node_id: id.into(),
        node_type: ty.into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "vb".into(),
        file_path: engram_core::RelPath::from(file),
        start_line: 1,
        end_line: 2,
        generation: 1,
        metadata: None,
    }
}

fn edge(src: &str, dst: &str, kind: engram_graph::EdgeKind) -> engram_graph::Edge {
    engram_graph::Edge {
        source_id: src.into(),
        target_id: dst.into(),
        namespace: "memory".into(),
        language: "vb".into(),
        edge_kind: kind,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

async fn run(engram: &engram_server::Engram, pid: &str, sym: &str) -> String {
    let res = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: pid.to_string(),
            file_path: None,
            symbol_fqn: Some(sym.to_string()),
            limit: 50,
        }))
        .await
        .unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

/// ACCEPTANCE: temporal-coupling edges land in the companion tier and never
/// change the causal count.
#[tokio::test]
async fn temporal_edges_are_companions_not_causal() {
    let (_tmp, engram, state, pid) = graph_fixture();
    let target = "sym:function:t.vb:Target:1";
    let mut nodes = vec![node(target, "Target", "t.vb", "function")];
    let mut edges = Vec::new();
    for i in 0..3 {
        let c = format!("sym:function:c{i}.vb:Caller{i}:1");
        nodes.push(node(
            &c,
            &format!("Caller{i}"),
            &format!("c{i}.vb"),
            "function",
        ));
        edges.push(edge(&c, target, engram_graph::EdgeKind::Calls));
    }
    for i in 0..20 {
        let f = format!("file:companion{i}.resx");
        nodes.push(node(
            &f,
            &format!("companion{i}.resx"),
            &format!("companion{i}.resx"),
            "file",
        ));
        edges.push(edge(&f, target, engram_graph::EdgeKind::TemporalCoupling));
    }
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    state.graph.upsert_edges(&pid, &edges).unwrap();

    let text = run(&engram, &pid, target).await;
    assert!(
        text.contains("3 causal (may break)"),
        "exactly 3 causal dependents despite 20 temporal edges: {text}"
    );
    assert!(
        text.contains("20 historical companions"),
        "temporal edges reported as companions: {text}"
    );
    let causal_section = text
        .split("## Causal dependents")
        .nth(1)
        .and_then(|s| s.split("## ").next())
        .unwrap_or("");
    assert!(
        !causal_section.contains("companion"),
        "no companion may be listed as causal: {causal_section}"
    );
}

/// ACCEPTANCE: two identical requests produce identical output.
#[tokio::test]
async fn impact_analysis_is_deterministic() {
    let (_tmp, engram, state, pid) = graph_fixture();
    let target = "sym:function:t.vb:Target:1";
    let mut nodes = vec![node(target, "Target", "t.vb", "function")];
    let mut edges = Vec::new();
    for i in 0..30 {
        let c = format!("sym:function:c{i}.vb:Caller{i}:1");
        nodes.push(node(
            &c,
            &format!("Caller{i}"),
            &format!("c{i}.vb"),
            "function",
        ));
        edges.push(edge(&c, target, engram_graph::EdgeKind::Calls));
    }
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    state.graph.upsert_edges(&pid, &edges).unwrap();
    let a = run(&engram, &pid, target).await;
    let b = run(&engram, &pid, target).await;
    let strip = |s: &str| s.split("\n---").next().unwrap_or(s).to_string();
    assert_eq!(
        strip(&a),
        strip(&b),
        "identical requests must render identically"
    );
}

/// ACCEPTANCE: a missing target is NOT FOUND, never "no dependents"; both
/// inputs at once are rejected.
#[tokio::test]
async fn missing_target_is_not_found_not_empty() {
    let (_tmp, engram, _state, pid) = graph_fixture();
    let text = run(&engram, &pid, "sym:function:nope.vb:Missing:1").await;
    assert!(
        text.contains("TARGET NOT FOUND"),
        "must distinguish resolution failure from empty impact: {text}"
    );
    assert!(!text.contains("No dependent nodes found"));

    let both = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: pid.clone(),
            file_path: Some("t.vb".into()),
            symbol_fqn: Some("Target".into()),
            limit: 10,
        }))
        .await;
    assert!(
        both.is_err(),
        "file_path AND symbol_fqn together must be rejected"
    );
}

/// P0 ADVERSARIAL: a flood of heavy non-causal edges must NOT consume the
/// budget and hide causal dependents (caps are per tier/kind, after
/// classification).
#[tokio::test]
async fn noncausal_flood_cannot_hide_causal_dependents() {
    let (_tmp, engram, state, pid) = graph_fixture();
    let target = "sym:function:t.vb:Target:1";
    let mut nodes = vec![node(target, "Target", "t.vb", "function")];
    let mut edges = Vec::new();
    // 3 real callers at weight 1.
    for i in 0..3 {
        let c = format!("sym:function:c{i}.vb:Caller{i}:1");
        nodes.push(node(
            &c,
            &format!("Caller{i}"),
            &format!("c{i}.vb"),
            "function",
        ));
        edges.push(edge(&c, target, engram_graph::EdgeKind::Calls));
    }
    // 600 temporal edges at weight 9999 — would have crowded out every call
    // under the old all-kinds weight-ranked cap.
    for i in 0..600 {
        let f = format!("file:hist{i}.sql");
        nodes.push(node(
            &f,
            &format!("hist{i}.sql"),
            &format!("hist{i}.sql"),
            "file",
        ));
        let mut e = edge(&f, target, engram_graph::EdgeKind::TemporalCoupling);
        e.weight = 9999;
        edges.push(e);
    }
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    state.graph.upsert_edges(&pid, &edges).unwrap();
    let text = run(&engram, &pid, target).await;
    assert!(
        text.contains("3 causal (may break)"),
        "all 3 causal dependents must survive a 600-edge temporal flood: {text}"
    );
    assert!(
        !text.contains("CAUSAL is a LOWER BOUND"),
        "causal coverage must be complete (temporal truncation is a separate tier): {text}"
    );
}

/// P0 ADVERSARIAL: a file whose contained symbols have hundreds of INTERNAL
/// callers plus one external caller must report the external caller, never
/// "no edges" (internal edges are filtered BEFORE they count toward any cap).
#[tokio::test]
async fn internal_flood_cannot_hide_external_caller() {
    let (_tmp, engram, state, pid) = graph_fixture();
    let file_id = "file:t.vb";
    let mut nodes = vec![node(file_id, "t.vb", "t.vb", "file")];
    let hub = "sym:function:t.vb:Hub:1".to_string();
    nodes.push(node(&hub, "Hub", "t.vb", "function"));
    let mut edges = Vec::new();
    // 250 internal callers (inside t.vb) of Hub.
    for i in 0..250 {
        let s = format!("sym:function:t.vb:Inner{i}:{}", i + 10);
        nodes.push(node(&s, &format!("Inner{i}"), "t.vb", "function"));
        edges.push(edge(&s, &hub, engram_graph::EdgeKind::Calls));
    }
    // ONE external caller.
    let ext = "sym:function:other.vb:External:1";
    nodes.push(node(ext, "External", "other.vb", "function"));
    edges.push(edge(ext, &hub, engram_graph::EdgeKind::Calls));
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    state.graph.upsert_edges(&pid, &edges).unwrap();

    let res = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: pid.clone(),
            file_path: Some("t.vb".into()),
            symbol_fqn: None,
            limit: 50,
        }))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();
    assert!(
        text.contains("1 causal (may break)"),
        "the single external caller must be found behind 250 internal ones: {text}"
    );
    assert!(text.contains("External"), "external caller named: {text}");
    assert!(
        !text.contains("no external incoming edges"),
        "must not report 'no edges' when an external caller exists: {text}"
    );
}

/// A source with BOTH a causal and a temporal edge is one causal dependent AND
/// one companion — the companion fact is not swallowed into the causal line.
#[tokio::test]
async fn mixed_tier_source_counted_in_both_tiers() {
    let (_tmp, engram, state, pid) = graph_fixture();
    let target = "sym:function:t.vb:Target:1";
    let caller = "sym:function:c.vb:Caller:1";
    let nodes = vec![
        node(target, "Target", "t.vb", "function"),
        node(caller, "Caller", "c.vb", "function"),
    ];
    let edges = vec![
        edge(caller, target, engram_graph::EdgeKind::Calls),
        edge(caller, target, engram_graph::EdgeKind::TemporalCoupling),
    ];
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    state.graph.upsert_edges(&pid, &edges).unwrap();
    let text = run(&engram, &pid, target).await;
    assert!(text.contains("1 causal (may break)"), "{text}");
    assert!(text.contains("1 historical companions"), "{text}");
    let causal_section = text
        .split("## Causal dependents")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .unwrap_or("");
    assert!(
        !causal_section.contains("often changed with this"),
        "the temporal relation must not render inside the causal line: {causal_section}"
    );
}

/// Dangling sources (edge with no node record) are quarantined: listed under
/// Unresolved, EXCLUDED from the confirmed causal count.
#[tokio::test]
async fn dangling_sources_excluded_from_confirmed_counts() {
    let (_tmp, engram, state, pid) = graph_fixture();
    let target = "sym:function:t.vb:Target:1";
    let real = "sym:function:c.vb:Real:1";
    let nodes = vec![
        node(target, "Target", "t.vb", "function"),
        node(real, "Real", "c.vb", "function"),
    ];
    let edges = vec![
        edge(real, target, engram_graph::EdgeKind::Calls),
        // No node record for this source: a dangling causal edge.
        edge(
            "sym:function:ghost.vb:Ghost:1",
            target,
            engram_graph::EdgeKind::Calls,
        ),
    ];
    state.graph.upsert_nodes(&pid, &nodes).unwrap();
    state.graph.upsert_edges(&pid, &edges).unwrap();
    let text = run(&engram, &pid, target).await;
    assert!(
        text.contains("1 causal (may break)"),
        "only the resolved caller is a confirmed causal dependent: {text}"
    );
    assert!(
        text.contains("1 UNRESOLVED endpoints"),
        "the dangling source is reported as unresolved: {text}"
    );
    assert!(text.contains("## Unresolved endpoints"), "{text}");
}
