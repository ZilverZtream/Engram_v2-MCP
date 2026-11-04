//! Public orientation describes the published code selection separately from
//! retained corpora and the live graph. No parser/model/native query is needed.
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_graph::{Edge, EdgeKind, Node};
use engram_index::{HybridSearchEngine, IndexDoc};
use engram_server::{state::AppState, tools::Engram};
use serde_json::json;
use std::{future::Future, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

const PID: &str = "overview-source";
const A: &str = "src/service/a.rs";
const B: &str = "src/model/b.py";

fn doc(namespace: &str, generation: u64, chunk: u64, path: &str, language: &str, text: &str) -> IndexDoc {
    IndexDoc {
        namespace: namespace.into(), generation, chunk_id: chunk,
        path: RelPath::new(path), language: language.into(), content: text.into(),
        author: None, timestamp: None, start_line: 1, end_line: 1,
        doc_id: format!("{path}-{chunk}"), content_hash: ContentHash::compute(text.as_bytes()).0,
    }
}

fn node(id: &str, kind: &str, name: &str, path: &str, generation: u64) -> Node {
    Node {
        node_id: id.into(), node_type: kind.into(), name: name.into(), namespace: "memory".into(),
        language: "rust".into(), file_path: RelPath::new(path), start_line: 1, end_line: 1,
        generation, metadata: None,
    }
}

async fn fixture(owned: &Path, managed: bool) -> (Engram, Arc<HybridSearchEngine>) {
    let root = owned.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let config = Config { data_dir: owned.join("data"), allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(), ..Default::default() };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _) = AppState::new(config).unwrap();
    let project = ProjectRecord {
        project_id: PID.into(), project_name: "Selected source overview".into(),
        project_type: "general".into(), directory: root.to_str().unwrap().into(),
        created_at_ms: 1, updated_at_ms: 1, reindex_required_since_ms: None,
    };
    if managed { state.registry.register_source_project(&project).unwrap(); }
    else { state.registry.put_project(&project).unwrap(); }
    let runtime = engram_server::services::project_service::ensure_project_runtime(&state, PID).await.unwrap();
    let search = runtime.search.clone();
    drop(runtime);
    (Engram::new(state), search)
}

// Database and native/blocking worker ownership ends before the owned tree,
// including panic unwinding. These fixtures start no application actors/jobs.
fn owned_case<F, Fut>(managed: bool, run: F)
where F: FnOnce(Engram, Arc<HybridSearchEngine>) -> Fut, Fut: Future<Output = ()> {
    let temporary = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(async {
        let (engram, search) = fixture(temporary.path(), managed).await;
        run(engram, search).await;
    });
    drop(runtime);
    temporary.close().unwrap();
}

async fn overview(engram: &Engram) -> String {
    let response = engram.handle_get_codebase_overview(serde_json::from_value(json!({"project_id":PID})).unwrap())
        .await.unwrap();
    assert!(!response.is_error.unwrap_or(false));
    response.content.iter().filter_map(|content| content.as_text())
        .map(|content| content.text.as_str()).collect()
}

fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    text.split_once(heading).unwrap().1.split("\n--- ").next().unwrap()
}

