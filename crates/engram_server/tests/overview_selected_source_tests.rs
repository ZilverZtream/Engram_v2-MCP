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
        let legacy=navigation(&report);
        assert_eq!(legacy["coverage"]["managed_source"],false);
        assert_eq!(legacy["coverage"]["matching_files"],Value::Null);
        let scoped=navigation_text(&engram,json!({"project_id":PID,"scope":A})).await;
        assert!(navigation(&scoped).get("detail").is_none(),"Legacy unknown membership is not an exact-file observation");
        assert!(scoped.contains("--- File Types") && scoped.contains("no file percentages calculated"));
        let request=json!({"project_id":PID,"scope":A,"symbol_node_id":format!("sym:function:{A}:Run:1")});
        let error=engram.handle_get_codebase_overview(serde_json::from_value(request).unwrap()).await.unwrap_err();
        assert!(error.message.contains("exact selected-file scope"),
            "Unknown legacy file coverage must not become a fabricated exact-symbol match or zero-result claim");
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

async fn navigation_text(engram: &Engram, arguments: Value) -> String {
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
    text
}

async fn navigate(engram: &Engram, arguments: Value) -> Value {
    navigation(&navigation_text(engram, arguments).await)
}

#[test]
fn scoped_codemap_actions_descend_without_losing_files_to_area_caps() {
    owned_case(true, |engram, search| async move {
        const SCOPE: &str = "src/application/api/v2";
        const EXCLUDED: &str = "src/application/api/v2/private";
        const CAPPED: &str = "src/application/many";
        let mut paths = vec![
            format!("{SCOPE}/entry.rs"),
            format!("{SCOPE}/controllers/catalog/List.rs"),
            format!("{SCOPE}/controllers/catalog/detail/Get.rs"),
            format!("{SCOPE}/services/catalog/Read.rs"),
            format!("{EXCLUDED}/Secret.rs"),
            "src/application/api/v20/Unrelated.rs".into(),
        ];
        paths.extend((0..130).map(|i| format!("{CAPPED}/child{i:03}/Leaf.rs")));
        let cancel = CancellationToken::new();
        let initial = engram.state.registry.capture_source_head(PID).unwrap();
        let identities: Vec<_> = paths.iter().map(|path| RelPath::new(path)).collect();
        search.prepare_full_source_snapshot(PID, 0, 1, &identities, &cancel).await.unwrap();
        let docs: Vec<_> = paths.iter().enumerate()
            .map(|(i,path)| doc("memory", 1, i as u64 + 1, path, "rust", "selected source"))
            .collect();
        search.index_docs(PID, &docs, &cancel).await.unwrap();
        search.seal_source_snapshot(PID, 1).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial, 1).unwrap());

        let requested = if cfg!(windows) { SCOPE.to_uppercase() } else { SCOPE.to_owned() };
        let start = json!({"project_id":PID,"scope":requested,"exclude_paths":[EXCLUDED],"limit":2});
        let first = navigate(&engram, start).await;
        assert_eq!(first["coverage"]["matching_files"], 4);
        assert_eq!(first["coverage"]["area_sample_truncated"], false);
        let areas = first["areas"].as_array().unwrap();
        let actual: std::collections::BTreeMap<_,_> = areas.iter()
            .map(|area|(area["scope"].as_str().unwrap().to_owned(), area["selected_files"].as_u64().unwrap()))
            .collect();
        assert_eq!(actual, std::collections::BTreeMap::from([
            (format!("{SCOPE}/controllers"), 2),
            (format!("{SCOPE}/services"), 1),
            (format!("{SCOPE}/entry.rs"), 1),
        ]), "Drill-down must expose immediate children, not repeat the current/ancestor scope");
        for area in areas {
            assert_eq!(area["explore"]["arguments"]["exclude_paths"], json!([EXCLUDED]));
            let child = navigate(&engram, area["explore"]["arguments"].clone()).await;
            assert_eq!(child["source_generation"], 1);
            let boundary = area["scope"].as_str().unwrap();
            assert!(child["files"].as_array().unwrap().iter().all(|file| {
                let path = file["path"].as_str().unwrap();
                path == boundary || path.starts_with(&format!("{boundary}/"))
            }));
            for next in child["areas"].as_array().unwrap() {
                let narrower = next["scope"].as_str().unwrap();
                assert!(narrower.starts_with(&format!("{boundary}/")), "Area actions must strictly narrow: {boundary} -> {narrower}");
                let grandchild = navigate(&engram, next["explore"]["arguments"].clone()).await;
                assert_eq!(grandchild["source_generation"], 1);
                assert!(grandchild["files"].as_array().unwrap().iter().all(|file| {
                    let path = file["path"].as_str().unwrap();
                    path == narrower || path.starts_with(&format!("{narrower}/"))
                }));
            }
            if boundary.ends_with("entry.rs") {
                assert_eq!(child["detail"], "exact_file");
                assert!(child["areas"].as_array().unwrap().is_empty());
            }
        }
        // Area discovery/display caps must not hide file identities or break
        // the independent lexical continuation, including a one-file child.
        let mut page = navigate(&engram, json!({"project_id":PID,"scope":CAPPED,"limit":8})).await;
        assert_eq!(page["coverage"]["matching_files"], 130);
        assert_eq!(page["coverage"]["area_sample_truncated"], true);
        assert_eq!(page["areas"].as_array().unwrap().len(), 32);
        let one = navigate(&engram, page["areas"][0]["explore"]["arguments"].clone()).await;
        assert_eq!(one["coverage"]["matching_files"], 1);
        assert!(one["areas"][0]["scope"].as_str().unwrap().ends_with("/Leaf.rs"));
        let mut seen = std::collections::BTreeSet::new();
        loop {
            assert_eq!(page["source_generation"], 1);
            for file in page["files"].as_array().unwrap() {
                assert!(seen.insert(file["path"].as_str().unwrap().to_owned()), "File continuation must not repeat identities");
            }
            if page["next_page"].is_null() { break; }
            page = navigate(&engram, page["next_page"]["arguments"].clone()).await;
        }
        assert_eq!(seen, paths.iter().filter(|path|path.starts_with(&format!("{CAPPED}/"))).cloned().collect());
        for scope in [EXCLUDED,"src/application/absent"] {
            let empty = navigate(&engram, json!({"project_id":PID,"scope":scope,"exclude_paths":[EXCLUDED]})).await;
            assert!(empty["files"].as_array().unwrap().is_empty());
            assert!(empty["areas"].as_array().unwrap().is_empty());
        }
    });
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
        sources.extend([(JOB,"pub fn RunJob() -> i32 { 7 }\npub fn ResolveZoom() -> i32 { 7 }\n"),
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
                    node(&format!("sym:function:{POLICY}:FutureOnly:1"),
                        "function", "FutureOnly", POLICY, 2),
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
        let first_text=navigation_text(&engram,file_action["arguments"].clone()).await;
        assert!(!first_text.contains("--- File Types") && !first_text.contains("--- Language Breakdown"));
        let first_page=navigation(&first_text);
        assert_eq!(first_page["detail"],"exact_file");
        assert!(first_page["areas"].as_array().unwrap().is_empty());
        assert!(first_page.get("other_inventories").is_none());
        let mut retained_file=file_action["arguments"].clone();
        retained_file["include_retained_context"]=json!(true);
        let retained_text=navigation_text(&engram,retained_file).await;
        let retained_page=navigation(&retained_text);
        assert!(retained_text.contains("--- File Types") && retained_text.contains("--- Retained Corpus Inventory"));
        assert_eq!(first_page["files"],retained_page["files"],"Compact file navigation must retain every declaration, source action and continuation");
        for flag in ["managed_source","matching_files","inspected_file_identities","scan_truncated","area_sample_truncated","representative_sample","declarations"] {
            assert_eq!(first_page["coverage"][flag],retained_page["coverage"][flag]);
        }
        let small=navigation_text(&engram,json!({"project_id":PID,"scope":JOB,"limit":1})).await;
        assert!(small.len()<4*1024,"A two-declaration inspection must leave task context: {}bytes",small.len());
        assert_eq!(navigation(&small)["files"][0]["symbols"].as_array().unwrap().len(),2);
        let single_file_area=navigation_text(&engram,json!({"project_id":PID,"scope":"src/bbb-jobs","limit":1})).await;
        assert!(single_file_area.contains("--- File Types"),"A directory with one selected file still needs area/project distributions");
        assert!(navigation(&single_file_area).get("detail").is_none());
        for scope in [PRIVATE,FUTURE,"src/missing.rs"] {
            let empty=navigation_text(&engram,json!({"project_id":PID,"scope":scope,"exclude_paths":[PRIVATE]})).await;
            let empty_page=navigation(&empty);
            assert!(empty_page["files"].as_array().unwrap().is_empty());
            assert!(empty_page.get("detail").is_none(),"Unobserved paths must not become exact-file matches");
            assert!(empty.contains("--- File Types"),"Unobserved scope keeps the ordinary overview's explicit coverage");
        }
        let first_policy=&first_page["files"][0];
        assert_eq!(first_policy["symbols"].as_array().unwrap().len(),16);
        for symbol in first_policy["symbols"].as_array().unwrap() {
            let actions=symbol["actions"].as_array().unwrap();
            assert_eq!(actions.len(),1,"Declaration pages must leave context room for the agent's task instead of repeating richer tool calls");
            assert_eq!(actions[0]["tool"],"get_full_method_body");
        }
        assert!(first_policy["symbols"].as_array().unwrap().iter()
            .all(|symbol|symbol["name"]!="ResolveZoom"),
            "The wanted declaration is intentionally beyond the first file symbol page");
        let continuation=&first_policy["next_symbols"];
        assert_eq!(continuation["tool"],"get_codebase_overview");
        assert_eq!(continuation["arguments"]["scope"],POLICY);
        assert_eq!(continuation["arguments"]["exclude_paths"],json!(["Site/Q",PRIVATE]));
        let later_text=navigation_text(&engram,continuation["arguments"].clone()).await;
        let later_page=navigation(&later_text);
        assert_eq!(later_page["detail"],"exact_file");
        assert!(!later_text.contains("--- File Types") && !later_text.contains("--- Language Breakdown"));
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
        if !crowded {
            let wanted_id=format!("sym:function:{POLICY}:ResolveZoom:1");
            let select=|id:&str|json!({"project_id":PID,"scope":POLICY,
                "exclude_paths":["Site/Q",PRIVATE],"symbol_node_id":id});
            let compact_text=navigation_text(&engram,select(&wanted_id)).await;
            assert!(compact_text.len()<4*1024,
                "A short exact-symbol inspection must leave room for the agent's task: {}bytes",compact_text.len());
            assert!(!compact_text.contains("--- File Types") && !compact_text.contains("--- Language Breakdown"));
            let expanded=navigation(&compact_text);
            assert_eq!(expanded["detail"],"exact_symbol");
            assert!(expanded.get("other_inventories").is_none());
            assert!(expanded["areas"].as_array().unwrap().is_empty());
            assert_eq!(expanded["scope"],POLICY);
            assert_eq!(expanded["exclude_paths"],json!(["Site/Q",PRIVATE]));
            assert_eq!(expanded["working_tree_checked"],false);
            for flag in ["managed_source","matching_files","inspected_file_identities","scan_truncated","area_sample_truncated","representative_sample"] {
                assert_eq!(expanded["coverage"][flag],later_page["coverage"][flag],"Narrow rendering must preserve discovery coverage: {flag}");
            }
            assert_eq!(expanded["source_generation"],1);
            let observation=&expanded["coverage"]["declarations"][0];
            assert_eq!(observation["observation"],"exact_bounded_identity_lookup");
            assert_eq!(observation["declaration_inventory"],"not_scanned");
            assert_eq!(observation["declaration_admission"]["nodes_observed"],1);
            assert!(observation.get("seed_discovery").is_none());
            let file=&expanded["files"][0];
            assert_eq!(file["declarations_observed"],1);
            assert_eq!(file["declaration_inventory"],"not_scanned");
            assert_eq!(file["symbol_selection"]["observed"],true);
            assert_eq!(file["symbols"].as_array().unwrap().len(),1);
            assert!(file["next_symbols"].is_null(),"Exact identity selection is not another declaration page");
            let declaration=&file["symbols"][0];
            assert_eq!(declaration["node_id"],wanted_id);
            assert_eq!(declaration["start_line"],1);
            let actions=declaration["actions"].as_array().unwrap();
            let read=actions.iter().find(|action|action["tool"]=="get_full_method_body").unwrap();
            assert_eq!(read["arguments"]["project_id"],PID);
            assert_eq!(read["arguments"]["file_path"],POLICY);
            assert_eq!(read["arguments"]["line_start"],1);
            assert_eq!(read["arguments"]["line_end"],1);
            assert_eq!(read["arguments"]["context_lines"],0);
            assert_eq!(read["arguments"]["output_json"],true);
            let source=engram.handle_get_full_method_body(serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
            let source: Value=serde_json::from_str(&source.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(source["source_file_hash"],ContentHash::compute(policy_source.as_bytes()).0);
            assert_eq!(source["source_code"].as_str().unwrap().trim(),POLICY_TEXT.trim());
            assert!(source["freshness"]["interpretation"].as_str().unwrap().contains("call-binding"));
            let normal=navigation_text(&engram,file["actions"][0]["arguments"].clone()).await;
            assert_eq!(navigation(&normal)["detail"],"exact_file");
            assert_eq!(navigation(&normal)["files"][0]["symbols"].as_array().unwrap().len(),16,
                "The exact-symbol file action must retain normal declaration paging");
            let mut retained_request=select(&wanted_id);
            retained_request["include_retained_context"]=json!(true);
            let retained=navigation_text(&engram,retained_request).await;
            assert!(retained.contains("--- Retained Corpus Inventory") && retained.contains("--- File Types"),
                "Explicit retained context must not be suppressed by the narrow response");
            let inspect=actions.iter().find(|a|a["tool"]=="get_method_edit_context").unwrap();
            let context=engram.handle_get_method_edit_context(serde_json::from_value(inspect["arguments"].clone()).unwrap()).await.unwrap();
            assert!(!context.is_error.unwrap_or(false));
            let context:Value=serde_json::from_str(&context.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(context["method_info"]["file_path"],POLICY,
                "A same-named declaration in another file must not receive this inspection");
            assert_eq!(context["method_info"]["line_start"],1);
            let impact=actions.iter().find(|a|a["tool"]=="impact_analysis").unwrap();
            assert_eq!(impact["arguments"]["symbol_fqn"],wanted_id);
            assert_eq!(impact["arguments"]["detail"],"summary");
            let impact=engram.handle_impact_analysis(serde_json::from_value(impact["arguments"].clone()).unwrap()).await.unwrap();
            assert!(!impact.is_error.unwrap_or(false));
            let text=impact.content.iter().filter_map(|c|c.as_text()).map(|c|c.text.as_str()).collect::<String>();
            assert!(text.len()<=12*1024,"The selected impact action must preserve room for the agent's task while retaining graph expansion");
            let packet:Value=serde_json::from_str(text.split_once("## Revision-admitted change impact").unwrap().1
                .split_once("```json").unwrap().1.split_once("```").unwrap().0).unwrap();
            assert_eq!(packet["target_revision_admitted"],true,
                "A successful RPC with unavailable evidence does not prove the selected action recovered its admitted target");
            assert_eq!(packet["target"]["node_id"],wanted_id);
            assert_eq!(packet["source_generation"],1);
            assert_eq!(packet["graph_continuation"]["arguments"]["symbol_fqn"],wanted_id);
            for id in ["ResolveZoom".to_owned(),format!("sym:function:{JOB}:ResolveZoom:2"),
                format!("sym:function:{PRIVATE}:PrivateOnly:1"),format!("sym:function:{POLICY}:FutureOnly:1"),
                "sym:function:src/missing.rs:Unknown:1".to_owned()] {
                let missing=navigate(&engram,select(&id)).await;
                let file=&missing["files"][0];
                assert_eq!(file["symbol_selection"]["observed"],false,
                    "Wrong-file, excluded, future and unknown identities must not fall back to a same-named admitted declaration");
                assert!(file["symbols"].as_array().unwrap().is_empty());
                assert!(file["next_symbols"].is_null());
                assert_eq!(file["declarations_observed"],0);
                assert_eq!(file["declaration_inventory"],"not_scanned");
                assert!(missing["limitations"].to_string().contains("Not observed is not absent"));
            }
            for arguments in [
                json!({"project_id":PID,"scope":PRIVATE,"exclude_paths":[PRIVATE],"symbol_node_id":wanted_id}),
                json!({"project_id":PID,"scope":"src/zzz-map","symbol_node_id":wanted_id}),
                json!({"project_id":PID,"symbol_node_id":wanted_id}),
                json!({"project_id":PID,"scope":POLICY,"symbol_node_id":""}),
                json!({"project_id":PID,"scope":POLICY,"symbol_node_id":"name\n"}),
                json!({"project_id":PID,"scope":POLICY,"symbol_node_id":"x".repeat(2049)}),
                json!({"project_id":PID,"scope":POLICY,"symbol_node_id":wanted_id,"symbol_offset":16}),
                json!({"project_id":PID,"scope":POLICY,"symbol_node_id":wanted_id,"offset":1}),
            ] {
                assert!(engram.handle_get_codebase_overview(serde_json::from_value(arguments).unwrap()).await.is_err());
            }
        }
    });
}

#[test]
fn exact_symbol_opens_known_identity_beyond_discovery_without_claiming_inventory() {
    owned_case(true, |engram, search| async move {
        const TARGET: &str = "src/late/crowded.rs";
        const DECOY: &str = "src/aaa/earlier.rs";
        const MISSING: &str = "src/late/missing-file-node.rs";
        const WANTED_LINE: u32 = 146;
        const WANTED: &str = "pub fn ResolveZoom() -> i32 { 42 }\n";
        let root = std::path::PathBuf::from(
            engram.state.registry.get_project(PID).unwrap().unwrap().directory,
        );
        let mut source = (0..145)
            .map(|i| format!("pub fn Aaa{i:03}() -> i32 {{ {i} }}\n"))
            .collect::<String>();
        source.push_str(WANTED);
        let bodies = [(TARGET, source.as_str()),
            (DECOY, "pub fn ResolveZoom() -> i32 { 7 }\n"),
            (MISSING, "pub fn Missing() {}\n")];
        let cancel = CancellationToken::new();
        let initial = engram.state.registry.capture_source_head(PID).unwrap();
        search.prepare_full_source_snapshot(PID, 0, 1,
            &bodies.map(|(path, _)| RelPath::new(path)), &cancel).await.unwrap();
        let mut docs = Vec::new();
        let mut graph_nodes = Vec::new();
        for (i, (path, body)) in bodies.iter().enumerate() {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), body).unwrap();
            let mut indexed = doc("memory", 1, i as u64 + 1, path, "rust", body);
            indexed.end_line = body.lines().count() as u32;
            docs.push(indexed);
            if *path != MISSING {
                let mut file = node(&format!("file:{path}"), "file", path, path, 1);
                file.end_line = body.lines().count() as u32;
                file.metadata = Some(json!({"file_hash":ContentHash::compute(body.as_bytes()).0}));
                graph_nodes.push(file);
            }
            for (line, text) in body.lines().enumerate() {
                let name = text.strip_prefix("pub fn ").unwrap().split('(').next().unwrap();
                let line = line as u32 + 1;
                let mut declaration = node(&format!("sym:function:{path}:{name}:{line}"),
                    "function", name, path, 1);
                declaration.start_line = line;
                declaration.end_line = line;
                declaration.metadata = Some(json!({}));
                graph_nodes.push(declaration);
            }
        }
        search.index_docs(PID, &docs, &cancel).await.unwrap();
        search.seal_source_snapshot(PID, 1).unwrap();
        engram.state.graph.upsert_nodes(PID, &graph_nodes).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial, 1).unwrap());
        let wanted_id = format!("sym:function:{TARGET}:ResolveZoom:{WANTED_LINE}");
        let wanted = engram.state.graph.get_node(PID, &wanted_id).unwrap().unwrap();
        let file = engram.state.graph.get_node(PID, &format!("file:{TARGET}")).unwrap().unwrap();
        let select = |id: &str| json!({"project_id":PID,"scope":TARGET,
            "symbol_node_id":id,"offset":0,"symbol_offset":0});
        let ordinary = navigate(&engram, json!({"project_id":PID,"scope":TARGET})).await;
        assert_eq!(ordinary["files"][0]["declarations_observed"], 128);
        assert!(ordinary["files"][0]["symbols"].as_array().unwrap().iter()
            .all(|row| row["node_id"] != wanted_id));

        let text = navigation_text(&engram, select(&wanted_id)).await;
        assert!(text.len() < 4 * 1024, "{} bytes", text.len());
        let report = navigation(&text);
        assert_eq!(report["source_generation"], 1);
        assert_eq!(report["detail"], "exact_symbol");
        assert_eq!(report["working_tree_checked"], false);
        let selected = &report["files"][0];
        assert_eq!(selected["path"], TARGET);
        assert_eq!(selected["physical_generation"], 1);
        assert_eq!(selected["declarations_observed"], 1);
        assert_eq!(selected["declaration_inventory"], "not_scanned");
        assert_eq!(selected["symbol_selection"]["observed"], true);
        assert!(selected["next_symbols"].is_null());
        assert_eq!(selected["symbols"].as_array().unwrap().len(), 1);
        let observation = &report["coverage"]["declarations"][0];
        assert_eq!(observation["observation"], "exact_bounded_identity_lookup");
        assert_eq!(observation["declaration_inventory"], "not_scanned");
        assert_eq!(observation["records_attempted"], 2);
        assert!(observation["raw_record_bytes_decoded"].as_u64().unwrap() <= 128 * 1024);
        assert!(observation.get("seed_discovery").is_none());
        let declaration = &selected["symbols"][0];
        assert_eq!(declaration["node_id"], wanted_id);
        assert_eq!(declaration["start_line"], WANTED_LINE);
        assert_eq!(declaration["end_line"], WANTED_LINE);
        let actions = declaration["actions"].as_array().unwrap();
        let read = actions.iter().find(|a| a["tool"] == "get_full_method_body").unwrap();
        let reply = engram.handle_get_full_method_body(
            serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
        assert!(!reply.is_error.unwrap_or(false));
        let read: Value = serde_json::from_str(&reply.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(read["file_path"], TARGET);
        assert_eq!(read["line_start"], WANTED_LINE);
        assert_eq!(read["line_end"], WANTED_LINE);
        assert_eq!(read["source_code"], WANTED.trim_end());
        assert_eq!(read["source_file_hash_algorithm"], "blake3");
        assert_eq!(read["source_file_hash"], ContentHash::compute(source.as_bytes()).0);
        let edit = actions.iter().find(|a| a["tool"] == "get_method_edit_context").unwrap();
        let reply = engram.handle_get_method_edit_context(
            serde_json::from_value(edit["arguments"].clone()).unwrap()).await.unwrap();
        assert!(!reply.is_error.unwrap_or(false));
        let edit: Value = serde_json::from_str(&reply.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(edit["method_info"]["file_path"], TARGET);
        assert_eq!(edit["method_info"]["line_start"], WANTED_LINE);
        let impact = actions.iter().find(|a| a["tool"] == "impact_analysis").unwrap();
        assert_eq!(impact["arguments"]["symbol_fqn"], wanted_id);
        let reply = engram.handle_impact_analysis(
            serde_json::from_value(impact["arguments"].clone()).unwrap()).await.unwrap();
        assert!(!reply.is_error.unwrap_or(false));
        let impact_text = reply.content.iter().filter_map(|c| c.as_text())
            .map(|c| c.text.as_str()).collect::<String>();
        assert!(impact_text.len() <= 12 * 1024);
        let impact: Value = serde_json::from_str(impact_text
            .split_once("## Revision-admitted change impact").unwrap().1
            .split_once("```json").unwrap().1.split_once("```").unwrap().0).unwrap();
        assert_eq!(impact["source_generation"], 1);
        assert_eq!(impact["target_revision_admitted"], true);
        assert_eq!(impact["target"]["node_id"], wanted_id);
        assert_eq!(impact["graph_continuation"]["arguments"]["symbol_fqn"], wanted_id);

        // Rejected records cannot borrow a selected file or leak their fields.
        for scenario in ["stale", "future", "history", "foreign", "oversized", "long_name"] {
            let mut bad = wanted.clone();
            match scenario {
                "stale" => bad.generation = 0,
                "future" => bad.generation = 2,
                "history" => bad.namespace = "history".into(),
                "foreign" => bad.file_path = RelPath::new(DECOY),
                "oversized" => bad.metadata = Some(json!({"payload":"x".repeat(65536)})),
                _ => bad.name = "x".repeat(2049),
            }
            if scenario != "long_name" { bad.name = "FOREIGN_POINT_SENTINEL".into(); }
            engram.state.graph.upsert_nodes(PID, &[bad]).unwrap();
            let text = navigation_text(&engram, select(&wanted_id)).await;
            let rejected = navigation(&text);
            assert_eq!(rejected["files"][0]["symbol_selection"]["observed"], false, "{scenario}");
            assert!(rejected["files"][0]["symbols"].as_array().unwrap().is_empty());
            assert_eq!(rejected["files"][0]["declarations_observed"],0);
            assert!(!text.contains("FOREIGN_POINT_SENTINEL"));
            engram.state.graph.upsert_nodes(PID, &[wanted.clone()]).unwrap();
        }
        for scenario in ["stale", "future", "history", "oversized"] {
            let mut bad_file = file.clone();
            match scenario {
                "stale" => bad_file.generation = 0,
                "future" => bad_file.generation = 2,
                "history" => bad_file.namespace = "history".into(),
                _ => bad_file.metadata = Some(json!({"payload":"x".repeat(65536)})),
            }
            engram.state.graph.upsert_nodes(PID, &[bad_file]).unwrap();
            let rejected = navigate(&engram, select(&wanted_id)).await;
            assert_eq!(rejected["files"][0]["symbol_selection"]["observed"], false,"{scenario}");
            assert!(rejected["files"][0]["symbols"].as_array().unwrap().is_empty());
            engram.state.graph.upsert_nodes(PID, &[file.clone()]).unwrap();
        }
        let missing = navigate(&engram, json!({"project_id":PID,"scope":MISSING,
            "symbol_node_id":format!("sym:function:{MISSING}:Missing:1")})).await;
        assert_eq!(missing["files"][0]["symbol_selection"]["observed"], false);
        assert!(missing["files"][0]["symbols"].as_array().unwrap().is_empty());
    });
}

async fn excerpt(engram: &Engram, arguments: Value) -> (String, Value) {
    let result = engram
        .handle_get_full_method_body(serde_json::from_value(arguments).unwrap())
        .await
        .unwrap();
    assert!(!result.is_error.unwrap_or(false));
    let text = result.content[0].as_text().unwrap().text.clone();
    assert!(
        text.len() <= 32 * 1024,
        "Complete rendered excerpt, including escaping and freshness, must fit the agent budget"
    );
    let value = serde_json::from_str(&text).unwrap();
    (text, value)
}

#[test]
fn current_file_excerpt_pages_preserve_lines_and_hashes_without_claiming_indexed_method_boundaries()
{
    owned_case(true, |engram, _search| async move {
        let root = std::path::PathBuf::from(
            engram
                .state
                .registry
                .get_project(PID)
                .unwrap()
                .unwrap()
                .directory,
        );
        let lines: Vec<_> = (1..=245)
            .map(|i| {
                if i == 123 {
                    String::new()
                } else {
                    format!("SELECT {i}; -- 日本 🙂 \\\" ")
                }
            })
            .collect();
        let source = format!("\u{feff}{}\r\n", lines.join("\r\n"));
        std::fs::write(root.join("procedure.sql"), &source).unwrap();
        let expected: Vec<_> = source.lines().map(str::to_owned).collect();
        let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        let mut arguments = json!({"project_id":PID,"file_path":"procedure.sql","output_json":true,
            "context_lines":20,"include_caller_bodies":true});
        let mut recovered = Vec::new();
        let mut next_line = 1u64;
        let mut first_next = None;
        loop {
            let (_, value) = excerpt(&engram, arguments).await;
            assert_eq!(value["retrieval_scope"], "current_file_excerpt");
            assert_eq!(
                value["source_verification"],
                "current_file_snapshot_not_index_verified"
            );
            assert_eq!(value["source_file_hash"], hash);
            assert_eq!(value["source_file_hash_algorithm"], "blake3");
            assert_eq!(value["language"], "sql");
            assert_eq!(value["line_start"], next_line);
            assert_eq!(value["file_excerpt"]["total_lines"], 245);
            assert!(value["file_excerpt"]["returned_lines"].as_u64().unwrap() <= 120);
            assert!(value["surrounding_context"].as_str().unwrap().is_empty());
            assert!(value["caller_bodies"].as_array().unwrap().is_empty());
            if next_line == 1 {
                assert_eq!(value["caller_expansion"]["requested"], true);
                assert_eq!(value["caller_expansion"]["attempted"], false);
                assert_eq!(
                    value["caller_expansion"]["status"],
                    "unsupported_file_excerpt"
                );
            }
            let count = value["file_excerpt"]["returned_lines"].as_u64().unwrap();
            assert!(count > 0);
            assert_eq!(value["line_end"].as_u64().unwrap(), next_line + count - 1);
            recovered.extend(
                value["source_code"]
                    .as_str()
                    .unwrap()
                    .split('\n')
                    .map(str::to_owned),
            );
            assert!(
                recovered.len() <= expected.len(),
                "Continuation must make progress rather than repeat forever"
            );
            let next = &value["file_excerpt"]["next_page"];
            if next.is_null() {
                assert_eq!(value["file_excerpt"]["reaches_eof"], true);
                break;
            }
            assert_eq!(next["tool"], "get_full_method_body");
            next_line = value["line_end"].as_u64().unwrap() + 1;
            assert_eq!(next["arguments"]["line_start"], next_line);
            first_next.get_or_insert_with(|| next["arguments"].clone());
            arguments = next["arguments"].clone();
        }
        assert_eq!(
            recovered, expected,
            "Unchanged-file pagination must preserve every complete line, including blanks, BOM and Unicode"
        );
        let changed = format!("-- inserted after the first observation\r\n{source}");
        std::fs::write(root.join("procedure.sql"), &changed).unwrap();
        let (_, fresh) = excerpt(&engram, first_next.unwrap()).await;
        assert_ne!(
            fresh["source_file_hash"], hash,
            "A fresh page must not silently reuse an earlier file snapshot"
        );
        assert_eq!(
            fresh["source_file_hash"],
            blake3::hash(changed.as_bytes()).to_hex().to_string()
        );
        assert_eq!(
            fresh["source_verification"],
            "current_file_snapshot_not_index_verified"
        );
    });
}

#[test]
fn current_file_excerpts_bound_escaped_output_and_refuse_unpageable_or_invalid_inputs() {
    owned_case(true, |engram, _search| async move {
        let root = std::path::PathBuf::from(
            engram
                .state
                .registry
                .get_project(PID)
                .unwrap()
                .unwrap()
                .directory,
        );
        let line = format!("-- 日本🙂 {}", "\0\"\\\t".repeat(80));
        let source = (0..150)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("escaped.sql"), &source).unwrap();
        let (_, value) = excerpt(
            &engram,
            json!({"project_id":PID,"file_path":"escaped.sql","output_json":true}),
        )
        .await;
        let returned = value["file_excerpt"]["returned_lines"].as_u64().unwrap() as usize;
        assert!(
            returned > 0 && returned < 120,
            "Escaped output must be budgeted, not just raw source bytes"
        );
        assert_eq!(
            value["source_code"],
            vec![line.as_str(); returned].join("\n")
        );
        let next = &value["file_excerpt"]["next_page"];
        assert_eq!(next["arguments"]["line_start"], returned + 1);
        let (_, continued) = excerpt(&engram, next["arguments"].clone()).await;
        assert_eq!(continued["line_start"], returned + 1);
        for (file, content) in [
            ("empty.sql", String::new()),
            ("fenced.sql", "SELECT '```';\nSELECT 2;\n".into()),
        ] {
            std::fs::write(root.join(file), &content).unwrap();
            let (_, value) = excerpt(
                &engram,
                json!({"project_id":PID,"file_path":file,"output_json":true}),
            )
            .await;
            assert_eq!(
                value["source_file_hash"],
                ContentHash::compute(content.as_bytes()).0
            );
            assert!(value["file_excerpt"]["next_page"].is_null());
            if content.is_empty() {
                assert_eq!(value["line_start"], 0);
                assert_eq!(value["line_end"], 0);
                assert_eq!(value["file_excerpt"]["returned_lines"], 0);
            }
            let result = engram
                .handle_get_full_method_body(
                    serde_json::from_value(json!({"project_id":PID,"file_path":file})).unwrap(),
                )
                .await
                .unwrap();
            let text = &result.content[0].as_text().unwrap().text;
            assert!(text.len() <= 32 * 1024 && text.contains("# Current File Excerpt"));
            assert!(!text.contains("Full Method Body"));
            if file == "fenced.sql" {
                assert!(text.contains("````sql\nSELECT '```';"));
            }
        }
        std::fs::write(root.join("oversized-line.sql"), "x".repeat(40_000)).unwrap();
        std::fs::write(
            root.join("oversized-file.sql"),
            vec![b'x'; 4 * 1024 * 1024 + 1],
        )
        .unwrap();
        std::fs::write(root.join("invalid.sql"), [0xff, 0xfe]).unwrap();
        for file in [
            "oversized-line.sql",
            "oversized-file.sql",
            "invalid.sql",
            "missing.sql",
            ".",
        ] {
            assert!(
                engram
                    .handle_get_full_method_body(
                        serde_json::from_value(
                            json!({"project_id":PID,"file_path":file,"output_json":true})
                        )
                        .unwrap()
                    )
                    .await
                    .is_err(),
                "{file} must fail without partial/lossy source or an unbounded response"
            );
        }
        for args in [
            json!({"file_path":"../outside.sql"}),
            json!({"file_path":"/outside.sql"}),
            json!({"file_path":"empty.sql","line_start":2}),
            json!({"file_path":"escaped.sql","line_start":0}),
            json!({"file_path":"escaped.sql","line_start":0,"line_end":0}),
            json!({"file_path":"escaped.sql","line_end":2}),
            json!({"file_path":"escaped.sql","fqn":"Any.Method"}),
        ] {
            let mut args = args;
            args["project_id"] = json!(PID);
            assert!(
                engram
                    .handle_get_full_method_body(serde_json::from_value(args).unwrap())
                    .await
                    .is_err()
            );
        }
    });
}

