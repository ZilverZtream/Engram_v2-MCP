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

fn selected_impact(text: &str) -> Value {
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

fn dependent<'a>(report: &'a Value, node_id: &str) -> &'a Value {
    report["dependents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["node_id"] == node_id)
        .unwrap_or_else(|| panic!("Missing current causal dependent: {node_id}"))
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
        let after_move = selected_impact(&impact_target(&fixture, &moved_root, None).await);
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
    });
}
