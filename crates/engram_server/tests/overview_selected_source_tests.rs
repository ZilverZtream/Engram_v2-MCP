//! Public orientation describes the published code selection separately from
//! retained corpora and the live graph. No parser/model/native query is needed.
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_index::{HybridSearchEngine, IndexDoc};
use engram_server::{state::AppState, tools::Engram};
use serde_json::{Value, json};
use std::{future::Future, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

const PID: &str = "overview-source";
const A: &str = "src/service/a.rs";
const B: &str = "src/model/b.py";

fn doc(
    namespace: &str,
    generation: u64,
    chunk: u64,
    path: &str,
    language: &str,
    text: &str,
) -> IndexDoc {
    IndexDoc {
        namespace: namespace.into(),
        generation,
        chunk_id: chunk,
        path: RelPath::new(path),
        language: language.into(),
        content: text.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: format!("{path}-{chunk}"),
        content_hash: ContentHash::compute(text.as_bytes()).0,
    }
}

fn node(id: &str, kind: &str, name: &str, path: &str, generation: u64) -> Node {
    Node {
        node_id: id.into(),
        node_type: kind.into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "rust".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 1,
        generation,
        metadata: None,
    }
}

async fn fixture(owned: &Path, managed: bool) -> (Engram, Arc<HybridSearchEngine>) {
    let root = owned.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let config = Config {
        data_dir: owned.join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _) = AppState::new(config).unwrap();
    let project = ProjectRecord {
        project_id: PID.into(),
        project_name: "Selected source overview".into(),
        project_type: "general".into(),
        directory: root.to_str().unwrap().into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        reindex_required_since_ms: None,
    };
    if managed {
        state.registry.register_source_project(&project).unwrap();
    } else {
        state.registry.put_project(&project).unwrap();
    }
    let runtime = engram_server::services::project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    let search = runtime.search.clone();
    drop(runtime);
    (Engram::new(state), search)
}

// Database and native/blocking worker ownership ends before the owned tree,
// including panic unwinding. These fixtures start no application actors/jobs.
fn owned_case<F, Fut>(managed: bool, run: F)
where
    F: FnOnce(Engram, Arc<HybridSearchEngine>) -> Fut,
    Fut: Future<Output = ()>,
{
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (engram, search) = fixture(temporary.path(), managed).await;
        run(engram, search).await;
    });
    drop(runtime);
    temporary.close().unwrap();
}

async fn overview(engram: &Engram) -> String {
    let response = engram
        .handle_get_codebase_overview(
            serde_json::from_value(json!({"project_id":PID,"include_retained_context":true}))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!response.is_error.unwrap_or(false));
    response
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|content| content.text.as_str())
        .collect()
}

fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    text.split_once(heading)
        .unwrap()
        .1
        .split("\n--- ")
        .next()
        .unwrap()
}

