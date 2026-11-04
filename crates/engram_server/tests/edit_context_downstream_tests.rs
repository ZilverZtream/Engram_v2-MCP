//! Shared edit context/safety must expose downstream consumers behind a small
//! direct caller count. Missing evidence is not permission to label an edit safe.
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_index::{HybridSearchEngine, IndexDoc, SourceSnapshotGuard};
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};
use std::{future::Future, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

const PID: &str = "edit-downstream";
const POLICY: &str = "Site/App_Code/Policy.vb";
const SPATIAL: &str = "Site/App_Code/Spatial.vb";
const HTTP: &str = "Site/kmlquery.aspx.vb";
const UI: &str = "Site/modules/map/map.aspx.vb";
const NOISE: &str = "Site/App_Code/Noise.vb";
const UNBOUND: &str = "Site/App_Code/Unbound.vb";
const POLICY_BODY: &str = "Public Class Policy\n Public Shared Function ClassifyAutomatic() As Integer\n  Return Helper()\n End Function\n Public Shared Function Resolve() As Integer\n  Return ClassifyAutomatic()\n End Function\n Public Shared Function Helper() As Integer\n  Return 1\n End Function\nEnd Class\n";
const OLD_POLICY: &str = "Public Class Policy\n Public Shared Function ClassifyAutomatic() As Integer\n  Return Helper()\n End Function\n Public Shared Function Resolve() As Integer\n  Return ClassifyAutomatic()\n End Function\n Public Shared Function Helper() As Integer\n  Return 1\n End Function\n Public Shared Function ObsoleteCaller() As Integer\n  Return ClassifyAutomatic()\n End Function\nEnd Class\n";
const SPATIAL_BODY: &str = "Public Class Spatial\n Public Shared Function ResolvePolicy() As Integer\n  Return Policy.Resolve()\n End Function\n Public Shared Function QueryCache() As Integer\n  Return ResolvePolicy()\n End Function\n Public Shared Function QueryViewport() As Integer\n  Return QueryCache()\n End Function\nEnd Class\n";
const HTTP_BODY: &str = "Public Class HttpViewport\n Public Shared Sub GetFileLayerViewport()\n  Spatial.QueryViewport()\n End Sub\nEnd Class\n";
const UI_BODY: &str = "Public Class MapPage\n Inherits System.Web.UI.Page\n Protected Sub Page_Load(sender As Object, e As EventArgs) Handles Me.Load\n  HttpViewport.GetFileLayerViewport()\n End Sub\nEnd Class\n";
const NOISE_BODY: &str = "Public Class Noise\n Public Shared Sub HistoricalCompanion()\n End Sub\n Public Shared Sub BadEdgeCaller()\n  Policy.ClassifyAutomatic()\n End Sub\nEnd Class\n";
const UNBOUND_BODY: &str = "Public Class Unbound\n Public Shared Sub UnknownCaller()\n  Policy.ClassifyAutomatic()\n End Sub\nEnd Class\n";

fn id(path: &str, fqn: &str, line: u32) -> String {
    format!("sym:function:{path}:{fqn}:{line}")
}

fn node(path: &str, fqn: &str, start: u32, end: u32, generation: u64) -> Node {
    Node {
        node_id: id(path, fqn, start),
        node_type: "function".into(),
        name: fqn.rsplit('.').next().unwrap().into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: start,
        end_line: end,
        generation,
        metadata: Some(
            json!({"fqn":fqn,"signature":format!("{}()",fqn.rsplit('.').next().unwrap()),"access_level":"Public"}),
        ),
    }
}

fn file_node(path: &str, body: &str, generation: u64) -> Node {
    Node {
        node_id: format!("file:{path}"),
        node_type: "file".into(),
        name: path.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: body.lines().count() as u32,
        generation,
        metadata: Some(json!({"file_hash":ContentHash::compute(body.as_bytes()).0})),
    }
}

fn edge(source: String, target: String, kind: EdgeKind, generation: u64) -> Edge {
    Edge {
        source_id: source,
        target_id: target,
        edge_kind: kind,
        namespace: "memory".into(),
        language: "vbnet".into(),
        generation,
        weight: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

fn doc(path: &str, body: &str, generation: u64, chunk: u64) -> IndexDoc {
    IndexDoc {
        generation,
        chunk_id: chunk,
        path: RelPath::new(path),
        language: "vbnet".into(),
        content: body.into(),
        namespace: "memory".into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: body.lines().count() as u32,
        doc_id: format!("{path}:{generation}:{chunk}"),
        content_hash: ContentHash::compute(body.as_bytes()).0,
    }
}

struct Fixture {
    engram: Engram,
    search: Arc<HybridSearchEngine>,
    _old_source: SourceSnapshotGuard,
}

async fn fixture(owned: &Path, with_route: bool) -> Fixture {
    let root = owned.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let mut sources = vec![(POLICY, OLD_POLICY)];
    if with_route {
        sources.extend([
            (SPATIAL, SPATIAL_BODY),
            (HTTP, HTTP_BODY),
            (UI, UI_BODY),
            (NOISE, NOISE_BODY),
            (UNBOUND, UNBOUND_BODY),
        ]);
    }
    for (path, body) in &sources {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, body).unwrap();
    }
    let config = Config {
        data_dir: owned.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _) = AppState::new(config).unwrap();
    state
        .registry
        .register_source_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: "Downstream edit evidence".into(),
            project_type: "dotnet_webforms_vb".into(),
            directory: root.to_str().unwrap().into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        })
        .unwrap();
    let runtime = engram_server::services::project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    let search = runtime.search.clone();
    drop(runtime);
    let cancel = CancellationToken::new();
    let initial = state.registry.capture_source_head(PID).unwrap();
    let paths: Vec<_> = sources.iter().map(|(path, _)| RelPath::new(path)).collect();
    search
        .prepare_full_source_snapshot(PID, 0, 1, &paths, &cancel)
        .await
        .unwrap();
    let docs: Vec<_> = sources
        .iter()
        .enumerate()
        .map(|(number, (path, body))| doc(path, body, 1, number as u64 + 1))
        .collect();
    search.index_docs(PID, &docs, &cancel).await.unwrap();
    search.seal_source_snapshot(PID, 1).unwrap();
    assert!(state.registry.publish_source_head(&initial, 1).unwrap());
    let old_source = search.pin_source_generation(PID, 1).unwrap();
    let previous = state.registry.capture_source_head(PID).unwrap();
    std::fs::write(root.join(POLICY), POLICY_BODY).unwrap();
    search
        .prepare_source_snapshot(PID, 1, 2, &[RelPath::new(POLICY)], &[], &cancel)
        .await
        .unwrap();
    search
        .index_docs(PID, &[doc(POLICY, POLICY_BODY, 2, 20)], &cancel)
        .await
        .unwrap();
    search.seal_source_snapshot(PID, 2).unwrap();
    assert!(state.registry.publish_source_head(&previous, 2).unwrap());

    let mut nodes = vec![
        file_node(POLICY, POLICY_BODY, 2),
        node(POLICY, "Policy.ClassifyAutomatic", 2, 4, 2),
        node(POLICY, "Policy.Resolve", 5, 7, 2),
        node(POLICY, "Policy.Helper", 8, 10, 2),
    ];
    let target = id(POLICY, "Policy.ClassifyAutomatic", 2);
    let mut edges = vec![
        edge(
            id(POLICY, "Policy.Resolve", 5),
            target.clone(),
            EdgeKind::Calls,
            2,
        ),
        edge(target, id(POLICY, "Policy.Helper", 8), EdgeKind::Calls, 2),
    ];
    if with_route {
        for (path, body) in [
            (SPATIAL, SPATIAL_BODY),
            (HTTP, HTTP_BODY),
            (UI, UI_BODY),
            (NOISE, NOISE_BODY),
        ] {
            nodes.push(file_node(path, body, 1));
        }
        nodes.extend([
            node(SPATIAL, "Spatial.ResolvePolicy", 2, 4, 1),
            node(SPATIAL, "Spatial.QueryCache", 5, 7, 1),
            node(SPATIAL, "Spatial.QueryViewport", 8, 10, 1),
            node(HTTP, "HttpViewport.GetFileLayerViewport", 2, 4, 1),
            node(UI, "MapPage.Page_Load", 3, 5, 1),
            node(NOISE, "Noise.HistoricalCompanion", 2, 3, 1),
            node(NOISE, "Noise.BadEdgeCaller", 4, 6, 1),
        ]);
        edges.extend([
            edge(
                id(SPATIAL, "Spatial.ResolvePolicy", 2),
                id(POLICY, "Policy.Resolve", 5),
                EdgeKind::Calls,
                1,
            ),
            edge(
                id(SPATIAL, "Spatial.QueryCache", 5),
                id(SPATIAL, "Spatial.ResolvePolicy", 2),
                EdgeKind::Calls,
                1,
            ),
            edge(
                id(SPATIAL, "Spatial.QueryViewport", 8),
                id(SPATIAL, "Spatial.QueryCache", 5),
                EdgeKind::Calls,
                1,
            ),
            edge(
                id(HTTP, "HttpViewport.GetFileLayerViewport", 2),
                id(SPATIAL, "Spatial.QueryViewport", 8),
                EdgeKind::Calls,
                1,
            ),
            edge(
                id(UI, "MapPage.Page_Load", 3),
                id(HTTP, "HttpViewport.GetFileLayerViewport", 2),
                EdgeKind::Calls,
                1,
            ),
        ]);
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    Fixture {
        engram: Engram::new(state),
        search,
        _old_source: old_source,
    }
}

// The owned runtime is dropped before the TempDir on success or panic. No
// background application actors/jobs or external source trees are involved.
fn owned_case<F, Fut>(with_route: bool, run: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        run(fixture(temporary.path(), with_route).await).await;
    });
    drop(runtime);
    temporary.close().unwrap();
}

