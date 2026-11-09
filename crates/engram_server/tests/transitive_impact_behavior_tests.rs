//! A change-impact answer must preserve the actual route to a downstream
//! consumer, without turning historical co-change or stale graph records into
//! current causal evidence. These fixtures execute no application code/models.
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_index::{HybridSearchEngine, IndexDoc, SourceSnapshotGuard};
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};
use std::{
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

const PID: &str = "transitive-impact";
const POLICY: &str = "src/policy.rs";
const SPATIAL: &str = "src/spatial.rs";
const HTTP: &str = "src/http.rs";
const COMPANION: &str = "src/companion.rs";
const UNBOUND: &str = "src/missing_file.rs";
const FUTURE_FILE: &str = "src/future_file.rs";
const POLICY_BODY: &str =
    "pub fn ClassifyAutomatic() -> i32 { 1 }\npub fn Resolve() -> i32 { ClassifyAutomatic() }\n";
const OLD_POLICY_BODY: &str = "pub fn ClassifyAutomatic() -> i32 { 0 }\npub fn Resolve() -> i32 { ClassifyAutomatic() }\npub fn ObsoletePolicyCaller() -> i32 { ClassifyAutomatic() }\n";
// This recursive branch deliberately gives the static dependency graph a
// cycle. The endpoint uses the terminating branch; nothing here is executed.
const SPATIAL_BODY: &str = "pub fn ResolvePolicy(stop: bool) -> i32 { if stop { crate::policy::Resolve() } else { QueryCacheCore(true) } }\npub fn QueryCacheCore(stop: bool) -> i32 { ResolvePolicy(stop) }\npub fn QueryViewport() -> i32 { QueryCacheCore(true) }\n";
const HTTP_BODY: &str =
    "pub fn GetFileLayerViewport() -> i32 { crate::spatial::QueryViewport() }\n";
const COMPANION_BODY: &str = "pub fn HistoricCompanion() {}\npub struct StructuralOnly;\npub fn BadEdgeOnly() -> i32 { crate::policy::ClassifyAutomatic() }\npub fn ValidAfterUnknown() -> i32 { crate::policy::ClassifyAutomatic() }\n";
const UNBOUND_BODY: &str =
    "pub fn MissingFileBinding() -> i32 { crate::policy::ClassifyAutomatic() }\n";
const FUTURE_BODY: &str =
    "pub fn FutureFileBinding() -> i32 { crate::policy::ClassifyAutomatic() }\n";

fn id(path: &str, name: &str, line: u32) -> String {
    format!("sym:function:{path}:{name}:{line}")
}

fn source_doc(generation: u64, chunk: u64, path: &str, body: &str) -> IndexDoc {
    IndexDoc {
        namespace: "memory".into(),
        generation,
        chunk_id: chunk,
        path: RelPath::new(path),
        language: "rust".into(),
        content: body.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: body.lines().count() as u32,
        doc_id: format!("{path}:{generation}:{chunk}"),
        content_hash: ContentHash::compute(body.as_bytes()).0,
    }
}

fn declaration(path: &str, name: &str, line: u32, generation: u64) -> Node {
    Node {
        node_id: id(path, name, line),
        node_type: "function".into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "rust".into(),
        file_path: RelPath::new(path),
        start_line: line,
        end_line: line,
        generation,
        metadata: None,
    }
}

fn file_node(path: &str, body: &str, generation: u64) -> Node {
    Node {
        node_id: format!("file:{path}"),
        node_type: "file".into(),
        name: path.into(),
        namespace: "memory".into(),
        language: "rust".into(),
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
        namespace: "memory".into(),
        language: "rust".into(),
        edge_kind: kind,
        weight: 1,
        generation,
        metadata: None,
        updated_at_ms: 1,
    }
}

struct Fixture {
    engram: Engram,
    search: Arc<HybridSearchEngine>,
    root: PathBuf,
    // A real retained old source reader proves that older physical rows can
    // coexist with the selected revision without becoming current callers.
    _old_view: SourceSnapshotGuard,
}

async fn fixture(owned: &Path) -> Fixture {
    let root = owned.join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let sources = [
        (POLICY, OLD_POLICY_BODY),
        (SPATIAL, SPATIAL_BODY),
        (HTTP, HTTP_BODY),
        (COMPANION, COMPANION_BODY),
        (UNBOUND, UNBOUND_BODY),
        (FUTURE_FILE, FUTURE_BODY),
    ];
    for (path, body) in sources {
        std::fs::write(root.join(path), body).unwrap();
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
            project_name: "Transitive impact behavior".into(),
            project_type: "rust".into(),
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
        .map(|(index, (path, body))| source_doc(1, index as u64 + 1, path, body))
        .collect();
    search.index_docs(PID, &docs, &cancel).await.unwrap();
    search.seal_source_snapshot(PID, 1).unwrap();
    assert!(state.registry.publish_source_head(&initial, 1).unwrap());
    let old_view = search.pin_source_generation(PID, 1).unwrap();
    let baseline = state.registry.capture_source_head(PID).unwrap();
    search
        .prepare_source_snapshot(PID, 1, 2, &[RelPath::new(POLICY)], &[], &cancel)
        .await
        .unwrap();
    search
        .index_docs(PID, &[source_doc(2, 20, POLICY, POLICY_BODY)], &cancel)
        .await
        .unwrap();
    search.seal_source_snapshot(PID, 2).unwrap();
    assert!(state.registry.publish_source_head(&baseline, 2).unwrap());
    std::fs::write(root.join(POLICY), POLICY_BODY).unwrap();

    let mut nodes: Vec<_> = sources
        .iter()
        .filter(|(path, _)| *path != UNBOUND)
        .map(|(path, body)| {
            file_node(
                path,
                if *path == POLICY { POLICY_BODY } else { body },
                if *path == POLICY { 2 } else { 1 },
            )
        })
        .collect();
    for (path, name, line, generation) in [
        (POLICY, "ClassifyAutomatic", 1, 2),
        (POLICY, "Resolve", 2, 2),
        (SPATIAL, "ResolvePolicy", 1, 1),
        (SPATIAL, "QueryCacheCore", 2, 1),
        (SPATIAL, "QueryViewport", 3, 1),
        (HTTP, "GetFileLayerViewport", 1, 1),
        (COMPANION, "HistoricCompanion", 1, 1),
        (COMPANION, "StructuralOnly", 2, 1),
    ] {
        nodes.push(declaration(path, name, line, generation));
    }
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    let edges = [
        edge(
            id(POLICY, "Resolve", 2),
            id(POLICY, "ClassifyAutomatic", 1),
            EdgeKind::Calls,
            2,
        ),
        edge(
            id(SPATIAL, "ResolvePolicy", 1),
            id(POLICY, "Resolve", 2),
            EdgeKind::Calls,
            1,
        ),
        edge(
            id(SPATIAL, "QueryCacheCore", 2),
            id(SPATIAL, "ResolvePolicy", 1),
            EdgeKind::Calls,
            1,
        ),
        edge(
            id(SPATIAL, "ResolvePolicy", 1),
            id(SPATIAL, "QueryCacheCore", 2),
            EdgeKind::Calls,
            1,
        ),
        edge(
            id(SPATIAL, "QueryViewport", 3),
            id(SPATIAL, "QueryCacheCore", 2),
            EdgeKind::Calls,
            1,
        ),
        edge(
            id(HTTP, "GetFileLayerViewport", 1),
            id(SPATIAL, "QueryViewport", 3),
            EdgeKind::Calls,
            1,
        ),
    ];
    state.graph.upsert_edges(PID, &edges).unwrap();
    Fixture {
        engram: Engram::new(state),
        search,
        root,
        _old_view: old_view,
    }
}

// The runtime is declared after TempDir and dropped first on success or panic;
// all AppState/database/worker ownership ends before temporary-tree teardown.
fn owned_case<F, Fut>(run: F)
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
        run(fixture(temporary.path()).await).await;
    });
    drop(runtime);
    temporary.close().unwrap();
}

#[test]
fn zero_graph_dependents_offer_executable_lexical_recovery_without_claiming_callers() {
    owned_case(|fixture| async move {
        const TARGET: &str = "src/zoom.vb";
        const CALLER: &str = "src/endpoint.vb";
        const EXCLUDED: &str = "src/excluded.vb";
        let sources = [
            (TARGET, "Function CanEditZoom() As Boolean\nReturn True\nEnd Function\n"),
            (CALLER, "If Policy.caneditzoom() Then Save()\n' CanEditZoom is also mentioned in a comment\n"),
            (EXCLUDED, "CanEditZoom()\n"),
        ];
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        let cancel = CancellationToken::new();
        let paths: Vec<_> = sources.iter().map(|(path, _)| RelPath::new(path)).collect();
        fixture.search.prepare_source_snapshot(PID, 2, 3, &paths, &[], &cancel).await.unwrap();
        let docs: Vec<_> = sources.iter().enumerate().map(|(i, (path, body))|
            source_doc(3, 90 + i as u64, path, body)).collect();
        fixture.search.index_docs(PID, &docs, &cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        let target = declaration(TARGET, "Policy.CanEditZoom", 1, 3);
        let mut nodes = vec![target.clone()];
        for (path, body) in sources {
            std::fs::write(fixture.root.join(path), body).unwrap();
            nodes.push(file_node(path, body, 3));
        }
        fixture.engram.state.graph.upsert_nodes(PID, &nodes).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline, 3).unwrap());
        let edges_before = serde_json::to_value(fixture.engram.state.graph.list_edges(PID, None).unwrap()).unwrap();
        let graph = impact_packet(&impact_target(&fixture, &target.node_id, Some(6)).await);
        assert_eq!(graph["selected_dependents_observed"], 0);
        assert_eq!(graph["target_revision_admitted"], true);
        let inspection = &graph["terminal_member_inspection"];
        let scope = inspection["scope"].as_str().unwrap();
        assert!(scope.contains("project-wide") && scope.contains("exclude_path_prefixes"));
        assert!(scope.contains("other overloads") && scope.contains("not Calls edges") && scope.contains("freshness"));
        let action = &inspection["action"];
        assert_eq!(action["tool"], "grep_project");
        assert_eq!(action["arguments"]["pattern"], "CanEditZoom");
        assert_eq!(action["arguments"]["max_results"], 20);
        assert_eq!(action["arguments"]["context_before"], 0);
        assert_eq!(action["arguments"]["context_after"], 0);
        assert_eq!(action["arguments"]["freshness"], "strict");
        assert_eq!(action["arguments"]["regex"], false);
        assert_eq!(action["arguments"]["case_sensitive"], false);
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let report = impact_packet(&summary_impact(&fixture, tool, &target.node_id, 6).await);
            assert_eq!(report["terminal_member_inspection"], *inspection);
            for field in ["target_revision_admitted", "selected_dependents_observed", "coverage", "source_generation"] {
                assert_eq!(report[field], graph[field]);
            }
            assert_eq!(report["direct_dependents_observed"], 0);
            assert_eq!(report["transitive_dependents_observed"], 0);
            assert_eq!(report["exhaustive_codebase"], false);
            assert_eq!(report["change_breakage_proven"], false);
            assert_eq!(report["target"]["source_read"]["tool"], "get_full_method_body");
        }
        // Execute the offered action with the task's own excluded path. A
        // differently cased real source use and a comment both remain leads.
        let mut args = action["arguments"].clone();
        args["exclude_path_prefixes"] = json!([EXCLUDED]);
        let hits = fixture.engram.handle_grep_project(serde_json::from_value(args).unwrap()).await.unwrap();
        assert!(!hits.is_error.unwrap_or(false));
        let hits: Value = serde_json::from_str(&hits.content[0].as_text().unwrap().text).unwrap();
        let matches = hits["matches"].as_array().unwrap();
        assert!(!matches.iter().any(|hit| hit["file_path"] == EXCLUDED));
        assert!(matches.iter().any(|hit| hit["file_path"] == CALLER && hit["line"] == 2));
        let hit = matches.iter().find(|hit| hit["file_path"] == CALLER && hit["line"] == 1).unwrap();
        let source = fixture.engram.handle_get_full_method_body(
            serde_json::from_value(hit["current_source_recovery"]["arguments"].clone()).unwrap()).await.unwrap();
        assert!(!source.is_error.unwrap_or(false));
        let source: Value = serde_json::from_str(&source.content[0].as_text().unwrap().text).unwrap();
        assert!(source["source_code"].as_str().unwrap().contains("Policy.caneditzoom()"));
        assert_eq!(source["source_file_hash"], ContentHash::compute(sources[1].1.as_bytes()).0);
        assert_eq!(edges_before, serde_json::to_value(fixture.engram.state.graph.list_edges(PID, None).unwrap()).unwrap());
        let after = impact_packet(&impact_target(&fixture, &target.node_id, Some(6)).await);
        assert_eq!(after, graph, "Following lexical recovery must not invent graph callers or change evidence");

        // The same public surface must not manufacture a search from invalid
        // recorded names, unavailable admission, a file, or nonzero exposure.
        for name in ["".to_owned(), "Policy.Bad(Name)".into(), "Policy.9bad".into(),
            "Policy.bad\nname".into(), format!("Policy.{}", "x".repeat(129)), "Policy.".into()] {
            let mut invalid = target.clone(); invalid.name = name;
            fixture.engram.state.graph.upsert_nodes(PID, &[invalid]).unwrap();
            for tool in ["impact_analysis", "compute_blast_radius"] {
                let report = impact_packet(&summary_impact(&fixture, tool, &target.node_id, 6).await);
                assert!(report.get("terminal_member_inspection").is_none());
            }
        }
        let mut stale = target.clone(); stale.generation = 1;
        fixture.engram.state.graph.upsert_nodes(PID, &[stale]).unwrap();
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let report = impact_packet(&summary_impact(&fixture, tool, &target.node_id, 6).await);
            assert_eq!(report["target_revision_admitted"], false);
            assert!(report.get("terminal_member_inspection").is_none());
            let file = impact_packet(&summary_impact(&fixture, tool, &format!("file:{TARGET}"), 6).await);
            assert!(file.get("terminal_member_inspection").is_none());
            let nonzero = impact_packet(&summary_impact(&fixture, tool, &id(POLICY, "ClassifyAutomatic", 1), 6).await);
            assert!(nonzero["selected_dependents_observed"].as_u64().unwrap() > 0);
            assert!(nonzero.get("terminal_member_inspection").is_none());
        }
    });
}

#[test]
fn optional_advice_uses_only_available_held_generation_ranking_and_preserves_impact() {
    owned_case(|fixture|async move {
        async fn blast(fixture:&Fixture,args:Value)->String {
            let response=fixture.engram.handle_compute_blast_radius(serde_json::from_value(args).unwrap()).await.unwrap();
            assert!(!response.is_error.unwrap_or(false));response.content.iter().filter_map(|c|c.as_text()).map(|t|t.text.as_str()).collect()
        }
        fn profile(text:&str)->Value { serde_json::from_str(text.split("## Unverified one-hop advisory profile").nth(1).unwrap().split("```json").nth(1).unwrap().split("```").next().unwrap()).unwrap() }
        let target=id(POLICY,"ClassifyAutomatic",1);
        let cache=fixture.engram.state.peek_project_cached(PID).unwrap().unwrap().pagerank_cache;
        let retention=cache.retention_observation();let mut expected=None;
        assert!(fixture.engram.state.graph.get_cached_centrality(PID,2).unwrap().is_none());
        for guidance in [Some(false),Some(true),None] {
            let mut args=json!({"project_id":PID,"symbol_fqn":target});
            if let Some(value)=guidance {args["include_guidance"]=json!(value);}
            let text=blast(&fixture,args).await;let facts=impact_packet(&text);
            if let Some(old)=&expected {assert_eq!(&facts,old);} else {expected=Some(facts);}
            if guidance==Some(false) {assert!(!text.contains("## Unverified one-hop advisory profile"));}
            else {
                let partial=profile(&text);
                assert!(partial["migration_risk"].is_null() && partial["risk_band"].is_null());
                assert!(partial["complexity_breakdown"]["pagerank_score"].is_null());
                assert_eq!(partial["centrality"]["score_available"],false);
            }
            assert_eq!(cache.retention_observation(),retention,"Advice must not start ranking or alter retention");
            assert!(fixture.engram.state.graph.get_cached_centrality(PID,2).unwrap().is_none(),"Default advice must not populate persistent project ranking");
        }
        let workers=Arc::new(tokio::sync::Semaphore::new(1));
        cache.start(2,workers.clone().try_acquire_owned().unwrap()).unwrap().complete(engram_graph::analysis::CentralityMetrics {
            pagerank:std::collections::HashMap::from([(target.clone(),0.04)]),
        });
        let cached=blast(&fixture,json!({"project_id":PID,"symbol_fqn":target})).await;
        assert_eq!(impact_packet(&cached),expected.clone().unwrap());
        assert_eq!(profile(&cached)["complexity_breakdown"]["pagerank_score"],8.0);
        assert_eq!(profile(&cached)["centrality"]["source_generation"],2);
        assert!(cache.get(3).is_none());
        cache.start(3,workers.clone().try_acquire_owned().unwrap()).unwrap().complete(engram_graph::analysis::CentralityMetrics {
            pagerank:std::collections::HashMap::from([(target.clone(),0.0)]),
        });
        let older=blast(&fixture,json!({"project_id":PID,"symbol_fqn":target})).await;
        assert_eq!(impact_packet(&older),expected.unwrap());
        assert_eq!(profile(&older)["centrality"]["status"],"generation_not_cached");
        assert!(profile(&older)["migration_risk"].is_null(),"A newer map must not turn a held older request into zero/Low");
        assert_eq!(workers.available_permits(),1);
        let complete=blast(&fixture,json!({"project_id":PID,"file_path":SPATIAL})).await;
        assert!(complete.contains("## Unverified one-hop advisory profile"));
        assert!(profile(&complete)["component_membership"].as_str().unwrap().contains("at collection"));
        // Retain an actual older declaration alongside the current graph.
        // A partial component must not treat it as a current external caller.
        fixture.engram.state.graph.upsert_nodes(PID,&[declaration(POLICY,"ObsoletePolicyCaller",3,1)]).unwrap();
        let file=blast(&fixture,json!({"project_id":PID,"file_path":POLICY})).await;
        assert!(file.contains("file_members_not_fully_admitted"));
        assert!(!file.contains("## Unverified one-hop advisory profile"));
        assert_eq!(impact_packet(&file)["target_revision_admitted"],true);
        let overflow=(0..130).map(|i|declaration(POLICY,&format!("Overflow{i}"),1,2)).collect::<Vec<_>>();
        fixture.engram.state.graph.upsert_nodes(PID,&overflow).unwrap();
        let capped=blast(&fixture,json!({"project_id":PID,"file_path":POLICY})).await;
        assert!(capped.contains("file_member_inventory_capped"));
        assert!(!capped.contains("## Unverified one-hop advisory profile"));
        assert!(impact_packet(&capped)["coverage"]["reasons"].to_string().contains("file_seed_cap"));
    });
}

async fn impact(fixture: &Fixture, max_depth: Option<usize>) -> String {
    impact_target(fixture, &id(POLICY, "ClassifyAutomatic", 1), max_depth).await
}

async fn impact_target(fixture: &Fixture, target: &str, max_depth: Option<usize>) -> String {
    let mut request = json!({"project_id":PID,
        "symbol_fqn":target,"limit":50});
    if let Some(depth) = max_depth {
        request["max_depth"] = json!(depth);
    }
    let result = fixture
        .engram
        .handle_impact_analysis(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|content| content.text.as_str())
        .collect()
}