#[test]
fn overview_counts_current_inherited_source_separately_from_retained_corpora_and_live_graph() {
    owned_case(true, |engram, search| async move {
        let cancel = CancellationToken::new();
        let initial = engram.state.registry.capture_source_head(PID).unwrap();
        search
            .prepare_full_source_snapshot(PID, 0, 1, &[RelPath::new(A), RelPath::new(B)], &cancel)
            .await
            .unwrap();
        search
            .index_docs(
                PID,
                &[
                    doc("memory", 1, 1, A, "rust", "old Rust chunk one"),
                    doc("memory", 1, 2, A, "rust", "old Rust chunk two"),
                    doc("memory", 1, 3, B, "python", "inherited Python source"),
                ],
                &cancel,
            )
            .await
            .unwrap();
        search.seal_source_snapshot(PID, 1).unwrap();
        assert!(
            engram
                .state
                .registry
                .publish_source_head(&initial, 1)
                .unwrap()
        );
        let old_view = search.pin_source_generation(PID, 1).unwrap();

        let base = engram.state.registry.capture_source_head(PID).unwrap();
        search
            .prepare_source_snapshot(PID, 1, 2, &[RelPath::new(A)], &[], &cancel)
            .await
            .unwrap();
        search
            .index_docs(
                PID,
                &[
                    doc("memory", 2, 4, A, "rust", "current Rust source one"),
                    doc("memory", 2, 10, A, "rust", "current Rust source two"),
                ],
                &cancel,
            )
            .await
            .unwrap();
        search.seal_source_snapshot(PID, 2).unwrap();
        assert!(engram.state.registry.publish_source_head(&base, 2).unwrap());
        // A real sealed, unpublished candidate remains physically present.
        search
            .prepare_source_snapshot(PID, 2, 3, &[RelPath::new("src/abandoned.rs")], &[], &cancel)
            .await
            .unwrap();
        search
            .index_docs(
                PID,
                &[doc(
                    "memory",
                    3,
                    5,
                    "src/abandoned.rs",
                    "vbnet",
                    "unpublished candidate",
                )],
                &cancel,
            )
            .await
            .unwrap();
        search.seal_source_snapshot(PID, 3).unwrap();
        let mut nodes = vec![
            node(&format!("file:{A}"), "file", "a.rs", A, 2),
            node(&format!("file:{B}"), "file", "b.py", B, 1),
            node(
                "file:src/abandoned.rs",
                "file",
                "abandoned.rs",
                "src/abandoned.rs",
                3,
            ),
            node("current-a", "function", "CurrentService", A, 2),
            node("inherited-b", "class", "InheritedModel", B, 1),
            node("obsolete-a", "function", "ObsoleteService", A, 1),
            node("future-a", "function", "FutureService", A, 3),
            node(
                "future-file",
                "class",
                "AbandonedType",
                "src/abandoned.rs",
                3,
            ),
            node("schema-table", "db_table", "AuditTable", A, 2),
            node("config-key", "app_setting", "AuditEnabled", A, 2),
        ];
        let mut edges = vec![
            Edge {
                source_id: format!("file:{A}"),
                target_id: "current-a".into(),
                namespace: "memory".into(),
                language: "rust".into(),
                edge_kind: EdgeKind::Contains,
                weight: 1,
                generation: 2,
                metadata: None,
                updated_at_ms: 1,
            },
            Edge {
                source_id: format!("file:{B}"),
                target_id: "inherited-b".into(),
                namespace: "memory".into(),
                language: "python".into(),
                edge_kind: EdgeKind::Contains,
                weight: 1,
                generation: 1,
                metadata: None,
                updated_at_ms: 1,
            },
        ];
        for number in 0..24 {
            let id = format!("schema-column-{number}");
            nodes.push(node(
                &id,
                "db_column",
                &format!("AuditColumn{number}"),
                A,
                2,
            ));
            edges.push(Edge {
                source_id: "schema-table".into(),
                target_id: id,
                namespace: "memory".into(),
                language: "sql".into(),
                edge_kind: EdgeKind::HasColumn,
                weight: 100,
                generation: 2,
                metadata: None,
                updated_at_ms: 1,
            });
        }
        engram.state.graph.upsert_nodes(PID, &nodes).unwrap();
        engram.state.graph.upsert_edges(PID, &edges).unwrap();
        let before_knowledge = overview(&engram).await;
        assert!(before_knowledge.contains("source_files: 2\n"));
        assert!(before_knowledge.contains("source_chunks: 3\n"));
        assert!(before_knowledge.contains("retained_memory_documents: 6\n"));
        let file_types = section(
            &before_knowledge,
            "--- File Types (selected source files) ---",
        );
        assert!(file_types.contains("denominator: 2 selected source files"));
        assert!(file_types.contains(".rs: 1 (50%)") && file_types.contains(".py: 1 (50%)"));
        assert!(!file_types.contains(".vb"));
        let languages = section(
            &before_knowledge,
            "--- Language Breakdown (selected source chunks) ---",
        );
        assert!(
            languages
                .contains("denominator: 3 selected source chunks (not file or byte percentages)")
        );
        assert!(languages.contains("rust: 2 (66%)") && languages.contains("python: 1 (33%)"));
        assert!(!languages.contains("vbnet"));
        let source_navigation = navigation(&before_knowledge);
        for scope in ["src/service", "src/model"] {
            let area = source_navigation["areas"]
                .as_array()
                .unwrap()
                .iter()
                .find(|area| area["scope"] == scope)
                .unwrap();
            assert_eq!(area["selected_files"], 1);
        }
        let candidates = source_navigation["files"].to_string();
        assert!(candidates.contains("CurrentService") && candidates.contains("InheritedModel"));
        for excluded in [
            "ObsoleteService",
            "FutureService",
            "AbandonedType",
            "AuditColumn",
        ] {
            assert!(
                !candidates.contains(excluded),
                "Unselected revisions and schema hubs must not describe current code: {excluded}"
            );
        }
        assert!(!before_knowledge.contains("Full Graph Inventory (all recorded revisions"));
        let inventories = source_navigation["other_inventories"].as_array().unwrap();
        let schema = inventories
            .iter()
            .find(|action| action["tool"] == "query_graph_nodes")
            .unwrap();
        assert!(schema["label"].as_str().unwrap().contains("not scanned"));
        let schema_result = engram
            .handle_query_graph_nodes(serde_json::from_value(schema["arguments"].clone()).unwrap())
            .await
            .unwrap();
        assert!(!schema_result.is_error.unwrap_or(false));
        assert!(
            schema_result
                .content
                .iter()
                .filter_map(|item| item.as_text())
                .any(|item| item.text.contains("schema-table")),
            "Deferred schema discovery must be executable and find recorded evidence rather than imply an empty database"
        );
        let settings = inventories
            .iter()
            .find(|action| action["tool"] == "list_settings")
            .unwrap();
        let settings_result = engram
            .handle_list_settings(serde_json::from_value(settings["arguments"].clone()).unwrap())
            .await
            .unwrap();
        assert!(!settings_result.is_error.unwrap_or(false));
        assert!(
            settings_result
                .content
                .iter()
                .filter_map(|item| item.as_text())
                .any(|item| item.text.contains("AuditEnabled"))
        );
        assert!(!before_knowledge.contains("Top Central Nodes (PageRank)"));
        assert!(before_knowledge.contains("retained_vector_rows: not measured"));

        search
            .index_docs(
                PID,
                &[
                    doc(
                        "history",
                        1,
                        6,
                        "history/one",
                        "vbnet",
                        "historical snippet",
                    ),
                    doc(
                        "history",
                        1,
                        7,
                        "history/two",
                        "vbnet",
                        "another historical snippet",
                    ),
                ],
                &cancel,
            )
            .await
            .unwrap();
        search
            .index_docs(
                PID,
                &[doc(
                    "business_logic",
                    1,
                    8,
                    "knowledge/note",
                    "vbnet",
                    "retained knowledge",
                )],
                &cancel,
            )
            .await
            .unwrap();
        search
            .index_docs(
                PID,
                &[doc(
                    "antipattern",
                    1,
                    9,
                    "history/pattern",
                    "vbnet",
                    "retained anti-pattern",
                )],
                &cancel,
            )
            .await
            .unwrap();
        let after_knowledge = overview(&engram).await;
        assert!(
            after_knowledge.contains("source_files: 2\n")
                && after_knowledge.contains("source_chunks: 3\n")
        );
        assert_eq!(
            section(
                &after_knowledge,
                "--- File Types (selected source files) ---"
            ),
            file_types
        );
        assert_eq!(
            section(
                &after_knowledge,
                "--- Language Breakdown (selected source chunks) ---"
            ),
            languages
        );
        assert!(after_knowledge.contains("retained_index_documents: 10\n"));
        assert!(after_knowledge.contains("retained_history_documents: 2\n"));
        assert!(after_knowledge.contains("retained_antipattern_documents: 1\n"));
        drop(old_view);
    });
}

