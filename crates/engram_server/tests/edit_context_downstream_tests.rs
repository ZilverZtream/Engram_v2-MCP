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

async fn expand_consumer(fixture: &Fixture, evidence: &Value) -> Value {
    let navigation = &evidence["consumer_navigation"];
    let action = &navigation["expansions"][0];
    assert_eq!(action["tool"], "impact_analysis");
    assert_eq!(action["arguments"]["symbol_fqn"], id(POLICY, "Policy.Resolve", 5));
    assert_eq!(action["arguments"]["detail"], "summary");
    assert_eq!(action["arguments"]["max_depth"], 1);
    assert!(action["arguments"]["limit"].as_u64().unwrap() <= 8);
    let result = fixture.engram.handle_impact_analysis(
        serde_json::from_value(action["arguments"].clone()).unwrap()).await.unwrap();
    let rendered = text(&result);
    assert!(rendered.len() <= 12 * 1024, "A next step must not require the complete graph");
    serde_json::from_str(rendered.split_once("## Revision-admitted change impact").unwrap().1
        .split_once("```json").unwrap().1.split_once("```").unwrap().0).unwrap()
}

async fn both(fixture: &Fixture) -> (Value, Value) {
    both_profile(fixture,None,false).await
}

async fn both_profile(fixture: &Fixture, detail: Option<&str>, minimal: bool) -> (Value, Value) {
    let mut context_request = request();
    let mut safety_request = request();
    context_request["include_history"] = json!(false);
    context_request["include_business_logic"] = json!(false);
    if minimal {
        context_request["include_full_body"]=json!(false);
        context_request["include_caller_bodies"]=json!(false);
        context_request["max_callers"]=json!(2);
    }
    if let Some(detail)=detail {
        context_request["downstream_detail"]=json!(detail);
        safety_request["downstream_detail"]=json!(detail);
    }
    let context = fixture
        .engram
        .handle_get_method_edit_context(serde_json::from_value(context_request).unwrap())
        .await
        .unwrap();
    let safety = fixture
        .engram
        .handle_check_edit_safety(serde_json::from_value(safety_request).unwrap())
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
        assert_eq!(downstream["evidence"]["detail"],"summary");
        let compact = &downstream["evidence"];
        let lead = compact["consumer_navigation"]["next_level"].as_array().unwrap().iter()
            .find(|lead| lead["node_id"] == id(SPATIAL, "Spatial.ResolvePolicy", 2)).unwrap();
        assert_eq!(lead["parent_node_id"], id(POLICY, "Policy.Resolve", 5));
        assert_eq!(lead["generation"], 1);
        assert_eq!(lead["connection_to_parent"]["kind"], "calls");
        assert_eq!(lead["connection_to_parent"]["source_position_scope"], "not_recorded");
        assert!(lead["connection_to_parent"]["source_line"].is_null());
        let expanded = expand_consumer(&fixture, compact).await;
        assert_eq!(expanded["target"]["node_id"], id(POLICY, "Policy.Resolve", 5));
        assert_eq!(expanded["selected_dependents_observed"], 1);
        assert_eq!(expanded["direct_dependents_observed"], 1);
        assert_eq!(expanded["transitive_dependents_observed"], 0);
        let source = &expanded["immediate_consumers"][0]["source_read"];
        let result = fixture.engram.handle_get_full_method_body(
            serde_json::from_value(source["arguments"].clone()).unwrap()).await.unwrap();
        let body: Value = serde_json::from_str(&text(&result)).unwrap();
        assert_eq!(body["source_file_hash"], ContentHash::compute(SPATIAL_BODY.as_bytes()).0);
        assert!(body["source_code"].as_str().unwrap().contains("Return Policy.Resolve()"));
        let (_,graph_safety)=both_profile(&fixture,Some("graph"),false).await;
        let mut preserved=graph_safety.clone();preserved["downstream_impact"]["evidence"]=downstream["evidence"].clone();
        assert_eq!(preserved,safety,"Choosing a display profile must preserve every safety decision and coverage field");
        let evidence = &graph_safety["downstream_impact"]["evidence"];
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
        let resolve = id(POLICY, "Policy.Resolve", 5);
        let decoys = [
            (historical.clone(), EdgeKind::TemporalCoupling, 1),
            (id(POLICY, "Policy.ObsoleteCaller", 11), EdgeKind::Calls, 1),
            (id(SPATIAL, "Spatial.FutureCaller", 11), EdgeKind::Calls, 3),
            (id(UNBOUND, "Unbound.UnknownCaller", 2), EdgeKind::Calls, 1),
            (id(NOISE, "Noise.BadEdgeCaller", 4), EdgeKind::ApiCall, 3),
            ("sym:function:Site/deleted.vb:Deleted.Caller:1".into(), EdgeKind::Calls, 1),
        ];
        fixture.engram.state.graph.upsert_edges(PID, &decoys.iter().map(|(source, kind, generation)|
            edge(source.clone(), resolve.clone(), kind.clone(), *generation)).collect::<Vec<_>>()).unwrap();
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
        assert_eq!(downstream["evidence"]["detail"],"summary");
        let expanded = expand_consumer(&fixture, &downstream["evidence"]).await;
        assert_eq!(expanded["direct_dependents_observed"], 1);
        for (unverified, _, _) in &decoys {
            for rows in [&downstream["evidence"]["consumer_navigation"]["next_level"], &expanded["immediate_consumers"]] {
                assert!(rows.as_array().unwrap().iter().all(|row| row["node_id"] != *unverified),
                    "Unselected, unknown, temporal and invalid edges must not become executable admitted leads");
            }
        }
        let (_,graph_safety)=both_profile(&fixture,Some("graph"),false).await;
        let mut preserved=graph_safety.clone();preserved["downstream_impact"]["evidence"]=downstream["evidence"].clone();
        assert_eq!(preserved,safety,"Unknown or unselected evidence cannot become safer in the compact profile");
        let evidence = &graph_safety["downstream_impact"]["evidence"];
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
fn a_selected_low_exposure_method_keeps_unknown_effects_visible_in_both_profiles() {
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
        let (_, graph_safety) = both_profile(&fixture, Some("graph"), false).await;
        let mut preserved = graph_safety.clone();
        preserved["downstream_impact"]["evidence"] = safety["downstream_impact"]["evidence"].clone();
        assert_eq!(preserved, safety, "Display selection must preserve low-exposure safety facts");
        assert_eq!(safety["completeness"]["method_semantics"]["status"], "not_run");
        assert_eq!(safety["verdict"], "yellow", "Unassessed method effects require caution even with complete low-exposure graph evidence: {safety}");
        assert!(safety["confidence"].as_f64().unwrap() <= 0.5);
        assert!(safety["reasons"].as_array().unwrap().iter().any(|reason|
            reason.as_str().unwrap().contains("method semantics not measured")));
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
        assert!(markdown.contains("CAUTION"));
        assert!(markdown.contains("method semantics not measured"));
        assert!(!markdown.contains("SAFE TO EDIT"));
    });
}