async fn summary_impact(fixture: &Fixture, tool: &str, target: &str, max_depth: usize) -> String {
    let request = json!({"project_id":PID,"symbol_fqn":target,
        "max_depth":max_depth,"limit":50,"detail":"summary","include_guidance":false});
    let result = match tool {
        "impact_analysis" => {
            let mut request = request;
            request.as_object_mut().unwrap().remove("include_guidance");
            fixture
                .engram
                .handle_impact_analysis(serde_json::from_value(request).unwrap())
                .await
        }
        "compute_blast_radius" => {
            let mut request = request;
            request.as_object_mut().unwrap().remove("limit");
            fixture
                .engram
                .handle_compute_blast_radius(serde_json::from_value(request).unwrap())
                .await
        }
        _ => panic!("Unexpected public impact tool: {tool}"),
    }
    .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text: String = result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|content| content.text.as_str())
        .collect();
    assert!(
        text.len() <= 12 * 1024,
        "The complete agent summary must fit a12KiB context budget, including navigation and caveats"
    );
    text
}

async fn expand_summary_graph(
    fixture: &Fixture,
    summary: &Value,
    target: &str,
    depth: usize,
) -> Value {
    let continuation = &summary["graph_continuation"];
    assert_eq!(continuation["tool"], "impact_analysis");
    let arguments = &continuation["arguments"];
    assert_eq!(arguments["project_id"], PID);
    assert_eq!(arguments["symbol_fqn"], target);
    assert_eq!(arguments["max_depth"], depth);
    assert_eq!(arguments["detail"], "graph");
    let result = fixture
        .engram
        .handle_impact_analysis(
            serde_json::from_value(arguments.clone())
                .expect("The returned continuation must be copyable without repair"),
        )
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text: String = result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|content| content.text.as_str())
        .collect();
    impact_packet(&text)
}

fn graph_connections(report: &Value) -> std::collections::BTreeSet<(String, String)> {
    let nodes: std::collections::BTreeMap<_, _> = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            (
                node["ref"].as_str().unwrap(),
                node["node_id"].as_str().unwrap(),
            )
        })
        .collect();
    report["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            (
                nodes[edge["source_ref"].as_str().unwrap()].to_owned(),
                nodes[edge["target_ref"].as_str().unwrap()].to_owned(),
            )
        })
        .collect()
}

fn impact_packet(text: &str) -> Value {
    let section = text
        .split_once("## Revision-admitted change impact")
        .expect("Public impact output needs a distinct revision-admitted section")
        .1;
    let json = section
        .split_once("```json")
        .expect("Impact evidence needs structured witnesses")
        .1
        .split_once("```")
        .expect("Impact evidence must be complete JSON")
        .0;
    serde_json::from_str(json).unwrap()
}

// Public compact objects are overlays: explicit row values win over defaults.
// Compare their meaning rather than requiring one particular wire encoding.
fn decoded_connection(defaults: &Value, row: &Value) -> Value {
    let mut decoded = defaults.as_object().unwrap().clone();
    decoded.extend(row.as_object().unwrap().clone());
    decoded.remove("recovery_ref");
    Value::Object(decoded)
}

fn assert_source_route_connections(sources: &Value, graph: &Value) {
    let defaults = &sources["connection_defaults"];
    assert!(defaults.is_object(), "A compact route must disclose its overlay defaults");
    for row in sources["connections"].as_array().unwrap() {
        assert!(row["edge_generation"].is_u64(),
            "Each connection keeps its explicit admitted edge generation");
        let original = graph["edges"].as_array().unwrap().iter()
            .find(|old| old["ref"] == row["ref"]).unwrap();
        assert_eq!(decoded_connection(defaults, row),
            decoded_connection(&graph["omitted_defaults"]["edges"], original),
            "Overlay decoding must preserve every connection field, including absent positions and mixed overrides");
    }
}

// Resolve the compact public tables to the semantic identities/paths asserted
// by the older behavior cases. This does not manufacture additional paths.
fn selected_impact(text: &str) -> Value {
    let mut report = impact_packet(text);
    if report.get("shortest_paths").is_none() {
        return report;
    }
    let nodes: std::collections::BTreeMap<_, _> = report["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["ref"].as_str().unwrap().to_owned(), node.clone()))
        .collect();
    let files: std::collections::BTreeMap<_, _> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["ref"].as_str().unwrap().to_owned(),
                file["path"].clone(),
            )
        })
        .collect();
    let semantic_nodes: Vec<_> = nodes
        .values()
        .filter(|node| node["admission"] == "selected")
        .map(|node| {
            let mut node = node.clone();
            node["file_path"] = files[node["file_ref"].as_str().unwrap()].clone();
            node
        })
        .collect();
    let dependents: Vec<_> = report["shortest_paths"].as_array().unwrap().iter().map(|path| {
        let dependent = &nodes[path["dependent_ref"].as_str().unwrap()];
        let node_refs = path["node_refs"].as_array().unwrap();
        let ids: Vec<_> = node_refs.iter().map(|node_ref|nodes[node_ref.as_str().unwrap()]["node_id"].clone()).collect();
        let seed = ids.first().unwrap();
        json!({"node_id":dependent["node_id"],"file_path":files[dependent["file_ref"].as_str().unwrap()],
            "start_line":dependent["start_line"],"generation":dependent["generation"],"depth":node_refs.len()-1,
            "seed_node_id":seed,"seed_relationship":path["seed_relationship"],"path":ids})
    }).collect();
    report["nodes"] = json!(semantic_nodes);
    report["dependents"] = json!(dependents);
    report
}

fn dependent<'a>(report: &'a Value, node_id: &str) -> &'a Value {
    report["dependents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["node_id"] == node_id)
        .unwrap_or_else(|| panic!("Missing current causal dependent: {node_id}"))
}