#[test]
fn overview_reports_unknown_legacy_file_coverage_without_fabricating_zero_or_current_architecture()
{
    owned_case(false, |engram, search| async move {
        let cancel = CancellationToken::new();
        // This producer really materializes a complete flat legacy generation
        // before attesting its absent HEAD. No managed/catalog flags are forged.
        search
            .index_docs(
                PID,
                &[doc("memory", 1, 1, A, "rust", "complete legacy source")],
                &cancel,
            )
            .await
            .unwrap();
        let base = engram.state.registry.capture_source_head(PID).unwrap();
        assert!(
            engram
                .state
                .registry
                .attest_legacy_source_head(&base)
                .unwrap()
        );
        let report = overview(&engram).await;
        assert!(
            report
                .contains("source_files: unknown (legacy selection has no complete file manifest)")
        );
        assert!(!report.contains("source_files: 0"));
        let file_types = section(&report, "--- File Types (selected source files) ---");
        assert!(
            file_types.contains("unknown") && file_types.contains("no file percentages calculated")
        );
        assert!(!file_types.contains(".rs: 0") && !file_types.contains("(0%)"));
        assert!(report.contains("source_chunks: 1\n"));
        assert!(report.contains("rust: 1 (100%)"));
        assert!(report.contains("coverage is unknown"));
        assert!(!report.contains("--- Architecture ---"));
    });
}