#[test]
fn downstream_profiles_bound_agent_context_without_losing_safety_or_graph_recovery() {
    owned_case(true,|fixture| async move {
        const EXTRA:&str="Site/App_Code/Consumers.vb";
        let body=(0..40).map(|i|format!("Public Function Caller{i:02}() As Integer\n Return Policy.Resolve()\nEnd Function\n")).collect::<String>();
        let root=std::path::PathBuf::from(fixture.engram.state.registry.get_project(PID).unwrap().unwrap().directory);
        std::fs::write(root.join(EXTRA),&body).unwrap();
        let cancel=CancellationToken::new();let base=fixture.engram.state.registry.capture_source_head(PID).unwrap();
        fixture.search.prepare_source_snapshot(PID,2,3,&[RelPath::new(EXTRA)],&[],&cancel).await.unwrap();
        fixture.search.index_docs(PID,&[doc(EXTRA,&body,3,40)],&cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID,3).unwrap();assert!(fixture.engram.state.registry.publish_source_head(&base,3).unwrap());
        let mut nodes=vec![file_node(EXTRA,&body,3)];let mut edges=Vec::new();
        for i in 0..40 {
            let name=format!("Consumers.Caller{i:02}");let line=i*3+1;
            nodes.push(node(EXTRA,&name,line,line+2,3));
            let mut call=edge(id(EXTRA,&name,line),id(POLICY,"Policy.Resolve",5),EdgeKind::Calls,3);
            call.metadata=Some(json!({"src_line":line,"call_site_line":line+1}));edges.push(call);
        }
        let mut direct=edge(id(POLICY,"Policy.Resolve",5),id(POLICY,"Policy.ClassifyAutomatic",2),EdgeKind::Calls,2);
        direct.metadata=Some(json!({"src_line":5,"call_site_line":6}));edges.push(direct);
        fixture.engram.state.graph.upsert_nodes(PID,&nodes).unwrap();fixture.engram.state.graph.upsert_edges(PID,&edges).unwrap();
        let (compact,safety)=both_profile(&fixture,None,true).await;
        let (explicit,explicit_safety)=both_profile(&fixture,Some("summary"),true).await;
        assert_eq!(compact,explicit);assert_eq!(safety,explicit_safety);
        let (graph,graph_safety)=both_profile(&fixture,Some("graph"),true).await;
        assert_eq!(compact["edit_safety"],safety);assert_eq!(graph["edit_safety"],graph_safety);
        let evidence=&safety["downstream_impact"]["evidence"];
        let graph_evidence=&graph_safety["downstream_impact"]["evidence"];
        let mut restored=compact.clone();restored["edit_safety"]["downstream_impact"]["evidence"]=graph_evidence.clone();
        assert_eq!(restored,graph,"The chosen evidence display is the ONLY context value allowed to differ");
        let mut restored=safety.clone();restored["downstream_impact"]["evidence"]=graph_evidence.clone();assert_eq!(restored,graph_safety);
        assert!(serde_json::to_vec(evidence).unwrap().len()<=12*1024);
        assert!(serde_json::to_vec(&compact).unwrap().len()<=24*1024);
        assert!(serde_json::to_vec(&graph).unwrap().len()>24*1024,"Use a genuinely connected graph that would consume the agent context budget");
        assert_eq!(compact["full_source"],Value::Null);assert_eq!(compact["caller_bodies"][0]["source_code"],"");
        assert_eq!(compact["caller_excerpts"][0]["status"],"verified_source");
        assert_eq!(evidence["detail"],"summary");assert!(evidence.get("nodes").is_none());
        assert_eq!(evidence["source_generation"],3);assert_eq!(evidence["selected_dependents_observed"],46);
        assert_eq!(evidence["direct_dependents_observed"],1);assert_eq!(evidence["transitive_dependents_observed"],45);
        let expanded = expand_consumer(&fixture, evidence).await;
        assert_eq!(expanded["direct_dependents_observed"], 41);
        assert_eq!(expanded["transitive_dependents_observed"], 0);
        let callers = expanded["immediate_consumers"].as_array().unwrap();
        assert!(callers.iter().any(|row| row["file_path"] == EXTRA));
        assert!(callers.iter().any(|row| row["file_path"] == SPATIAL),
            "A busy same-file branch must not hide the other direct consumer file");
        let navigation = &evidence["consumer_navigation"];
        let inline = navigation["next_level"].as_array().unwrap();
        assert_eq!(inline.len() + navigation["next_level_omitted"].as_u64().unwrap() as usize, 41);
        if inline.len() >= 2 {
            assert!(inline.iter().any(|lead| lead["file_path"] == EXTRA));
            assert!(inline.iter().any(|lead| lead["file_path"] == SPATIAL),
                "Inline branch samples also expose another caller file before repeating one");
        }
        for lead in inline {
            assert_eq!(lead["parent_node_id"], id(POLICY, "Policy.Resolve", 5));
            assert_eq!(lead["admission"], "selected");
            assert_eq!(lead["connection_to_parent"]["kind"], "calls");
            if lead["file_path"] == EXTRA {
                assert_eq!(lead["generation"], 3);
                assert_eq!(lead["connection_to_parent"]["source_line"].as_u64().unwrap(),
                    lead["start_line"].as_u64().unwrap() + 1);
            }
        }
        assert_eq!(evidence["coverage"],graph_evidence["coverage"]);assert_ne!(safety["verdict"],"green");
        let omitted=graph_evidence["omitted_from_display"].as_u64().unwrap();assert!(omitted>0);
        assert!(safety["reasons"].as_array().unwrap().iter().any(|r|r.as_str().unwrap().contains(&format!("{omitted} observed downstream dependents omitted"))),"Safety must be computed before compact projection removes the legacy display field");
        assert_eq!(evidence["graph_omissions"]["shortest_paths_omitted"],graph_evidence["shortest_paths_omitted"]);
        let witness=&evidence["immediate_consumers"][0];assert_eq!(witness["node_id"],id(POLICY,"Policy.Resolve",5));
        assert_eq!(witness["sample_connection_to_target"]["source_line"],6);
        assert_eq!(witness["sample_connection_to_target"]["reference_lines"],json!([6]));
        let source=&witness["source_read"];assert_eq!(source["tool"],"get_full_method_body");
        let result=fixture.engram.handle_get_full_method_body(serde_json::from_value(source["arguments"].clone()).unwrap()).await.unwrap();
        let current:Value=serde_json::from_str(&text(&result)).unwrap();assert_eq!(current["source_file_hash"],ContentHash::compute(POLICY_BODY.as_bytes()).0);
        assert!(current["source_code"].as_str().unwrap().contains("Return ClassifyAutomatic()"));
        let recovery=&evidence["graph_continuation"];assert_eq!(recovery["tool"],"impact_analysis");
        let result=fixture.engram.handle_impact_analysis(serde_json::from_value(recovery["arguments"].clone()).unwrap()).await.unwrap();
        let full=text(&result);let recovered:Value=serde_json::from_str(full.split_once("## Revision-admitted change impact").unwrap().1.split_once("```json").unwrap().1.split_once("```").unwrap().0).unwrap();
        assert_eq!(recovered,*graph_evidence,"Executed graph recovery must retain the same canonical identities, positions and coverage when the source has not changed");
        for tool in ["context","safety"] {
            let mut args=request();args["output_json"]=json!(false);
            let result=if tool=="context" {
                args["include_full_body"]=json!(false);args["include_history"]=json!(false);args["include_business_logic"]=json!(false);
                fixture.engram.handle_get_method_edit_context(serde_json::from_value(args).unwrap()).await.unwrap()
            } else {fixture.engram.handle_check_edit_safety(serde_json::from_value(args).unwrap()).await.unwrap()};
            let markdown=text(&result);assert!(markdown.contains("\"detail\":\"summary\""));
            assert!(!markdown.contains("\"shortest_paths\":["),"Default Markdown must also avoid embedding the full graph inventory");
        }
    });
}

