#![allow(clippy::unwrap_used)]
//! Row-4 audit (docs/audits/04-concept-and-consumer-discovery.md) A6/D6:
//! consumers of the core anchors were labelled by the raw edge kind only
//! (`[queries_table] src -> table`) — no read/write/delete/export/test
//! role, so a reader had to open every consumer to learn who mutates the
//! table. Every consumer line carries a role derived from the edge kind +
//! the source member name/path, the section header tallies the roles and
//! names the rule (bodies are not inspected — that limit is stated).

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::models::GetConceptFootprintRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use serde_json::json;

const PID: &str = "footprint-consumer-role-test";

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state)
}

fn func(path: &str, class: &str, name: &str) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{class}.{name}:1"),
        node_type: "function".into(),
        name: name.into(),
        namespace: class.into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 3,
        generation: 1,
        metadata: None,
    }
}

fn table(name: &str) -> Node {
    Node {
        node_id: format!("db:table:{name}"),
        node_type: "db_table".into(),
        name: name.into(),
        namespace: "dbo".into(),
        language: "sql".into(),
        file_path: RelPath::new("Site/App_Code/db.dbml"),
        start_line: 1,
        end_line: 1,
        generation: 1,
        metadata: None,
    }
}

fn edge(src: &str, tgt: &str, kind: EdgeKind) -> Edge {
    Edge {
        source_id: src.into(),
        target_id: tgt.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        edge_kind: kind,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

async fn footprint(engram: &Engram) -> String {
    let req: GetConceptFootprintRequest =
        serde_json::from_value(json!({"project_id": PID, "concept": "personalliggare"})).unwrap();
    let res = engram.handle_get_concept_footprint(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

fn consumer_line<'a>(out: &'a str, needle: &str) -> &'a str {
    out.lines()
        .find(|l| l.starts_with("- [") && l.contains(needle))
        .unwrap_or_else(|| panic!("no consumer line mentions {needle}:\n{out}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_consumer_carries_a_role_and_the_header_tallies_the_roles() {
    let (_tmp, state) = build_state();
    let t = table("personalliggare");
    // LINQ writes/deletes reach the graph as QueriesTable edges — the role
    // has to come from the member name, not the edge kind alone.
    let saver = func(
        "Site/App_Code/gd/personalliggare.vb",
        "personalliggare",
        "Save",
    );
    let deleter = func(
        "Site/App_Code/gd/personalliggare.vb",
        "personalliggare",
        "DeleteInBulk",
    );
    let reader = func("Site/pages/PL/list.aspx.vb", "list", "BindGrid");
    let exporter = func(
        "Site/pages/PL/export_personalliggare.aspx.vb",
        "export_personalliggare",
        "Page_Load",
    );
    let tester = func(
        "Site/App_Code/test_personalliggare.vb",
        "test_personalliggare",
        "Save_writes",
    );
    let session = func("Site/pages/PL/filter.aspx.vb", "filter", "RememberFilter");
    let nodes = vec![
        t.clone(),
        saver.clone(),
        deleter.clone(),
        reader.clone(),
        exporter.clone(),
        tester.clone(),
        session.clone(),
    ];
    let edges = vec![
        edge(&saver.node_id, &t.node_id, EdgeKind::QueriesTable),
        edge(&deleter.node_id, &t.node_id, EdgeKind::QueriesTable),
        edge(&reader.node_id, &t.node_id, EdgeKind::QueriesTable),
        edge(&exporter.node_id, &t.node_id, EdgeKind::QueriesTable),
        edge(&tester.node_id, &t.node_id, EdgeKind::QueriesTable),
        edge(&session.node_id, &t.node_id, EdgeKind::WritesState),
    ];
    state.graph.upsert_nodes(PID, &nodes).unwrap();
    state.graph.upsert_edges(PID, &edges).unwrap();
    let engram = Engram::new(state.clone());

    let out = footprint(&engram).await;

    for (needle, role) in [
        ("personalliggare.Save:", "write"),
        ("personalliggare.DeleteInBulk:", "delete"),
        ("list.BindGrid:", "read"),
        ("export_personalliggare.Page_Load:", "export"),
        ("test_personalliggare.Save_writes:", "test"),
        ("filter.RememberFilter:", "write"),
    ] {
        let line = consumer_line(&out, needle);
        assert!(
            line.starts_with(&format!("- [{role}:")),
            "{needle} must be rendered with role {role} (then the raw edge kind), got:\n{line}"
        );
    }
    let header = out
        .lines()
        .find(|l| l.starts_with("## Consumers of core anchors"))
        .unwrap_or_else(|| panic!("no consumers header:\n{out}"));
    for tally in ["write 2", "read 1", "delete 1", "export 1", "test 1"] {
        assert!(
            header.contains(tally),
            "header must tally `{tally}`: {header}"
        );
    }
    assert!(
        header.contains("edge kind") && header.contains("not inspected"),
        "header must name the rule and its limit (bodies not inspected): {header}"
    );
}

/// Live finding (release 15, `redovisningskategori`): the DAL reader
/// `GetCodeWithEstimateAndReportedQty` was labelled `export` because its
/// name contains "report" — the export words must be about producing an
/// export (export / excel / pdf / download / .rdl), not any "report".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reported_quantity_reader_is_a_reader_not_an_export() {
    let (_tmp, state) = build_state();
    let t = table("personalliggare");
    let reader = func(
        "Site/App_Code/gd/personalliggare.vb",
        "personalliggare",
        "GetCodeWithEstimateAndReportedQty",
    );
    let rdl = func(
        "Site/Reports/personalliggare.rdl",
        "personalliggare",
        "Dataset1",
    );
    state
        .graph
        .upsert_nodes(PID, &[t.clone(), reader.clone(), rdl.clone()])
        .unwrap();
    state
        .graph
        .upsert_edges(
            PID,
            &[
                edge(&reader.node_id, &t.node_id, EdgeKind::QueriesTable),
                edge(&rdl.node_id, &t.node_id, EdgeKind::QueriesTable),
            ],
        )
        .unwrap();
    let engram = Engram::new(state.clone());

    let out = footprint(&engram).await;

    let line = consumer_line(&out, "GetCodeWithEstimateAndReportedQty:");
    assert!(
        line.starts_with("- [read:"),
        "a *ReportedQty* reader is a read, not an export:\n{line}"
    );
    let line = consumer_line(&out, "personalliggare.rdl:");
    assert!(
        line.starts_with("- [export:"),
        "an .rdl report definition is an export:\n{line}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sql_call_with_an_unknown_verb_is_not_guessed() {
    let (_tmp, state) = build_state();
    let t = table("personalliggare");
    let sp = func(
        "Site/App_Code/gd/personalliggare.vb",
        "personalliggare",
        "RunProc",
    );
    state
        .graph
        .upsert_nodes(PID, &[t.clone(), sp.clone()])
        .unwrap();
    state
        .graph
        .upsert_edges(PID, &[edge(&sp.node_id, &t.node_id, EdgeKind::SqlCalls)])
        .unwrap();
    let engram = Engram::new(state.clone());

    let out = footprint(&engram).await;

    let line = consumer_line(&out, "personalliggare.RunProc:");
    assert!(
        line.starts_with("- [sql?:sql_calls]"),
        "a SqlCalls consumer whose name states no verb must be `sql?` (unknown), never read/write:\n{line}"
    );
}
