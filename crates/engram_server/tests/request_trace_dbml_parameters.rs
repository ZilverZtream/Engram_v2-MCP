//! Reached VB parameter declarations can supply explicitly qualified DBML evidence.
use engram_core::{NodeId, RelPath};
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use engram_server::services::request_trace::{
    execution,
    routing::{self, Source},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const FILE: &str = "Site/Query.aspx.vb";
const DBML: &str = "Site/App_Code/Inventory.dbml";
const MAPPING: &str = "<Database Class='WarehouseDataContext' xmlns='http://schemas.microsoft.com/linqtosql/dbml/2007'>\n<Table Name='dbo.OrderRelation' Member='OrderRelations'><Type Name='OrderRelation'/></Table>\n</Database>\n";
struct Fixture {
    graph: Arc<GraphStore>,
    _owned: tempfile::TempDir,
    files: Vec<(String, String)>,
    selected: BTreeMap<String, u64>,
    helper_id: String,
}
fn node(id: String, kind: &str, name: &str, path: &str, start: usize, end: usize) -> Node {
    Node {
        node_id: id,
        node_type: kind.into(),
        name: name.into(),
        namespace: "code".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: start as u32,
        end_line: end as u32,
        generation: 1,
        metadata: None,
    }
}
impl Fixture {
    fn new(parameter: &str, statement: &str) -> Self {
        let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new()
            .prefix("d")
            .rand_bytes(2)
            .tempdir_in(fixture_root)
            .unwrap();
        let graph = Arc::new(GraphStore::open(&owned.path().join("g.redb")).unwrap());
        let vb = format!(
            "Public Class Query\nPublic Sub Page_Load()\nQuery.Load(Nothing)\nEnd Sub\nPublic Shared Function Load({parameter}) As Object\n{statement}\nReturn db.OrderRelations\nEnd Function\nEnd Class\n"
        );
        let files:Vec<(String,String)>=vec![("Site/Global.asax".into(),"<%@ Application Language='VB' %>\n".into()),
            ("Site/Query.aspx".into(),"<%@ Page Language='VB' CodeFile='Query.aspx.vb' Inherits='Query' AutoEventWireup='true' %>\n".into()),
            (FILE.into(),vb),(DBML.into(),MAPPING.into())];
        let mut selected = BTreeMap::new();
        let mut nodes = Vec::new();
        for (path, text) in &files {
            selected.insert(path.clone(), 1);
            let mut file = node(
                NodeId::file(path).0,
                "file",
                path,
                path,
                1,
                text.lines().count(),
            );
            file.metadata =
                Some(json!({"file_hash":blake3::hash(text.as_bytes()).to_hex().to_string()}));
            nodes.push(file);
        }
        let page = node(
            NodeId::symbol("function", None, FILE, "Query.Page_Load", 2).0,
            "function",
            "Query.Page_Load",
            FILE,
            2,
            4,
        );
        let helper = node(
            NodeId::symbol("function", None, FILE, "Query.Load", 5).0,
            "function",
            "Query.Load",
            FILE,
            5,
            8,
        );
        let helper_id = helper.node_id.clone();
        let edges = vec![
            Edge {
                source_id: page.node_id.clone(),
                target_id: helper_id.clone(),
                namespace: "code".into(),
                language: "vbnet".into(),
                edge_kind: EdgeKind::Calls,
                weight: 1,
                generation: 1,
                updated_at_ms: 1,
                metadata: None,
            },
            Edge {
                source_id: helper_id.clone(),
                target_id: "table:orderrelations".into(),
                namespace: "code".into(),
                language: "vbnet".into(),
                edge_kind: EdgeKind::QueriesTable,
                weight: 1,
                generation: 1,
                updated_at_ms: 1,
                metadata: Some(json!({"orm":"true","src_line":"7"})),
            },
        ];
        nodes.extend([page, helper]);
        graph.upsert_nodes("owned", &nodes).unwrap();
        graph.upsert_edges("owned", &edges).unwrap();
        Self {
            graph,
            _owned: owned,
            files,
            selected,
            helper_id,
        }
    }
    fn trace(&self, include_db: bool) -> Value {
        let sources = self
            .files
            .iter()
            .map(|(path, content)| Source { path, content })
            .collect::<Vec<_>>();
        let mut routes = routing::resolve("/Query.aspx", &sources).unwrap();
        assert_eq!(routes.candidates.len(), 1);
        let trace = execution::trace_page(
            &self.graph,
            "owned",
            &routes.candidates.remove(0),
            None,
            include_db,
            false,
            &sources,
            &self.selected,
        )
        .unwrap();
        assert!(
            trace
                .paths
                .iter()
                .any(|p| p.to == self.helper_id && p.target_source_verified),
            "Helper must be reached before judging its mapping"
        );
        serde_json::to_value(trace).unwrap()
    }
}
fn bindings(trace: &Value) -> Vec<&Value> {
    trace["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.get("table_mapping"))
        .filter(|v| !v.is_null())
        .collect()
}
#[test]
fn typed_parameter_and_local_constructor_keep_distinct_source_binding_evidence() {
    for (parameter, statement, basis) in [
        (
            "db As WarehouseDataContext",
            "",
            "selected_current_dbml_and_declared_vb_parameter_type",
        ),
        (
            "ByVal db As Global.WarehouseDataContext",
            "",
            "selected_current_dbml_and_declared_vb_parameter_type",
        ),
        (
            "",
            "Dim db As New WarehouseDataContext()",
            "selected_current_dbml_and_explicit_local_vb_context",
        ),
    ] {
        let f = Fixture::new(parameter, statement);
        let trace = f.trace(true);
        let found = bindings(&trace);
        assert_eq!(found.len(), 1);
        let mapping = found[0];
        assert_eq!(mapping["table"], "dbo.OrderRelation");
        assert_eq!(mapping["context"], "WarehouseDataContext");
        assert_eq!(mapping["basis"], basis);
        assert_eq!(mapping["query_source"], json!({"file":FILE,"line":7}));
        assert_eq!(
            mapping["mapping_source"],
            json!({"file":DBML,"line":2,"source_blake3":blake3::hash(MAPPING.as_bytes()).to_hex().to_string()})
        );
        if !parameter.is_empty() {
            assert_eq!(mapping["receiver_binding"]["declaration_line"], 5);
            assert_eq!(
                mapping["receiver_binding"]["compiler_binding_verified"],
                false
            );
            assert_eq!(mapping["receiver_binding"]["runtime_argument_known"], false);
        } else {
            assert!(mapping.get("receiver_binding").is_none());
        }
        assert!(bindings(&f.trace(false)).is_empty());
    }
}
#[test]
fn unsupported_parameter_flows_and_stale_dbml_do_not_acquire_a_binding() {
    for (parameter, statement) in [
        ("ByRef db As WarehouseDataContext", ""),
        ("Optional db As WarehouseDataContext = Nothing", ""),
        ("db As WarehouseDataContext", "db = Other()"),
        ("db As WarehouseDataContext", "Consume(db)"),
    ] {
        let f = Fixture::new(parameter, statement);
        let trace = f.trace(true);
        assert!(
            trace["paths"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["kind"] == "queries_table")
        );
        assert!(bindings(&trace).is_empty(), "{parameter}: {statement}");
    }
    let mut f = Fixture::new("db As WarehouseDataContext", "");
    f.files
        .iter_mut()
        .find(|(path, _)| path == DBML)
        .unwrap()
        .1
        .push_str("\n");
    let stale = f.trace(true);
    assert!(bindings(&stale).is_empty());
    assert!(
        stale["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap.as_str().unwrap().contains("DBML"))
    );
}