#[test]
fn downstream_profiles_expose_summary_default_and_explicit_graph_in_both_request_schemas() {
    use engram_server::models::{CheckEditSafetyRequest,GetMethodEditContextRequest,ImpactDetail};
    let base=json!({"project_id":PID,"file_path":POLICY,"method_name":"ClassifyAutomatic"});
    let ctx:GetMethodEditContextRequest=serde_json::from_value(base.clone()).unwrap();
    let safety:CheckEditSafetyRequest=serde_json::from_value(base.clone()).unwrap();
    assert_eq!(ctx.downstream_detail,ImpactDetail::Summary);assert_eq!(safety.downstream_detail,ImpactDetail::Summary);
    assert_eq!(ImpactDetail::default(),ImpactDetail::Graph,"Standalone impact-tool defaults must remain graph");
    for mode in ["summary","graph","tiny"] {
        let mut request=base.clone();request["downstream_detail"]=json!(mode);
        let ctx=serde_json::from_value::<GetMethodEditContextRequest>(request.clone());
        let safety=serde_json::from_value::<CheckEditSafetyRequest>(request);
        if mode=="tiny" {assert!(ctx.is_err() && safety.is_err());}
        else {let expected=if mode=="summary"{ImpactDetail::Summary}else{ImpactDetail::Graph};assert_eq!(ctx.unwrap().downstream_detail,expected);assert_eq!(safety.unwrap().downstream_detail,expected);}
    }
    for schema in [serde_json::to_value(schemars::schema_for!(GetMethodEditContextRequest)).unwrap(),serde_json::to_value(schemars::schema_for!(CheckEditSafetyRequest)).unwrap()] {
        assert!(!schema["required"].as_array().unwrap().contains(&json!("downstream_detail")));
        let field=&schema["properties"]["downstream_detail"];assert_eq!(field["default"],"summary");
        let definition=field["$ref"].as_str().and_then(|reference|schema.pointer(reference.strip_prefix('#')?)).unwrap_or(field);
        assert_eq!(definition["enum"],json!(["summary","graph"]));
    }
}
