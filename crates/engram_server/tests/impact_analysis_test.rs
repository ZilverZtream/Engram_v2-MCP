#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_core::{ContentHash, RelPath};
use engram_index::IndexDoc;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

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
            max_depth: 1,
            detail: Default::default(),
        }))
        .await
        .unwrap();

    let text = &res.content[0].as_text().unwrap().text;
    println!(
        "IMPACT OUTPUT:
{}",
        text
    );

    let report = impact(text);
    let callers = selected(&report);
    let caller = callers
        .iter()
        .find(|node| node["name"] == "btn_Click")
        .expect("Impact should include the revision-admitted caller");
    assert_eq!(caller["node_id"], "sym:function:Page.aspx.cs:btn_Click:4");
    assert_eq!(caller["start_line"], 4);
    assert_eq!(caller["end_line"], 6);
    assert!(
        report["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["ref"] == caller["file_ref"] && file["path"] == "Page.aspx.cs")
    );
    let target = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["ref"] == report["target_ref"])
        .unwrap();
    assert_eq!(target["name"], "Helper");
    // Raw `calls` edges map to EdgeKind::Calls (not Dependency) since the
    // calls edge kind was restored through the ingest pipeline.
    assert!(
        report["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["kind"] == "calls"
                && edge["source_ref"] == caller["ref"]
                && edge["target_ref"] == target["ref"]),
        "Should include the causal call witness"
    );

    // 3. Analyze impact of file
    let res_file = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: project_id.clone(),
            file_path: Some("Page.aspx.cs".to_string()),
            symbol_fqn: None,
            limit: 10,
            max_depth: 1,
            detail: Default::default(),
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
        impact(text_file)["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "Page.aspx"),
        "File impact should include the distinct dependent markup page"
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
        .register_source_project(&engram_core::ProjectRecord {
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

/// Publish real files and their managed source selection before asking the
/// public tool to admit deterministic graph relationships from those files.
async fn publish_fixture(
    state: &AppState,
    pid: &str,
    mut nodes: Vec<engram_graph::Node>,
    mut edges: Vec<engram_graph::Edge>,
) {
    let root =
        std::path::PathBuf::from(state.registry.get_project(pid).unwrap().unwrap().directory);
    let names: std::collections::BTreeMap<_, _> = nodes
        .iter()
        .map(|node| (node.node_id.clone(), node.name.clone()))
        .collect();
    let mut files: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for node in &mut nodes {
        let body = files
            .entry(node.file_path.to_string())
            .or_insert_with(|| "Public Module Fixture\n".into());
        if node.node_type == "file" {
            continue;
        }
        node.start_line = body.lines().count() as u32 + 1;
        body.push_str(&format!(" Public Shared Sub {}()\n", node.name));
        for binding in edges.iter().filter(|binding| {
            binding.source_id == node.node_id && binding.edge_kind == engram_graph::EdgeKind::Calls
        }) {
            if let Some(name) = names.get(&binding.target_id) {
                body.push_str(&format!("  {name}()\n"));
            }
        }
        body.push_str(" End Sub\n");
        node.end_line = body.lines().count() as u32;
    }
    let mut docs = Vec::new();
    for (index, (path, body)) in files.iter_mut().enumerate() {
        let path = path.as_str();
        body.push_str("End Module\n");
        if let Some(parent) = root.join(path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(root.join(path), body.as_bytes()).unwrap();
        nodes.retain(|node| node.node_id != format!("file:{path}"));
        let mut file = node(&format!("file:{path}"), path, path, "file");
        file.end_line = body.lines().count() as u32;
        file.metadata = Some(json!({"file_hash":ContentHash::compute(body.as_bytes()).0}));
        nodes.push(file);
        docs.push(IndexDoc {
            namespace: "memory".into(),
            generation: 1,
            chunk_id: index as u64 + 1,
            path: RelPath::new(path),
            language: "vbnet".into(),
            content: body.clone(),
            author: None,
            timestamp: None,
            start_line: 1,
            end_line: body.lines().count() as u32,
            doc_id: format!("{path}:source"),
            content_hash: ContentHash::compute(body.as_bytes()).0,
        });
    }
    let runtime = engram_server::services::project_service::ensure_project_runtime(state, pid)
        .await
        .unwrap();
    edges.extend(
        nodes
            .iter()
            .filter(|node| node.node_type != "file")
            .map(|node| {
                edge(
                    &format!("file:{}", node.file_path),
                    &node.node_id,
                    engram_graph::EdgeKind::Contains,
                )
            }),
    );
    let cancel = CancellationToken::new();
    let initial = state.registry.capture_source_head(pid).unwrap();
    let paths: Vec<_> = files.keys().map(|path| RelPath::new(path)).collect();
    runtime
        .search
        .prepare_full_source_snapshot(pid, 0, 1, &paths, &cancel)
        .await
        .unwrap();
    runtime
        .search
        .index_docs(pid, &docs, &cancel)
        .await
        .unwrap();
    runtime.search.seal_source_snapshot(pid, 1).unwrap();
    state.graph.upsert_nodes(pid, &nodes).unwrap();
    state.graph.upsert_edges(pid, &edges).unwrap();
    assert!(state.registry.publish_source_head(&initial, 1).unwrap());
}

fn packet(text: &str, heading: &str) -> Value {
    serde_json::from_str(
        text.split_once(heading)
            .unwrap()
            .1
            .split_once("```json")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0,
    )
    .unwrap()
}
fn impact(text: &str) -> Value {
    packet(text, "## Revision-admitted change impact")
}
fn selected<'a>(packet: &'a Value) -> Vec<&'a Value> {
    packet["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["admission"] == "selected" && node["is_seed"] == false)
        .collect()
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
            max_depth: 1,
            detail: Default::default(),
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
    publish_fixture(&state, &pid, nodes, edges).await;

    let text = run(&engram, &pid, target).await;
    let report = impact(&text);
    assert_eq!(report["selected_dependents_observed"], 3);
    assert!(
        selected(&report)
            .iter()
            .all(|node| !node["node_id"].as_str().unwrap().contains("companion"))
    );
    let history = packet(&text, "## Historical companion observations");
    assert_eq!(history["historical_companions_observed"], 20);
    assert_eq!(history["count_is_lower_bound"], false);
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
    publish_fixture(&state, &pid, nodes, edges).await;
    let a = run(&engram, &pid, target).await;
    let b = run(&engram, &pid, target).await;
    // The freshness footer renders wall-clock index age, so the COMPLETE
    // response is inherently time-dependent; the guarantee (and the public
    // contract) is a byte-identical EVIDENCE BODY, footer excluded.
    assert_eq!(
        impact(&a),
        impact(&b),
        "identical requests must render an identical evidence body"
    );
    assert_eq!(
        packet(&a, "## Historical companion observations"),
        packet(&b, "## Historical companion observations")
    );
}

/// ACCEPTANCE: a missing target is NOT FOUND, never "no dependents"; both
/// inputs at once are rejected.
#[tokio::test]
async fn missing_target_is_not_found_not_empty() {
    let (_tmp, engram, state, pid) = graph_fixture();
    publish_fixture(
        &state,
        &pid,
        vec![node(
            "sym:function:t.vb:Target:1",
            "Target",
            "t.vb",
            "function",
        )],
        vec![],
    )
    .await;
    let text = run(&engram, &pid, "sym:function:nope.vb:Missing:1").await;
    let report = impact(&text);
    assert_eq!(
        report["status"], "unavailable",
        "Resolution failure is unavailable evidence, not empty impact"
    );
    assert!(report["selected_dependents_observed"].is_null());
    assert!(report["error"].as_str().unwrap().contains("target"));

    let both = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: pid.clone(),
            file_path: Some("t.vb".into()),
            symbol_fqn: Some("Target".into()),
            limit: 10,
            max_depth: 1,
            detail: Default::default(),
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
    publish_fixture(&state, &pid, nodes, edges).await;
    let text = run(&engram, &pid, target).await;
    let report = impact(&text);
    assert_eq!(
        report["selected_dependents_observed"], 3,
        "All real callers must survive the historical flood"
    );
    assert_eq!(selected(&report).len(), 3);
    assert_eq!(
        report["coverage"]["truncated"], false,
        "Historical truncation must not falsely truncate causal coverage"
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
    // The auditor's fixture: 501 HIGH-WEIGHT internal callers (over the
    // 500/kind causal cap) whose ids sort BEFORE the external one — under a
    // raw first-N fetch they consume the entire budget and the external
    // caller is silently lost.
    for i in 0..501 {
        let s = format!("sym:function:t.vb:Inner{i:04}:{}", i + 10);
        nodes.push(node(&s, &format!("Inner{i}"), "t.vb", "function"));
        let mut e = edge(&s, &hub, engram_graph::EdgeKind::Calls);
        e.weight = 9999;
        edges.push(e);
    }
    // ONE low-weight external caller, id sorting AFTER every internal id.
    let ext = "sym:function:zz_other.vb:ZzExternal:1";
    nodes.push(node(ext, "ZzExternal", "zz_other.vb", "function"));
    edges.push(edge(ext, &hub, engram_graph::EdgeKind::Calls));
    publish_fixture(&state, &pid, nodes, edges).await;

    let res = engram
        .impact_analysis(Parameters(engram_server::ImpactAnalysisRequest {
            project_id: pid.clone(),
            file_path: Some("t.vb".into()),
            symbol_fqn: None,
            limit: 50,
            max_depth: 1,
            detail: Default::default(),
        }))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap().text.clone();
    let report = impact(&text);
    assert_eq!(
        report["selected_dependents_observed"], 1,
        "Internal sources must not consume the external-dependent count"
    );
    assert!(
        selected(&report).iter().any(|node| node["node_id"] == ext),
        "The external caller must remain visible/recoverable"
    );
    assert!(selected(&report).iter().all(|node| node["node_id"] == ext));
    // The declared per-file seed cap may remain incomplete. It must name that
    // boundary rather than lose this already-known external consumer.
    assert!(
        report["coverage"]["reasons"]
            .to_string()
            .contains("file_seed_cap")
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
    publish_fixture(&state, &pid, nodes, edges).await;
    let text = run(&engram, &pid, target).await;
    let report = impact(&text);
    assert_eq!(report["selected_dependents_observed"], 1);
    assert!(
        selected(&report)
            .iter()
            .any(|node| node["node_id"] == caller)
    );
    assert!(
        report["edges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|edge| edge["kind"] != "temporal_coupling")
    );
    let history = packet(&text, "## Historical companion observations");
    assert_eq!(history["historical_companions_observed"], 1);
    assert!(
        history["examples"]
            .as_array()
            .unwrap()
            .iter()
            .any(|example| example["node_id"] == caller),
        "The independent historical observation must survive even when the same identity is also causal"
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
    publish_fixture(&state, &pid, nodes, edges).await;
    let text = run(&engram, &pid, target).await;
    let report = impact(&text);
    assert_eq!(report["selected_dependents_observed"], 1);
    assert_eq!(selected(&report).len(), 1);
    assert_eq!(selected(&report)[0]["node_id"], real);
    let ghost = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["node_id"] == "sym:function:ghost.vb:Ghost:1")
        .unwrap();
    assert_eq!(ghost["admission"], "unresolved");
    assert!(
        report["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .any(|frontier| frontier["source_ref"] == ghost["ref"])
    );
}
