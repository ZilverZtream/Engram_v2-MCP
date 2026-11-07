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
        let root_area=source_navigation["areas"].as_array().unwrap().iter()
            .find(|area|area["scope"]=="src").unwrap();
        assert_eq!(root_area["selected_files"],2);
        let source_children=navigate(&engram,root_area["explore"]["arguments"].clone()).await;
        for scope in ["src/service", "src/model"] {
            let area = source_children["areas"]
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
        let filtered=navigate(&engram,json!({"project_id":PID,"path_contains":"Catalog"})).await;
        assert_eq!(filtered["detail"],"filtered_paths");
        assert_eq!(filtered["coverage"]["managed_source"],false);
        assert!(filtered["coverage"]["matching_files"].is_null(),"Attested legacy file membership stays unknown, never fabricated zero matches");
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
fn filtered_codemap_preserves_search_and_explicitly_browses_neighboring_selected_files() {
    owned_case(true, |engram, search| async move {
        const SERVICE: &str = "src/domain/api/Services/CatalogService.rs";
        const CONTROLLER: &str = "src/domain/api/Controllers/CatalogController.rs";
        const CONTRACT: &str = "src/domain/api/Contracts/CatalogContract.rs";
        const NEIGHBOR: &str = "src/domain/api/Services/State.rs";
        const PRIVATE: &str = "src/domain/api/private";
        const FUTURE: &str = "src/domain/api/Services/CatalogFuture.rs";
        let aux: String = (0..20).map(|i|format!("fn Aux{i:02}() {{}}\n")).collect();
        let current = format!("fn read() {{ let current = true; }}\n{aux}");
        let mut sources = vec![
            (SERVICE.to_owned(), format!("fn read() {{ let old = true; }}\n{aux}")),
            (CONTROLLER.to_owned(), "fn read() {}\n".to_owned()),
            (CONTRACT.to_owned(), "fn read() {}\n".to_owned()),
            (NEIGHBOR.to_owned(), "fn read() {}\n".to_owned()),
            (format!("{PRIVATE}/CatalogSecret.rs"), "fn secret() {}\n".to_owned()),
        ];
        sources.extend((0..130).map(|i|(format!("src/aaa/unrelated/File{i:03}.rs"), "fn unrelated() {}\n".to_owned())));
        let cancel = CancellationToken::new();
        let initial = engram.state.registry.capture_source_head(PID).unwrap();
        let paths: Vec<_> = sources.iter().map(|(path,_)|RelPath::new(path)).collect();
        search.prepare_full_source_snapshot(PID, 0, 1, &paths, &cancel).await.unwrap();
        let docs: Vec<_> = sources.iter().enumerate()
            .map(|(i,(path,body))|doc("memory",1,i as u64+1,path,"rust",body)).collect();
        search.index_docs(PID,&docs,&cancel).await.unwrap();
        search.seal_source_snapshot(PID,1).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let base=engram.state.registry.capture_source_head(PID).unwrap();
        search.prepare_source_snapshot(PID,1,2,&[RelPath::new(SERVICE)],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",2,2000,SERVICE,"rust",&current)],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,2).unwrap();
        assert!(engram.state.registry.publish_source_head(&base,2).unwrap());
        search.prepare_source_snapshot(PID,2,3,&[RelPath::new(FUTURE)],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",3,3000,FUTURE,"rust","unpublished")],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,3).unwrap();
        let mut nodes=Vec::new();let mut edges=Vec::new();
        for (path,_) in &sources {
            let revision=if path==SERVICE {2}else{1};
            let file=format!("file:{path}");let function=format!("sym:function:{path}:read:1");
            nodes.push(node(&file,"file",path,path,revision));
            nodes.push(node(&function,"function","read",path,revision));
            edges.push(Edge{source_id:file,target_id:function,namespace:"memory".into(),language:"rust".into(),edge_kind:EdgeKind::Contains,weight:1,generation:revision,metadata:None,updated_at_ms:1});
        }
        for i in 0..20 {
            let name=format!("Aux{i:02}");let id=format!("sym:function:{SERVICE}:{name}:{}",i+2);
            let mut declaration=node(&id,"function",&name,SERVICE,2);
            declaration.start_line=i+2;declaration.end_line=i+2;
            nodes.push(declaration);
            edges.push(Edge{source_id:format!("file:{SERVICE}"),target_id:id,namespace:"memory".into(),language:"rust".into(),edge_kind:EdgeKind::Contains,weight:1,generation:2,metadata:None,updated_at_ms:1});
        }
        for (name,revision) in [("OldRead",1),("FutureRead",3)] {
            let id=format!("sym:function:{SERVICE}:{name}:1");
            nodes.push(node(&id,"function",name,SERVICE,revision));
            edges.push(Edge{source_id:format!("file:{SERVICE}"),target_id:id,namespace:"memory".into(),language:"rust".into(),edge_kind:EdgeKind::Contains,weight:1,generation:revision,metadata:None,updated_at_ms:1});
        }
        nodes.push(node(&format!("file:{FUTURE}"),"file",FUTURE,FUTURE,3));
        engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        engram.state.graph.upsert_edges(PID,&edges).unwrap();
        let repo=std::path::PathBuf::from(engram.state.registry.get_project(PID).unwrap().unwrap().directory);
        for (path,body) in [(SERVICE,current.as_str()),(CONTROLLER,"fn read() {}\n"),(CONTRACT,"fn read() {}\n"),(NEIGHBOR,"fn read() {}\n")] {
            let file=repo.join(path);std::fs::create_dir_all(file.parent().unwrap()).unwrap();std::fs::write(file,body).unwrap();
        }
        let request=json!({"project_id":PID,"path_contains":"cAtAlOg","exclude_paths":[PRIVATE],"limit":1});
        let first_text=navigation_text(&engram,request.clone()).await;
        assert!(!first_text.contains("--- File Types") && !first_text.contains("--- Language Breakdown"));
        let mut page=navigation(&first_text);let mut observed=std::collections::BTreeMap::new();
        loop {
            assert_eq!(page["detail"],"filtered_paths");assert_eq!(page["source_generation"],2);
            assert_eq!(page["coverage"]["matching_files"],3);
            assert_eq!(page["coverage"]["representative_sample"],false);
            assert_eq!(page["coverage"]["file_order"],"lexical_filtered_paths");
            assert!(page["coverage"].get("representative_priority").is_none());
            for file in page["files"].as_array().unwrap() {
                let path=file["path"].as_str().unwrap().to_owned();
                assert!(observed.insert(path,file.clone()).is_none());
                assert!(file["symbols"].as_array().unwrap().iter().all(|symbol|symbol["name"]!="OldRead" && symbol["name"]!="FutureRead"));
            }
            if page["next_page"].is_null() {break;}
            assert_eq!(page["next_page"]["arguments"]["path_contains"],"cAtAlOg");
            assert_eq!(page["next_page"]["arguments"]["exclude_paths"],json!([PRIVATE]));
            page=navigate(&engram,page["next_page"]["arguments"].clone()).await;
        }
        assert_eq!(observed.keys().map(String::as_str).collect::<Vec<_>>(),vec![CONTRACT,CONTROLLER,SERVICE]);
        assert_eq!(observed[SERVICE]["physical_generation"],2);
        assert_eq!(observed[CONTRACT]["physical_generation"],1);
        let mut all=request.clone();all["limit"]=json!(4);
        let compact=navigation_text(&engram,all.clone()).await;
        assert!(compact.len()<12*1024,"A small selected filename search must leave room for source/impact evidence: {}bytes",compact.len());
        let result=navigation(&compact);
        let mut neighboring_files=std::collections::BTreeSet::new();
        for area in result["areas"].as_array().unwrap() {
            assert_eq!(area["explore"]["arguments"]["path_contains"],"cAtAlOg");
            let child=navigate(&engram,area["explore"]["arguments"].clone()).await;
            assert!(child["files"].as_array().unwrap().iter().all(|file|observed.contains_key(file["path"].as_str().unwrap())));
            let browse=&area["browse_without_path_filter"];
            assert_eq!(browse["tool"],"get_codebase_overview");
            assert_eq!(browse["arguments"]["scope"],area["scope"]);
            assert_eq!(browse["arguments"]["exclude_paths"],json!([PRIVATE]));
            assert!(browse["arguments"].get("path_contains").is_none());
            let neighbors=navigate(&engram,browse["arguments"].clone()).await;
            assert!(neighbors["path_contains"].is_null());
            assert!(neighbors["areas"].as_array().unwrap().iter().all(|entry|entry.get("browse_without_path_filter").is_none()));
            for file in neighbors["files"].as_array().unwrap() {
                neighboring_files.insert(file["path"].as_str().unwrap().to_owned());
                assert!(!file["path"].as_str().unwrap().starts_with(PRIVATE));
                assert_ne!(file["path"],FUTURE);
                if file["path"]==NEIGHBOR {
                    let source=engram.handle_get_full_method_body(serde_json::from_value(file["symbols"][0]["actions"][0]["arguments"].clone()).unwrap()).await.unwrap();
                    let source:Value=serde_json::from_str(&source.content[0].as_text().unwrap().text).unwrap();
                    assert_eq!(source["source_file_hash"],ContentHash::compute(b"fn read() {}\n").0);
                    assert_eq!(source["source_code"],"fn read() {}");
                }
            }
        }
        assert!(neighboring_files.contains(NEIGHBOR),"Browsing a matching area must recover selected neighbors whose names do not contain the search term");
        let mut crowded=request.clone();crowded["scope"]=json!("src/domain/api");crowded["limit"]=json!(4);
        let mut exclusions=vec![PRIVATE.to_owned()];
        exclusions.extend((0..15).map(|i|format!("ignored/{i:02}/{}","x".repeat(250))));
        assert!(exclusions.iter().map(String::len).sum::<usize>()<=4096);
        crowded["exclude_paths"]=json!(exclusions);
        let mut crowded_files=std::collections::BTreeSet::new();let mut browsing_omitted=false;
        loop {
            let text=navigation_text(&engram,crowded).await;
            assert!(text.len()<64*1024);
            let page=navigation(&text);
            assert_eq!(page["coverage"]["matching_files"],3);
            browsing_omitted|=page["coverage"]["browse_actions_omitted"].as_u64().unwrap_or(0)>0;
            for file in page["files"].as_array().unwrap() {
                assert!(crowded_files.insert(file["path"].as_str().unwrap().to_owned()));
            }
            if page["next_page"].is_null() {break;}
            assert_eq!(page["next_page"]["arguments"]["exclude_paths"],json!(exclusions));
            assert_eq!(page["next_page"]["arguments"]["path_contains"],"cAtAlOg");
            crowded=page["next_page"]["arguments"].clone();
        }
        assert!(browsing_omitted,"Optional sibling browsing must yield to existing filtered navigation when exclusions crowd the area budget");
        assert_eq!(crowded_files,observed.keys().cloned().collect(),"Crowding must leave every filtered file reachable through its preserved continuation");
        let file_action=&observed[SERVICE]["actions"][0];
        assert_eq!(file_action["arguments"]["path_contains"],"cAtAlOg");
        let file=navigate(&engram,file_action["arguments"].clone()).await;
        assert_eq!(file["detail"],"exact_file");
        let next=&file["files"][0]["next_symbols"];
        assert_eq!(next["arguments"]["path_contains"],"cAtAlOg");
        let later=navigate(&engram,next["arguments"].clone()).await;
        let read=later["files"][0]["symbols"].as_array().unwrap().iter().find(|symbol|symbol["name"]=="read").unwrap();
        let mut exact=file_action["arguments"].clone();exact["symbol_node_id"]=read["node_id"].clone();
        let expanded=navigate(&engram,exact).await;assert_eq!(expanded["detail"],"exact_symbol");
        assert_eq!(expanded["files"][0]["actions"][0]["arguments"]["path_contains"],"cAtAlOg");
        let source=engram.handle_get_full_method_body(serde_json::from_value(read["actions"][0]["arguments"].clone()).unwrap()).await.unwrap();
        let source:Value=serde_json::from_str(&source.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(source["source_file_hash"],ContentHash::compute(current.as_bytes()).0);
        assert!(source["source_code"].as_str().unwrap().contains("current = true"));
        all["include_retained_context"]=json!(true);
        let full=navigation_text(&engram,all).await;
        assert!(full.contains("source_files: 135\n") && full.contains("--- Retained Corpus Inventory"));
        let full=navigation(&full);assert_eq!(full["files"],result["files"]);
        assert_eq!(full["coverage"]["matching_files"],3);
        assert!(full["coverage"].get("representative_priority").is_none());
        let mut all=navigate(&engram,json!({"project_id":PID,"path_contains":".RS","exclude_paths":[PRIVATE],"limit":64})).await;
        let mut every=std::collections::BTreeSet::new();
        loop {
            assert_eq!(all["coverage"]["matching_files"],134);
            for file in all["files"].as_array().unwrap() {
                assert!(every.insert(file["path"].as_str().unwrap().to_owned()),"Filtered root pages must not repeat file identities");
            }
            if all["next_page"].is_null() {break;}
            assert_eq!(all["next_page"]["arguments"]["path_contains"],".RS");
            all=navigate(&engram,all["next_page"]["arguments"].clone()).await;
        }
        assert_eq!(every.len(),134,"Filtered root search must recover every matching file, beyond two-per-area representatives and response display caps");
        for term in ["Missing","*.rs"] {
            let empty=navigate(&engram,json!({"project_id":PID,"path_contains":term})).await;
            assert_eq!(empty["detail"],"filtered_paths");assert_eq!(empty["coverage"]["matching_files"],0);
            assert!(empty["files"].as_array().unwrap().is_empty());
        }
        for term in [String::new(),"x".repeat(129),"bad\nterm".to_owned()] {
            assert!(engram.handle_get_codebase_overview(serde_json::from_value(json!({"project_id":PID,"path_contains":term})).unwrap()).await.is_err());
        }
    });
    owned_case(false, |engram,_| async move {
        let request=serde_json::from_value(json!({"project_id":PID,"path_contains":"Catalog"})).unwrap();
        let error=engram.handle_get_codebase_overview(request).await.unwrap_err();
        assert!(error.message.contains("source snapshot unavailable"),"Unproven source provenance must fail closed instead of fabricating an empty filename search");
    });
}

#[test]
fn directory_name_lookup_preserves_ambiguity_selected_revisions_and_literal_recovery_across_empty_pages() {
    owned_case(true, |engram,search| async move {
        const SCOPE:&str="src/actions";
        const PRIVATE:&str="src/actions/private";
        const FUTURE:&str="src/actions/Future.rs";
        let paths:Vec<_>=(0..10).map(|i|format!("{SCOPE}/Part{i:02}.rs")).collect();
        let cancel=CancellationToken::new();
        let initial=engram.state.registry.capture_source_head(PID).unwrap();
        let mut identities:Vec<_>=paths.iter().map(|path|RelPath::new(path)).collect();
        let private=format!("{PRIVATE}/Hidden.rs");identities.push(RelPath::new(&private));
        search.prepare_full_source_snapshot(PID,0,1,&identities,&cancel).await.unwrap();
        let docs:Vec<_>=identities.iter().enumerate().map(|(i,path)|doc("memory",1,i as u64+1,path.as_str(),"rust","fn reset() {}\n")).collect();
        search.index_docs(PID,&docs,&cancel).await.unwrap();search.seal_source_snapshot(PID,1).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let base=engram.state.registry.capture_source_head(PID).unwrap();
        let current="fn reset() { let current = true; }\n";
        search.prepare_source_snapshot(PID,1,2,&[RelPath::new(&paths[1])],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",2,100,&paths[1],"rust",current)],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,2).unwrap();assert!(engram.state.registry.publish_source_head(&base,2).unwrap());
        search.prepare_source_snapshot(PID,2,3,&[RelPath::new(FUTURE)],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",3,200,FUTURE,"rust","unpublished")],&cancel).await.unwrap();search.seal_source_snapshot(PID,3).unwrap();
        let root=std::path::PathBuf::from(engram.state.registry.get_project(PID).unwrap().unwrap().directory);
        let mut nodes=Vec::new();
        for (i,path) in paths.iter().enumerate() {
            let file=root.join(path);std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file,if i==1{current}else{"fn reset() {}\n"}).unwrap();
            if i==3 {continue;} // Selected source without graph metadata stays unknown.
            let generation=if i==1{2}else{1};
            nodes.push(node(&format!("file:{path}"),"file",path,path,generation));
            if i==4 {continue;} // A native file may have no observed exact-name callable.
            let id=format!("sym:function:{path}:reset:1");
            let mut declaration=node(&id,"function","reset",path,generation);
            if i==5 {declaration.generation=0;}
            if i==6 {declaration.namespace="history".into();}
            if i==7 {declaration.file_path=RelPath::new(&paths[0]);}
            if i==8 {declaration.metadata=Some(json!({"sentinel":"ForeignMetadataSentinel".repeat(4096)}));}
            nodes.push(declaration);
        }
        for (path,generation) in [(private.as_str(),1),(FUTURE,3)] {
            nodes.push(node(&format!("file:{path}"),"file",path,path,generation));
            nodes.push(node(&format!("sym:function:{path}:reset:1"),"function","reset",path,generation));
        }
        engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        let mut arguments=json!({"project_id":PID,"scope":SCOPE,"symbol_name":"reset","exclude_paths":[PRIVATE],"limit":3});
        let mut found=std::collections::BTreeSet::new();let mut pages=0;let mut empty_page=false;
        loop {
            let text=navigation_text(&engram,arguments.clone()).await;assert!(text.len()<64*1024);
            assert!(!text.contains("ForeignMetadataSentinel") && !text.contains("--- File Types"));
            let page=navigation(&text);pages+=1;assert!(pages<=4);
            assert_eq!(page["detail"],"directory_exact_name_candidates");assert_eq!(page["source_generation"],2);
            assert_eq!(page["coverage"]["matching_files"],10);
            assert!(page.get("areas").is_none());
            assert!(page["coverage"]["raw_keys"].as_u64().unwrap()<=64);
            assert!(page["coverage"]["decoded_bytes"].as_u64().unwrap()<=2*1024*1024);
            let files=page["files"].as_array().unwrap();empty_page|=files.is_empty();
            if pages==1 {assert_eq!(files.len(),3,"Same-name declarations must remain distinct candidates, not a selected winner");}
            for file in files {
                let path=file["path"].as_str().unwrap();assert!(found.insert(path.to_owned()));
                assert!(!path.starts_with(PRIVATE) && path!=FUTURE);
                assert_eq!(file["physical_generation"],if path==paths[1]{2}else{1});
                let symbol=&file["symbols"][0];assert_eq!(symbol["generation"],file["physical_generation"]);
                let actions=symbol["actions"].as_array().unwrap();assert_eq!(actions.len(),2);
                let (_,body)=excerpt(&engram,actions[0]["arguments"].clone()).await;
                let raw=std::fs::read(root.join(path)).unwrap();
                assert_eq!(body["source_file_hash"],ContentHash::compute(&raw).0);
                let response=engram.handle_impact_analysis(serde_json::from_value(actions[1]["arguments"].clone()).unwrap()).await.unwrap();
                let text=&response.content[0].as_text().unwrap().text;
                let impact:Value=serde_json::from_str(text.split_once("## Revision-admitted change impact").unwrap().1.split_once("```json").unwrap().1.split_once("```").unwrap().0).unwrap();
                assert_eq!(impact["target"]["node_id"],symbol["node_id"]);assert_eq!(impact["target_revision_admitted"],true);
                // Context remains available through existing exact identity expansion.
                let expanded=navigate(&engram,json!({"project_id":PID,"scope":path,"symbol_node_id":symbol["node_id"]})).await;
                assert!(expanded["files"][0]["symbols"][0]["actions"].as_array().unwrap().iter().any(|action|action["tool"]=="get_method_edit_context"));
            }
            if page["next_page"].is_null(){break;}
            let next=&page["next_page"]["arguments"];
            assert_eq!(next["symbol_name"],"reset");assert_eq!(next["exclude_paths"],json!([PRIVATE]));
            assert!(next["offset"].as_u64().unwrap()>arguments.get("offset").and_then(Value::as_u64).unwrap_or(0));
            arguments=next.clone();
        }
        assert!(empty_page,"An empty matched-candidate page must retain selected-file continuation");
        assert_eq!(found,[paths[0].clone(),paths[1].clone(),paths[2].clone(),paths[9].clone()].into_iter().collect());
        let filtered=navigate(&engram,json!({"project_id":PID,"scope":SCOPE,"symbol_name":"reset","path_contains":"Part01","exclude_paths":[PRIVATE],"limit":3})).await;
        assert_eq!(filtered["files"].as_array().unwrap().len(),1);assert_eq!(filtered["files"][0]["path"],paths[1]);
        assert_eq!(filtered["path_contains"],"Part01");
        for arguments in [json!({"project_id":PID,"scope":SCOPE,"symbol_name":"reset","limit":9}),
            json!({"project_id":PID,"scope":SCOPE,"symbol_name":"reset","include_retained_context":true})] {
            assert!(engram.handle_get_codebase_overview(serde_json::from_value(arguments).unwrap()).await.is_err());
        }
    });
}

#[test]
fn directory_name_lookup_shares_caps_and_recovers_partial_files_without_skipping_unattempted_paths() {
    owned_case(true, |engram,search| async move {
        let paths:Vec<_>=(0..10).map(|i|format!("src/commands/Part{i:02}.rs")).collect();
        let cancel=CancellationToken::new();let initial=engram.state.registry.capture_source_head(PID).unwrap();
        let identities:Vec<_>=paths.iter().map(|path|RelPath::new(path)).collect();
        search.prepare_full_source_snapshot(PID,0,1,&identities,&cancel).await.unwrap();
        let docs:Vec<_>=paths.iter().enumerate().map(|(i,path)|doc("memory",1,i as u64+1,path,"rust","mod a {fn perform() {}}\nmod b {fn perform() {}}\n")).collect();
        search.index_docs(PID,&docs,&cancel).await.unwrap();search.seal_source_snapshot(PID,1).unwrap();assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let mut nodes=Vec::new();
        for path in &paths {
            nodes.push(node(&format!("file:{path}"),"file",path,path,1));
            for line in 1..=2 {
                let mut candidate=node(&format!("sym:function:{path}:perform:{line}"),"function","perform",path,1);
                candidate.start_line=line;candidate.end_line=line;nodes.push(candidate);
            }
        }
        engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        let request=json!({"project_id":PID,"scope":"src/commands","symbol_name":"perform","limit":8});
        let mut arguments=request.clone();let mut seen=std::collections::BTreeSet::new();let mut pages=0;
        loop {
            let page=navigate(&engram,arguments.clone()).await;pages+=1;assert!(pages<=3);
            let count:usize=page["files"].as_array().unwrap().iter().map(|file|file["symbols"].as_array().unwrap().len()).sum();
            assert!(count<=8,"Candidate limit applies to the whole request, not each selected file");
            assert!(page["coverage"]["raw_keys"].as_u64().unwrap()<=64);
            assert!(page["coverage"]["decoded_bytes"].as_u64().unwrap()<=2*1024*1024);
            for file in page["files"].as_array().unwrap(){for symbol in file["symbols"].as_array().unwrap(){seen.insert(symbol["node_id"].as_str().unwrap().to_owned());}}
            if !page["retry_file"].is_null() {
                assert_eq!(page["retry_file"]["arguments"]["scope"],page["coverage"]["partial_file"]);
                let retried=navigate(&engram,page["retry_file"]["arguments"].clone()).await;
                assert_eq!(retried["detail"],"exact_name_candidates");
                for symbol in retried["files"][0]["symbols"].as_array().unwrap(){seen.insert(symbol["node_id"].as_str().unwrap().to_owned());}
            }
            if page["next_page"].is_null(){break;}
            let attempted=page["coverage"]["files_attempted"].as_u64().unwrap();
            let offset=arguments.get("offset").and_then(Value::as_u64).unwrap_or(0);
            assert_eq!(page["next_page"]["arguments"]["offset"],offset+attempted);
            arguments=page["next_page"]["arguments"].clone();
        }
        assert_eq!(seen.len(),20,"Partial-file recovery plus directory continuation must not silently lose overloads or later files");
        // Valid colon-containing native names share the requested prefix but
        // are not aliases. Crowding must consume a single global budget.
        let mut crowd=Vec::new();
        for i in 0..80 {
            let name=format!("perform:alias{i:02}");let line=100+i;
            let mut candidate=node(&format!("sym:function:{}:{name}:{line}",paths[0]),"function",&name,&paths[0],1);
            candidate.start_line=line;candidate.end_line=line;crowd.push(candidate);
        }
        engram.state.graph.upsert_nodes(PID,&crowd).unwrap();
        let capped=navigate(&engram,request.clone()).await;
        assert!(capped["coverage"]["reasons"].as_array().unwrap().iter().any(|reason|reason=="named_raw_key_limit"));
        assert!(capped["coverage"]["files_not_attempted"].as_u64().unwrap()>0);
        assert!(capped["coverage"]["raw_keys"].as_u64().unwrap()<=64);
        assert!(!capped["retry_file"].is_null() && !capped["next_page"].is_null());
        for candidate in &mut crowd {candidate.metadata=Some(json!({"padding":"x".repeat(60000)}));}
        engram.state.graph.upsert_nodes(PID,&crowd).unwrap();
        let capped=navigate(&engram,request).await;
        assert!(capped["coverage"]["reasons"].as_array().unwrap().iter().any(|reason|reason=="named_decode_budget"));
        assert!(capped["coverage"]["decoded_bytes"].as_u64().unwrap()<=2*1024*1024);
        assert!(capped["coverage"]["files_not_attempted"].as_u64().unwrap()>0);
        assert!(capped["files"].as_array().unwrap().iter().all(|file|file["symbols"].as_array().unwrap().iter().all(|symbol|symbol["name"]=="perform")));
    });
}

#[test]
fn directory_name_display_omissions_keep_identities_and_offer_progressing_exact_expansion() {
    owned_case(true, |engram,search| async move {
        // Legal stored identities exercise response bounds without claiming
        // these long metadata paths are existing filesystem components.
        let path=format!("src/{}","a".repeat(1796));let name="n".repeat(220);
        let excluded:Vec<_>=(0..16).map(|i|format!("excluded/{i:02}/{}","x".repeat(244))).collect();
        assert_eq!(excluded.iter().map(String::len).sum::<usize>(),4096);
        let cancel=CancellationToken::new();let initial=engram.state.registry.capture_source_head(PID).unwrap();
        search.prepare_full_source_snapshot(PID,0,1,&[RelPath::new(&path)],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",1,1,&path,"rust","indexed identity")],&cancel).await.unwrap();search.seal_source_snapshot(PID,1).unwrap();assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let mut nodes=vec![node(&format!("file:{path}"),"file",&path,&path,1)];
        let mut expected=std::collections::BTreeSet::new();
        for line in 10..18 {
            let id=format!("sym:function:{path}:{name}:{line}");assert!(id.len()<=2048);expected.insert(id.clone());
            let mut declaration=node(&id,"function",&name,&path,1);declaration.start_line=line;declaration.end_line=line;nodes.push(declaration);
        }
        engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        let text=navigation_text(&engram,json!({"project_id":PID,"scope":"src","symbol_name":name,"exclude_paths":excluded,"limit":1})).await;
        assert!(text.len()<64*1024);let page=navigation(&text);
        assert!(page["coverage"]["display_omitted_candidates"].as_u64().unwrap()>0);
        let mut observed=std::collections::BTreeSet::new();
        for file in page["files"].as_array().unwrap(){for symbol in file["symbols"].as_array().unwrap(){assert!(observed.insert(symbol["node_id"].as_str().unwrap().to_owned()));}}
        for group in page["omitted_candidates"].as_array().unwrap(){assert_eq!(group["path"],path);for id in group["node_ids"].as_array().unwrap(){assert!(observed.insert(id.as_str().unwrap().to_owned()));}}
        assert_eq!(observed,expected,"Response fitting must retain every observed candidate identity, not hide overloads");
        let action=&page["display_retry"];assert_eq!(action["tool"],"get_codebase_overview");
        assert!(action["arguments"].get("symbol_name").is_none());assert_eq!(action["arguments"]["scope"],path);
        let id=action["arguments"]["symbol_node_id"].as_str().unwrap();assert!(expected.contains(id));
        let expanded=navigate(&engram,action["arguments"].clone()).await;
        assert_eq!(expanded["detail"],"exact_symbol");assert_eq!(expanded["files"][0]["symbols"].as_array().unwrap().len(),1);
        assert_eq!(expanded["files"][0]["symbols"][0]["node_id"],id);
        assert!(page["next_page"].is_null(),"A display omission must not pretend that another selected file page exists");
    });
}

#[test]
fn paths_only_codemap_preserves_selected_files_and_executable_declaration_and_source_recovery() {
    owned_case(true, |engram, search| async move {
        const SCOPE: &str = "src/media";
        const IMAGE: &str = "src/media/ImageEditor.rs";
        const HANDLER: &str = "src/media/Handler.rs";
        const PRIVATE: &str = "src/media/private";
        const FUTURE: &str = "src/media/ImageFuture.rs";
        let mut sources=vec![(IMAGE.to_owned(),"fn draw() {}\n".to_owned()),
            (HANDLER.to_owned(),"fn reset() { let old = true; }\n".to_owned())];
        sources.extend((0..8).map(|i|(format!("{SCOPE}/Part{i:02}.rs"),"fn part() {}\n".to_owned())));
        sources.push((format!("{PRIVATE}/ImageSecret.rs"),"fn secret() {}\n".to_owned()));
        let cancel=CancellationToken::new();
        let initial=engram.state.registry.capture_source_head(PID).unwrap();
        let paths:Vec<_>=sources.iter().map(|(path,_)|RelPath::new(path)).collect();
        search.prepare_full_source_snapshot(PID,0,1,&paths,&cancel).await.unwrap();
        let docs:Vec<_>=sources.iter().enumerate().map(|(i,(path,body))|
            doc("memory",1,i as u64+1,path,"rust",body)).collect();
        search.index_docs(PID,&docs,&cancel).await.unwrap();search.seal_source_snapshot(PID,1).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let base=engram.state.registry.capture_source_head(PID).unwrap();
        let current="fn reset() { let current = true; }\n";
        search.prepare_source_snapshot(PID,1,2,&[RelPath::new(HANDLER)],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",2,2000,HANDLER,"rust",current)],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,2).unwrap();assert!(engram.state.registry.publish_source_head(&base,2).unwrap());
        search.prepare_source_snapshot(PID,2,3,&[RelPath::new(FUTURE)],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",3,3000,FUTURE,"rust","unpublished")],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,3).unwrap();
        // Only one selected file has graph declarations; missing graph data
        // must not turn the other selected file identities into absent code.
        let id=format!("sym:function:{HANDLER}:reset:1");
        engram.state.graph.upsert_nodes(PID,&[
            node(&format!("file:{HANDLER}"),"file",HANDLER,HANDLER,2),
            node(&id,"function","reset",HANDLER,2),
            node(&format!("file:{FUTURE}"),"file",FUTURE,FUTURE,3),
        ]).unwrap();
        let root=std::path::PathBuf::from(engram.state.registry.get_project(PID).unwrap().unwrap().directory);
        for (path,body) in [(IMAGE,"fn draw() {}\n"),(HANDLER,current)] {
            let file=root.join(path);std::fs::create_dir_all(file.parent().unwrap()).unwrap();std::fs::write(file,body).unwrap();
        }
        let normal=json!({"project_id":PID,"scope":SCOPE,"exclude_paths":[PRIVATE],"limit":8});
        let mut explicit=normal.clone();explicit["include_declarations"]=json!(true);
        assert_eq!(navigation_text(&engram,normal.clone()).await,navigation_text(&engram,explicit).await,
            "Omitting the new mode must preserve the existing declaration overview");
        let old:engram_server::models::ProjectIdRequest=serde_json::from_value(json!({"project_id":PID})).unwrap();
        let old:engram_server::models::CodebaseOverviewRequest=old.into();assert!(old.include_declarations);
        let query=json!({"project_id":PID,"path_contains":"Image","exclude_paths":[PRIVATE],"limit":8,"include_declarations":false});
        let filtered_text=navigation_text(&engram,query).await;let filtered=navigation(&filtered_text);
        assert_eq!(filtered["detail"],"paths_only");assert_eq!(filtered["source_generation"],2);
        assert_eq!(filtered["coverage"]["matching_files"],1);
        assert_eq!(filtered["coverage"]["declaration_lookup"],"not_requested");
        assert!(filtered["coverage"].get("declarations").is_none());
        assert!(!filtered_text.contains("--- File Types") && !filtered_text.contains("--- Language Breakdown"));
        assert_eq!(filtered["files"][0]["path"],IMAGE);
        let area=filtered["areas"].as_array().unwrap().iter().find(|area|area["scope"]==SCOPE).unwrap();
        assert_eq!(area["explore"]["arguments"]["path_contains"],"Image");
        assert_eq!(area["explore"]["arguments"]["include_declarations"],false);
        let browse=&area["browse_without_path_filter"];
        assert!(browse["arguments"].get("path_contains").is_none());
        assert_eq!(browse["arguments"]["include_declarations"],false);
        assert_eq!(browse["arguments"]["exclude_paths"],json!([PRIVATE]));
        let first=navigate(&engram,browse["arguments"].clone()).await;
        assert_eq!(first["files"].as_array().unwrap().len(),8,"Cheap mode must retain every requested file that fits, not shrink the sample to pass a byte gate");
        assert_eq!(first["areas"].as_array().unwrap().len(),10);
        for area in first["areas"].as_array().unwrap() {
            let file=first["files"].as_array().unwrap().iter().find(|file|file["path"]==area["scope"]);
            if area["use_file_actions"]==true {
                assert!(file.is_some());assert!(area.get("explore").is_none());
            } else {
                assert!(file.is_none());assert_eq!(area["explore"]["arguments"]["include_declarations"],false);
            }
        }
        let handler=first["files"].as_array().unwrap().iter().find(|file|file["path"]==HANDLER).unwrap();
        assert_eq!(handler["physical_generation"],2);
        assert!(handler.get("symbols").is_none() && handler.get("declarations_observed").is_none());
        let mut inspect=handler["actions"][0]["arguments"].clone();assert_eq!(inspect["include_declarations"],true);
        assert_eq!(inspect["exclude_paths"],json!([PRIVATE]));inspect["symbol_name"]=json!("reset");
        let declaration=navigate(&engram,inspect).await;
        assert_eq!(declaration["files"][0]["symbols"][0]["node_id"],id);
        let source=engram.handle_get_full_method_body(serde_json::from_value(declaration["files"][0]["symbols"][0]["actions"][0]["arguments"].clone()).unwrap()).await.unwrap();
        let source:Value=serde_json::from_str(&source.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(source["source_file_hash"],ContentHash::compute(current.as_bytes()).0);
        assert!(source["source_code"].as_str().unwrap().contains("current = true"));
        let image=first["files"].as_array().unwrap().iter().find(|file|file["path"]==IMAGE).unwrap();
        assert_eq!(image["physical_generation"],1);
        let source=engram.handle_get_full_method_body(serde_json::from_value(image["actions"][1]["arguments"].clone()).unwrap()).await.unwrap();
        let source:Value=serde_json::from_str(&source.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(source["retrieval_scope"],"current_file_excerpt");
        assert_eq!(source["source_file_hash"],ContentHash::compute(b"fn draw() {}\n").0);
        let mut page=first;let mut found=std::collections::BTreeSet::new();
        loop {
            assert_eq!(page["coverage"]["matching_files"],10);
            for file in page["files"].as_array().unwrap() {
                let path=file["path"].as_str().unwrap();assert!(!path.starts_with(PRIVATE) && path!=FUTURE);
                assert!(found.insert(path.to_owned()));
            }
            if page["next_page"].is_null() {break;}
            assert_eq!(page["next_page"]["arguments"]["include_declarations"],false);
            assert_eq!(page["next_page"]["arguments"]["exclude_paths"],json!([PRIVATE]));
            page=navigate(&engram,page["next_page"]["arguments"].clone()).await;
        }
        assert_eq!(found.len(),10);
        let mut retained=normal.clone();retained["include_declarations"]=json!(false);retained["include_retained_context"]=json!(true);
        let text=navigation_text(&engram,retained).await;assert!(text.contains("--- Retained Corpus Inventory"));
        assert_eq!(navigation(&text)["coverage"]["declaration_lookup"],"not_requested");
        for selector in [json!({"symbol_name":"reset"}),json!({"symbol_node_id":id}),json!({"symbol_offset":1})] {
            let mut invalid=json!({"project_id":PID,"scope":HANDLER,"include_declarations":false});
            invalid.as_object_mut().unwrap().extend(selector.as_object().unwrap().clone());
            assert!(engram.handle_get_codebase_overview(serde_json::from_value(invalid).unwrap()).await.is_err());
        }
    });
}

#[test]
fn paths_only_area_actions_do_not_alias_files_omitted_by_response_budget() {
    owned_case(true, |engram,search| async move {
        // These are indexed identities, not claims of current filesystem
        // existence. Long valid metadata forces both response budgets.
        let paths:Vec<String>=(0..12).map(|i|format!("crowded/F{i:02}_{}.rs","x".repeat(1825))).collect();
        let cancel=CancellationToken::new();
        let initial=engram.state.registry.capture_source_head(PID).unwrap();
        let identities:Vec<_>=paths.iter().map(|path|RelPath::new(path)).collect();
        search.prepare_full_source_snapshot(PID,0,1,&identities,&cancel).await.unwrap();
        let docs:Vec<_>=paths.iter().enumerate().map(|(i,path)|doc("memory",1,i as u64+1,path,"rust","fn recorded() {}\n")).collect();
        search.index_docs(PID,&docs,&cancel).await.unwrap();search.seal_source_snapshot(PID,1).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let text=navigation_text(&engram,json!({"project_id":PID,"scope":"crowded","limit":12,"include_declarations":false})).await;
        assert!(text.len()<64*1024);let page=navigation(&text);
        let files=page["files"].as_array().unwrap();assert!(!files.is_empty() && files.len()<paths.len());
        assert_eq!(page["coverage"]["matching_files"],12);
        assert!(page["coverage"]["response_omitted_files"].as_u64().unwrap()>0);
        let mut omitted_area=None;
        for area in page["areas"].as_array().unwrap() {
            let displayed=files.iter().any(|file|file["path"]==area["scope"]);
            if area["use_file_actions"]==true {
                assert!(displayed,"An area must never point to actions on a file omitted from the response");
                assert!(area.get("explore").is_none());
            } else if !displayed {
                assert_eq!(area["explore"]["tool"],"get_codebase_overview");
                assert_eq!(area["explore"]["arguments"]["include_declarations"],false);
                omitted_area=Some(area.clone());
            }
        }
        let area=omitted_area.expect("The valid crowded fixture must retain an area for a file that did not fit its file page");
        let recovered=navigate(&engram,area["explore"]["arguments"].clone()).await;
        assert_eq!(recovered["source_generation"],1);assert_eq!(recovered["coverage"]["matching_files"],1);
        assert_eq!(recovered["files"][0]["path"],area["scope"]);
        assert_eq!(recovered["files"][0]["physical_generation"],1);
        assert_eq!(page["next_page"]["arguments"]["include_declarations"],false);
    });
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
        let mut filtered=navigate(&engram,json!({"project_id":PID,"scope":CAPPED,"path_contains":"Leaf","limit":8})).await;
        assert_eq!(filtered["coverage"]["area_sample_truncated"],true);
        let mut filtered_paths=std::collections::BTreeSet::new();
        loop {
            for file in filtered["files"].as_array().unwrap() {
                assert!(filtered_paths.insert(file["path"].as_str().unwrap().to_owned()));
            }
            if filtered["next_page"].is_null() {break;}
            assert_eq!(filtered["next_page"]["arguments"]["path_contains"],"Leaf");
            filtered=navigate(&engram,filtered["next_page"]["arguments"].clone()).await;
        }
        assert_eq!(filtered_paths,seen,"Filtered directory pages also recover every file despite capped child-area groups");
        for scope in [EXCLUDED,"src/application/absent"] {
            let empty = navigate(&engram, json!({"project_id":PID,"scope":scope,"exclude_paths":[EXCLUDED]})).await;
            assert!(empty["files"].as_array().unwrap().is_empty());
            assert!(empty["areas"].as_array().unwrap().is_empty());
        }
    });
}

#[test]
fn root_area_counts_match_literal_actions_without_hiding_representative_or_display_gaps() {
    owned_case(true, |engram, search| async move {
        let mut paths=vec!["README.md".to_owned(),"src/Direct.rs".into(),
            "src/app/a.rs".into(),"src/app/deeper/b.rs".into(),"srcx/Other.rs".into(),
            "private/Secret.rs".into(),"Alias/One.rs".into(),"alias/Two.rs".into(),
            "İrea/One.rs".into(),"i\u{307}rea/Two.rs".into()];
        // Many deep representative groups must not make one shallow directory's
        // count partial, or silently hide that the representative pool is capped.
        paths.extend((0..130).map(|i|format!("src/many{i:03}/Part.rs")));
        let cancel=CancellationToken::new();
        let initial=engram.state.registry.capture_source_head(PID).unwrap();
        let identities:Vec<_>=paths.iter().map(|path|RelPath::new(path)).collect();
        search.prepare_full_source_snapshot(PID,0,1,&identities,&cancel).await.unwrap();
        let text="pub fn Selected() {}\n";
        let docs:Vec<_>=paths.iter().enumerate().map(|(i,path)|doc("memory",1,i as u64+1,path,"rust",text)).collect();
        search.index_docs(PID,&docs,&cancel).await.unwrap();
        search.seal_source_snapshot(PID,1).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let root=std::path::PathBuf::from(engram.state.registry.get_project(PID).unwrap().unwrap().directory);
        std::fs::write(root.join("README.md"),text).unwrap();
        let request=json!({"project_id":PID,"exclude_paths":["private"],"limit":8});
        let first=navigation_text(&engram,request.clone()).await;
        assert!(first.len()<16*1024,"Root orientation should leave room for the source task: {}bytes",first.len());
        assert!(first.contains(&format!("source_files: {}\n",paths.len())));
        assert!(first.contains(&format!("source_chunks: {}\n",paths.len())));
        let report=navigation(&first);
        assert_eq!(report["coverage"]["matching_files"],paths.len()-1);
        assert_eq!(report["coverage"]["area_sample_truncated"],false);
        assert_eq!(report["coverage"]["representative_groups_truncated"],true);
        let mut expected=std::collections::BTreeMap::from([
            ("README.md",1u64),("src",133),("srcx",1),
        ]);
        if cfg!(windows) {
            expected.extend([("Alias",2),("i\u{307}rea",2)]);
        } else {
            expected.extend([("Alias",1),("alias",1),("İrea",1),("i\u{307}rea",1)]);
        }
        let actual:std::collections::BTreeMap<_,_>=report["areas"].as_array().unwrap().iter()
            .map(|area|(area["scope"].as_str().unwrap(),area["selected_files"].as_u64().unwrap())).collect();
        assert_eq!(actual,expected,"Top-level scopes partition the selected files, including Windows aliases");
        assert_eq!(actual.values().sum::<u64>(),report["coverage"]["matching_files"].as_u64().unwrap());
        for area in report["areas"].as_array().unwrap() {
            assert_eq!(area["explore"]["arguments"]["exclude_paths"],json!(["private"]));
            let child=navigate(&engram,area["explore"]["arguments"].clone()).await;
            assert_eq!(child["source_generation"],1);
            assert_eq!(child["coverage"]["matching_files"],area["selected_files"],
                "The literal area action must match its reported count, including root files and aliases");
            if area["scope"]=="README.md" {
                assert_eq!(child["detail"],"exact_file");
                let read=child["files"][0]["actions"].as_array().unwrap().iter()
                    .find(|action|action["tool"]=="get_full_method_body").unwrap();
                let response=engram.handle_get_full_method_body(serde_json::from_value(read["arguments"].clone()).unwrap()).await.unwrap();
                let source:Value=serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
                assert_eq!(source["source_code"],text.trim_end());
                assert_eq!(source["source_file_hash"],ContentHash::compute(text.as_bytes()).0);
            }
        }
        // A separate cap on displayed top-level areas remains explicit. It
        // must not prevent an exact scoped/filtered lookup beyond that sample.
        let added:Vec<_>=(0..130).map(|i|format!("top{i:03}/Part.rs")).collect();
        let head=engram.state.registry.capture_source_head(PID).unwrap();
        let identities:Vec<_>=added.iter().map(|path|RelPath::new(path)).collect();
        search.prepare_source_snapshot(PID,1,2,&identities,&[],&cancel).await.unwrap();
        let docs:Vec<_>=added.iter().enumerate().map(|(i,path)|doc("memory",2,2000+i as u64,path,"rust",text)).collect();
        search.index_docs(PID,&docs,&cancel).await.unwrap();
        search.seal_source_snapshot(PID,2).unwrap();
        assert!(engram.state.registry.publish_source_head(&head,2).unwrap());
        let crowded=navigate(&engram,request).await;
        assert_eq!(crowded["coverage"]["matching_files"],paths.len()+added.len()-1);
        assert_eq!(crowded["coverage"]["area_sample_truncated"],true);
        assert_eq!(crowded["areas"].as_array().unwrap().len(),32);
        assert_eq!(crowded["coverage"]["representative_groups_truncated"],true);
        assert!(!crowded["areas"].as_array().unwrap().iter().any(|area|area["scope"]=="top129"));
        for args in [json!({"project_id":PID,"scope":"top129","exclude_paths":["private"]}),
            json!({"project_id":PID,"path_contains":"top129","exclude_paths":["private"]})] {
            let found=navigate(&engram,args).await;
            assert_eq!(found["coverage"]["matching_files"],1);
            assert_eq!(found["files"][0]["path"],"top129/Part.rs");
            assert!(found["coverage"].get("representative_groups_truncated").is_none(),
                "A root representative limit must not become a false gap in independent lexical file lookup");
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
        assert_eq!(report["coverage"]["area_sample_truncated"], false);
        assert_eq!(report["coverage"]["representative_groups_truncated"],false);
        let src_area=report["areas"].as_array().unwrap().iter()
            .find(|area|area["scope"]=="src").unwrap();
        let src_page=navigate(&engram,src_area["explore"]["arguments"].clone()).await;
        assert_eq!(src_page["coverage"]["matching_files"],src_area["selected_files"]);
        assert_eq!(src_page["coverage"]["area_sample_truncated"],crowded);
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
            let ordinary_area = src_page["areas"]
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
            let map_area = src_page["areas"]
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
fn named_candidates_and_exact_symbol_open_known_identity_beyond_discovery_without_claiming_inventory() {
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

        // Knowing the file and extracted name must not require finding the
        // declaration's ID through128 discovery seeds first.
        let named_request=json!({"project_id":PID,"scope":TARGET,"symbol_name":"ResolveZoom"});
        let named_text=navigation_text(&engram,named_request.clone()).await;
        assert!(named_text.len()<8*1024);
        let named=navigation(&named_text);
        assert_eq!(named["detail"],"exact_name_candidates");
        assert_eq!(named["files"][0]["symbols"][0],*declaration);
        assert!(named["files"][0]["next_symbols"].is_null());
        assert_eq!(named["files"][0]["declaration_inventory"],"not_scanned");
        let lookup=&named["coverage"]["declarations"][0];
        assert_eq!(lookup["observation"],"bounded_exact_name_prefix_lookup");
        assert_eq!(lookup["matching_admitted_records_observed"],1);
        assert_eq!(lookup["records_attempted"],2);
        assert!(lookup["declaration_admission"]["raw_rows"].as_u64().unwrap()<=5);
        let named_read=&named["files"][0]["symbols"][0]["actions"][0];
        let (_,body)=excerpt(&engram,named_read["arguments"].clone()).await;
        assert_eq!(body["source_code"],WANTED.trim_end());
        assert_eq!(body["source_file_hash"],ContentHash::compute(source.as_bytes()).0);
        for scenario in ["stale","future","history","foreign","kind","name","oversized"] {
            let mut bad=wanted.clone();
            match scenario {
                "stale"=>bad.generation=0,
                "future"=>bad.generation=2,
                "history"=>bad.namespace="history".into(),
                "foreign"=>bad.file_path=RelPath::new(DECOY),
                "kind"=>bad.node_type="property".into(),
                "name"=>bad.name="ForeignNameSentinel".into(),
                _=>bad.metadata=Some(json!({"payload":"ForeignMetadataSentinel".repeat(4096)})),
            }
            engram.state.graph.upsert_nodes(PID,&[bad]).unwrap();
            let text=navigation_text(&engram,named_request.clone()).await;
            let rejected=navigation(&text);
            assert!(rejected["files"][0]["symbols"].as_array().unwrap().is_empty(),"{scenario}");
            assert_eq!(rejected["coverage"]["declarations"][0]["declaration_admission"]["truncated"],true);
            assert!(!text.contains("ForeignNameSentinel") && !text.contains("ForeignMetadataSentinel"));
            engram.state.graph.upsert_nodes(PID,&[wanted.clone()]).unwrap();
        }
        for spelling in ["resolvezoom","Owner.ResolveZoom"] {
            let unobserved=navigate(&engram,json!({"project_id":PID,"scope":TARGET,"symbol_name":spelling})).await;
            assert!(unobserved["files"][0]["symbols"].as_array().unwrap().is_empty());
            assert!(!unobserved["coverage"]["declarations"][0]["unassessed"].as_array().unwrap().is_empty());
        }
        for arguments in [
            json!({"project_id":PID,"scope":TARGET,"exclude_paths":[TARGET],"symbol_name":"ResolveZoom"}),
            json!({"project_id":PID,"scope":TARGET,"symbol_name":"ResolveZoom","symbol_node_id":wanted_id}),
            json!({"project_id":PID,"scope":TARGET,"symbol_name":"ResolveZoom","offset":1}),
            json!({"project_id":PID,"scope":TARGET,"symbol_name":"ResolveZoom","symbol_offset":1}),
        ] {
            assert!(engram.handle_get_codebase_overview(serde_json::from_value(arguments).unwrap()).await.is_err());
        }

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
            let named=navigate(&engram,named_request.clone()).await;
            assert!(named["files"][0]["symbols"].as_array().unwrap().is_empty());
            assert_eq!(named["coverage"]["declarations"][0]["canonical_file_admitted"],false);
            engram.state.graph.upsert_nodes(PID, &[file.clone()]).unwrap();
        }
        let missing = navigate(&engram, json!({"project_id":PID,"scope":MISSING,
            "symbol_node_id":format!("sym:function:{MISSING}:Missing:1")})).await;
        assert_eq!(missing["files"][0]["symbol_selection"]["observed"], false);
        assert!(missing["files"][0]["symbols"].as_array().unwrap().is_empty());
        let missing=navigate(&engram,json!({"project_id":PID,"scope":MISSING,"symbol_name":"Missing"})).await;
        assert_eq!(missing["coverage"]["declarations"][0]["canonical_file_admitted"],false);
        assert!(missing["files"][0]["symbols"].as_array().unwrap().is_empty());
    });
}

#[test]
fn named_candidates_preserve_ambiguity_and_usable_actions_under_work_and_response_caps() {
    owned_case(true, |engram,search| async move {
        let path=format!("src/names/{}/crowded.ts",(0..20).map(|i|format!("segment{i:02}{}","x".repeat(61)))
            .collect::<Vec<_>>().join("/"));
        let name=format!("Inspect{}","N".repeat(190));
        let body=(0..10).map(|i|format!("class Owner{i} {{ {name}() {{}} }}\n")).collect::<String>();
        let root=std::path::PathBuf::from(engram.state.registry.get_project(PID).unwrap().unwrap().directory);
        std::fs::create_dir_all(root.join(&path).parent().unwrap()).unwrap();
        std::fs::write(root.join(&path),&body).unwrap();
        let initial=engram.state.registry.capture_source_head(PID).unwrap();
        let cancel=CancellationToken::new();
        search.prepare_full_source_snapshot(PID,0,1,&[RelPath::new(&path)],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",1,1,&path,"typescript",&body)],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,1).unwrap();
        let mut file=node(&format!("file:{path}"),"file",&path,&path,1);
        file.end_line=10;
        let mut nodes=vec![file];
        for line in 1..=10 {
            let mut candidate=node(&format!("sym:function:{path}:{name}:{line}"),"function",&name,&path,1);
            candidate.start_line=line;candidate.end_line=line;nodes.push(candidate);
        }
        // A project-scanning implementation would decode this unrelated large
        // record. It must not consume the exact file/name lookup budget.
        let mut unrelated=node("sym:function:aaa/unrelated.ts:Other:1","function","Other","aaa/unrelated.ts",1);
        unrelated.metadata=Some(json!({"large":"x".repeat(128*1024)}));nodes.push(unrelated);
        engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let text=navigation_text(&engram,json!({"project_id":PID,"scope":path,"symbol_name":name})).await;
        assert!(text.len()<=64*1024);
        let result=navigation(&text);
        assert_eq!(result["detail"],"exact_name_candidates");
        assert_eq!(result["files"].as_array().unwrap().len(),1,"Crowding must not discard the sole matching file");
        let row=&result["files"][0];let symbols=row["symbols"].as_array().unwrap();
        assert!(!symbols.is_empty() && symbols.len()<8);
        assert_eq!(row["symbols_omitted_from_display"],8-symbols.len());
        assert_eq!(row["symbol_display_limited"],true);
        assert!(row["next_symbols"].is_null());
        assert!(symbols.iter().all(|s|s["name"]==name && s["actions"].as_array().unwrap().len()==3));
        assert!(symbols.windows(2).all(|pair|pair[0]["node_id"]!=pair[1]["node_id"]));
        let lookup=&result["coverage"]["declarations"][0];
        assert_eq!(lookup["matching_admitted_records_observed"],9);
        assert_eq!(lookup["candidate_identities_retained"],8);
        assert!(lookup["declaration_admission"]["reasons"].as_array().unwrap().contains(&json!("named_candidate_limit")));
        assert!(!lookup["declaration_admission"]["reasons"].as_array().unwrap().contains(&json!("named_record_limit")));
        let action=&symbols[0]["actions"][0];
        let (_,read)=excerpt(&engram,action["arguments"].clone()).await;
        assert_eq!(read["source_file_hash"],ContentHash::compute(body.as_bytes()).0);
        assert!(read["source_code"].as_str().unwrap().contains(&name));

        // Optional retained context/exclusion metadata must not starve the
        // matched file. A late valid range can be clipped from the display;
        // the remaining identities must still offer a current-file read.
        for candidate in nodes.iter_mut().filter(|n|n.file_path.as_str()==path && n.node_type=="function") {
            if candidate.start_line!=7 {candidate.end_line=0;}
        }
        engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        let exclusions=(0..16).map(|i|format!("outside/unused{i:02}/{}","E".repeat(220))).collect::<Vec<_>>();
        let crowded=navigation_text(&engram,json!({"project_id":PID,"scope":path,"symbol_name":name,
            "exclude_paths":exclusions,"include_retained_context":true})).await;
        let crowded=navigation(&crowded);
        assert_eq!(crowded["detail"],"exact_name_candidates");
        assert_eq!(crowded["files"].as_array().unwrap().len(),1);
        let shown=&crowded["files"][0];
        assert!(!shown["symbols"].as_array().unwrap().is_empty());
        assert!(shown["symbols_omitted_from_display"].as_u64().unwrap()>0);
        assert!(shown["symbols"].as_array().unwrap().iter().all(|s|
            s["actions"].as_array().unwrap().iter().all(|a|a["tool"]!="get_full_method_body")));
        let fallback=shown["actions"].as_array().unwrap().iter().find(|a|a["tool"]=="get_full_method_body").unwrap();
        let (_,page)=excerpt(&engram,fallback["arguments"].clone()).await;
        assert_eq!(page["retrieval_scope"],"current_file_excerpt");
        assert_eq!(page["source_file_hash"],ContentHash::compute(body.as_bytes()).0);

        // Valid longer extracted names sharing "Needle:" are not exact-name
        // matches. Their inspected rows/bytes still have hard work limits.
        let mut neighbors=Vec::new();
        for i in 0..80 {
            let alias=format!("Needle::Neighbor{i:03}");
            let n=node(&format!("sym:function:{path}:{alias}:1"),"function",&alias,&path,1);
            neighbors.push(n);
        }
        engram.state.graph.upsert_nodes(PID,&neighbors).unwrap();
        let bounded=navigate(&engram,json!({"project_id":PID,"scope":path,"symbol_name":"Needle"})).await;
        assert!(bounded["files"][0]["symbols"].as_array().unwrap().is_empty());
        let work=&bounded["coverage"]["declarations"][0];
        assert_eq!(work["declaration_admission"]["raw_rows"],64);
        assert!(work["declaration_admission"]["reasons"].as_array().unwrap().contains(&json!("named_raw_key_limit")));
        for n in &mut neighbors {n.metadata=Some(json!({"payload":"x".repeat(60*1024)}));}
        engram.state.graph.upsert_nodes(PID,&neighbors).unwrap();
        let bytes=navigate(&engram,json!({"project_id":PID,"scope":path,"symbol_name":"Needle"})).await;
        let work=&bytes["coverage"]["declarations"][0];
        assert!(work["raw_record_bytes_decoded"].as_u64().unwrap()<=2*1024*1024);
        assert!(work["declaration_admission"]["raw_rows"].as_u64().unwrap()<=64);
        assert!(work["declaration_admission"]["reasons"].as_array().unwrap().contains(&json!("named_decode_budget")));
        assert!(bytes["files"][0]["symbols"].as_array().unwrap().is_empty());
    });
}

async fn publish_named_sources(engram: &Engram, search: &HybridSearchEngine, sources: &[(&str, &str)]) {
    let root=std::path::PathBuf::from(engram.state.registry.get_project(PID).unwrap().unwrap().directory);
    let cancel=CancellationToken::new();
    let initial=engram.state.registry.capture_source_head(PID).unwrap();
    let paths:Vec<_>=sources.iter().map(|(path,_)|RelPath::new(path)).collect();
    search.prepare_full_source_snapshot(PID,0,1,&paths,&cancel).await.unwrap();
    let docs:Vec<_>=sources.iter().enumerate().map(|(i,(path,body))| {
        std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        std::fs::write(root.join(path),body).unwrap();
        let mut indexed=doc("memory",1,i as u64+1,path,"vb",body);
        indexed.end_line=body.lines().count() as u32;indexed
    }).collect();
    search.index_docs(PID,&docs,&cancel).await.unwrap();search.seal_source_snapshot(PID,1).unwrap();
    let files:Vec<_>=sources.iter().map(|(path,body)| {
        let mut file=node(&format!("file:{path}"),"file",path,path,1);
        file.end_line=body.lines().count() as u32;
        file.metadata=Some(json!({"file_hash":ContentHash::compute(body.as_bytes()).0}));file
    }).collect();
    engram.state.graph.upsert_nodes(PID,&files).unwrap();
    assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
}

#[test]
fn named_terminal_lookup_recovers_qualified_method_and_executable_method_impact() {
    owned_case(true,|engram,search| async move {
        const PATH:&str="src/Policy.vb";
        const BODY:&str="Namespace Demo\nPublic Class Policy\nPublic Shared Function ChooseLevel(features As Long, coordinates As Long) As Integer\nReturn 10\nEnd Function\nPublic Shared Function Resolve() As Integer\nReturn ChooseLevel(1, 1)\nEnd Function\nEnd Class\nEnd Namespace\n";
        publish_named_sources(&engram,&search,&[(PATH,BODY)]).await;
        // A newer published head may inherit this file's physical revision.
        let cancel=CancellationToken::new();let base=engram.state.registry.capture_source_head(PID).unwrap();
        search.prepare_source_snapshot(PID,1,2,&[RelPath::new("src/Other.vb")],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",2,2,"src/Other.vb","vb","other source")],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,2).unwrap();assert!(engram.state.registry.publish_source_head(&base,2).unwrap());
        let wanted=format!("sym:function:{PATH}:Demo.Policy.ChooseLevel:3");
        let caller=format!("sym:function:{PATH}:Demo.Policy.Resolve:6");
        let mut target=node(&wanted,"function","Demo.Policy.ChooseLevel",PATH,1);target.start_line=3;target.end_line=5;
        let mut consumer=node(&caller,"function","Demo.Policy.Resolve",PATH,1);consumer.start_line=6;consumer.end_line=8;
        let mut declarations=vec![target.clone(),consumer];
        for (owner,revision) in [("Future",3),("History",1),("Foreign",1)] {
            let name=format!("Demo.{owner}.ChooseLevel");
            let mut rejected=node(&format!("sym:function:{PATH}:{name}:13"),"function",&name,PATH,revision);
            rejected.start_line=13;rejected.end_line=13;
            if owner=="History" {rejected.namespace="history".into();}
            if owner=="Foreign" {rejected.file_path=RelPath::new("src/Other.vb");}
            rejected.metadata=Some(json!({"sentinel":"RejectedQualifiedNamePayload"}));declarations.push(rejected);
        }
        engram.state.graph.upsert_nodes(PID,&declarations).unwrap();
        engram.state.graph.upsert_edges(PID,&[Edge{source_id:caller.clone(),target_id:wanted.clone(),namespace:"memory".into(),language:"vb".into(),edge_kind:EdgeKind::Calls,weight:1,generation:1,metadata:Some(json!({"src_line":6,"call_site_line":7})),updated_at_ms:1}]).unwrap();
        let text=navigation_text(&engram,json!({"project_id":PID,"scope":PATH,"symbol_name":"ChooseLevel"})).await;
        assert!(text.len()<12*1024 && !text.contains("RejectedQualifiedNamePayload"));
        let page=navigation(&text);assert_eq!(page["source_generation"],2);assert_eq!(page["working_tree_checked"],false);
        let file=&page["files"][0];assert_eq!(file["physical_generation"],1);
        assert_eq!(file["symbols"].as_array().unwrap().len(),1);
        let selected=&file["symbols"][0];assert_eq!(selected["node_id"],wanted);assert_eq!(selected["name"],"Demo.Policy.ChooseLevel");
        assert_eq!(file["name_selection"]["match"],"exact_case_sensitive_terminal_segment");
        let lookup=&page["coverage"]["declarations"][0];assert_eq!(lookup["terminal_name_fallback"]["attempted"],true);
        assert!(lookup["declaration_admission"]["non_selected_nodes"].as_u64().unwrap()>0);
        let actions=selected["actions"].as_array().unwrap();
        let read=actions.iter().find(|a|a["tool"]=="get_full_method_body").unwrap();
        let (_,body)=excerpt(&engram,read["arguments"].clone()).await;
        assert_eq!(body["source_code"],BODY.lines().skip(2).take(3).collect::<Vec<_>>().join("\n"));
        assert_eq!(body["source_file_hash"],ContentHash::compute(BODY.as_bytes()).0);
        let impact=actions.iter().find(|a|a["tool"]=="impact_analysis").unwrap();assert_eq!(impact["arguments"]["symbol_fqn"],wanted);
        let result=engram.handle_impact_analysis(serde_json::from_value(impact["arguments"].clone()).unwrap()).await.unwrap();
        assert!(!result.is_error.unwrap_or(false));
        let impact:Value=serde_json::from_str(result.content[0].as_text().unwrap().text.split_once("## Revision-admitted change impact").unwrap().1.split_once("```json").unwrap().1.split_once("```").unwrap().0).unwrap();
        assert_eq!(impact["target"]["node_id"],wanted);assert_eq!(impact["target_revision_admitted"],true);
        assert_eq!(impact["direct_dependents_observed"],1,"The returned action must inspect method callers, including same-file calls, rather than whole-file consumers");
        assert_eq!(impact["immediate_consumers"][0]["node_id"],caller);
        assert_eq!(impact["immediate_consumers"][0]["sample_connection_to_target"]["source_line"],7);
        for name in ["chooselevel","Level","Other.Policy.ChooseLevel"] {
            let absent=navigate(&engram,json!({"project_id":PID,"scope":PATH,"symbol_name":name})).await;
            assert!(absent["files"][0]["symbols"].as_array().unwrap().is_empty());
        }
        let directory=navigate(&engram,json!({"project_id":PID,"scope":"src","symbol_name":"ChooseLevel","limit":8})).await;
        assert!(directory["files"].as_array().unwrap().is_empty(),"Directory mode retains its exact-name contract");
    });
}

#[test]
fn named_terminal_lookup_preserves_ambiguity_and_exact_name_priority() {
    owned_case(true,|engram,search| async move {
        const PATH:&str="src/Overloads.vb";
        let body=(1..=12).map(|i|format!("Public Function Run(arg{i} As Integer) As Integer : Return {i} : End Function\n")).collect::<String>();
        publish_named_sources(&engram,&search,&[(PATH,&body)]).await;
        let mut declarations=Vec::new();
        for line in 1..=10 {
            let name=if line<=2 {"Demo.First.Run".to_owned()}else{format!("Demo.Owner{line:02}.Run")};
            let mut declaration=node(&format!("sym:function:{PATH}:{name}:{line}"),"function",&name,PATH,1);
            declaration.start_line=line;declaration.end_line=line;declarations.push(declaration);
        }
        engram.state.graph.upsert_nodes(PID,&declarations).unwrap();
        let request=json!({"project_id":PID,"scope":PATH,"symbol_name":"Run"});
        let all=navigate(&engram,request.clone()).await;let file=&all["files"][0];let symbols=file["symbols"].as_array().unwrap();
        assert_eq!(symbols.len(),8);assert_eq!(all["coverage"]["declarations"][0]["matching_admitted_records_observed"],9);
        assert!(all["coverage"]["declarations"][0]["declaration_admission"]["reasons"].as_array().unwrap().contains(&json!("named_candidate_limit")));
        assert_ne!(symbols[0]["node_id"],symbols[1]["node_id"]);assert_eq!(symbols[0]["name"],symbols[1]["name"]);
        for selected in symbols {
            assert!(selected["name"].as_str().unwrap().ends_with(".Run"));
            let (_,read)=excerpt(&engram,selected["actions"][0]["arguments"].clone()).await;
            assert_eq!(read["source_file_hash"],ContentHash::compute(body.as_bytes()).0);
            assert!(selected["actions"].as_array().unwrap().iter().any(|a|a["tool"]=="impact_analysis" && a["arguments"]["symbol_fqn"]==selected["node_id"]));
        }
        let qualified=navigate(&engram,json!({"project_id":PID,"scope":PATH,"symbol_name":"Demo.First.Run"})).await;
        assert_eq!(qualified["files"][0]["symbols"].as_array().unwrap().len(),2);
        assert!(qualified["coverage"]["declarations"][0].get("terminal_name_fallback").is_none());
        let mut exact=node(&format!("sym:function:{PATH}:Run:11"),"function","Run",PATH,1);exact.start_line=11;exact.end_line=11;
        engram.state.graph.upsert_nodes(PID,&[exact.clone()]).unwrap();
        let preferred=navigate(&engram,request.clone()).await;
        assert_eq!(preferred["files"][0]["symbols"].as_array().unwrap().len(),1);assert_eq!(preferred["files"][0]["symbols"][0]["node_id"],exact.node_id);
        assert!(preferred["coverage"]["declarations"][0].get("terminal_name_fallback").is_none());
        assert!(preferred["coverage"]["declarations"][0]["declaration_admission"]["raw_rows"].as_u64().unwrap()<=5,"A healthy exact match must not scan unrelated qualified declarations");
        exact.generation=2;engram.state.graph.upsert_nodes(PID,&[exact]).unwrap();
        let recovered=navigate(&engram,request).await;
        assert_eq!(recovered["files"][0]["symbols"].as_array().unwrap().len(),8,"An unselected exact declaration must not hide selected qualified candidates");
        assert_eq!(recovered["coverage"]["declarations"][0]["terminal_name_fallback"]["attempted"],true);
    });
}

#[test]
fn named_terminal_lookup_shares_remaining_key_and_decode_budgets() {
    owned_case(true,|engram,search| async move {
        let body="Public Function Run() As Integer : Return 1 : End Function\n";
        publish_named_sources(&engram,&search,&[("src/keys.vb",body),("src/bytes.vb",body)]).await;
        for (path,fillers,neighbors,padded,reason) in [("src/keys.vb",32,16,false,"named_raw_key_limit"),("src/bytes.vb",20,12,true,"named_decode_budget")] {
            let mut declarations=Vec::new();
            let names=(0..fillers).map(|i|format!("A.Fill{i:02}")).chain((0..neighbors).map(|i|format!("Run:Shadow{i:02}")));
            for name in names {
                let mut declaration=node(&format!("sym:function:{path}:{name}:1"),"function",&name,path,1);
                if padded {declaration.metadata=Some(json!({"padding":"x".repeat(60*1024)}));}
                declarations.push(declaration);
            }
            declarations.push(node(&format!("sym:function:{path}:Zed.Run:1"),"function","Zed.Run",path,1));
            engram.state.graph.upsert_nodes(PID,&declarations).unwrap();
            let page=navigate(&engram,json!({"project_id":PID,"scope":path,"symbol_name":"Run"})).await;
            assert!(page["files"][0]["symbols"].as_array().unwrap().is_empty(),"The second phase must spend the remainder of the existing work budget, not reset it");
            let lookup=&page["coverage"]["declarations"][0];assert_eq!(lookup["terminal_name_fallback"]["attempted"],true);
            assert_eq!(lookup["terminal_name_fallback"]["bounded_procedure_completed"],false);
            assert!(lookup["declaration_admission"]["reasons"].as_array().unwrap().contains(&json!(reason)));
            assert!(lookup["declaration_admission"]["raw_rows"].as_u64().unwrap()<=64);
            assert!(lookup["raw_record_bytes_decoded"].as_u64().unwrap()<=2*1024*1024);
            let fallback=page["files"][0]["actions"].as_array().unwrap().iter().find(|a|a["tool"]=="get_full_method_body").unwrap();
            let (_,current)=excerpt(&engram,fallback["arguments"].clone()).await;
            assert_eq!(current["source_file_hash"],ContentHash::compute(body.as_bytes()).0);
        }
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