#[test]
fn overview_counts_current_inherited_source_separately_from_retained_corpora_and_live_graph() {
    owned_case(true, |engram, search| async move {
        let cancel = CancellationToken::new();
        let initial = engram.state.registry.capture_source_head(PID).unwrap();
        search.prepare_full_source_snapshot(PID, 0, 1, &[RelPath::new(A), RelPath::new(B)], &cancel).await.unwrap();
        search.index_docs(PID, &[
            doc("memory",1,1,A,"rust","old Rust chunk one"),
            doc("memory",1,2,A,"rust","old Rust chunk two"),
            doc("memory",1,3,B,"python","inherited Python source"),
        ], &cancel).await.unwrap();
        search.seal_source_snapshot(PID,1).unwrap();
        assert!(engram.state.registry.publish_source_head(&initial,1).unwrap());
        let old_view = search.pin_source_generation(PID,1).unwrap();

        let base = engram.state.registry.capture_source_head(PID).unwrap();
        search.prepare_source_snapshot(PID,1,2,&[RelPath::new(A)],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",2,4,A,"rust","current Rust source")],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,2).unwrap();
        assert!(engram.state.registry.publish_source_head(&base,2).unwrap());
        // A real sealed, unpublished candidate remains physically present.
        search.prepare_source_snapshot(PID,2,3,&[RelPath::new("src/abandoned.rs")],&[],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("memory",3,5,"src/abandoned.rs","vbnet","unpublished candidate")],&cancel).await.unwrap();
        search.seal_source_snapshot(PID,3).unwrap();
        let mut nodes = vec![
            node(&format!("file:{A}"),"file","a.rs",A,2),
            node(&format!("file:{B}"),"file","b.py",B,1),
            node("file:src/abandoned.rs","file","abandoned.rs","src/abandoned.rs",3),
            node("current-a","function","CurrentService",A,2),
            node("inherited-b","class","InheritedModel",B,1),
            node("obsolete-a","function","ObsoleteService",A,1),
            node("future-a","function","FutureService",A,3),
            node("future-file","class","AbandonedType","src/abandoned.rs",3),
            node("schema-table","db_table","AuditTable",A,2),
            node("config-key","app_setting","AuditEnabled",A,2),
        ];
        let mut edges = Vec::new();
        for number in 0..24 {
            let id = format!("schema-column-{number}");
            nodes.push(node(&id,"db_column",&format!("AuditColumn{number}"),A,2));
            edges.push(Edge { source_id:"schema-table".into(),target_id:id,
                namespace:"memory".into(),language:"sql".into(),edge_kind:EdgeKind::HasColumn,
                weight:100,generation:2,metadata:None,updated_at_ms:1 });
        }
        engram.state.graph.upsert_nodes(PID,&nodes).unwrap();
        engram.state.graph.upsert_edges(PID,&edges).unwrap();
        let before_knowledge = overview(&engram).await;
        assert!(before_knowledge.contains("source_files: 2\n"));
        assert!(before_knowledge.contains("source_chunks: 2\n"));
        assert!(before_knowledge.contains("retained_memory_documents: 5\n"));
        assert!(before_knowledge.contains("src/service: 1 files") && before_knowledge.contains("src/model: 1 files"));
        let languages = section(&before_knowledge,"--- Language Breakdown (selected source chunks) ---");
        assert!(languages.contains("rust: 1 (50%)") && languages.contains("python: 1 (50%)"));
        assert!(!languages.contains("vbnet"));
        let candidates = section(&before_knowledge,"--- Source Entry/Type Candidates");
        assert!(candidates.contains("CurrentService") && candidates.contains("InheritedModel"));
        for excluded in ["ObsoleteService","FutureService","AbandonedType","AuditColumn"] {
            assert!(!candidates.contains(excluded), "Unselected revisions and schema hubs must not describe current code: {excluded}");
        }
        assert!(before_knowledge.contains("Full Graph Inventory (all recorded revisions"));
        assert!(before_knowledge.contains("Recorded file nodes: 3"));
        assert!(before_knowledge.contains("Database tables: 1") && before_knowledge.contains("AuditTable"));
        assert!(before_knowledge.contains("Config: 1 app settings"));
        assert!(!before_knowledge.contains("Top Central Nodes (PageRank)"));
        assert!(before_knowledge.contains("retained_vector_rows: not measured"));

        search.index_docs(PID,&[
            doc("history",1,6,"history/one","vbnet","historical snippet"),
            doc("history",1,7,"history/two","vbnet","another historical snippet"),
        ],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("business_logic",1,8,"knowledge/note","vbnet","retained knowledge")],&cancel).await.unwrap();
        search.index_docs(PID,&[doc("antipattern",1,9,"history/pattern","vbnet","retained anti-pattern")],&cancel).await.unwrap();
        let after_knowledge = overview(&engram).await;
        assert!(after_knowledge.contains("source_files: 2\n") && after_knowledge.contains("source_chunks: 2\n"));
        assert_eq!(section(&after_knowledge,"--- Language Breakdown (selected source chunks) ---"), languages);
        assert!(after_knowledge.contains("retained_index_documents: 9\n"));
        assert!(after_knowledge.contains("retained_history_documents: 2\n"));
        assert!(after_knowledge.contains("retained_antipattern_documents: 1\n"));
        drop(old_view);
    });
}

#[test]
fn overview_reports_unknown_legacy_file_coverage_without_fabricating_zero_or_current_architecture() {
    owned_case(false, |engram, search| async move {
        let cancel = CancellationToken::new();
        // This producer really materializes a complete flat legacy generation
        // before attesting its absent HEAD. No managed/catalog flags are forged.
        search.index_docs(PID,&[doc("memory",1,1,A,"rust","complete legacy source")],&cancel).await.unwrap();
        let base = engram.state.registry.capture_source_head(PID).unwrap();
        assert!(engram.state.registry.attest_legacy_source_head(&base).unwrap());
        let report = overview(&engram).await;
        assert!(report.contains("source_files: unknown (legacy selection has no complete file manifest)"));
        assert!(!report.contains("source_files: 0"));
        assert!(report.contains("source_chunks: 1\n"));
        assert!(report.contains("rust: 1 (100%)"));
        assert!(report.contains("coverage is unknown"));
        assert!(!report.contains("--- Architecture ---"));
    });
}