fn request() -> Value {
    json!({"project_id":PID,"file_path":POLICY,"method_name":"ClassifyAutomatic",
        "class_name":"Policy","line":2,"output_json":true})
}

fn text(result: &rmcp::model::CallToolResult) -> String {
    assert!(!result.is_error.unwrap_or(false));
    result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|content| content.text.as_str())
        .collect()
}

async fn both(fixture: &Fixture) -> (Value, Value) {
    let mut context_request = request();
    context_request["include_history"] = json!(false);
    context_request["include_business_logic"] = json!(false);
    let context = fixture
        .engram
        .handle_get_method_edit_context(serde_json::from_value(context_request).unwrap())
        .await
        .unwrap();
    let safety = fixture
        .engram
        .handle_check_edit_safety(serde_json::from_value(request()).unwrap())
        .await
        .unwrap();
    (
        serde_json::from_str(&text(&context)).unwrap(),
        serde_json::from_str(&text(&safety)).unwrap(),
    )
}

#[test]
fn one_direct_caller_still_requires_downstream_review_of_http_and_ui_in_both_edit_tools() {
    owned_case(true, |fixture| async move {
        let (context, safety) = both(&fixture).await;
        assert_eq!(
            context["edit_safety"], safety,
            "Context and standalone safety must derive the verdict from the same downstream facts"
        );
        assert_eq!(
            context["method_info"]["called_by"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let downstream = &safety["downstream_impact"];
        assert_eq!(downstream["status"], "bounded_indexed_exposure");
        assert_eq!(downstream["source_generation"], 2);
        assert_eq!(downstream["source_layout"], "managed");
        assert_eq!(downstream["target_revision_admitted"], true);
        assert_eq!(downstream["direct_dependents_observed"], 1);
        assert_eq!(downstream["transitive_dependents_observed"], 5);
        assert_eq!(downstream["selected_dependents_observed"], 6);
        assert_ne!(
            safety["verdict"], "green",
            "An innocent method with downstream page/HTTP consumers requires review despite one direct caller"
        );
        let reasons = safety["reasons"].to_string().to_lowercase();
        assert!(reasons.contains("downstream") || reasons.contains("transitive"));
        let evidence = &downstream["evidence"];
        assert_eq!(evidence["change_breakage_proven"], false);
        assert_eq!(evidence["working_tree_checked"], false);
        let rows = evidence["nodes"].as_array().unwrap();
        let root = rows
            .iter()
            .find(|row| row["node_id"] == id(POLICY, "Policy.ClassifyAutomatic", 2))
            .unwrap();
        for (path, fqn, line, depth) in [
            (HTTP, "HttpViewport.GetFileLayerViewport", 2, 5),
            (UI, "MapPage.Page_Load", 3, 6),
        ] {
            let wanted = id(path, fqn, line);
            let row = rows.iter().find(|row| row["node_id"] == wanted).unwrap();
            assert_eq!(row["admission"], "selected");
            assert_eq!(row["depth"], depth);
            assert_eq!(
                row["generation"], 1,
                "Unchanged inherited source is valid under the newer selected head"
            );
            let witness = evidence["shortest_paths"]
                .as_array()
                .unwrap()
                .iter()
                .find(|path| path["dependent_ref"] == row["ref"])
                .unwrap();
            assert_eq!(witness["node_refs"][0], root["ref"]);
            assert_eq!(witness["node_refs"].as_array().unwrap().len(), depth + 1);
        }
    });
}

#[test]
fn unverified_missing_and_noncausal_records_do_not_inflate_shared_downstream_counts_or_allow_green()
{
    owned_case(true, |fixture| async move {
        let cancel = CancellationToken::new();
        let future_body = SPATIAL_BODY.replace("End Class\n",
            " Public Shared Function FutureCaller() As Integer\n  Return Policy.ClassifyAutomatic()\n End Function\nEnd Class\n");
        fixture
            .search
            .prepare_source_snapshot(PID, 2, 3, &[RelPath::new(SPATIAL)], &[], &cancel)
            .await
            .unwrap();
        fixture
            .search
            .index_docs(PID, &[doc(SPATIAL, &future_body, 3, 30)], &cancel)
            .await
            .unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(
                PID,
                &[
                    node(POLICY, "Policy.ObsoleteCaller", 11, 13, 1),
                    node(SPATIAL, "Spatial.FutureCaller", 11, 13, 3),
                    node(UNBOUND, "Unbound.UnknownCaller", 2, 4, 1),
                ],
            )
            .unwrap();
        let target = id(POLICY, "Policy.ClassifyAutomatic", 2);
        let historical = id(NOISE, "Noise.HistoricalCompanion", 2);
        let mut cochange = edge(
            historical.clone(),
            target.clone(),
            EdgeKind::TemporalCoupling,
            1,
        );
        cochange.weight = 10_000;
        fixture
            .engram
            .state
            .graph
            .upsert_edges(
                PID,
                &[
                    cochange,
                    edge(
                        format!("file:{POLICY}"),
                        target.clone(),
                        EdgeKind::Contains,
                        2,
                    ),
                    edge(
                        id(POLICY, "Policy.ObsoleteCaller", 11),
                        target.clone(),
                        EdgeKind::Calls,
                        1,
                    ),
                    edge(
                        id(SPATIAL, "Spatial.FutureCaller", 11),
                        target.clone(),
                        EdgeKind::Calls,
                        3,
                    ),
                    edge(
                        id(UNBOUND, "Unbound.UnknownCaller", 2),
                        target.clone(),
                        EdgeKind::Calls,
                        1,
                    ),
                    edge(
                        id(NOISE, "Noise.BadEdgeCaller", 4),
                        target.clone(),
                        EdgeKind::ApiCall,
                        3,
                    ),
                    edge(
                        "sym:function:Site/deleted.vb:Deleted.Caller:1".into(),
                        target,
                        EdgeKind::Calls,
                        1,
                    ),
                ],
            )
            .unwrap();
        let (context, safety) = both(&fixture).await;
        assert_eq!(context["edit_safety"], safety);
        assert_ne!(safety["verdict"], "green");
        assert!(
            safety["confidence"].as_f64().unwrap() <= 0.5,
            "Missing/unverified downstream evidence must cap confidence rather than disappear"
        );
        assert_ne!(safety["completeness"]["downstream"]["status"], "complete");
        let downstream = &safety["downstream_impact"];
        assert_eq!(downstream["source_generation"], 2);
        assert_eq!(downstream["direct_dependents_observed"], 1);
        assert_eq!(downstream["transitive_dependents_observed"], 5);
        assert_eq!(
            downstream["selected_dependents_observed"], 6,
            "History, containment and unselected/unverified graph rows are not current causal consumers"
        );
        assert!(
            downstream["coverage"]["non_selected_nodes"]
                .as_u64()
                .unwrap()
                >= 2
        );
        assert!(downstream["coverage"]["unknown_nodes"].as_u64().unwrap() >= 1);
        assert!(downstream["coverage"]["missing_nodes"].as_u64().unwrap() >= 1);
        let coverage_reasons = downstream["coverage"]["reasons"].to_string();
        assert!(coverage_reasons.contains("unverified_edge_source_revision"));
        assert!(coverage_reasons.contains("missing_graph_node"));
        assert!(
            safety["completeness"]["downstream"]["reasons"]
                .as_array()
                .is_some_and(|reasons| !reasons.is_empty()),
            "Partial downstream status must retain reasons instead of pretending a display cap was hit"
        );
        let evidence = &downstream["evidence"];
        for row in evidence["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["admission"] == "selected")
        {
            assert_ne!(row["node_id"], historical);
            assert_ne!(row["node_id"], id(POLICY, "Policy.ObsoleteCaller", 11));
            assert_ne!(row["node_id"], id(SPATIAL, "Spatial.FutureCaller", 11));
            assert_ne!(row["node_id"], id(UNBOUND, "Unbound.UnknownCaller", 2));
            assert_ne!(row["node_id"], id(NOISE, "Noise.BadEdgeCaller", 4));
        }
        assert_eq!(evidence["change_breakage_proven"], false);
    });
}

#[test]
fn a_selected_low_exposure_method_can_be_lower_observed_risk_without_claiming_safe_execution() {
    owned_case(false, |fixture| async move {
        let (context, safety) = both(&fixture).await;
        assert_eq!(context["edit_safety"], safety);
        assert_eq!(safety["completeness"]["downstream"]["status"], "complete");
        assert_eq!(safety["downstream_impact"]["source_generation"], 2);
        assert_eq!(safety["downstream_impact"]["direct_dependents_observed"], 1);
        assert_eq!(
            safety["downstream_impact"]["transitive_dependents_observed"],
            0
        );
        assert_eq!(safety["verdict"], "green");
        assert_eq!(
            safety["downstream_impact"]["evidence"]["change_breakage_proven"],
            false
        );
        let mut markdown_request = request();
        markdown_request["output_json"] = json!(false);
        let result = fixture
            .engram
            .handle_check_edit_safety(serde_json::from_value(markdown_request).unwrap())
            .await
            .unwrap();
        let markdown = text(&result);
        assert!(markdown.contains("LOWER OBSERVED RISK"));
        assert!(!markdown.contains("SAFE TO EDIT"));
    });
}