fn navigation(text: &str) -> Value {
    let section = text
        .split_once("## CodeMap Navigation")
        .expect("A CodeMap needs copyable source navigation, not only aggregate counts")
        .1;
    serde_json::from_str(
        section
            .split_once("```json")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0,
    )
    .unwrap()
}

async fn navigate(engram: &Engram, arguments: Value) -> Value {
    let result = engram
        .handle_get_codebase_overview(serde_json::from_value(arguments).unwrap())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text: String = result
        .content
        .iter()
        .filter_map(|item| item.as_text())
        .map(|item| item.text.as_str())
        .collect();
    navigation(&text)
}

#[test]
fn codemap_diversifies_areas_and_its_scoped_recovery_opens_selected_source_without_prefix_starvation()
{
    for crowded in [false, true] {
        codemap_navigation_case(crowded);
    }
}

fn codemap_navigation_case(crowded: bool) {
    owned_case(true, move |engram, search| async move {
        const JOB: &str = "src/bbb-jobs/service.rs";
        const POLICY: &str = "src/zzz-map/policy.rs";
        const PRIVATE: &str = "src/zzz-map/private.rs";
        const FROZEN: &str = "Site/Q/frozen.vb";
        const FUTURE: &str = "src/zzz-map/future.rs";
        const SCHEMA: &str = "db/schema.sql";
        const SETTINGS: &str = "config/application.config";
        const POLICY_TEXT: &str = "pub fn ResolveZoom() -> i32 { 42 }\n";
        let root = std::path::PathBuf::from(
            engram
                .state
                .registry
                .get_project(PID)
                .unwrap()
                .unwrap()
                .directory,
        );
        let mut sources: Vec<(String, String)> = (0..32)
            .map(|number| {
                let path = format!("src/aaa-core/Part{number:02}.rs");
                let body = (0..3)
                    .map(|part| format!("pub fn Core{number:02}_{part}() -> i32 {{ {part} }}\n"))
                    .collect();
                (path, body)
            })
            .collect();
        // More than the displayed area budget contain ordinary/resource files,
        // each larger than the later policy and service areas. Initial task
        // navigation must still offer those useful leads without a larger limit.
        if crowded {
            sources.extend((0..40).flat_map(|area| {
                (0..3).map(move |part| {
                    (
                        format!("src/ordinary{area:02}/Part{part:02}.rs"),
                        format!("pub fn Ordinary{area:02}_{part}() -> i32 {{ {part} }}\n"),
                    )
                })
            }));
        }
        let policy_source=format!("{POLICY_TEXT}{}",(0..24)
            .map(|number|format!("pub fn AaaExtra{number:02}() -> i32 {{ {number} }}\n"))
            .collect::<String>());
        sources.push((POLICY.to_owned(),policy_source.clone()));
        sources.extend([(JOB,"pub fn RunJob() -> i32 { 7 }\n"),
            (PRIVATE,"pub fn PrivateOnly() -> i32 { 9 }\n"),(FROZEN,"Public Class FrozenOnly\nEnd Class\n"),
            (SCHEMA,"CREATE TABLE UserSettings (Id INT);\n"),
            (SETTINGS,"<configuration><appSettings><add key=\"MapEnabled\" value=\"true\" /></appSettings></configuration>\n")]
            .into_iter().map(|(path,body)|(path.to_owned(),body.to_owned())));
        let cancel = CancellationToken::new();
        let initial = engram.state.registry.capture_source_head(PID).unwrap();
        let paths: Vec<_> = sources.iter().map(|(path, _)| RelPath::new(path)).collect();
        search
            .prepare_full_source_snapshot(PID, 0, 1, &paths, &cancel)
            .await
            .unwrap();
        let mut docs = Vec::new();
        let mut nodes = Vec::new();
        for (index, (path, body)) in sources.iter().enumerate() {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), body).unwrap();
            let language = if path.ends_with(".sql") {
                "sql"
            } else if path.ends_with(".config") {
                "xml"
            } else if path.ends_with(".vb") {
                "vbnet"
            } else {
                "rust"
            };
            let mut source = doc("memory", 1, index as u64 + 1, path, language, body);
            source.end_line = body.lines().count() as u32;
            docs.push(source);
            let mut file = node(&format!("file:{path}"), "file", path, path, 1);
            file.end_line = body.lines().count() as u32;
            file.metadata = Some(json!({"file_hash":ContentHash::compute(body.as_bytes()).0}));
            nodes.push(file);
            for (offset, line) in body
                .lines()
                .enumerate()
                .filter(|(_, line)| line.starts_with("pub fn "))
            {
                let name = line
                    .strip_prefix("pub fn ")
                    .unwrap()
                    .split('(')
                    .next()
                    .unwrap();
                let mut symbol = node(
                    &format!("sym:function:{path}:{name}:{}", offset + 1),
                    "function",
                    name,
                    path,
                    1,
                );
                symbol.start_line = offset as u32 + 1;
                symbol.end_line = symbol.start_line;
                nodes.push(symbol);
            }
        }
        search.index_docs(PID, &docs, &cancel).await.unwrap();
        search.seal_source_snapshot(PID, 1).unwrap();
        engram.state.graph.upsert_nodes(PID, &nodes).unwrap();
        assert!(
            engram
                .state
                .registry
                .publish_source_head(&initial, 1)
                .unwrap()
        );
        // Physical candidate rows and graph nodes are not selected source.
        let future_body = "pub fn FutureOnly() -> i32 { 99 }\n";
        std::fs::write(root.join(FUTURE), future_body).unwrap();
        search
            .prepare_source_snapshot(PID, 1, 2, &[RelPath::new(FUTURE)], &[], &cancel)
            .await
            .unwrap();
        search
            .index_docs(
                PID,
                &[doc("memory", 2, 1000, FUTURE, "rust", future_body)],
                &cancel,
            )
            .await
            .unwrap();
        search.seal_source_snapshot(PID, 2).unwrap();
        engram
            .state
            .graph
            .upsert_nodes(
                PID,
                &[
                    node(&format!("file:{FUTURE}"), "file", FUTURE, FUTURE, 2),
                    node(
                        &format!("sym:function:{FUTURE}:FutureOnly:1"),
                        "function",
                        "FutureOnly",
                        FUTURE,
                        2,
                    ),
                ],
            )
            .unwrap();

        let report = navigate(
            &engram,
            json!({"project_id":PID,"exclude_paths":["Site/Q",PRIVATE]}),
        )
        .await;
        assert_eq!(report["source_generation"], 1);
        assert_eq!(report["working_tree_checked"], false);
        let files = report["files"].as_array().unwrap();
        if crowded {
            assert_eq!(files.len(), 8, "Useful leads must fit the default file budget");
        }
        assert_eq!(report["coverage"]["scan_truncated"], false);
        assert_eq!(report["coverage"]["area_sample_truncated"], crowded);
        assert!(
            files
                .iter()
                .any(|file| file["path"].as_str().unwrap().starts_with("src/aaa-core/"))
        );
        let needed: &[&str] = if crowded { &[JOB, POLICY, SETTINGS] }
            else { &[JOB, POLICY, SCHEMA, SETTINGS] };
        for &needed in needed {
            assert!(
                files.iter().any(|file| file["path"] == needed),
                "Initial orientation must preserve useful leads (crowded={crowded}): {needed}"
            );
        }
        assert!(
            files
                .iter()
                .all(|file| ![PRIVATE, FROZEN, FUTURE].contains(&file["path"].as_str().unwrap()))
        );
        if crowded {
            let ordinary_area = report["areas"]
                .as_array()
                .unwrap()
                .iter()
                .find(|area| area["scope"] == "src/aaa-core")
                .expect("Ordinary areas must retain copyable drill-down");
            assert_eq!(ordinary_area["selected_files"], 32);
            assert_eq!(ordinary_area["explore"]["tool"], "get_codebase_overview");
            let mut ordinary = navigate(&engram, ordinary_area["explore"]["arguments"].clone()).await;
            let mut ordinary_paths = std::collections::BTreeSet::new();
            loop {
                assert_eq!(ordinary["source_generation"], 1);
                for file in ordinary["files"].as_array().unwrap() {
                    let path = file["path"].as_str().unwrap();
                    assert!(path.starts_with("src/aaa-core/"));
                    assert!(ordinary_paths.insert(path.to_owned()), "Scoped file pages must not repeat identities");
                }
                if ordinary["next_page"].is_null() { break; }
                assert_eq!(ordinary["next_page"]["arguments"]["exclude_paths"], json!(["Site/Q",PRIVATE]));
                ordinary = navigate(&engram, ordinary["next_page"]["arguments"].clone()).await;
            }
            let expected_ordinary: std::collections::BTreeSet<_> = sources.iter()
                .filter(|(path,_)|path.starts_with("src/aaa-core/"))
                .map(|(path,_)|path.clone()).collect();
            assert_eq!(ordinary_paths, expected_ordinary, "Scoped pagination must recover every selected ordinary file");
        }
        let policy_report = if crowded {
            // The small area can be absent from the count-ranked area display;
            // its root file representative must itself offer working recovery.
            report.clone()
        } else {
            let map_area = report["areas"]
                .as_array().unwrap().iter()
                .find(|area| area["scope"] == "src/zzz-map")
                .expect("The map area needs copyable drill-down");
            assert_eq!(map_area["explore"]["tool"], "get_codebase_overview");
            let scoped = navigate(&engram, map_area["explore"]["arguments"].clone()).await;
            assert!(scoped["files"].as_array().unwrap().iter().all(|file| file["path"] == POLICY),
                "A returned scoped action must preserve exclusions and the same selected source membership");
            scoped
        };
        let policy = policy_report["files"].as_array().unwrap()
            .iter()
            .find(|file| file["path"] == POLICY)
            .unwrap();
        assert_eq!(policy["physical_generation"], 1);
        let file_action=policy["actions"].as_array().unwrap().iter()
            .find(|action|action["tool"]=="get_codebase_overview").unwrap();
        let first_page=navigate(&engram,file_action["arguments"].clone()).await;
        let first_policy=&first_page["files"][0];
        assert!(first_policy["symbols"].as_array().unwrap().iter()
            .all(|symbol|symbol["name"]!="ResolveZoom"),
            "The wanted declaration is intentionally beyond the first file symbol page");
        let continuation=&first_policy["next_symbols"];
        assert_eq!(continuation["tool"],"get_codebase_overview");
        assert_eq!(continuation["arguments"]["scope"],POLICY);
        assert_eq!(continuation["arguments"]["exclude_paths"],json!(["Site/Q",PRIVATE]));
        let later_page=navigate(&engram,continuation["arguments"].clone()).await;
        assert_eq!(later_page["source_generation"],1);
        let policy=&later_page["files"][0];
        assert_eq!(policy["path"],POLICY);
        assert_eq!(policy["physical_generation"],1);
        let symbol = policy["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|symbol| symbol["name"] == "ResolveZoom")
            .unwrap();
        let recovery = symbol["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|action| action["tool"] == "get_full_method_body")
            .unwrap();
        let recovered = engram
            .handle_get_full_method_body(
                serde_json::from_value(recovery["arguments"].clone()).unwrap(),
            )
            .await
            .unwrap();
        assert!(!recovered.is_error.unwrap_or(false));
        let recovered: Value =
            serde_json::from_str(&recovered.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(recovered["retrieval_scope"], "explicit_source_range");
        assert_eq!(
            recovered["source_code"].as_str().unwrap().trim(),
            POLICY_TEXT.trim()
        );
        assert_eq!(
            recovered["source_file_hash"],
            ContentHash::compute(policy_source.as_bytes()).0
        );
    });
}