#[test]
fn codemap_and_both_impact_tools_recover_selected_sql_with_zero_declaration_bounds() {
    owned_case(true, |engram, search| async move {
        const SQL: &str = "db/Lockout.sql";
        let source = "CREATE PROCEDURE Lockout\nAS\nSELECT 1;\n";
        let root = std::path::PathBuf::from(
            engram
                .state
                .registry
                .get_project(PID)
                .unwrap()
                .unwrap()
                .directory,
        );
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join(SQL), source).unwrap();
        let cancel = CancellationToken::new();
        let before = engram.state.registry.capture_source_head(PID).unwrap();
        search
            .prepare_full_source_snapshot(PID, 0, 1, &[RelPath::new(SQL)], &cancel)
            .await
            .unwrap();
        let mut source_doc = doc("memory", 1, 1, SQL, "sql", source);
        source_doc.end_line = 3;
        search
            .index_docs(PID, &[source_doc], &cancel)
            .await
            .unwrap();
        search.seal_source_snapshot(PID, 1).unwrap();
        let mut file = node(&format!("file:{SQL}"), "file", SQL, SQL, 1);
        file.start_line = 0;
        file.end_line = 0;
        file.metadata = Some(json!({"file_hash":ContentHash::compute(source.as_bytes()).0}));
        engram.state.graph.upsert_nodes(PID, &[file]).unwrap();
        engram
            .state
            .registry
            .publish_source_head(&before, 1)
            .unwrap();
        let map = navigate(&engram, json!({"project_id":PID,"scope":SQL})).await;
        assert_eq!(map["detail"],"exact_file");
        assert!(map["files"][0]["symbols"].as_array().unwrap().is_empty());
        assert!(map["limitations"].to_string().contains("current-file excerpts"));
        let mut actions = vec![
            map["files"][0]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["tool"] == "get_full_method_body")
                .unwrap()
                .clone(),
        ];
        for tool in ["impact_analysis", "compute_blast_radius"] {
            let mut args = json!({"project_id":PID,"file_path":SQL,"detail":"summary"});
            let result = if tool == "impact_analysis" {
                engram
                    .handle_impact_analysis(serde_json::from_value(args).unwrap())
                    .await
                    .unwrap()
            } else {
                args["include_guidance"] = json!(false);
                engram
                    .handle_compute_blast_radius(serde_json::from_value(args).unwrap())
                    .await
                    .unwrap()
            };
            assert!(!result.is_error.unwrap_or(false));
            let text = &result.content[0].as_text().unwrap().text;
            let packet: Value = serde_json::from_str(
                text.split_once("## Revision-admitted change impact")
                    .unwrap()
                    .1
                    .split_once("```json")
                    .unwrap()
                    .1
                    .split_once("```")
                    .unwrap()
                    .0,
            )
            .unwrap();
            assert_eq!(packet["target_revision_admitted"], true);
            assert_eq!(packet["working_tree_checked"], false);
            assert_eq!(
                packet["target"]["source_read_scope"],
                "current_file_excerpt"
            );
            actions.push(packet["target"]["source_read"].clone());
        }
        for action in actions {
            assert_eq!(action["tool"], "get_full_method_body");
            let (_, value) = excerpt(&engram, action["arguments"].clone()).await;
            assert_eq!(value["retrieval_scope"], "current_file_excerpt");
            assert_eq!(value["language"], "sql");
            assert_eq!(
                value["source_file_hash"],
                ContentHash::compute(source.as_bytes()).0
            );
            assert_eq!(value["source_code"], source.trim_end_matches('\n'));
            assert_eq!(value["line_start"], 1);
            assert_eq!(value["line_end"], 3);
        }
        let excluded = navigate(
            &engram,
            json!({"project_id":PID,"scope":SQL,"exclude_paths":[SQL]}),
        )
        .await;
        assert!(excluded["files"].as_array().unwrap().is_empty());
    });
}
