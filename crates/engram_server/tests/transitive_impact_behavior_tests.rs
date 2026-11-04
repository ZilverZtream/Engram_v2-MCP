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
            let call = &immediate[0]["source_read"];
            assert_eq!(call["tool"], "get_full_method_body");
            let prefix = &text[..text.floor_char_boundary(2048)];
            let prefix_call: Value = serde_json::from_str(
                prefix
                    .split_once("## First immediate indexed consumer")
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
            assert_eq!(prefix_call, *call);
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