#[test]
fn agents_can_inspect_recorded_references_without_reassigning_same_named_callers() {
    owned_case(|fixture| async move {
        let mut lines = vec!["// padding"; 120];
        lines[0] = "pub fn ClassifyAutomatic() -> i32 { 1 }";
        lines[91] = "pub fn Resolve() -> i32 {";
        lines[95] = "    let first = ClassifyAutomatic();";
        lines[100] = "    ClassifyAutomatic()";
        lines[105] = "}";
        lines[109] = "pub fn Resolve(value: i32) -> i32 {";
        lines[113] = "    let callback = ClassifyAutomatic;";
        lines[119] = "}";
        let body = lines.join("\n") + "\n";
        let cancel = CancellationToken::new();
        let baseline = fixture
            .engram
            .state
            .registry
            .capture_source_head(PID)
            .unwrap();
        fixture
            .search
            .prepare_source_snapshot(PID, 2, 3, &[RelPath::new(POLICY)], &[], &cancel)
            .await
            .unwrap();
        fixture
            .search
            .index_docs(PID, &[source_doc(3, 300, POLICY, &body)], &cancel)
            .await
            .unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        std::fs::write(fixture.root.join(POLICY), &body).unwrap();
        let root = id(POLICY, "ClassifyAutomatic", 1);
        let first = id(POLICY, "Resolve", 92);
        let second = id(POLICY, "Resolve", 110);
        let mut first_node = declaration(POLICY, "Resolve", 92, 3);
        first_node.end_line = 106;
        let mut second_node = declaration(POLICY, "Resolve", 110, 3);
        second_node.end_line = 120;
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(
                PID,
                &[
                    file_node(POLICY, &body, 3),
                    declaration(POLICY, "ClassifyAutomatic", 1, 3),
                    first_node,
                    second_node,
                ],
            )
            .unwrap();
        let mut first_edge = edge(first.clone(), root.clone(), EdgeKind::Calls, 3);
        let mut second_edge = edge(second.clone(), root.clone(), EdgeKind::Calls, 3);
        fixture
            .engram
            .state
            .graph
            .upsert_edges(PID, &[first_edge.clone(), second_edge.clone()])
            .unwrap();
        assert!(
            fixture
                .engram
                .state
                .registry
                .publish_source_head(&baseline, 3)
                .unwrap()
        );
        let before = impact_packet(&impact_target(&fixture, &root, Some(6)).await);
        first_edge.metadata =
            Some(json!({"src_line":"92","call_site_line":"96","call_site_lines":[101,96,101]}));
        second_edge.metadata = Some(json!({"src_line":110,"call_site_line":114,"via":"addressof"}));
        fixture
            .engram
            .state
            .graph
            .upsert_edges(PID, &[first_edge, second_edge])
            .unwrap();
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let request = json!({"project_id":PID,"symbol_fqn":root,"max_depth":6});
            let result = match tool {
                "impact_analysis" => {
                    fixture
                        .engram
                        .handle_impact_analysis(serde_json::from_value(request).unwrap())
                        .await
                }
                _ => {
                    fixture
                        .engram
                        .handle_compute_blast_radius(serde_json::from_value(request).unwrap())
                        .await
                }
            }
            .unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let text: String = result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .map(|c| c.text.as_str())
                .collect();
            let graph = impact_packet(&text);
            assert_eq!(graph["coverage"], before["coverage"]);
            assert_eq!(
                graph["nodes"], before["nodes"],
                "Reference metadata cannot change owner identity, bounds or admission"
            );
            assert_eq!(graph["shortest_paths"], before["shortest_paths"]);
            assert_eq!(
                graph["edges"].as_array().unwrap().len(),
                before["edges"].as_array().unwrap().len()
            );
            for (id, anchor, positions) in
                [(&first, 92, json!([96, 101])), (&second, 110, json!([114]))]
            {
                let node = graph["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["node_id"] == *id)
                    .unwrap();
                let witness = graph["edges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["source_ref"] == node["ref"])
                    .unwrap();
                assert_eq!(witness["source_line"], positions[0]);
                assert_eq!(witness["extractor_anchor_line"], anchor);
                assert_eq!(
                    witness["source_position_kind"],
                    "call_reference_syntax_metadata"
                );
                assert_eq!(witness["reference_lines"], positions);
            }
            let summary = impact_packet(&summary_impact(&fixture, tool, &root, 6).await);
            assert_eq!(summary["coverage"], graph["coverage"]);
            let consumer = summary["immediate_consumers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|n| n["node_id"] == first)
                .unwrap();
            assert_eq!(consumer["start_line"], 92);
            let connection = &consumer["sample_connection_to_target"];
            assert_eq!(connection["source_line"], 96);
            assert_eq!(connection["extractor_anchor_line"], 92);
            assert_eq!(connection["reference_lines"], json!([96, 101]));
            assert_eq!(connection["recorded_position_uses_source_read"], true);
            assert!(connection.get("recorded_position_read").is_none(),
                "An identical executable read should appear once, with an explicit reuse marker");
            let recovered = fixture
                .engram
                .handle_get_full_method_body(
                    serde_json::from_value(
                        consumer["source_read"]["arguments"].clone(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(!recovered.is_error.unwrap_or(false));
            let recovered: Value =
                serde_json::from_str(&recovered.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(
                recovered["source_file_hash"],
                ContentHash::compute(body.as_bytes()).0
            );
            assert!(
                recovered["source_code"]
                    .as_str()
                    .unwrap()
                    .contains("let first = ClassifyAutomatic();")
            );
            assert!(
                graph["limitations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v.as_str().unwrap().contains("delegate/property reference"))
            );
        }
        // A stitched UI edge can carry the target handler's anchor on a
        // source control in another file. Even an in-file line114 must not
        // be attributed to the known92..106 caller merely because it exists.
        for (anchor, explicit_reference, known_span, expected_scope, file_wide) in [
            (114, None, true, "unattributed_extractor_anchor", false),
            (616, None, true, "unattributed_extractor_anchor", false),
            (96, None, true, "indexed_metadata_only", false),
            (114, Some(114), true, "indexed_metadata_only", false),
            (114, None, false, "unverified_file_anchor", false),
            (114, None, true, "unverified_file_anchor", true),
        ] {
            let mut owner = declaration(POLICY, "Resolve", 92, 3);
            owner.end_line = 106;
            if file_wide {
                owner.node_type = "page".into();
                owner.start_line = 1;
                owner.end_line = 1;
            }
            if !known_span {
                owner.start_line = 0;
                owner.end_line = 0;
            }
            fixture
                .engram
                .state
                .graph
                .upsert_nodes(PID, &[owner])
                .unwrap();
            let mut call = edge(first.clone(), root.clone(), EdgeKind::Calls, 3);
            let mut metadata = json!({"src_line":anchor});
            if let Some(line) = explicit_reference {
                metadata["call_site_line"] = json!(line);
            }
            call.metadata = Some(metadata);
            fixture
                .engram
                .state
                .graph
                .upsert_edges(PID, &[call])
                .unwrap();
            for tool in ["impact_analysis", "compute_blast_radius"] {
                let summary = impact_packet(&summary_impact(&fixture, tool, &root, 6).await);
                let consumer = summary["immediate_consumers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["node_id"] == first)
                    .unwrap();
                let connection = &consumer["sample_connection_to_target"];
                assert_eq!(connection["source_line"], anchor);
                assert_eq!(connection["extractor_anchor_line"], anchor);
                assert_eq!(connection["source_position_scope"], expected_scope);
                let unattributed = expected_scope == "unattributed_extractor_anchor";
                let shared = connection["recorded_position_uses_source_read"] == true;
                if shared {
                    assert!(!unattributed);
                    assert!(connection.get("recorded_position_read").is_none());
                } else {
                    assert_eq!(connection["recorded_position_read"].is_null(), unattributed);
                }
                let action = if unattributed || shared {
                    &consumer["source_read"]
                } else {
                    &connection["recorded_position_read"]
                };
                assert_eq!(action["tool"], "get_full_method_body");
                if unattributed {
                    assert_eq!(action["arguments"]["line_start"], 92);
                    assert_eq!(action["arguments"]["line_end"], 106);
                } else if anchor == 114 {
                    assert!(!shared, "A position outside the declaration keeps its own read action");
                    assert_eq!(action["arguments"]["line_start"], 114);
                    assert_eq!(action["arguments"]["line_end"], 114);
                }
                let result = fixture
                    .engram
                    .handle_get_full_method_body(
                        serde_json::from_value(action["arguments"].clone()).unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(!result.is_error.unwrap_or(false));
                let recovered: Value =
                    serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
                assert_eq!(
                    recovered["source_file_hash"],
                    ContentHash::compute(body.as_bytes()).0
                );
                let graph = expand_summary_graph(&fixture, &summary, &root, 6).await;
                assert_eq!(graph["coverage"], before["coverage"]);
                assert_eq!(graph["shortest_paths"], before["shortest_paths"]);
                assert_eq!(
                    graph["edges"].as_array().unwrap().len(),
                    before["edges"].as_array().unwrap().len()
                );
                let node = graph["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["node_id"] == first)
                    .unwrap();
                let witness = graph["edges"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["source_ref"] == node["ref"])
                    .unwrap();
                assert_eq!(witness["source_position_scope"], expected_scope);
                if unattributed {
                    assert!(
                        graph["recoveries"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|r| r["arguments"]["line_start"] != anchor
                                || r["arguments"]["line_end"] != anchor)
                    );
                }
            }
        }
    });
}

#[test]
fn a_shortcut_preserves_the_prepared_alternate_route_and_returns_executable_identity_bound_recovery()
 {
    owned_case(|fixture| async move {
        let spatial = "pub fn ResolvePolicy(stop: bool) -> i32 { crate::policy::Resolve() }\npub fn QueryCacheCore(stop: bool) -> i32 { ResolvePolicy(stop) }\npub fn QueryViewport() -> i32 { let prepared = true; if prepared { QueryCacheCore(true) } else { ResolvePolicy(true) } }\n";
        let cancel = CancellationToken::new();
        let baseline = fixture
            .engram
            .state
            .registry
            .capture_source_head(PID)
            .unwrap();
        fixture
            .search
            .prepare_source_snapshot(PID, 2, 3, &[RelPath::new(SPATIAL)], &[], &cancel)
            .await
            .unwrap();
        fixture
            .search
            .index_docs(PID, &[source_doc(3, 100, SPATIAL, spatial)], &cancel)
            .await
            .unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        std::fs::write(fixture.root.join(SPATIAL), spatial).unwrap();
        let root = id(POLICY, "ClassifyAutomatic", 1);
        let resolve = id(POLICY, "Resolve", 2);
        let policy = id(SPATIAL, "ResolvePolicy", 1);
        let cache = id(SPATIAL, "QueryCacheCore", 2);
        let viewport = id(SPATIAL, "QueryViewport", 3);
        let http = id(HTTP, "GetFileLayerViewport", 1);
        let bad = id(COMPANION, "BadEdgeOnly", 3);
        let dangling = "sym:function:src/deleted.rs:DeletedCaller:1";
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(
                PID,
                &[
                    file_node(SPATIAL, spatial, 3),
                    declaration(SPATIAL, "ResolvePolicy", 1, 3),
                    declaration(SPATIAL, "QueryCacheCore", 2, 3),
                    declaration(SPATIAL, "QueryViewport", 3, 3),
                    declaration(COMPANION, "BadEdgeOnly", 3, 1),
                ],
            )
            .unwrap();
        let mut invalid_origin = edge(bad.clone(), root.clone(), EdgeKind::Calls, 1);
        invalid_origin.metadata = Some(json!({"_engram_source_origin":{
            "version":1,"source_id":bad,"source_generation":0}}));
        let mut prepared_call = edge(viewport.clone(), cache.clone(), EdgeKind::Calls, 3);
        prepared_call.metadata = Some(json!({"src_line":"3"}));
        // This case is a diamond, not recursion. Other cases retain genuine
        // source cycles; retire this owned fixture's old recursive bindings.
        fixture
            .engram
            .state
            .graph
            .delete_edges_of_kind(PID, &EdgeKind::Calls)
            .unwrap();
        fixture
            .engram
            .state
            .graph
            .upsert_edges(
                PID,
                &[
                    edge(resolve.clone(), root.clone(), EdgeKind::Calls, 2),
                    edge(policy.clone(), resolve.clone(), EdgeKind::Calls, 3),
                    edge(cache.clone(), policy.clone(), EdgeKind::Calls, 3),
                    prepared_call,
                    edge(viewport.clone(), policy.clone(), EdgeKind::Calls, 3),
                    edge(http.clone(), viewport.clone(), EdgeKind::Calls, 1),
                    edge(
                        id(COMPANION, "HistoricCompanion", 1),
                        root.clone(),
                        EdgeKind::TemporalCoupling,
                        1,
                    ),
                    edge(
                        format!("file:{POLICY}"),
                        root.clone(),
                        EdgeKind::Contains,
                        2,
                    ),
                    edge(dangling.into(), root.clone(), EdgeKind::Calls, 1),
                    invalid_origin,
                ],
            )
            .unwrap();
        assert!(
            fixture
                .engram
                .state
                .registry
                .publish_source_head(&baseline, 3)
                .unwrap()
        );
        let impact_text = impact(&fixture, None).await;
        let blast = fixture
            .engram
            .handle_compute_blast_radius(
                serde_json::from_value(json!({
                    "project_id":PID,"symbol_fqn":root,"include_guidance":false,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!blast.is_error.unwrap_or(false));
        let blast_text: String = blast
            .content
            .iter()
            .filter_map(|item| item.as_text())
            .map(|item| item.text.as_str())
            .collect();
        let mut observed_pairs = Vec::new();
        for public_text in [&impact_text, &blast_text] {
            let first = &public_text[..public_text.floor_char_boundary(2048)];
            assert!(
                first.contains("Resolve — src/policy.rs:2"),
                "An agent must see the immediate selected caller before the large graph"
            );
            let immediate = first
                .split_once("## First immediate indexed consumer")
                .unwrap()
                .1;
            let call: Value = serde_json::from_str(
                immediate
                    .split_once("```json")
                    .unwrap()
                    .1
                    .split_once("```")
                    .expect("The first2KiB must contain the complete executable next read")
                    .0,
            )
            .unwrap();
            assert_eq!(call["tool"], "get_full_method_body");
            assert_eq!(call["arguments"]["file_path"], POLICY);
            assert_eq!(call["arguments"]["line_start"], 2);
            assert_eq!(call["arguments"]["line_end"], 2);
            let next = fixture
                .engram
                .handle_get_full_method_body(
                    serde_json::from_value(call["arguments"].clone()).unwrap(),
                )
                .await
                .unwrap();
            assert!(!next.is_error.unwrap_or(false));
            let next: Value =
                serde_json::from_str(&next.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(
                next["source_code"],
                "pub fn Resolve() -> i32 { ClassifyAutomatic() }"
            );
            assert_eq!(
                next["source_file_hash"],
                ContentHash::compute(POLICY_BODY.as_bytes()).0
            );
            assert_eq!(
                next["source_verification"],
                "explicit_range_current_file_snapshot"
            );
            let report = impact_packet(public_text);
            assert_eq!(report["source_generation"], 3);
            let nodes = report["nodes"].as_array().unwrap();
            let by_ref: std::collections::BTreeMap<_, _> = nodes
                .iter()
                .map(|node| (node["ref"].as_str().unwrap(), node))
                .collect();
            let identities: std::collections::BTreeSet<_> = nodes
                .iter()
                .map(|node| node["node_id"].as_str().unwrap())
                .collect();
            assert_eq!(
                identities.len(),
                nodes.len(),
                "Each confirmed identity belongs in the table once, even on cyclic/diamond paths"
            );
            assert_eq!(by_ref.len(), nodes.len());
            for expected in [&root, &resolve, &policy, &cache, &viewport, &http] {
                assert!(
                    nodes.iter().any(|node| node["node_id"] == expected.as_str()
                        && node["admission"] == "selected")
                );
            }
            let selected: Vec<_> = nodes
                .iter()
                .filter(|node| node["admission"] == "selected")
                .collect();
            assert_eq!(selected.len(), 6);
            assert!(
                nodes
                    .iter()
                    .filter(|node| node["node_id"] == bad || node["node_id"] == dangling)
                    .all(|node| node["admission"] != "selected")
            );
            assert_eq!(
                report["selected_dependents_observed"].as_u64().unwrap() as usize,
                selected
                    .iter()
                    .filter(|node| node["is_seed"] == false)
                    .count(),
                "Observed counts must reconcile with distinct selected identities, not queued visits"
            );
            let files = report["files"].as_array().unwrap();
            let file_refs: std::collections::BTreeSet<_> = files
                .iter()
                .map(|file| file["ref"].as_str().unwrap())
                .collect();
            for node in nodes {
                if let Some(reference) = node["file_ref"].as_str() {
                    assert!(file_refs.contains(reference));
                } else {
                    assert_ne!(
                        node["admission"], "selected",
                        "An unresolved frontier must not masquerade as selected source"
                    );
                }
            }
            let pairs: std::collections::BTreeSet<_> = report["edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|edge| {
                    let source = by_ref[edge["source_ref"]
                        .as_str()
                        .expect("Every admitted edge needs a resolvable source")]["node_id"]
                        .as_str()
                        .unwrap();
                    let target = by_ref[edge["target_ref"]
                        .as_str()
                        .expect("Every admitted edge needs a resolvable target")]["node_id"]
                        .as_str()
                        .unwrap();
                    (source.to_owned(), target.to_owned())
                })
                .collect();
            for expected in [
                (&viewport, &policy),
                (&viewport, &cache),
                (&cache, &policy),
                (&http, &viewport),
            ] {
                assert!(
                    pairs.contains(&(expected.0.clone(), expected.1.clone())),
                    "The shortest-path shortcut must not erase the longer prepared branch: {expected:?}"
                );
            }
            assert!(
                !pairs.contains(&(policy.clone(), cache.clone())),
                "A diamond revisit is not evidence of a reverse call or a cycle"
            );
            assert!(!pairs.iter().any(|(source, _)| source == &bad
                || source == dangling
                || source.starts_with("file:")
                || source.contains("HistoricCompanion")));
            for path in report["shortest_paths"].as_array().unwrap() {
                let refs = path["node_refs"].as_array().unwrap();
                let unique: std::collections::BTreeSet<_> =
                    refs.iter().map(|item| item.as_str().unwrap()).collect();
                assert_eq!(
                    unique.len(),
                    refs.len(),
                    "Revisiting an identity must not fabricate a cyclic shortest-path witness"
                );
                for reference in refs {
                    assert!(by_ref.contains_key(reference.as_str().unwrap()));
                }
                let edge_refs = path["edge_refs"].as_array().unwrap();
                assert_eq!(edge_refs.len() + 1, refs.len());
                for (index, reference) in edge_refs.iter().enumerate() {
                    let witness = report["edges"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|edge| edge["ref"] == *reference)
                        .unwrap();
                    assert_eq!(witness["source_ref"], refs[index + 1]);
                    assert_eq!(witness["target_ref"], refs[index]);
                }
            }
            let frontier_ids: std::collections::BTreeSet<_> = report["frontier"]
                .as_array()
                .unwrap()
                .iter()
                .map(|boundary| {
                    by_ref[boundary["source_ref"].as_str().unwrap()]["node_id"]
                        .as_str()
                        .unwrap()
                })
                .collect();
            assert!(
                frontier_ids.contains(dangling) && frontier_ids.contains(bad.as_str()),
                "Missing/unverified endpoints need named frontier identities, not anonymous counter-only gaps"
            );
            let viewport_node = nodes
                .iter()
                .find(|node| node["node_id"] == viewport)
                .unwrap();
            assert_eq!(
                report.to_string().matches(&viewport).count(),
                1,
                "Shared node references must avoid repeating the same full identity in every path and witness"
            );
            let recovery_ref = viewport_node["recovery_ref"].as_str().unwrap();
            let prepared_call = report["edges"]
                .as_array()
                .unwrap()
                .iter()
                .find(|edge| {
                    by_ref[edge["source_ref"].as_str().unwrap()]["node_id"] == viewport
                        && by_ref[edge["target_ref"].as_str().unwrap()]["node_id"] == cache
                })
                .unwrap();
            assert_eq!(
                prepared_call["source_line"], 3,
                "The recorded source reference line must survive compact rendering"
            );
            assert_eq!(
                prepared_call["source_position_kind"], "extractor_anchor_metadata",
                "A legacy extractor anchor must not become a recorded invocation by relabeling it"
            );
            assert_eq!(
                prepared_call
                    .get("recovery_ref")
                    .unwrap_or(&viewport_node["recovery_ref"]),
                &viewport_node["recovery_ref"],
                "A reference inside the caller span uses its executable caller range"
            );
            let recovery = report["recoveries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|action| action["ref"] == recovery_ref)
                .unwrap();
            assert_eq!(recovery["tool"], "get_full_method_body");
            let recovered = fixture
                .engram
                .handle_get_full_method_body(
                    serde_json::from_value(recovery["arguments"].clone()).unwrap(),
                )
                .await
                .unwrap();
            assert!(!recovered.is_error.unwrap_or(false));
            let recovered: Value =
                serde_json::from_str(&recovered.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(
                recovered["source_file_hash"],
                ContentHash::compute(spatial.as_bytes()).0
            );
            let body = recovered["source_code"].as_str().unwrap();
            assert!(
                body.contains("if prepared")
                    && body.contains("QueryCacheCore(true)")
                    && body.contains("ResolvePolicy(true)")
            );
            observed_pairs.push(pairs);
        }
        assert_eq!(
            observed_pairs[0], observed_pairs[1],
            "Both public impact tools must expose the same admitted causal graph"
        );

        let graph = impact_packet(&impact_text);
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let text = summary_impact(&fixture, tool, &root, 6).await;
            let summary = impact_packet(&text);
            assert_eq!(summary["detail"], "summary");
            for field in [
                "source_generation",
                "source_layout",
                "requested_max_depth",
                "status",
                "target_revision_admitted",
                "selected_dependents_observed",
                "coverage",
                "working_tree_checked",
                "exhaustive_codebase",
                "change_breakage_proven",
            ] {
                assert_eq!(
                    summary[field], graph[field],
                    "Summary must preserve the admitted evidence and uncertainty for {field}, not perform a smaller walk"
                );
            }
            assert_eq!(summary["target"]["node_id"], root);
            assert_eq!(summary["target"]["generation"], 2);
            assert_eq!(
                summary["verified_connections_returned_in_graph"],
                graph["edges"].as_array().unwrap().len()
            );
            for field in [
                "node_identities_omitted",
                "edges_omitted",
                "shortest_paths_omitted",
                "frontier_rows_omitted",
            ] {
                assert_eq!(
                    summary["graph_omissions"][field], graph[field],
                    "A compact summary must retain graph omissions separately from its own display limits"
                );
            }
            let immediate = summary["immediate_consumers"].as_array().unwrap();
            assert_eq!(immediate.len(), 1);
            assert_eq!(summary["immediate_consumers_omitted_from_summary"], 0);
            assert_eq!(immediate[0]["node_id"], resolve);
            assert_eq!(immediate[0]["file_path"], POLICY);
            assert_eq!(immediate[0]["admission"], "selected");
            assert_eq!(immediate[0]["source_read_scope"], "explicit_source_range");
            let sources = &summary["source_routes"];
            assert_eq!(sources["admission"], "selected");
            assert_source_route_connections(sources, &graph);
            assert!(serde_json::to_vec(sources).unwrap().len() <= 4 * 1024);
            let source_nodes: std::collections::BTreeMap<_, _> = sources["nodes"].as_array().unwrap().iter()
                .map(|node| (node["ref"].as_str().unwrap(), node)).collect();
            for node in source_nodes.values() {
                let original = graph["nodes"].as_array().unwrap().iter()
                    .find(|old| old["node_id"] == node["node_id"]).unwrap();
                assert_eq!(original["admission"], "selected");
                assert_eq!(node["generation"], original["generation"]);
                assert_ne!(node["node_id"], dangling);
                assert_ne!(node["node_id"], bad);
            }
            for edge in sources["connections"].as_array().unwrap() {
                assert!(source_nodes.contains_key(edge["source_ref"].as_str().unwrap())
                    && source_nodes.contains_key(edge["target_ref"].as_str().unwrap()),
                    "An agent must not resolve dangling local graph references");
            }
            let reads = sources["reads"].as_array().unwrap();
            assert_eq!(reads.len() + sources["windows_omitted"].as_u64().unwrap() as usize,
                sources["eligible_windows"].as_u64().unwrap() as usize);
            let prepared = reads.iter().find(|read| read["arguments"]["file_path"] == SPATIAL).unwrap();
            assert_eq!(prepared["arguments"]["line_start"], 1);
            assert_eq!(prepared["arguments"]["line_end"], 3,
                "The shortcut must not prevent one bounded read from inspecting the prepared alternate route");
            let covered: std::collections::BTreeSet<_> = prepared["covered_refs"].as_array().unwrap().iter()
                .map(|r| source_nodes[r.as_str().unwrap()]["node_id"].as_str().unwrap()).collect();
            assert!(covered.contains(policy.as_str()) && covered.contains(cache.as_str()) && covered.contains(viewport.as_str()));
            let http_source = reads.iter().find(|read| read["arguments"]["file_path"] == HTTP).unwrap();
            for read in [prepared, http_source] {
                assert_eq!(read["tool"], "get_full_method_body");
                let args = &read["arguments"];
                assert!(args["line_end"].as_u64().unwrap() - args["line_start"].as_u64().unwrap() < 120);
                let reply = fixture.engram.handle_get_full_method_body(serde_json::from_value(args.clone()).unwrap()).await.unwrap();
                let current: Value = serde_json::from_str(&reply.content[0].as_text().unwrap().text).unwrap();
                let path = args["file_path"].as_str().unwrap();
                let raw = std::fs::read(fixture.root.join(path)).unwrap();
                assert_eq!(current["source_file_hash"], ContentHash::compute(&raw).0);
                if path == SPATIAL {
                    let text = current["source_code"].as_str().unwrap();
                    assert!(text.contains("if prepared") && text.contains("QueryCacheCore(true)") && text.contains("ResolvePolicy(true)"));
                } else { assert!(current["source_code"].as_str().unwrap().contains("QueryViewport()")); }
            }
            let navigation = &summary["consumer_navigation"];
            let next = &navigation["expansions"][0];
            assert_eq!(next["tool"], "impact_analysis");
            assert_eq!(next["arguments"]["symbol_fqn"], resolve);
            assert_eq!(next["arguments"]["detail"], "summary");
            assert_eq!(next["arguments"]["max_depth"], 1);
            let result = fixture.engram.handle_impact_analysis(
                serde_json::from_value(next["arguments"].clone()).unwrap()).await.unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let returned = &result.content[0].as_text().unwrap().text;
            assert!(returned.len() <= 12 * 1024);
            let expanded_callers = impact_packet(returned);
            assert_eq!(expanded_callers["target"]["node_id"], resolve);
            assert_eq!(expanded_callers["direct_dependents_observed"], 1);
            assert!(expanded_callers["immediate_consumers"].as_array().unwrap().iter()
                .any(|node| node["node_id"] == policy));
            let inline = navigation["next_level"].as_array().unwrap().iter()
                .find(|node| node["node_id"] == policy).unwrap();
            assert_eq!(inline["parent_node_id"], resolve);
            assert_eq!(inline["generation"], 3);
            assert_eq!(inline["connection_to_parent"]["kind"], "calls");
            assert!(inline["connection_to_parent"]["source_line"].is_null());
            let connection = &immediate[0]["sample_connection_to_target"];
            assert_eq!(connection["source_node_id"], resolve);
            assert_eq!(connection["target_node_id"], root);
            assert_eq!(connection["kind"], "calls");
            assert_eq!(connection["edge_generation"], 2);
            assert!(connection["source_line"].is_null());
            assert_eq!(
                connection["source_position_scope"], "not_recorded",
                "A caller declaration range must not masquerade as an observed invocation location"
            );
            let call = &immediate[0]["source_read"];
            assert_eq!(call["tool"], "get_full_method_body");
            let prefix = &text[..text.floor_char_boundary(2048)];
            assert!(
                !prefix.contains("(node ")
                    && !prefix.contains("; edge ")
                    && !prefix.contains("; recovery "),
                "A standalone summary cannot require registries available only after graph expansion"
            );
            assert!(
                prefix.contains("indexed declaration range")
                    && prefix.contains("boundaries and invocation location are unverified"),
                "An agent must know the action inspects current source lines without certifying method or callsite boundaries"
            );
            let prefix_call: Value = serde_json::from_str(
                prefix
                    .split_once("## Next indexed source read")
                    .unwrap()
                    .1
                    .split_once("```json")
                    .unwrap()
                    .1
                    .split_once("```")
                    .expect("The first2KiB must retain a complete source read in summary mode")
                    .0,
            )
            .unwrap();
            let first_route = summary["source_routes"]["reads"].as_array().unwrap().iter()
                .find(|read|read["arguments"]["file_path"] != POLICY).unwrap();
            assert_eq!(prefix_call,json!({"tool":first_route["tool"],"arguments":first_route["arguments"]}));
            assert_ne!(prefix_call["arguments"]["file_path"],POLICY,
                "The prominent handoff leaves the target file while retaining the direct-consumer read below");
            let read = fixture
                .engram
                .handle_get_full_method_body(
                    serde_json::from_value(call["arguments"].clone()).unwrap(),
                )
                .await
                .unwrap();
            assert!(!read.is_error.unwrap_or(false));
            let body: Value =
                serde_json::from_str(&read.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(
                body["source_code"],
                "pub fn Resolve() -> i32 { ClassifyAutomatic() }"
            );
            assert_eq!(
                body["source_file_hash"],
                ContentHash::compute(POLICY_BODY.as_bytes()).0
            );
            assert_eq!(
                body["source_verification"],
                "explicit_range_current_file_snapshot"
            );

            let frontier = summary["frontier"].as_array().unwrap();
            for unconfirmed in [dangling, bad.as_str()] {
                let branch = frontier.iter().find(|branch| branch["node_id"] == unconfirmed)
                    .expect("An agent needs the missing or unverified branch identity, not only a gap count");
                assert_eq!(branch["parent_node_id"], root);
                assert_ne!(branch["admission"], "selected");
                assert!(
                    branch["reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.is_empty())
                );
                assert!(branch["recovery"]["tool"].is_string());
                assert!(branch["recovery"]["arguments"].is_object());
            }
            assert_eq!(summary["frontier_rows_omitted_from_summary"], 0);
            let missing = frontier
                .iter()
                .find(|row| row["node_id"] == dangling)
                .unwrap();
            assert_eq!(missing["lookup_hint"]["file_path"], "src/deleted.rs");
            assert_eq!(missing["lookup_hint"]["symbol_name"], "DeletedCaller");
            assert_eq!(
                missing["admission"], "unresolved",
                "A parsed declaration hint must never promote a missing endpoint into admitted evidence"
            );
            assert!(
                summary["coverage_counter_semantics"]["missing_nodes"]
                    .as_str()
                    .unwrap()
                    .contains("more than once")
            );
            assert!(
                summary["coverage_counter_semantics"]["unknown_nodes"]
                    .as_str()
                    .unwrap()
                    .contains("Zero does not resolve")
            );
            let expanded = expand_summary_graph(&fixture, &summary, &root, 6).await;
            assert_eq!(expanded["coverage"], graph["coverage"]);
            assert_eq!(
                expanded["selected_dependents_observed"],
                graph["selected_dependents_observed"]
            );
            assert_eq!(
                graph_connections(&expanded),
                observed_pairs[0],
                "Expanding either summary must recover the longer prepared route alongside the shortcut"
            );
            for identity in [&root, &resolve, &policy, &cache, &viewport, &http] {
                assert!(
                    expanded["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|node| node["node_id"] == identity.as_str()
                            && node["admission"] == "selected")
                );
            }
        }
    });
}

#[test]
fn a_separated_cache_bridge_upgrades_the_existing_read_without_losing_other_evidence() {
    owned_case(|fixture| async move {
        // The original connected Core/ResolvePolicy read is 38..136 (99 lines).
        // QueryCache is 10..30 (21 lines): together 120, but its enclosing span
        // would be 127. Preserve the original intervening preparation context.
        let mut lines = vec!["// prepared-file context"; 150];
        lines[9] = "pub fn QueryCache() -> i32 {";
        lines[10] = "    let result = QueryCacheCore();";
        lines[28] = "    result";
        lines[29] = "}";
        lines[30] = "// outside the bridge's requested window";
        lines[37] = "pub fn QueryCacheCore() -> i32 {";
        lines[38] = "    let policy = ResolvePolicy();";
        lines[78] = "    policy";
        lines[79] = "}";
        lines[100] = "// retain this context between the original declarations";
        lines[133] = "pub fn ResolvePolicy() -> i32 {";
        lines[134] = "    crate::policy::Resolve()";
        lines[135] = "}";
        let spatial = lines.join("\n") + "\n";
        let viewport = "pub fn QueryViewport(prepared: bool) -> i32 { if prepared { crate::spatial::QueryCache() } else { crate::spatial::ResolvePolicy() } }\n";
        let endpoint = "pub fn GetFileLayerViewport() -> i32 { crate::http::QueryViewport(true) }\n";
        let cancel = CancellationToken::new();
        let head = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        fixture.search.prepare_source_snapshot(PID, 2, 3,
            &[RelPath::new(SPATIAL), RelPath::new(HTTP), RelPath::new(COMPANION)],
            &[], &cancel).await.unwrap();
        for (chunk, (path, body)) in [(SPATIAL, spatial.as_str()), (HTTP, viewport),
            (COMPANION, endpoint)].into_iter().enumerate() {
            std::fs::write(fixture.root.join(path), body).unwrap();
            fixture.search.index_docs(PID, &[source_doc(3, 500 + chunk as u64, path, body)],
                &cancel).await.unwrap();
            fixture.engram.state.graph.upsert_nodes(PID, &[file_node(path, body, 3)]).unwrap();
        }
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        let target = id(POLICY, "ClassifyAutomatic", 1);
        let resolve = id(POLICY, "Resolve", 2);
        let policy = id(SPATIAL, "ResolvePolicy", 134);
        let core = id(SPATIAL, "QueryCacheCore", 38);
        let bridge = id(SPATIAL, "QueryCache", 10);
        let view = id(HTTP, "QueryViewport", 1);
        let http = id(COMPANION, "GetFileLayerViewport", 1);
        let mut bridge_node = declaration(SPATIAL, "QueryCache", 10, 3);
        // One extra indexed context line crosses the 120-line union boundary.
        // Source-range recovery does not certify complete method boundaries.
        bridge_node.end_line = 31;
        let mut core_node = declaration(SPATIAL, "QueryCacheCore", 38, 3);
        core_node.end_line = 80;
        let mut policy_node = declaration(SPATIAL, "ResolvePolicy", 134, 3);
        policy_node.end_line = 136;
        fixture.engram.state.graph.upsert_nodes(PID, &[
            bridge_node.clone(), core_node, policy_node,
            declaration(HTTP, "QueryViewport", 1, 3),
            declaration(COMPANION, "GetFileLayerViewport", 1, 3),
        ]).unwrap();
        fixture.engram.state.graph.delete_edges_of_kind(PID, &EdgeKind::Calls).unwrap();
        let mut policy_call = edge(policy.clone(), resolve.clone(), EdgeKind::Calls, 3);
        policy_call.metadata = Some(json!({"src_line":134,"call_site_line":135}));
        let mut core_call = edge(core.clone(), policy.clone(), EdgeKind::Calls, 3);
        core_call.metadata = Some(json!({"src_line":38}));
        let mut bridge_call = edge(bridge.clone(), core.clone(), EdgeKind::Calls, 3);
        bridge_call.metadata = Some(json!({"src_line":10,"call_site_line":11,"call_site_lines":[11]}));
        fixture.engram.state.graph.upsert_edges(PID, &[
            edge(resolve, target.clone(), EdgeKind::Calls, 2),
            policy_call, core_call, bridge_call,
            edge(view.clone(), bridge.clone(), EdgeKind::Calls, 3),
            edge(view.clone(), policy.clone(), EdgeKind::Calls, 3),
            edge(http, view, EdgeKind::Calls, 3),
            // File-only evidence stays a hint, not an invented callable route.
            edge(format!("file:{FUTURE_FILE}"), target.clone(), EdgeKind::Calls, 1),
        ]).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&head, 3).unwrap());

        let before_graph = impact_packet(&impact_target(&fixture, &target, Some(6)).await);
        let mut baselines = std::collections::BTreeMap::new();
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let baseline = impact_packet(&summary_impact(&fixture, tool, &target, 6).await);
            let routes = &baseline["source_routes"];
            assert_source_route_connections(routes, &before_graph);
            let reads = routes["reads"].as_array().unwrap();
            assert_eq!(reads.len(), 3, "Allocate all original reads before attempting an upgrade");
            let prepared = reads.iter().find(|read| read["arguments"]["file_path"] == SPATIAL).unwrap();
            assert_eq!(prepared["arguments"]["line_start"], 38);
            assert_eq!(prepared["arguments"]["line_end"], 136);
            assert!(prepared["arguments"].get("source_ranges").is_none(),
                "A 121-line union must preserve the complete original contiguous action");
            assert!(reads.iter().any(|read| read["arguments"]["file_path"] == HTTP));
            assert!(reads.iter().any(|read| read["arguments"]["file_path"] == COMPANION));
            assert!(routes["file_hints"].as_array().unwrap().iter().any(|path| path == FUTURE_FILE));
            assert_eq!(routes["eligible_windows"], 4);
            assert_eq!(routes["windows_omitted"], 1);
            baselines.insert(tool, baseline);
        }

        bridge_node.end_line = 30;
        fixture.engram.state.graph.upsert_nodes(PID, &[bridge_node]).unwrap();
        let graph = impact_packet(&impact_target(&fixture, &target, Some(6)).await);
        assert_eq!(graph["edges"], before_graph["edges"],
            "Changing an indexed read bound must not alter the admitted causal connections");
        assert_eq!(graph["coverage"], before_graph["coverage"]);
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let text = summary_impact(&fixture, tool, &target, 6).await;
            let summary = impact_packet(&text);
            let baseline = &baselines[tool];
            let mut unchanged = summary.clone();
            let mut previous = baseline.clone();
            unchanged.as_object_mut().unwrap().remove("source_routes");
            previous.as_object_mut().unwrap().remove("source_routes");
            assert_eq!(unchanged, previous,
                "Read upgrading cannot evict earlier hints, navigation, counts, or uncertainty disclosures");
            let routes = &summary["source_routes"];
            let old = &baseline["source_routes"];
            assert!(serde_json::to_vec(routes).unwrap().len() <= 4 * 1024);
            assert_source_route_connections(routes, &graph);
            assert_eq!(routes["file_hints"], old["file_hints"]);
            assert_eq!(routes["file_hints_omitted"], old["file_hints_omitted"]);
            assert_eq!(routes["ineligible_retained_callables"], old["ineligible_retained_callables"]);
            for node in old["nodes"].as_array().unwrap() {
                assert!(routes["nodes"].as_array().unwrap().contains(node),
                    "An added bridge cannot drop or rewrite an already displayed node");
            }
            for connection in old["connections"].as_array().unwrap() {
                let retained = routes["connections"].as_array().unwrap().iter()
                    .find(|row| row["ref"] == connection["ref"]).unwrap();
                assert_eq!(decoded_connection(&routes["connection_defaults"], retained),
                    decoded_connection(&old["connection_defaults"], connection));
            }
            let reads = routes["reads"].as_array().unwrap();
            assert_eq!(reads.len(), 3);
            assert_eq!(routes["eligible_windows"], 3);
            assert_eq!(routes["windows_omitted"], 0);
            for other in old["reads"].as_array().unwrap().iter()
                .filter(|read| read["arguments"]["file_path"] != SPATIAL) {
                assert!(reads.contains(other), "Other fully allocated reads remain byte-for-byte equivalent");
            }
            let read = reads.iter().find(|read| read["arguments"]["file_path"] == SPATIAL).unwrap();
            assert_eq!(read["tool"], "get_full_method_body");
            let prefix = text.split_once("## Revision-admitted change impact").unwrap().0;
            assert!(prefix.contains("## Next indexed source read"));
            assert!(prefix.contains("Action unexecuted") && prefix.contains("unknown branches remain below"));
            let prominent:Value=serde_json::from_str(prefix.split_once("```json\n").unwrap().1
                .split_once("\n```").unwrap().0).unwrap();
            assert_eq!(prominent,json!({"tool":read["tool"],"arguments":read["arguments"]}),
                "An agent can copy the prominent cross-file action without searching nested route arrays");
            let arguments = &prominent["arguments"];
            assert!(arguments.get("line_start").is_none() && arguments.get("line_end").is_none());
            assert_eq!(arguments["source_ranges"], json!([
                {"line_start":10,"line_end":30}, {"line_start":38,"line_end":136}
            ]), "Keep both original envelopes, including context between Core and ResolvePolicy");
            let windows = arguments["source_ranges"].as_array().unwrap();
            assert!(windows.len() <= 3);
            assert!(windows.windows(2).all(|pair|
                pair[0]["line_end"].as_u64().unwrap() < pair[1]["line_start"].as_u64().unwrap()));
            assert_eq!(windows.iter().map(|window|
                window["line_end"].as_u64().unwrap() - window["line_start"].as_u64().unwrap() + 1)
                .sum::<u64>(), 120);
            let covered: Vec<_> = read["covered_refs"].as_array().unwrap().iter().map(|reference|
                routes["nodes"].as_array().unwrap().iter()
                    .find(|node| node["ref"] == *reference).unwrap()).collect();
            for expected in [&bridge, &core, &policy] {
                assert!(covered.iter().any(|node| node["node_id"] == expected.as_str()));
            }
            assert!(covered.iter().all(|node| node["generation"] == 3));

            let result = fixture.engram.handle_get_full_method_body(
                serde_json::from_value(arguments.clone()).expect("The returned action must execute without repair")
            ).await.unwrap();
            assert!(!result.is_error.unwrap_or(false));
            let current: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(current["source_file_hash"], ContentHash::compute(spatial.as_bytes()).0);
            assert_eq!(current["source_verification"], "explicit_ranges_current_file_snapshot");
            assert_eq!(current["total_returned_lines"], 120);
            assert_eq!(current["ranges_complete"], true);
            let returned = current["windows"].as_array().unwrap();
            assert_eq!(returned.len(), 2);
            assert_eq!(returned[0]["source_code"], lines[9..30].join("\n"));
            assert_eq!(returned[1]["source_code"], lines[37..136].join("\n"));
            assert!(returned[1]["source_code"].as_str().unwrap()
                .contains("retain this context between the original declarations"));
        }
    });
}

#[test]
fn public_impact_reaches_the_viewport_endpoint_with_every_real_hop_and_honors_requested_depth() {
    owned_case(|fixture| async move {
        let text = impact(&fixture, None).await;
        let report = selected_impact(&text);
        assert_eq!(report["source_generation"], 2);
        assert_eq!(report["requested_max_depth"], 6);
        let chain = [
            id(POLICY, "ClassifyAutomatic", 1),
            id(POLICY, "Resolve", 2),
            id(SPATIAL, "ResolvePolicy", 1),
            id(SPATIAL, "QueryCacheCore", 2),
            id(SPATIAL, "QueryViewport", 3),
            id(HTTP, "GetFileLayerViewport", 1),
        ];
        assert_eq!(report["dependents"].as_array().unwrap().len(), 5);
        for (depth, node_id) in chain.iter().enumerate().skip(1) {
            let row = dependent(&report, node_id);
            assert_eq!(row["depth"], depth);
            assert_eq!(
                row["path"],
                json!(&chain[..=depth]),
                "A downstream witness must preserve each actual intermediate, not invent a direct root call"
            );
        }
        let endpoint = dependent(&report, chain.last().unwrap());
        assert_eq!(endpoint["file_path"], HTTP);
        assert_eq!(endpoint["start_line"], 1);
        assert_eq!(
            endpoint["generation"], 1,
            "An inherited physical source revision remains valid under logical head 2"
        );

        let blast = fixture
            .engram
            .handle_compute_blast_radius(
                serde_json::from_value(json!({"project_id":PID,
                    "symbol_fqn":chain[0],"include_guidance":false}))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!blast.is_error.unwrap_or(false));
        let blast_text: String = blast
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .map(|content| content.text.as_str())
            .collect();
        let blast_report = selected_impact(&blast_text);
        assert_eq!(
            dependent(&blast_report, chain.last().unwrap())["path"],
            endpoint["path"],
            "The advisory blast score must expose the same actual downstream route as impact analysis"
        );

        let file_result = fixture
            .engram
            .handle_impact_analysis(
                serde_json::from_value(json!({"project_id":PID,"file_path":POLICY,"limit":50}))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!file_result.is_error.unwrap_or(false));
        let file_text: String = file_result
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .map(|content| content.text.as_str())
            .collect();
        let file_report = selected_impact(&file_text);
        let file_endpoint = dependent(&file_report, chain.last().unwrap());
        assert_eq!(
            file_endpoint["path"],
            json!(&chain[1..]),
            "File impact starts at a contained declaration, not an invented causal file-to-function hop"
        );
        for row in file_report["dependents"].as_array().unwrap() {
            assert_eq!(row["seed_node_id"], row["path"][0]);
            assert_eq!(
                row["seed_relationship"],
                "contained_declaration_in_requested_file; structural_origin_not_a_causal_edge"
            );
            assert_ne!(row["path"][0], format!("file:{POLICY}"));
        }

        let direct_only = selected_impact(&impact(&fixture, Some(1)).await);
        assert_eq!(direct_only["requested_max_depth"], 1);
        let direct = direct_only["dependents"].as_array().unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0]["node_id"], chain[1]);
        assert!(!direct.iter().any(|row| row["node_id"] == chain[5]));
        assert!(
            direct_only["coverage"]["reasons"]
                .to_string()
                .to_lowercase()
                .contains("depth"),
            "Stopping at a requested depth must disclose the unexamined downstream boundary"
        );
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let summary = impact_packet(&summary_impact(&fixture, tool, &chain[0], 1).await);
            assert_eq!(summary["source_generation"], 2);
            assert_eq!(summary["requested_max_depth"], 1);
            assert_eq!(summary["selected_dependents_observed"], 1);
            assert_eq!(summary["coverage"], direct_only["coverage"]);
            let boundary = summary["frontier"].as_array().unwrap().iter()
                .find(|branch| branch["node_id"] == chain[2])
                .expect("A direct-only summary must name ResolvePolicy beyond the admitted Resolve caller");
            assert_eq!(boundary["parent_node_id"], chain[1]);
            assert_eq!(boundary["reason"], "depth_limit");
            assert_eq!(boundary["depth"], 2);
            assert!(boundary["recovery"]["tool"].is_string());
            assert!(boundary["recovery"]["arguments"].is_object());
            let graph = expand_summary_graph(&fixture, &summary, &chain[0], 1).await;
            assert_eq!(graph["coverage"], direct_only["coverage"]);
            assert_eq!(graph["selected_dependents_observed"], 1);
        }
    });
}

#[test]
fn historical_cochange_and_declaration_containment_do_not_become_transitive_causal_paths() {
    owned_case(|fixture| async move {
        let root = id(POLICY, "ClassifyAutomatic", 1);
        let historical_id = id(COMPANION, "HistoricCompanion", 1);
        let mut historical = edge(
            historical_id.clone(),
            root.clone(),
            EdgeKind::TemporalCoupling,
            1,
        );
        historical.weight = 10_000;
        let mut contains = edge(format!("file:{POLICY}"), root, EdgeKind::Contains, 2);
        contains.weight = 10_000;
        fixture
            .engram
            .state
            .graph
            .upsert_edges(PID, &[historical, contains])
            .unwrap();
        let text = impact(&fixture, None).await;
        assert!(
            text.contains("HistoricCompanion"),
            "Historical companions should remain visible as separately classified evidence"
        );
        let report = selected_impact(&text);
        let causal = report["dependents"].as_array().unwrap();
        assert_eq!(causal.len(), 5);
        assert!(
            causal
                .iter()
                .all(|row| row["node_id"] != historical_id
                    && row["node_id"] != format!("file:{POLICY}")),
            "A high-weight historical/structural edge is not a source-level causal path"
        );
        for row in causal {
            assert!(
                !row["path"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|step| step == &json!(historical_id))
            );
        }
    });
}

#[test]
fn cycles_terminate_and_stale_future_missing_or_unbound_edges_never_claim_current_causal_paths() {
    owned_case(|fixture| async move {
        let cancel = CancellationToken::new();
        let future_spatial = format!(
            "{SPATIAL_BODY}pub fn FutureCaller() -> i32 {{ crate::policy::ClassifyAutomatic() }}\n"
        );
        fixture
            .search
            .prepare_source_snapshot(
                PID,
                2,
                3,
                &[RelPath::new(SPATIAL), RelPath::new(FUTURE_FILE)],
                &[],
                &cancel,
            )
            .await
            .unwrap();
        fixture
            .search
            .index_docs(
                PID,
                &[
                    source_doc(3, 30, SPATIAL, &future_spatial),
                    source_doc(3, 31, FUTURE_FILE, FUTURE_BODY),
                ],
                &cancel,
            )
            .await
            .unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        // The future candidate was genuinely materialized but never published.
        let nodes = [
            declaration(SPATIAL, "FutureCaller", 4, 3),
            declaration(POLICY, "ObsoletePolicyCaller", 3, 1),
            declaration(UNBOUND, "MissingFileBinding", 1, 1),
            declaration(FUTURE_FILE, "FutureFileBinding", 1, 1),
            file_node(FUTURE_FILE, FUTURE_BODY, 3),
            declaration(COMPANION, "BadEdgeOnly", 3, 1),
            declaration(COMPANION, "ValidAfterUnknown", 4, 1),
        ];
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(PID, &nodes)
            .unwrap();
        let target = id(POLICY, "ClassifyAutomatic", 1);
        let future = id(SPATIAL, "FutureCaller", 4);
        let obsolete = id(POLICY, "ObsoletePolicyCaller", 3);
        let missing_file = id(UNBOUND, "MissingFileBinding", 1);
        let future_file = id(FUTURE_FILE, "FutureFileBinding", 1);
        let bad_edge = id(COMPANION, "BadEdgeOnly", 3);
        let recovered = id(COMPANION, "ValidAfterUnknown", 4);
        let edges = [
            edge(future.clone(), target.clone(), EdgeKind::Calls, 3),
            edge(obsolete.clone(), target.clone(), EdgeKind::Calls, 1),
            edge(missing_file.clone(), target.clone(), EdgeKind::Calls, 1),
            edge(future_file.clone(), target.clone(), EdgeKind::Calls, 1),
            edge(
                "sym:function:src/deleted.rs:DanglingCaller:1".into(),
                target.clone(),
                EdgeKind::Calls,
                1,
            ),
            edge(bad_edge.clone(), target.clone(), EdgeKind::ApiCall, 3),
            edge(recovered.clone(), target.clone(), EdgeKind::ApiCall, 3),
            edge(recovered.clone(), target, EdgeKind::Calls, 1),
        ];
        fixture
            .engram
            .state
            .graph
            .upsert_edges(PID, &edges)
            .unwrap();

        let first = selected_impact(&impact(&fixture, None).await);
        let second = selected_impact(&impact(&fixture, None).await);
        assert_eq!(
            first["dependents"], second["dependents"],
            "The same cyclic source evidence should yield stable unique witnesses"
        );
        let causal = first["dependents"].as_array().unwrap();
        for rejected in [&future, &obsolete, &missing_file, &future_file, &bad_edge] {
            assert!(
                causal.iter().all(|row| row["node_id"] != rejected.as_str()),
                "An unselected or unverified source must not be confirmed: {rejected}"
            );
        }
        assert!(
            causal.iter().any(|row| row["node_id"] == recovered),
            "An unverified edge must not poison a caller's independent valid edge"
        );
        assert!(
            causal
                .iter()
                .any(|row| row["node_id"] == id(HTTP, "GetFileLayerViewport", 1))
        );
        let unique: std::collections::BTreeSet<_> = causal
            .iter()
            .map(|row| row["node_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            unique.len(),
            causal.len(),
            "A dependency cycle must not duplicate dependents"
        );
        assert!(first["coverage"]["non_selected_nodes"].as_u64().unwrap() >= 2);
        assert!(first["coverage"]["unknown_nodes"].as_u64().unwrap() >= 1);
        assert!(first["coverage"]["missing_nodes"].as_u64().unwrap() >= 1);
        assert!(
            first["coverage"]["reasons"]
                .to_string()
                .contains("unverified_edge_source_revision")
        );
        let unselected_target = impact_target(&fixture, &future, None).await;
        let unavailable = impact_packet(&unselected_target);
        assert_eq!(unavailable["status"], "unavailable");
        assert_eq!(unavailable["target_revision_admitted"], false);
        assert!(unavailable["selected_dependents_observed"].is_null());
        assert!(unselected_target.contains("Admitted indexed dependents: unknown."));
        assert!(unselected_target.contains("Separate potential interface callers: unknown"),
            "An unadmitted root must not advertise zero potential consumers");
        assert!(
            !unselected_target.contains("## First immediate indexed consumer"),
            "An unpublished target cannot acquire a confirmed-consumer prefix"
        );
        for tool in ["impact_analysis", "compute_blast_radius"] {
            for target in [
                future.as_str(),
                "sym:function:src/not_indexed.rs:UnknownRoot:1",
            ] {
                let text = summary_impact(&fixture, tool, target, 6).await;
                let summary = impact_packet(&text);
                assert_eq!(summary["status"], "unavailable");
                assert_eq!(summary["target_revision_admitted"], false);
                for field in [
                    "selected_dependents_observed",
                    "direct_dependents_observed",
                    "transitive_dependents_observed",
                ] {
                    assert!(
                        summary[field].is_null(),
                        "Missing target evidence must remain unknown instead of reporting zero for {field}"
                    );
                }
                assert_eq!(summary["change_breakage_proven"], false);
                assert_eq!(summary["exhaustive_codebase"], false);
                assert!(
                    summary["immediate_consumers"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
                assert!(!text.contains("## First immediate indexed consumer"));
                let graph = expand_summary_graph(&fixture, &summary, target, 6).await;
                assert_eq!(graph["status"], "unavailable");
                assert_eq!(graph["target_revision_admitted"], false);
                assert!(graph["selected_dependents_observed"].is_null());
            }
        }

        for invalid_depth in [0, 9] {
            let request = json!({"project_id":PID,"symbol_fqn":id(POLICY, "ClassifyAutomatic", 1),
                "max_depth":invalid_depth,"limit":50});
            assert!(
                fixture
                    .engram
                    .handle_impact_analysis(serde_json::from_value(request).unwrap())
                    .await
                    .is_err(),
                "Requested traversal bounds must reject unsupported depth {invalid_depth}"
            );
        }
    });
}

#[test]
fn unverified_relationship_producers_do_not_hide_later_valid_caller_evidence() {
    owned_case(|fixture| async move {
        let source = id(COMPANION, "HistoricCompanion", 1);
        let target = id(POLICY, "ClassifyAutomatic", 1);
        for producer in [
            json!({"version":1,"path":"src/missing-producer.vb","generation":1,"owner_class":"Home"}),
            json!({"version":1,"path":FUTURE_FILE,"generation":2,"owner_class":"Home"}),
            json!("unverified extractor string"),
        ] {
            fixture
                .engram
                .state
                .graph
                .delete_edges_of_kind(PID, &EdgeKind::Dependency)
                .unwrap();
            let mut unverified = edge(source.clone(), target.clone(), EdgeKind::Calls, 2);
            unverified.metadata = Some(json!({"_engram_event_producer":producer,
                "_engram_source_origin":{"version":1,"source_id":source,"source_generation":1}}));
            fixture
                .engram
                .state
                .graph
                .upsert_edges(PID, &[unverified])
                .unwrap();
            let report = impact_packet(&impact(&fixture, None).await);
            assert!(
                !report["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["node_id"] == source && n["admission"] == "selected"),
                "Missing, stale or unverified producer evidence cannot establish a causal caller"
            );
            assert!(
                report["frontier"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["reason"] == "unverified_event_producer_revision")
            );
            fixture
                .engram
                .state
                .graph
                .upsert_edges(
                    PID,
                    &[edge(
                        source.clone(),
                        target.clone(),
                        EdgeKind::Dependency,
                        1,
                    )],
                )
                .unwrap();
            let recovered = impact_packet(&impact(&fixture, None).await);
            assert!(
                recovered["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["node_id"] == source && n["admission"] == "selected"),
                "A rejected relationship must not suppress a later valid relationship from the same caller"
            );
            let connections = graph_connections(&recovered);
            assert!(connections.contains(&(source.clone(), target.clone())));
            assert!(
                !recovered["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["node_id"] == format!("file:{FUTURE_FILE}")
                        && n["admission"] == "selected"),
                "Producer validation does not add unrelated file identities to the consumer set"
            );
        }
    });
}

#[test]
fn moving_the_callee_preserves_an_inherited_call_but_refreshing_away_that_call_removes_its_impact()
{
    owned_case(|fixture| async move {
        let cancel = CancellationToken::new();
        let moved_body =
            format!("// Callee declarations moved without changing the caller.\n\n{POLICY_BODY}");
        std::fs::write(fixture.root.join(POLICY), &moved_body).unwrap();
        let previous = fixture
            .engram
            .state
            .registry
            .capture_source_head(PID)
            .unwrap();
        fixture
            .search
            .prepare_source_snapshot(PID, 2, 3, &[RelPath::new(POLICY)], &[], &cancel)
            .await
            .unwrap();
        fixture
            .search
            .index_docs(PID, &[source_doc(3, 40, POLICY, &moved_body)], &cancel)
            .await
            .unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();

        let moved_root = id(POLICY, "ClassifyAutomatic", 3);
        let moved_resolve = id(POLICY, "Resolve", 4);
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(
                PID,
                &[
                    file_node(POLICY, &moved_body, 3),
                    declaration(POLICY, "ClassifyAutomatic", 3, 3),
                    declaration(POLICY, "Resolve", 4, 3),
                ],
            )
            .unwrap();
        fixture
            .engram
            .state
            .graph
            .upsert_edges(
                PID,
                &[edge(
                    moved_resolve.clone(),
                    moved_root.clone(),
                    EdgeKind::Calls,
                    3,
                )],
            )
            .unwrap();
        let callee_paths = std::collections::HashSet::from([POLICY.to_owned()]);
        fixture
            .engram
            .state
            .graph
            .purge_stale_nodes_for_paths(PID, &callee_paths, 3)
            .unwrap();
        assert!(
            fixture
                .engram
                .state
                .registry
                .publish_source_head(&previous, 3)
                .unwrap()
        );

        let unchanged_caller = id(SPATIAL, "ResolvePolicy", 1);
        let remapped = fixture
            .engram
            .state
            .graph
            .get_edges_by_endpoints(
                PID,
                &[(
                    unchanged_caller.clone(),
                    EdgeKind::Calls,
                    moved_resolve.clone(),
                )],
            )
            .unwrap();
        assert_eq!(
            remapped.len(),
            1,
            "The real scoped remapper must preserve the unchanged caller"
        );
        assert_eq!(
            remapped[0].generation, 3,
            "The producer advances edge cleanup generation while caller source remains physical 1"
        );
        assert_eq!(
            fixture
                .engram
                .state
                .graph
                .get_node(PID, &unchanged_caller)
                .unwrap()
                .unwrap()
                .generation,
            1
        );
        let moved_text = impact_target(&fixture, &moved_root, None).await;
        let compact = impact_packet(&moved_text);
        let caller = compact["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["node_id"] == unchanged_caller)
            .unwrap();
        let callee = compact["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["node_id"] == moved_resolve)
            .unwrap();
        let public_remap = compact["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["source_ref"] == caller["ref"] && edge["target_ref"] == callee["ref"])
            .unwrap();
        assert_eq!(public_remap["edge_generation"], 3);
        assert_eq!(
            public_remap
                .get("source_generation")
                .unwrap_or(&caller["generation"]),
            &json!(1),
            "Compact edge source revision comes from the inherited caller, never published HEAD3"
        );
        assert_eq!(
            public_remap["edge_source_binding"], "verified_unchanged_caller_origin",
            "The remapper's physical-caller proof must remain explicit when edge generations differ"
        );
        let after_move = selected_impact(&moved_text);
        assert_eq!(after_move["source_generation"], 3);
        let endpoint = id(HTTP, "GetFileLayerViewport", 1);
        assert_eq!(
            dependent(&after_move, &endpoint)["path"],
            json!([
                moved_root,
                moved_resolve,
                unchanged_caller,
                id(SPATIAL, "QueryCacheCore", 2),
                id(SPATIAL, "QueryViewport", 3),
                endpoint,
            ])
        );
        assert_eq!(dependent(&after_move, &unchanged_caller)["generation"], 1);

        // A genuine caller refresh removes the call while retaining the same
        // symbol IDs and the remaining viewport/cache relationships.
        let without_policy = "pub fn ResolvePolicy(stop: bool) -> i32 { if stop { 0 } else { QueryCacheCore(true) } }\npub fn QueryCacheCore(stop: bool) -> i32 { ResolvePolicy(stop) }\npub fn QueryViewport() -> i32 { QueryCacheCore(true) }\n";
        std::fs::write(fixture.root.join(SPATIAL), without_policy).unwrap();
        let previous = fixture
            .engram
            .state
            .registry
            .capture_source_head(PID)
            .unwrap();
        fixture
            .search
            .prepare_source_snapshot(PID, 3, 4, &[RelPath::new(SPATIAL)], &[], &cancel)
            .await
            .unwrap();
        fixture
            .search
            .index_docs(PID, &[source_doc(4, 41, SPATIAL, without_policy)], &cancel)
            .await
            .unwrap();
        fixture.search.seal_source_snapshot(PID, 4).unwrap();
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(
                PID,
                &[
                    file_node(SPATIAL, without_policy, 4),
                    declaration(SPATIAL, "ResolvePolicy", 1, 4),
                    declaration(SPATIAL, "QueryCacheCore", 2, 4),
                    declaration(SPATIAL, "QueryViewport", 3, 4),
                ],
            )
            .unwrap();
        fixture
            .engram
            .state
            .graph
            .upsert_edges(
                PID,
                &[
                    edge(
                        id(SPATIAL, "QueryCacheCore", 2),
                        id(SPATIAL, "ResolvePolicy", 1),
                        EdgeKind::Calls,
                        4,
                    ),
                    edge(
                        id(SPATIAL, "ResolvePolicy", 1),
                        id(SPATIAL, "QueryCacheCore", 2),
                        EdgeKind::Calls,
                        4,
                    ),
                    edge(
                        id(SPATIAL, "QueryViewport", 3),
                        id(SPATIAL, "QueryCacheCore", 2),
                        EdgeKind::Calls,
                        4,
                    ),
                ],
            )
            .unwrap();
        let caller_paths = std::collections::HashSet::from([SPATIAL.to_owned()]);
        fixture
            .engram
            .state
            .graph
            .purge_stale_nodes_for_paths(PID, &caller_paths, 4)
            .unwrap();
        assert!(
            fixture
                .engram
                .state
                .registry
                .publish_source_head(&previous, 4)
                .unwrap()
        );

        assert!(
            fixture
                .engram
                .state
                .graph
                .get_edges_by_endpoints(
                    PID,
                    &[(
                        unchanged_caller.clone(),
                        EdgeKind::Calls,
                        moved_resolve.clone()
                    ),]
                )
                .unwrap()
                .is_empty(),
            "The refreshed caller's removed outgoing call cannot survive an origin proof"
        );
        let after_removal = selected_impact(&impact_target(&fixture, &moved_root, None).await);
        assert_eq!(after_removal["source_generation"], 4);
        let dependents = after_removal["dependents"].as_array().unwrap();
        assert!(dependents.iter().any(|row| row["node_id"] == moved_resolve));
        assert!(
            dependents
                .iter()
                .all(|row| row["node_id"] != unchanged_caller && row["node_id"] != endpoint),
            "An unchanged callee must no longer claim the removed route to the HTTP endpoint"
        );
    });
}

#[test]
fn default_impact_display_retains_the_distant_http_route_when_direct_callers_exceed_the_display_cap()
 {
    owned_case(|fixture| async move {
        let cancel = CancellationToken::new();
        const FANOUT: &str = "src/a_direct_fanout.rs";
        let body: String = (0..30)
            .map(|number| {
                format!(
                    "pub fn Direct{number:02}() -> i32 {{ crate::policy::ClassifyAutomatic() }}\n"
                )
            })
            .collect();
        std::fs::write(fixture.root.join(FANOUT), &body).unwrap();
        let previous = fixture
            .engram
            .state
            .registry
            .capture_source_head(PID)
            .unwrap();
        fixture
            .search
            .prepare_source_snapshot(PID, 2, 3, &[RelPath::new(FANOUT)], &[], &cancel)
            .await
            .unwrap();
        fixture
            .search
            .index_docs(PID, &[source_doc(3, 50, FANOUT, &body)], &cancel)
            .await
            .unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        let mut nodes = vec![file_node(FANOUT, &body, 3)];
        let mut edges = Vec::new();
        for number in 0..30 {
            let name = format!("Direct{number:02}");
            let line = number + 1;
            nodes.push(declaration(FANOUT, &name, line, 3));
            edges.push(edge(
                id(FANOUT, &name, line),
                id(POLICY, "ClassifyAutomatic", 1),
                EdgeKind::Calls,
                3,
            ));
        }
        fixture
            .engram
            .state
            .graph
            .upsert_nodes(PID, &nodes)
            .unwrap();
        fixture
            .engram
            .state
            .graph
            .upsert_edges(PID, &edges)
            .unwrap();
        assert!(
            fixture
                .engram
                .state
                .registry
                .publish_source_head(&previous, 3)
                .unwrap()
        );

        // Omit display/depth options: this is the normal question an agent asks,
        // not a special recovery call tuned around the implementation's cap.
        let result = fixture
            .engram
            .handle_impact_analysis(
                serde_json::from_value(json!({
                    "project_id":PID,"symbol_fqn":id(POLICY, "ClassifyAutomatic", 1),
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let text: String = result
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .map(|content| content.text.as_str())
            .collect();
        let report = selected_impact(&text);
        assert_eq!(report["source_generation"], 3);
        assert!(report["omitted_from_display"].as_u64().unwrap() > 0);
        let chain = [
            id(POLICY, "ClassifyAutomatic", 1),
            id(POLICY, "Resolve", 2),
            id(SPATIAL, "ResolvePolicy", 1),
            id(SPATIAL, "QueryCacheCore", 2),
            id(SPATIAL, "QueryViewport", 3),
            id(HTTP, "GetFileLayerViewport", 1),
        ];
        let endpoint = dependent(&report, chain.last().unwrap());
        assert_eq!(endpoint["depth"], 5);
        assert_eq!(
            endpoint["path"],
            json!(chain),
            "Direct-caller volume must not hide the farther endpoint from the ordinary response"
        );
        let displayed_nodes: std::collections::BTreeSet<_> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["node_id"].as_str().unwrap())
            .collect();
        for step in &chain {
            assert!(
                displayed_nodes.contains(step.as_str()),
                "A displayed distant path needs its independently resolvable intermediate: {step}"
            );
        }
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let summary = impact_packet(&summary_impact(&fixture, tool, &chain[0], 6).await);
            assert_eq!(summary["source_generation"], 3);
            assert_eq!(summary["selected_dependents_observed"], 35);
            assert_eq!(summary["direct_dependents_observed"], 31);
            assert_eq!(summary["transitive_dependents_observed"], 4);
            let shown = summary["immediate_consumers"].as_array().unwrap().len();
            let omitted = summary["immediate_consumers_omitted_from_summary"]
                .as_u64()
                .unwrap();
            assert!(shown <= 4 && omitted > 0);
            assert_eq!(
                shown as u64 + omitted,
                31,
                "A compact summary must distinguish its displayed caller sample from all admitted immediate callers"
            );
            let expanded = expand_summary_graph(&fixture, &summary, &chain[0], 6).await;
            assert_eq!(expanded["selected_dependents_observed"], 35);
            assert_eq!(expanded["coverage"], report["coverage"]);
            assert!(
                expanded["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|node| node["node_id"] == *chain.last().unwrap()
                        && node["admission"] == "selected")
            );
        }
    });
}


#[test]
fn missing_identities_offer_working_inspection_candidates_without_repairing_the_graph() {
    owned_case(|fixture| async move {
        let target = id(POLICY, "ClassifyAutomatic", 1);
        let before = impact_packet(&impact(&fixture, Some(6)).await);
        let expected = [(POLICY, "Resolve", 2), (SPATIAL, "ResolvePolicy", 1),
            (SPATIAL, "QueryViewport", 3)];
        let ghosts: Vec<_> = expected.iter().map(|(path, name, line)|
            id(path, &format!("value.{name}"), *line)).collect();
        let edges: Vec<_> = ghosts.iter().flat_map(|ghost| [
            edge(ghost.clone(), target.clone(), EdgeKind::Calls, 2),
            edge(ghost.clone(), target.clone(), EdgeKind::Dependency, 2),
        ]).collect();
        fixture.engram.state.graph.upsert_edges(PID, &edges).unwrap();
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let summary = impact_packet(&summary_impact(&fixture, tool, &target, 6).await);
            assert_eq!(summary["coverage"]["missing_nodes"], 6);
            assert!(summary["inspection_candidate_scope"].as_str().unwrap().contains("remain unresolved"));
            let report = expand_summary_graph(&fixture, &summary, &target, 6).await;
            assert_eq!(report["selected_dependents_observed"], before["selected_dependents_observed"]);
            assert_eq!(graph_connections(&report), graph_connections(&before),
                "Inspection leads must not manufacture causal connections");
            for ((path, name, line), ghost) in expected.iter().zip(&ghosts) {
                let node = report["nodes"].as_array().unwrap().iter()
                    .find(|node| node["node_id"] == *ghost).unwrap();
                assert_eq!(node["admission"], "unresolved");
                assert_eq!(report["frontier"].as_array().unwrap().iter()
                    .filter(|row| row["source_ref"] == node["ref"]
                        && row["reason"] == "missing_graph_node").count(), 2);
                let recovery = report["recoveries"].as_array().unwrap().iter()
                    .find(|row| row["ref"] == node["recovery_ref"]).unwrap();
                assert_eq!(recovery["scope_ref"], "inspection_candidate");
                assert_eq!(recovery["arguments"]["symbol_node_id"], id(path, name, *line));
                let navigation = fixture.engram.handle_get_codebase_overview(
                    serde_json::from_value(recovery["arguments"].clone()).unwrap()
                ).await.unwrap();
                let text = &navigation.content[0].as_text().unwrap().text;
                let navigation: Value = serde_json::from_str(text.split("## CodeMap Navigation").nth(1).unwrap()
                    .split("```json\n").nth(1).unwrap().split("```").next().unwrap()).unwrap();
                let row = &navigation["files"][0];
                assert_eq!(row["symbol_selection"]["observed"], true);
                let action = row["symbols"][0]["actions"].as_array().unwrap().iter()
                    .find(|action| action["tool"] == "get_full_method_body").unwrap();
                let body = fixture.engram.handle_get_full_method_body(
                    serde_json::from_value(action["arguments"].clone()).unwrap()
                ).await.unwrap();
                let body: Value = serde_json::from_str(&body.content[0].as_text().unwrap().text).unwrap();
                let source = std::fs::read(fixture.root.join(path)).unwrap();
                assert_eq!(body["source_file_hash"], ContentHash::compute(&source).0);
                assert_eq!(body["source_code"], String::from_utf8(source).unwrap()
                    .lines().nth(*line as usize - 1).unwrap());
            }
        }
    });
}

async fn publish_reset_candidates(fixture: &Fixture, count: usize) -> Vec<Node> {
    let paths = [POLICY, HTTP];
    let mut nodes = Vec::new();
    let cancel = CancellationToken::new();
    let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
    fixture.search.prepare_source_snapshot(PID, 2, 3,
        &paths.map(RelPath::new), &[], &cancel).await.unwrap();
    for (index, path) in paths.iter().enumerate() {
        let names: Vec<_> = (index..count).step_by(2).collect();
        let body = names.iter().map(|n|
            format!("mod area{n:03} {{ pub fn resetState() {{}} }}\n")).collect::<String>();
        std::fs::write(fixture.root.join(path), &body).unwrap();
        fixture.search.index_docs(PID, &[source_doc(3, 900 + index as u64, path, &body)], &cancel)
            .await.unwrap();
        fixture.engram.state.graph.upsert_nodes(PID, &[file_node(path, &body, 3)]).unwrap();
        nodes.extend(names.iter().enumerate().map(|(line, n)|
            declaration(path, &format!("area{n:03}.resetState"), line as u32 + 1, 3)));
    }
    fixture.search.seal_source_snapshot(PID, 3).unwrap();
    fixture.engram.state.graph.upsert_nodes(PID, &nodes).unwrap();
    assert!(fixture.engram.state.registry.publish_source_head(&baseline, 3).unwrap());
    nodes
}

async fn ambiguous_reset(fixture: &Fixture, tool: &str, detail: &str) -> Value {
    let mut args = json!({"project_id":PID,"symbol_fqn":"policy.resetState",
        "max_depth":6,"detail":detail});
    let response = if tool == "compute_blast_radius" {
        args["include_guidance"] = json!(false);
        fixture.engram.handle_compute_blast_radius(serde_json::from_value(args).unwrap()).await
    } else {
        fixture.engram.handle_impact_analysis(serde_json::from_value(args).unwrap()).await
    }.unwrap();
    assert!(!response.is_error.unwrap_or(false));
    let text: String = response.content.iter().filter_map(|part| part.as_text())
        .map(|part| part.text.as_str()).collect();
    assert!(text.len() <= if detail == "summary" { 12 * 1024 } else { 64 * 1024 });
    impact_packet(&text)
}

#[test]
fn ambiguous_names_offer_executable_exact_target_and_source_choices_without_selecting_one() {
    owned_case(|fixture| async move {
        let nodes = publish_reset_candidates(&fixture, 2).await;
        let before_nodes = serde_json::to_value(fixture.engram.state.graph
            .query_nodes(PID, None, None, None, 1000).unwrap()).unwrap();
        let before_edges = serde_json::to_value(fixture.engram.state.graph.list_edges(PID, None).unwrap()).unwrap();
        for tool in ["impact_analysis", "compute_blast_radius"] {
            for detail in ["summary", "graph"] {
                let report = ambiguous_reset(&fixture, tool, detail).await;
                assert_eq!(report["source_generation"], 3);
                assert_eq!(report["status"], "unavailable");
                assert_eq!(report["target_revision_admitted"], false);
                assert!(report["selected_dependents_observed"].is_null());
                assert!(report["graph_continuation"].is_null(), "Do not repeat the unresolved request");
                let resolution = &report["target_resolution"];
                assert_eq!(resolution["status"], "ambiguous");
                assert_eq!(resolution["selection_required"], true);
                let choices = resolution["candidates"].as_array().unwrap();
                assert_eq!(choices.len(), 2);
                for node in &nodes {
                    let choice = choices.iter().find(|row| row["node_id"] == node.node_id).unwrap();
                    let body = fixture.engram.handle_get_full_method_body(serde_json::from_value(
                        choice["source_read"]["arguments"].clone()).unwrap()).await.unwrap();
                    assert!(!body.is_error.unwrap_or(false));
                    let body: Value = serde_json::from_str(&body.content[0].as_text().unwrap().text).unwrap();
                    let raw = std::fs::read(fixture.root.join(node.file_path.as_str())).unwrap();
                    assert_eq!(body["source_file_hash"], ContentHash::compute(&raw).0);
                    assert_eq!(body["source_code"], String::from_utf8(raw).unwrap().trim_end());
                    assert_eq!(choice["impact"]["arguments"]["symbol_fqn"], node.node_id);
                    let exact = fixture.engram.handle_impact_analysis(serde_json::from_value(
                        choice["impact"]["arguments"].clone()).unwrap()).await.unwrap();
                    assert!(!exact.is_error.unwrap_or(false));
                    let exact = impact_packet(&exact.content[0].as_text().unwrap().text);
                    assert_eq!(exact["target_revision_admitted"], true);
                    assert!(exact["nodes"].as_array().unwrap().iter().any(|row|
                        row["node_id"] == node.node_id && row["is_seed"] == true));
                }
            }
        }
        assert_eq!(serde_json::to_value(fixture.engram.state.graph
            .query_nodes(PID, None, None, None, 1000).unwrap()).unwrap(), before_nodes);
        assert_eq!(serde_json::to_value(fixture.engram.state.graph.list_edges(PID, None).unwrap()).unwrap(), before_edges);
    });
}

#[test]
fn ambiguity_remains_when_only_one_candidate_has_current_bounded_source_evidence() {
    for scenario in ["stale", "future", "other_namespace", "oversized_file"] {
        owned_case(move |fixture| async move {
            let nodes = publish_reset_candidates(&fixture, 2).await;
            let mut other = nodes.iter().find(|node| node.file_path.as_str() == HTTP).unwrap().clone();
            match scenario {
                "stale" => other.generation = 2,
                "future" => other.generation = 4,
                "other_namespace" => other.namespace = "history".into(),
                _ => {
                    let mut file = fixture.engram.state.graph.get_node(PID, &format!("file:{HTTP}"))
                        .unwrap().unwrap();
                    file.metadata = Some(json!({"oversized": "x".repeat(65 * 1024)}));
                    fixture.engram.state.graph.upsert_nodes(PID, &[file]).unwrap();
                }
            }
            fixture.engram.state.graph.upsert_nodes(PID, &[other]).unwrap();
            for tool in ["impact_analysis", "compute_blast_radius"] {
                let report = ambiguous_reset(&fixture, tool, "summary").await;
                assert_eq!(report["status"], "unavailable", "{scenario}");
                assert_eq!(report["target_revision_admitted"], false);
                assert!(report["selected_dependents_observed"].is_null());
                let resolution = &report["target_resolution"];
                assert_eq!(resolution["selection_required"], true);
                assert_eq!(resolution["candidates"].as_array().unwrap().len(), 1, "{scenario}");
                assert_eq!(resolution["candidates"][0]["file_path"], POLICY);
                let coverage = &resolution["coverage"];
                assert_eq!(coverage["resolver_candidates_observed"], 2);
                assert_eq!(coverage["selected_candidates_observed"], 1);
                assert_eq!(coverage["resolver_completeness"], "unknown");
            }
        });
    }
}

#[test]
fn crowded_ambiguity_discloses_uninspected_and_omitted_choices_with_bounded_recovery() {
    owned_case(|fixture| async move {
        publish_reset_candidates(&fixture, 80).await;
        for tool in ["impact_analysis", "compute_blast_radius"] {
            for detail in ["summary", "graph"] {
                let report = ambiguous_reset(&fixture, tool, detail).await;
                let resolution = &report["target_resolution"];
                assert!(serde_json::to_vec(resolution).unwrap().len() <= 6 * 1024);
                let coverage = &resolution["coverage"];
                assert_eq!(coverage["resolver_candidates_observed"], 80);
                assert_eq!(coverage["candidates_inspected"], 64);
                assert_eq!(coverage["candidates_not_inspected"], 16);
                let displayed = resolution["candidates"].as_array().unwrap().len();
                assert!((1..=8).contains(&displayed));
                assert_eq!(coverage["selected_candidates_observed"], 64);
                assert_eq!(coverage["selected_candidates_omitted"].as_u64().unwrap() as usize,
                    64 - displayed);
                assert!(report["selected_dependents_observed"].is_null());
            }
        }
    });
}

#[test]
fn unresolved_receiver_navigation_opens_its_enclosing_declaration_without_resolving_it() {
    owned_case(|fixture| async move {
        // The enclosing method lies beyond the ordinary first declaration page.
        let mut lines: Vec<_> = (0..18).map(|i| format!("pub fn A{i:02}() {{}}" )).collect();
        lines.resize(24, String::new());
        lines.push("pub fn Page_Load() {".into());
        lines.push("    crate::policy::ClassifyAutomatic();".into());
        lines.resize(41, String::new());
        lines.push("}".into());
        let body = lines.join("\n") + "\n";
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        fixture.search.prepare_source_snapshot(PID, 2, 3, &[RelPath::new(SPATIAL)], &[], &cancel).await.unwrap();
        fixture.search.index_docs(PID, &[source_doc(3, 400, SPATIAL, &body)], &cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        std::fs::write(fixture.root.join(SPATIAL), &body).unwrap();
        let mut nodes = vec![file_node(SPATIAL, &body, 3)];
        nodes.extend((0..18).map(|i| declaration(SPATIAL, &format!("A{i:02}"), i+1, 3)));
        let mut parent = declaration(SPATIAL, "Page_Load", 25, 3);
        parent.end_line = 42;
        nodes.push(parent.clone());
        fixture.engram.state.graph.upsert_nodes(PID, &nodes).unwrap();
        let target = id(POLICY, "ClassifyAutomatic", 1);
        fixture.engram.state.graph.upsert_edges(PID, &[
            edge(parent.node_id.clone(), target.clone(), EdgeKind::Calls, 3)
        ]).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline, 3).unwrap());
        let before = impact_packet(&impact(&fixture, Some(6)).await);
        let receiver = format!("sym:self:{SPATIAL}:Me:25");
        fixture.engram.state.graph.upsert_edges(PID, &[
            edge(receiver.clone(), parent.node_id.clone(), EdgeKind::Calls, 3)
        ]).unwrap();
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let text = summary_impact(&fixture, tool, &target, 6).await;
            assert!(text.len() <= 12*1024);
            let summary = impact_packet(&text);
            assert!(summary["enclosing_declaration_scope"].as_str().unwrap().contains("unresolved"));
            let report = expand_summary_graph(&fixture, &summary, &target, 6).await;
            assert_eq!(report["selected_dependents_observed"], before["selected_dependents_observed"]);
            assert_eq!(graph_connections(&report), graph_connections(&before));
            let missing = report["nodes"].as_array().unwrap().iter().find(|node| node["node_id"] == receiver).unwrap();
            assert_eq!(missing["admission"], "unresolved");
            assert!(report["frontier"].as_array().unwrap().iter().any(|row|
                row["source_ref"] == missing["ref"] && row["reason"] == "missing_graph_node"));
            let recovery = report["recoveries"].as_array().unwrap().iter().find(|row| row["ref"] == missing["recovery_ref"]).unwrap();
            assert_eq!(recovery["scope_ref"], "enclosing_declaration");
            assert_eq!(recovery["arguments"]["symbol_node_id"], parent.node_id);
            let navigation = fixture.engram.handle_get_codebase_overview(
                serde_json::from_value(recovery["arguments"].clone()).unwrap()
            ).await.unwrap();
            let text = &navigation.content[0].as_text().unwrap().text;
            let navigation: Value = serde_json::from_str(text.split("## CodeMap Navigation").nth(1).unwrap()
                .split("```json\n").nth(1).unwrap().split("```").next().unwrap()).unwrap();
            let row = &navigation["files"][0];
            assert_eq!(row["symbol_selection"]["observed"], true);
            assert_eq!(row["symbols"].as_array().unwrap().len(), 1);
            let action = row["symbols"][0]["actions"].as_array().unwrap().iter()
                .find(|action| action["tool"] == "get_full_method_body").unwrap();
            let source = fixture.engram.handle_get_full_method_body(
                serde_json::from_value(action["arguments"].clone()).unwrap()
            ).await.unwrap();
            let source: Value = serde_json::from_str(&source.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(source["source_file_hash"], ContentHash::compute(body.as_bytes()).0);
            assert_eq!(source["source_code"], lines[24..42].join("\n"));
        }
    });
}

#[test]
fn receiver_context_stays_generic_when_enclosing_evidence_is_ambiguous_or_unadmitted() {
    for scenario in ["ambiguous", "stale", "future", "wrong_file", "wrong_line", "malformed"] {
        owned_case(move |fixture| async move {
            let target = id(POLICY, "ClassifyAutomatic", 1);
            if scenario == "ambiguous" {
                let other = declaration(SPATIAL, "Other.ResolvePolicy", 1, 1);
                fixture.engram.state.graph.upsert_nodes(PID, &[other.clone()]).unwrap();
                fixture.engram.state.graph.upsert_edges(PID, &[
                    edge(other.node_id, target.clone(), EdgeKind::Calls, 1)
                ]).unwrap();
            } else if scenario == "stale" || scenario == "future" {
                let candidate = declaration(SPATIAL, "ResolvePolicy", 1, if scenario == "stale" { 0 } else { 3 });
                fixture.engram.state.graph.upsert_nodes(PID, &[candidate]).unwrap();
            }
            let receiver = match scenario {
                "wrong_file" => format!("sym:self:{UNBOUND}:Me:1"),
                "wrong_line" => format!("sym:self:{SPATIAL}:Me:99"),
                "malformed" => format!("sym:self:{SPATIAL}:Me:not-a-line"),
                _ => format!("sym:self:{SPATIAL}:Me:1"),
            };
            fixture.engram.state.graph.upsert_edges(PID, &[
                edge(receiver.clone(), target, EdgeKind::Calls, 1)
            ]).unwrap();
            let report = impact_packet(&impact(&fixture, Some(6)).await);
            let missing = report["nodes"].as_array().unwrap().iter().find(|node| node["node_id"] == receiver).unwrap();
            assert_eq!(missing["admission"], "unresolved", "{scenario}");
            let recovery = report["recoveries"].as_array().unwrap().iter().find(|row| row["ref"] == missing["recovery_ref"]).unwrap();
            assert_eq!(recovery["scope_ref"], "navigation", "{scenario}");
            assert!(recovery["arguments"].get("symbol_node_id").is_none(), "{scenario}");
        });
    }
}

#[test]
fn ambiguous_or_unadmitted_declarations_do_not_become_missing_identity_candidates() {
    for scenario in ["ambiguous", "stale", "future", "wrong_file", "wrong_line", "malformed"] {
        owned_case(move |fixture| async move {
            let target = id(POLICY, "ClassifyAutomatic", 1);
            if scenario == "ambiguous" {
                let alternative = declaration(POLICY, "Other.Resolve", 2, 2);
                fixture.engram.state.graph.upsert_nodes(PID, &[alternative.clone()]).unwrap();
                fixture.engram.state.graph.upsert_edges(PID, &[
                    edge(alternative.node_id, target.clone(), EdgeKind::Calls, 2)
                ]).unwrap();
            } else if scenario == "stale" || scenario == "future" {
                let candidate = declaration(POLICY, "Resolve", 2, if scenario == "stale" { 1 } else { 3 });
                fixture.engram.state.graph.upsert_nodes(PID, &[candidate]).unwrap();
            }
            let ghost = match scenario {
                "wrong_file" => id(HTTP, "value.Resolve", 2),
                "wrong_line" => id(POLICY, "value.Resolve", 1),
                "malformed" => format!("sym:function:{POLICY}:value.Resolve:not-a-line"),
                _ => id(POLICY, "value.Resolve", 2),
            };
            fixture.engram.state.graph.upsert_edges(PID, &[
                edge(ghost.clone(), target, EdgeKind::Calls, 2)
            ]).unwrap();
            let report = impact_packet(&impact(&fixture, Some(6)).await);
            let node = report["nodes"].as_array().unwrap().iter()
                .find(|node| node["node_id"] == ghost).unwrap();
            assert_eq!(node["admission"], "unresolved", "{scenario}");
            let recovery = report["recoveries"].as_array().unwrap().iter()
                .find(|row| row["ref"] == node["recovery_ref"]).unwrap();
            assert_eq!(recovery["scope_ref"], "navigation", "{scenario}");
            assert!(recovery["arguments"].get("symbol_node_id").is_none(), "{scenario}");
        });
    }
}


#[test]
fn file_impact_omits_only_unused_inventory_and_recovers_it_through_codemap() {
    owned_case(|fixture| async move {
        const SEEDS: &str="src/seeded.rs";
        const READER: &str="src/reader.rs";
        let mut lines: Vec<_>=(0..24).map(|n|format!("pub fn Unused{n:02}() {{}}" )).collect();
        lines.push("pub fn Used() -> i32 { 1 }".into());
        lines.push("pub fn FrontierOnly() {}".into());
        let source=lines.join("\n")+"\n";
        let reader="pub fn ReadSeed() -> i32 { crate::seeded::Used() }\n";
        let cancel=CancellationToken::new();
        let baseline=fixture.engram.state.registry.capture_source_head(PID).unwrap();
        fixture.search.prepare_source_snapshot(PID,2,3,&[RelPath::new(SEEDS),RelPath::new(READER)],&[],&cancel).await.unwrap();
        fixture.search.index_docs(PID,&[source_doc(3,500,SEEDS,&source),source_doc(3,501,READER,reader)],&cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID,3).unwrap();
        std::fs::write(fixture.root.join(SEEDS),&source).unwrap();
        std::fs::write(fixture.root.join(READER),reader).unwrap();
        let mut nodes=vec![file_node(SEEDS,&source,3),file_node(READER,reader,3),
            declaration(SEEDS,"Used",25,3),declaration(SEEDS,"FrontierOnly",26,3),declaration(READER,"ReadSeed",1,3)];
        nodes.extend((0..24).map(|n|declaration(SEEDS,&format!("Unused{n:02}"),n+1,3)));
        fixture.engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        let unknown=id("src/unrecorded.rs","MissingCaller",1);
        fixture.engram.state.graph.upsert_edges(PID,&[
            edge(id(READER,"ReadSeed",1),id(SEEDS,"Used",25),EdgeKind::Calls,3),
            edge(id(READER,"ReadSeed",1),id(SEEDS,"Used",25),EdgeKind::Dependency,3),
            edge(unknown.clone(),id(SEEDS,"FrontierOnly",26),EdgeKind::Calls,3),
        ]).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline,3).unwrap());
        let target=format!("file:{SEEDS}");
        for tool in ["impact_analysis","compute_blast_radius"] {
            let summary=impact_packet(&summary_impact(&fixture,tool,&target,6).await);
            let graph=expand_summary_graph(&fixture,&summary,&target,6).await;
            assert_eq!(graph["selected_identities_observed"],28);
            assert_eq!(graph["selected_identities_returned"],4);
            assert_eq!(graph["selected_dependent_identities_returned"],1);
            assert_eq!(graph["selected_dependents_observed"],1);
            assert_eq!(graph["node_identities_omitted"],24);
            assert_eq!(graph["unconnected_file_seeds_omitted"],24);
            assert_eq!(summary["graph_omissions"]["unconnected_file_seeds"],24);
            assert_eq!(summary["graph_omissions"]["node_identities_omitted"],24);
            assert_eq!(graph["display_truncated"],true);
            assert_eq!(graph["edges_omitted"],0);
            assert_eq!(graph["frontier_rows_omitted"],0);
            assert_eq!(graph["shortest_paths_omitted"],0);
            assert_eq!(graph["edges"].as_array().unwrap().len(),2);
            assert_eq!(graph["shortest_paths"].as_array().unwrap().len(),1);
            let by_ref: std::collections::BTreeMap<_,_>=graph["nodes"].as_array().unwrap().iter()
                .map(|node|(node["ref"].as_str().unwrap(),node)).collect();
            assert!(!by_ref.values().any(|node|node["node_id"].as_str().unwrap().contains("Unused")));
            assert!(graph["frontier"].as_array().unwrap().iter().any(|lead|
                by_ref[lead["source_ref"].as_str().unwrap()]["node_id"]==unknown
                    && by_ref[lead["target_ref"].as_str().unwrap()]["node_id"]==id(SEEDS,"FrontierOnly",26)));
            for edge in graph["edges"].as_array().unwrap() {
                assert_eq!(by_ref[edge["source_ref"].as_str().unwrap()]["node_id"],id(READER,"ReadSeed",1));
                assert_eq!(by_ref[edge["target_ref"].as_str().unwrap()]["node_id"],id(SEEDS,"Used",25));
            }
            let recovery_refs: std::collections::BTreeSet<_>=graph["recoveries"].as_array().unwrap().iter()
                .map(|row|row["ref"].as_str().unwrap()).collect();
            assert!(by_ref.values().all(|node|recovery_refs.contains(node["recovery_ref"].as_str().unwrap())));
            assert!(!graph["recoveries"].as_array().unwrap().iter().any(|row|
                row["tool"]=="get_full_method_body" && row["arguments"]["file_path"]==SEEDS
                    && row["arguments"]["line_start"]==row["arguments"]["line_end"]
                    && row["arguments"]["line_start"].as_u64().is_some_and(|line|(1..=24).contains(&line))),
                "Unused declarations' private range actions must not consume the reduced graph response");
            let inventory=&graph["declaration_inventory"];
            assert_eq!(inventory["tool"],"get_codebase_overview");
            let mut action=inventory.clone();
            let mut found=std::collections::BTreeSet::new();
            let mut recovered=false;
            for _ in 0..4 {
                let response=fixture.engram.handle_get_codebase_overview(serde_json::from_value(action["arguments"].clone()).unwrap()).await.unwrap();
                let text=&response.content[0].as_text().unwrap().text;
                let page: Value=serde_json::from_str(text.split("## CodeMap Navigation").nth(1).unwrap()
                    .split("```json\n").nth(1).unwrap().split("```").next().unwrap()).unwrap();
                assert_eq!(page["source_generation"],3);
                let file=&page["files"][0];
                assert_eq!(file["path"],SEEDS);
                for symbol in file["symbols"].as_array().unwrap() {
                    found.insert(symbol["name"].as_str().unwrap().to_owned());
                    if symbol["name"]=="Unused23" {
                        let read=&symbol["actions"][0];
                        let result=fixture.engram.handle_get_full_method_body(serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
                        let body: Value=serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
                        assert_eq!(body["source_file_hash"],ContentHash::compute(source.as_bytes()).0);
                        assert_eq!(body["source_code"],lines[23]);recovered=true;
                    }
                }
                if file["next_symbols"].is_null() { break; }
                action=file["next_symbols"].clone();
            }
            assert!(recovered && (0..24).all(|n|found.contains(&format!("Unused{n:02}"))),
                "Display-only omission must leave the full declaration inventory discoverable through its literal paging actions");
        }
        let symbol=impact_packet(&impact_target(&fixture,&id(SEEDS,"Unused23",24),Some(6)).await);
        assert_eq!(symbol["unconnected_file_seeds_omitted"],0);
        assert_eq!(symbol["node_identities_omitted"],0);
        assert!(symbol["declaration_inventory"].is_null());
    });
}

#[test]
fn file_impact_summary_exposes_distinct_consumer_files_and_actual_member_connections() {
    owned_case(|fixture| async move {
        const WIDGET: &str = "src/widget.rs";
        const LONG: &str = "src/aaaa.rs";
        let long_name="A".repeat(1950);
        let long_body=format!("pub fn {long_name}() {{ crate::policy::ClassifyAutomatic(); }}\n");
        let companion = "pub fn HistoricCompanion() { crate::policy::ClassifyAutomatic(); }\npub struct StructuralOnly;\npub fn BadEdgeOnly() { crate::policy::ClassifyAutomatic(); }\npub fn ValidAfterUnknown() { crate::policy::ClassifyAutomatic(); }\n";
        let widget = "pub fn WidgetRead() { crate::policy::Resolve(); }\n";
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        fixture.search.prepare_source_snapshot(PID, 2, 3,
            &[RelPath::new(COMPANION), RelPath::new(WIDGET), RelPath::new(LONG)], &[], &cancel).await.unwrap();
        fixture.search.index_docs(PID, &[
            source_doc(3, 300, COMPANION, companion), source_doc(3, 301, WIDGET, widget), source_doc(3,302,LONG,&long_body)
        ], &cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        std::fs::write(fixture.root.join(COMPANION), companion).unwrap();
        std::fs::write(fixture.root.join(WIDGET), widget).unwrap();
        std::fs::write(fixture.root.join(LONG), &long_body).unwrap();
        fixture.engram.state.graph.upsert_nodes(PID, &[
            file_node(COMPANION, companion, 3), file_node(WIDGET, widget, 3), file_node(LONG,&long_body,3),
            declaration(LONG,&long_name,1,3),
            declaration(COMPANION, "HistoricCompanion", 1, 3),
            declaration(COMPANION, "BadEdgeOnly", 3, 3),
            declaration(COMPANION, "ValidAfterUnknown", 4, 3),
            declaration(WIDGET, "WidgetRead", 1, 3),
        ]).unwrap();
        let mut edges: Vec<_> = [("HistoricCompanion",1),("BadEdgeOnly",3),("ValidAfterUnknown",4)]
            .into_iter().map(|(name,line)| {
                let mut edge = edge(id(COMPANION,name,line),id(POLICY,"ClassifyAutomatic",1),EdgeKind::Calls,3);
                edge.metadata=Some(json!({"call_site_line":line}));edge
            }).collect();
        let mut widget_edge = edge(id(WIDGET,"WidgetRead",1),id(POLICY,"Resolve",2),EdgeKind::Calls,3);
        widget_edge.metadata=Some(json!({"call_site_line":1}));edges.push(widget_edge);
        edges.push(edge(id(LONG,&long_name,1),id(POLICY,"ClassifyAutomatic",1),EdgeKind::Calls,3));
        fixture.engram.state.graph.upsert_edges(PID,&edges).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline,3).unwrap());
        let target=format!("file:{POLICY}");
        let full=impact_packet(&impact_target(&fixture,&target,Some(6)).await);
        let by_ref:std::collections::BTreeMap<_,_>=full["nodes"].as_array().unwrap().iter()
            .map(|node|(node["ref"].as_str().unwrap(),node)).collect();
        for tool in ["impact_analysis","compute_blast_radius"] {
            let summary=impact_packet(&summary_impact(&fixture,tool,&target,6).await);
            assert_eq!(summary["selected_dependents_observed"],full["selected_dependents_observed"]);
            assert_eq!(summary["coverage"],full["coverage"]);
            if let Some(routes) = summary.get("source_routes") {
                assert!(routes["seed_scope"].as_str().unwrap().contains("structural seeds"));
                assert!(routes["connections"].as_array().unwrap().iter().all(|edge|
                    edge["source_ref"] != routes["requested_target_ref"]),
                    "Callable file members are structural origins, not invented outgoing causal file edges");
            }
            let immediate=summary["immediate_consumers"].as_array().unwrap();
            let files:std::collections::BTreeSet<_>=immediate.iter()
                .map(|node|node["file_path"].as_str().unwrap()).collect();
            assert_eq!(files,std::collections::BTreeSet::from([COMPANION,SPATIAL,WIDGET]),
                "Busy or oversized first-file rows must not hide other consumer files that fit the summary");
            assert_eq!(summary["immediate_consumers_omitted_from_summary"].as_u64().unwrap(),
                summary["direct_dependents_observed"].as_u64().unwrap()-immediate.len() as u64);
            for node in immediate {
                let witness=&node["sample_connection_to_target"];
                assert_eq!(witness["target_scope"],"file_member");
                assert_eq!(summary["requested_file_node_id"],target);
                assert_ne!(witness["target_node_id"],target,"Do not fabricate an edge to the file");
                let edge=full["edges"].as_array().unwrap().iter().find(|edge|
                    by_ref[edge["source_ref"].as_str().unwrap()]["node_id"]==node["node_id"]
                    && by_ref[edge["target_ref"].as_str().unwrap()]["node_id"]==witness["target_node_id"]
                    && edge["kind"]==witness["kind"]).unwrap();
                let member=by_ref[edge["target_ref"].as_str().unwrap()];
                assert_eq!(member["admission"],"selected");assert_eq!(member["is_seed"],true);
                assert_eq!(witness["target_name"],member["name"]);
                assert_eq!(witness["target_generation"],member["generation"]);
                assert_eq!(witness["source_line"],edge["source_line"]);
                let action=&node["source_read"];
                let recovered=fixture.engram.handle_get_full_method_body(
                    serde_json::from_value(action["arguments"].clone()).unwrap()
                ).await.unwrap();
                let body:Value=serde_json::from_str(&recovered.content[0].as_text().unwrap().text).unwrap();
                let current=std::fs::read(fixture.root.join(node["file_path"].as_str().unwrap())).unwrap();
                assert_eq!(body["source_file_hash"],ContentHash::compute(&current).0);
                assert!(body["source_code"].as_str().unwrap().contains("crate::policy::"));
            }
            let expanded=expand_summary_graph(&fixture,&summary,&target,6).await;
            assert_eq!(graph_connections(&expanded),graph_connections(&full));
        }
    });
}

#[test]
fn crowded_file_summary_recovers_downstream_source_and_omitted_declarations_without_graph_query() {
    owned_case(|fixture| async move {
        const SEEDS: &str = "src/crowded_seeds.rs";
        let mut seed_lines: Vec<_> = (0..24).map(|n| format!("pub fn Unused{n:02}() {{}}" )).collect();
        seed_lines.push("pub fn Used() -> i32 { 1 }".into());
        let source = seed_lines.join("\n") + "\n";
        let paths: Vec<_> = (0..4).map(|n| format!("src/crowded_consumer_{n}.rs")).collect();
        // Realistic verbose canonical identities make four standalone witness
        // rows compete with the actions needed to continue an agent's task.
        let names: Vec<_> = (0..4).map(|n| format!("Read{n}{}", "Policy".repeat(16))).collect();
        let bodies: Vec<_> = names.iter().map(|name| format!(
            "pub fn {name}() -> i32 {{ crate::crowded_seeds::Used() }}\npub fn Downstream() -> i32 {{ {name}() }}\n"
        )).collect();
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        let revisions: Vec<_> = std::iter::once(RelPath::new(SEEDS))
            .chain(paths.iter().map(|path| RelPath::new(path))).collect();
        fixture.search.prepare_source_snapshot(PID, 2, 3, &revisions, &[], &cancel).await.unwrap();
        let mut docs = vec![source_doc(3, 600, SEEDS, &source)];
        docs.extend(paths.iter().enumerate().map(|(n, path)| source_doc(3, 601+n as u64, path, &bodies[n])));
        fixture.search.index_docs(PID, &docs, &cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        std::fs::write(fixture.root.join(SEEDS), &source).unwrap();
        let mut nodes = vec![file_node(SEEDS, &source, 3), declaration(SEEDS, "Used", 25, 3)];
        nodes.extend((0..24).map(|n| declaration(SEEDS, &format!("Unused{n:02}"), n+1, 3)));
        let mut edges = Vec::new();
        for (n, path) in paths.iter().enumerate() {
            std::fs::write(fixture.root.join(path), &bodies[n]).unwrap();
            nodes.extend([file_node(path, &bodies[n], 3), declaration(path, &names[n], 1, 3),
                declaration(path, "Downstream", 2, 3)]);
            edges.extend([
                edge(id(path, &names[n], 1), id(SEEDS, "Used", 25), EdgeKind::Calls, 3),
                edge(id(path, "Downstream", 2), id(path, &names[n], 1), EdgeKind::Calls, 3),
            ]);
        }
        fixture.engram.state.graph.upsert_nodes(PID, &nodes).unwrap();
        fixture.engram.state.graph.upsert_edges(PID, &edges).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline, 3).unwrap());
        let target = format!("file:{SEEDS}");
        // Independent evidence oracle only: every task continuation below is
        // taken directly from SUMMARY, never from this graph response.
        let graph = impact_packet(&impact_target(&fixture, &target, Some(6)).await);
        assert_eq!(graph["selected_dependents_observed"], 8);
        assert_eq!(graph["unconnected_file_seeds_omitted"], 24);
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let text = summary_impact(&fixture, tool, &target, 6).await;
            assert!(text.len() <= 12 * 1024, "The complete crowded file reply must fit the public budget");
            let report = impact_packet(&text);
            for field in ["source_generation", "target_revision_admitted", "selected_dependents_observed",
                "coverage", "working_tree_checked", "change_breakage_proven"] {
                assert_eq!(report[field], graph[field], "Navigation allocation changed {field}");
            }
            assert_eq!(report["direct_dependents_observed"], 4);
            assert_eq!(report["transitive_dependents_observed"], 4);
            assert_eq!(report["graph_omissions"]["unconnected_file_seeds"], 24);
            assert_eq!(report["verified_connections_returned_in_graph"], graph["edges"].as_array().unwrap().len());
            let immediate = report["immediate_consumers"].as_array().unwrap();
            assert_eq!(immediate.len(), 4, "Source context must preserve the existing four direct witnesses");
            assert_eq!(report["immediate_consumers_omitted_from_summary"], 4-immediate.len());
            let distinct: std::collections::BTreeSet<_> = immediate.iter()
                .map(|row| row["file_path"].as_str().unwrap()).collect();
            assert_eq!(distinct.len(), immediate.len());
            for consumer in immediate {
                let witness = &consumer["sample_connection_to_target"];
                assert_eq!(witness["source_node_id"], consumer["node_id"]);
                assert_eq!(witness["target_node_id"], id(SEEDS, "Used", 25));
                assert_eq!(witness["target_scope"], "file_member");
            }
            let consumer = immediate.iter().find(|row| row["source_read_caller_samples"].as_array()
                .is_some_and(|samples| !samples.is_empty()))
                .expect("Crowded file SUMMARY must expose the admitted caller behind an expanded read");
            let lead = &consumer["source_read_caller_samples"][0];
            let index = paths.iter().position(|path| consumer["file_path"] == *path).unwrap();
            assert_eq!(lead["node_id"], id(&paths[index], "Downstream", 2));
            assert_eq!(lead["generation"], 3);
            assert_eq!(lead["indexed_lines"], json!([2, 2]));
            let graph_nodes: std::collections::BTreeMap<_, _> = graph["nodes"].as_array().unwrap().iter()
                .map(|row| (row["ref"].as_str().unwrap(), &row["node_id"])).collect();
            let edge = graph["edges"].as_array().unwrap().iter().find(|edge|
                graph_nodes[edge["source_ref"].as_str().unwrap()] == &lead["node_id"]
                    && graph_nodes[edge["target_ref"].as_str().unwrap()] == &consumer["node_id"]).unwrap();
            let mut connection = decoded_connection(&graph["omitted_defaults"]["edges"], edge);
            for key in ["ref", "source_ref", "target_ref", "connection_role"] { connection.as_object_mut().unwrap().remove(key); }
            assert_eq!(lead["connection_to_consumer"], connection, "Expanded reads must retain actual admitted edge provenance");
            let read = &consumer["source_read"];
            assert_eq!(read["tool"], "get_full_method_body");
            assert_eq!(read["arguments"]["line_start"], 1);
            assert_eq!(read["arguments"]["line_end"], 2);
            let result = fixture.engram.handle_get_full_method_body(
                serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
            let body: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(body["source_code"], bodies[index].trim_end());
            assert_eq!(body["source_file_hash"], ContentHash::compute(bodies[index].as_bytes()).0);

            let mut action = report["declaration_inventory"].clone();
            assert_eq!(action["tool"], "get_codebase_overview",
                "The agent must recover omitted file members without paying for a graph re-query");
            let mut recovered = false;
            for _ in 0..4 {
                let result = fixture.engram.handle_get_codebase_overview(
                    serde_json::from_value(action["arguments"].clone()).unwrap()).await.unwrap();
                let text = &result.content[0].as_text().unwrap().text;
                let page: Value = serde_json::from_str(text.split("## CodeMap Navigation").nth(1).unwrap()
                    .split("```json\n").nth(1).unwrap().split("```").next().unwrap()).unwrap();
                assert_eq!(page["source_generation"], 3);
                let file = &page["files"][0];
                assert_eq!(file["path"], SEEDS);
                if let Some(symbol) = file["symbols"].as_array().unwrap().iter().find(|row| row["name"] == "Unused23") {
                    let read = &symbol["actions"][0];
                    assert_eq!(read["tool"], "get_full_method_body");
                    let result = fixture.engram.handle_get_full_method_body(
                        serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
                    let body: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
                    assert_eq!(body["source_code"], seed_lines[23]);
                    assert_eq!(body["source_file_hash"], ContentHash::compute(source.as_bytes()).0);
                    recovered = true;
                    break;
                }
                if file["next_symbols"].is_null() { break; }
                action = file["next_symbols"].clone();
            }
            assert!(recovered, "Follow the emitted inventory pages to an omitted declaration and read its current source");
        }
    });
}

#[test]
fn file_summary_keeps_executable_prefix_when_every_direct_row_is_oversized() {
    owned_case(|fixture| async move {
        const SEED: &str = "src/prefix_seed.rs";
        const CALLER: &str = "src/prefix_caller.rs";
        let seed = "pub fn Used() -> i32 { 1 }\n";
        let name = "LongConsumer".repeat(100);
        let caller = format!("pub fn {name}() -> i32 {{ crate::prefix_seed::Used() }}\n");
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        fixture.search.prepare_source_snapshot(PID, 2, 3,
            &[RelPath::new(SEED), RelPath::new(CALLER)], &[], &cancel).await.unwrap();
        fixture.search.index_docs(PID, &[source_doc(3, 700, SEED, seed), source_doc(3, 701, CALLER, &caller)],
            &cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        std::fs::write(fixture.root.join(SEED), seed).unwrap();
        std::fs::write(fixture.root.join(CALLER), &caller).unwrap();
        fixture.engram.state.graph.upsert_nodes(PID, &[
            file_node(SEED, seed, 3), declaration(SEED, "Used", 1, 3),
            file_node(CALLER, &caller, 3), declaration(CALLER, &name, 1, 3),
        ]).unwrap();
        fixture.engram.state.graph.upsert_edges(PID, &[
            edge(id(CALLER, &name, 1), id(SEED, "Used", 1), EdgeKind::Calls, 3),
        ]).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline, 3).unwrap());
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let text = summary_impact(&fixture, tool, &format!("file:{SEED}"), 6).await;
            let report = impact_packet(&text);
            assert_eq!(report["direct_dependents_observed"], 1);
            assert_eq!(report["immediate_consumers"], json!([]));
            assert_eq!(report["immediate_consumers_omitted_from_summary"], 1);
            let prefix = text.split("## Revision-admitted change impact").next().unwrap();
            assert!(prefix.contains("## First immediate indexed consumer"));
            let action: Value = serde_json::from_str(prefix.split("```json\n").nth(1).unwrap()
                .split("```").next().unwrap()).unwrap();
            assert_eq!(action["tool"], "get_full_method_body");
            assert_eq!(action["arguments"]["file_path"], CALLER);
            let result = fixture.engram.handle_get_full_method_body(
                serde_json::from_value(action["arguments"].clone()).unwrap()).await.unwrap();
            let body: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(body["source_code"], caller.trim_end());
            assert_eq!(body["source_file_hash"], ContentHash::compute(caller.as_bytes()).0);
        }
    });
}

#[test]
fn file_impact_summary_reveals_files_beyond_samples_without_counting_unadmitted_references() {
    owned_case(|fixture| async move {
        let paths: Vec<_> = (0..6).map(|i|format!("src/consumer_{i}.rs")).collect();
        let body = "pub fn UsePolicy() { crate::policy::ClassifyAutomatic(); }\n";
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        let revisions: Vec<_> = paths.iter().map(|path|RelPath::new(path)).collect();
        fixture.search.prepare_source_snapshot(PID,2,3,&revisions,&[],&cancel).await.unwrap();
        let docs: Vec<_> = paths.iter().enumerate()
            .map(|(i,path)|source_doc(3,300+i as u64,path,body)).collect();
        fixture.search.index_docs(PID,&docs,&cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID,3).unwrap();
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for path in &paths {
            std::fs::write(fixture.root.join(path),body).unwrap();
            nodes.extend([file_node(path,body,3),declaration(path,"UsePolicy",1,3)]);
            edges.push(edge(id(path,"UsePolicy",1),id(POLICY,"ClassifyAutomatic",1),EdgeKind::Calls,3));
        }
        // A known declaration with no admitted physical file, and a future
        // declaration revision, must remain unknown/non-selected leads.
        nodes.extend([declaration(UNBOUND,"MissingFileBinding",1,1),
            declaration(FUTURE_FILE,"FutureFileBinding",1,2)]);
        edges.extend([edge(id(UNBOUND,"MissingFileBinding",1),id(POLICY,"ClassifyAutomatic",1),EdgeKind::Calls,1),
            edge(id(FUTURE_FILE,"FutureFileBinding",1),id(POLICY,"ClassifyAutomatic",1),EdgeKind::Calls,2)]);
        fixture.engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        fixture.engram.state.graph.upsert_edges(PID,&edges).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline,3).unwrap());
        let target = format!("file:{POLICY}");
        for tool in ["impact_analysis","compute_blast_radius"] {
            let report = impact_packet(&summary_impact(&fixture,tool,&target,6).await);
            let inventory = &report["observed_dependent_files"];
            assert_eq!(inventory["files_observed"],8);
            assert_eq!(inventory["files_omitted"],0);
            let files: std::collections::BTreeMap<_,_> = inventory["files"].as_array().unwrap().iter()
                .map(|row|(row["file_path"].as_str().unwrap(),row["min_depth"].as_u64().unwrap())).collect();
            for path in &paths { assert_eq!(files[path.as_str()],1); }
            assert_eq!(files[SPATIAL],1);
            assert_eq!(files[HTTP],4,"Transitive-only files must remain navigable beyond the direct witness sample");
            assert_eq!(files.len(),8,"Multiple declarations in a file are one observed path");
            assert!(!files.contains_key(POLICY) && !files.contains_key(UNBOUND) && !files.contains_key(FUTURE_FILE));
            assert!(report["selected_dependents_observed"].as_u64().unwrap()>8);
            assert_eq!(report["immediate_consumers"].as_array().unwrap().len(),4);
            assert!(report["coverage"]["unknown_nodes"].as_u64().unwrap()>0);
            assert_eq!(report["working_tree_checked"],false);
            assert!(inventory["scope"].as_str().unwrap().contains("not runtime"));

            let empty = impact_packet(&summary_impact(&fixture,tool,&format!("file:{COMPANION}"),6).await);
            assert_eq!(empty["observed_dependent_files"]["files_observed"],0);
            assert_eq!(empty["observed_dependent_files"]["files_omitted"],0);
            assert_eq!(empty["observed_dependent_files"]["files"],json!([]));
            let missing = impact_packet(&summary_impact(&fixture,tool,"file:src/absent.rs",6).await);
            assert!(missing["observed_dependent_files"].is_null(),"Unavailable evidence is not an observed empty inventory");
        }
    });
}

#[test]
fn file_impact_summary_keeps_observed_file_counts_when_graph_display_drops_identities() {
    owned_case(|fixture| async move {
        let paths: Vec<_> = (0..40).map(|i|format!("src/consumer_{i:02}.rs")).collect();
        // Long canonical identities exhaust graph display bytes well before
        // the bounded walk's admitted file inventory; summary must not recount
        // only whichever identities happen to survive that display budget.
        let names:Vec<_>=(0..40).map(|i|if i<36 {"A".repeat(600)}else{format!("Useful{i:02}")}).collect();
        let bodies:Vec<_>=names.iter().map(|name|
            format!("pub fn {name}() {{ crate::policy::ClassifyAutomatic(); }}\n")).collect();
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        let revisions: Vec<_> = paths.iter().map(|path|RelPath::new(path)).collect();
        fixture.search.prepare_source_snapshot(PID,2,3,&revisions,&[],&cancel).await.unwrap();
        let docs: Vec<_> = paths.iter().enumerate()
            .map(|(i,path)|source_doc(3,300+i as u64,path,&bodies[i])).collect();
        fixture.search.index_docs(PID,&docs,&cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID,3).unwrap();
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for (i,path) in paths.iter().enumerate() {
            std::fs::write(fixture.root.join(path),&bodies[i]).unwrap();
            nodes.extend([file_node(path,&bodies[i],3),declaration(path,&names[i],1,3)]);
            edges.push(edge(id(path,&names[i],1),id(POLICY,"ClassifyAutomatic",1),EdgeKind::Calls,3));
        }
        fixture.engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        fixture.engram.state.graph.upsert_edges(PID,&edges).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline,3).unwrap());
        let target = format!("file:{POLICY}");
        let graph = impact_packet(&impact_target(&fixture,&target,Some(6)).await);
        assert!(!graph["coverage"]["reasons"].as_array().unwrap().iter()
            .any(|reason|reason=="retained_string_byte_limit"),
            "This fixture must reach display clipping without exhausting the traversal's separate retained-data budget");
        assert!(graph["selected_dependent_identities_returned"].as_u64().unwrap()<44);
        for tool in ["impact_analysis","compute_blast_radius"] {
            let report = impact_packet(&summary_impact(&fixture,tool,&target,6).await);
            let inventory = &report["observed_dependent_files"];
            assert_eq!(inventory["files_observed"],42);
            let retained = inventory["files"].as_array().unwrap();
            assert!(!retained.is_empty() && retained.len()<=32);
            assert_eq!(retained.len() as u64+inventory["files_omitted"].as_u64().unwrap(),42);
            assert_eq!(report["selected_dependents_observed"],44);
            assert_eq!(report["coverage"],graph["coverage"]);
            let consumers=report["immediate_consumers"].as_array().unwrap();
            assert_eq!(consumers.len(),4,"Oversized inventory rows must leave room for four usable direct witnesses");
            let consumer_files:std::collections::BTreeSet<_>=consumers.iter()
                .map(|row|row["file_path"].as_str().unwrap()).collect();
            assert_eq!(consumer_files.len(),4);
            for consumer in consumers {
                let witness=&consumer["sample_connection_to_target"];
                assert_eq!(witness["source_node_id"],consumer["node_id"]);
                assert_eq!(witness["target_node_id"],id(POLICY,"ClassifyAutomatic",1));
                assert_eq!(witness["target_scope"],"file_member");
                let response=fixture.engram.handle_get_full_method_body(
                    serde_json::from_value(consumer["source_read"]["arguments"].clone()).unwrap()).await.unwrap();
                let body:Value=serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
                let current=std::fs::read(fixture.root.join(consumer["file_path"].as_str().unwrap())).unwrap();
                assert_eq!(body["source_file_hash"],ContentHash::compute(&current).0);
                assert!(body["source_code"].as_str().unwrap().contains("ClassifyAutomatic"));
            }
        }
    });
}

#[test]
fn file_impact_shared_excerpt_keeps_unrecorded_positions_unknown() {
    owned_case(|fixture| async move {
        const A: &str = "src/a_ui.rs";
        const Z: &str = "src/z_ui.rs";
        let body = "pub fn display() { crate::spatial::QueryViewport(); }\n";
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        for path in [A, Z] { std::fs::write(fixture.root.join(path), body).unwrap(); }
        fixture.search.prepare_source_snapshot(PID, 2, 3,
            &[RelPath::new(A), RelPath::new(Z)], &[], &cancel).await.unwrap();
        fixture.search.index_docs(PID,
            &[source_doc(3, 91, A, body), source_doc(3, 92, Z, body)], &cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID, 3).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline, 3).unwrap());
        let mut consumer = file_node(SPATIAL,SPATIAL_BODY,1);
        consumer.start_line=0;
        consumer.end_line=0;
        fixture.engram.state.graph.upsert_nodes(PID,&[
            consumer, file_node(A, body, 3), file_node(Z, body, 3),
            file_node(FUTURE_FILE, FUTURE_BODY, 4),
        ]).unwrap();
        fixture.engram.state.graph.upsert_edges(PID,&[
            edge(format!("file:{SPATIAL}"),format!("file:{POLICY}"),EdgeKind::Imports,1),
            edge(format!("file:{A}"),format!("file:{SPATIAL}"),EdgeKind::Imports,3),
            edge(format!("file:{A}"),format!("file:{SPATIAL}"),EdgeKind::Dependency,3),
            edge(format!("file:{Z}"),format!("file:{SPATIAL}"),EdgeKind::Imports,3),
            edge(format!("file:{UNBOUND}"),format!("file:{SPATIAL}"),EdgeKind::Imports,1),
            edge(format!("file:{FUTURE_FILE}"),format!("file:{SPATIAL}"),EdgeKind::Imports,4),
        ]).unwrap();
        let target = format!("file:{POLICY}");
        let graph = impact_packet(&impact_target(&fixture, &target, Some(6)).await);
        for tool in ["impact_analysis","compute_blast_radius"] {
            let report=impact_packet(&summary_impact(&fixture,tool,&target,6).await);
            assert_eq!(report["selected_dependents_observed"], graph["selected_dependents_observed"]);
            assert_eq!(report["coverage"], graph["coverage"]);
            let hints = &report["consumer_navigation"]["selected_file_hints"];
            assert_eq!(*hints, json!([SPATIAL, A]),
                "Show nearer consumer files first, then a deterministic path sample; parallel edges must not duplicate files");
            assert_eq!(report["consumer_navigation"]["selected_file_hints_omitted"], 1,
                "Only retained selected external file identities belong in the hint sample or its omitted count");
            let result = fixture.engram.handle_get_codebase_overview(serde_json::from_value(json!({
                "project_id":PID,"scope":hints[1],"include_declarations":false,
                "include_retained_context":false,"limit":8
            })).unwrap()).await.unwrap();
            let output = &result.content[0].as_text().unwrap().text;
            let navigation: Value = serde_json::from_str(output.split("## CodeMap Navigation").nth(1).unwrap()
                .split("```json\n").nth(1).unwrap().split("```").next().unwrap()).unwrap();
            assert_eq!(navigation["source_generation"], 3);
            assert_eq!(navigation["files"][0]["path"], A,
                "A compact hint must let an agent discover the selected consumer through an exact-file CodeMap query");
            assert_eq!(navigation["files"][0]["physical_generation"], 3);
            let read = &navigation["files"][0]["actions"][1];
            assert_eq!(read["tool"], "get_full_method_body");
            let result = fixture.engram.handle_get_full_method_body(
                serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
            let source: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(source["source_file_hash"], ContentHash::compute(body.as_bytes()).0);
            assert_eq!(source["source_code"], body.trim_end());
            let consumer=report["immediate_consumers"].as_array().unwrap().iter()
                .find(|row|row["node_id"]==format!("file:{SPATIAL}")).unwrap();
            let witness=&consumer["sample_connection_to_target"];
            assert_eq!(witness["recorded_position_uses_source_read"],true);
            assert!(witness.get("recorded_position_read").is_none());
            assert!(witness["source_line"].is_null() && witness["extractor_anchor_line"].is_null());
            assert_eq!(witness["source_position_kind"],"not_recorded");
            assert_eq!(witness["source_position_scope"],"not_recorded");
            assert_eq!(witness["reference_lines"],json!([]));
            assert_eq!(witness["reference_lines_truncated"],false);
            assert_eq!(consumer["source_read_scope"],"current_file_excerpt");
            let action=&consumer["source_read"];
            let result=fixture.engram.handle_get_full_method_body(
                serde_json::from_value(action["arguments"].clone()).unwrap()).await.unwrap();
            let body:Value=serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(body["source_file_hash"],ContentHash::compute(SPATIAL_BODY.as_bytes()).0);
            assert_eq!(body["retrieval_scope"],"current_file_excerpt");
            assert_eq!(body["source_verification"],"current_file_snapshot_not_index_verified");
        }
    });
}

#[test]
fn hinted_file_declarations_recover_current_source_across_a_non_callable_boundary() {
    owned_case(|fixture| async move {
        const UI: &str = "src/policy_client.rs";
        let body = "// Client policy editing\n\npub fn save_override() {\n    send_policy();\n}\n";
        let cancel = CancellationToken::new();
        let baseline = fixture.engram.state.registry.capture_source_head(PID).unwrap();
        std::fs::write(fixture.root.join(UI), body).unwrap();
        fixture.search.prepare_source_snapshot(PID,2,3,&[RelPath::new(UI)],&[],&cancel).await.unwrap();
        fixture.search.index_docs(PID,&[source_doc(3,98,UI,body)],&cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID,3).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline,3).unwrap());
        let mut consumer = declaration(UI,"save_override",3,3);
        consumer.end_line=5;
        fixture.engram.state.graph.upsert_nodes(PID,&[file_node(UI,body,3),consumer]).unwrap();
        // The retained route passes through a file dependency, so it cannot be
        // described as a continuous callable-only source route.
        fixture.engram.state.graph.upsert_edges(PID,&[
            edge(format!("file:{SPATIAL}"),id(POLICY,"ClassifyAutomatic",1),EdgeKind::Dependency,1),
            edge(format!("file:{UI}"),format!("file:{SPATIAL}"),EdgeKind::Dependency,3),
            edge(id(UI,"save_override",3),format!("file:{SPATIAL}"),EdgeKind::Dependency,3),
        ]).unwrap();
        let target=id(POLICY,"ClassifyAutomatic",1);
        let graph=impact_packet(&impact_target(&fixture,&target,Some(6)).await);
        for tool in ["impact_analysis","compute_blast_radius"] {
            let text=summary_impact(&fixture,tool,&target,6).await;
            assert!(text.len()<=12*1024);
            let report=impact_packet(&text);
            assert_eq!(report["coverage"],graph["coverage"]);
            assert_eq!(report["selected_dependents_observed"],graph["selected_dependents_observed"]);
            let hints=report["source_routes"]["file_hints"].as_array().unwrap();
            assert!(hints.contains(&json!(UI)));
            assert!(!report["source_routes"]["nodes"].as_array().unwrap().iter()
                .any(|node|node["node_id"]==id(UI,"save_override",3)));
            let leads=&report["hinted_file_declarations"];
            let lead=leads["declarations"].as_array().unwrap().iter()
                .find(|row|row["node_id"]==id(UI,"save_override",3)).unwrap();
            assert_eq!(lead["generation"],3);
            assert!(leads["scope"].as_str().unwrap().contains("not value-flow routes or runtime binding"));
            let read=&lead["source_read"];
            assert_eq!(read["tool"],"get_full_method_body");
            let response=fixture.engram.handle_get_full_method_body(
                serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
            let recovered:Value=serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(recovered["source_code"],"pub fn save_override() {\n    send_policy();\n}");
            assert_eq!(recovered["source_file_hash"],ContentHash::compute(body.as_bytes()).0);
            assert_eq!(recovered["source_verification"],"explicit_range_current_file_snapshot");
        }
    });
}

#[test]
fn file_impact_deeper_inventory_cannot_erase_first_hop_connections() {
    owned_case(|fixture| async move {
        let mut entries=Vec::new();
        for i in 0..6 {
            let path=format!("src/direct_{i:02}.rs");let name=format!("Direct{i:02}");
            entries.push((path.clone(),name.clone(),
                format!("pub fn {name}() {{ crate::policy::ClassifyAutomatic(); }}\n"),
                id(POLICY,"ClassifyAutomatic",1)));
            for j in 0..20 {
                let child=format!("src/fanout_{i:02}_{j:02}.rs");let child_name=format!("UseDirect{i:02}{j:02}");
                entries.push((child,child_name.clone(),
                    format!("pub fn {child_name}() {{ crate::direct_{i:02}::{name}(); }}\n"),
                    id(&path,&name,1)));
            }
        }
        let cancel=CancellationToken::new();
        let baseline=fixture.engram.state.registry.capture_source_head(PID).unwrap();
        let paths:Vec<_>=entries.iter().map(|(path,_,_,_)|RelPath::new(path)).collect();
        fixture.search.prepare_source_snapshot(PID,2,3,&paths,&[],&cancel).await.unwrap();
        let docs:Vec<_>=entries.iter().enumerate().map(|(i,(path,_,body,_))|
            source_doc(3,300+i as u64,path,body)).collect();
        fixture.search.index_docs(PID,&docs,&cancel).await.unwrap();
        fixture.search.seal_source_snapshot(PID,3).unwrap();
        let mut nodes=Vec::new();let mut edges=Vec::new();
        for (path,name,body,callee) in &entries {
            std::fs::write(fixture.root.join(path),body).unwrap();
            nodes.extend([file_node(path,body,3),declaration(path,name,1,3)]);
            let mut call=edge(id(path,name,1),callee.clone(),EdgeKind::Calls,3);
            call.metadata=Some(json!({"call_site_line":1}));edges.push(call);
        }
        fixture.engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        fixture.engram.state.graph.upsert_edges(PID,&edges).unwrap();
        assert!(fixture.engram.state.registry.publish_source_head(&baseline,3).unwrap());
        let target=format!("file:{POLICY}");
        let full=impact_packet(&impact_target(&fixture,&target,Some(6)).await);
        assert_eq!(full["selected_dependents_observed"],130);
        assert!(full["display_truncated"].as_bool().unwrap(),"Exercise the response budget, not a tiny graph");
        let by_ref:std::collections::BTreeMap<_,_>=full["nodes"].as_array().unwrap().iter()
            .map(|row|(row["ref"].as_str().unwrap(),row)).collect();
        let recovery_refs:std::collections::BTreeSet<_>=full["recoveries"].as_array().unwrap().iter()
            .map(|row|row["ref"].as_str().unwrap()).collect();
        for connection in full["edges"].as_array().unwrap() {
            assert!(by_ref.contains_key(connection["source_ref"].as_str().unwrap()));
            assert!(by_ref.contains_key(connection["target_ref"].as_str().unwrap()));
            if let Some(reference)=connection["recovery_ref"].as_str() { assert!(recovery_refs.contains(reference)); }
        }
        for row in by_ref.values() { assert!(recovery_refs.contains(row["recovery_ref"].as_str().unwrap())); }
        for tool in ["impact_analysis","compute_blast_radius"] {
            let shallow=impact_packet(&summary_impact(&fixture,tool,&target,1).await);
            let deep=impact_packet(&summary_impact(&fixture,tool,&target,6).await);
            assert_eq!(deep["selected_dependents_observed"],130);
            assert_eq!(deep["direct_dependents_observed"],7);
            assert_eq!(deep["coverage"],full["coverage"]);
            let shallow_ids:std::collections::BTreeSet<_>=shallow["immediate_consumers"].as_array().unwrap().iter()
                .map(|row|row["node_id"].as_str().unwrap()).collect();
            let consumers=deep["immediate_consumers"].as_array().unwrap();
            assert_eq!(consumers.len(),4);
            assert_eq!(consumers.iter().map(|row|row["node_id"].as_str().unwrap()).collect::<std::collections::BTreeSet<_>>(),shallow_ids,
                "Adding downstream inventory must preserve the usable first-hop sample");
            assert_eq!(consumers.iter().map(|row|row["file_path"].as_str().unwrap()).collect::<std::collections::BTreeSet<_>>().len(),4);
            for consumer in consumers {
                let witness=&consumer["sample_connection_to_target"];
                assert_eq!(witness["source_node_id"],consumer["node_id"]);
                assert_eq!(witness["target_scope"],"file_member");
                assert_eq!(witness["reference_lines"],json!([1]));
                assert!(full["edges"].as_array().unwrap().iter().any(|connection|
                    by_ref[connection["source_ref"].as_str().unwrap()]["node_id"]==witness["source_node_id"]
                    && by_ref[connection["target_ref"].as_str().unwrap()]["node_id"]==witness["target_node_id"]
                    && connection["kind"]==witness["kind"]),"Every summary witness must exist as a complete full-graph connection");
            }
        }
    });
}
