use super::{
    execution,
    routing::{self, Source},
};
use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

const PAGE: &str = r#"<%@ Page CodeFile="Checkout.aspx.vb" AutoEventWireup="false" %>
<asp:Button ID="btnBuy" runat="server" OnClick="Purchase" />
<!-- <asp:Button ID="btnBuy" OnClick="Wrong" /> -->"#;
const VB: &str = "Public Class Checkout\nPublic Sub Initial() Handles Me.Load\nIf Not IsPostBack Then LoadCart()\nEnd Sub\nPublic Sub Purchase() Handles btnBuy.Click\nOrders.Save()\nResponse.Redirect(\"done.aspx\")\nEnd Sub\nPublic Sub Wrong() ' Handles btnBuy.Click\nEnd Sub\nEnd Class\n";
const HELPER: &str = "Public Class Orders\nPublic Shared Sub Save()\nCallProcedure(\"dbo.CreateOrder\")\nEnd Sub\nEnd Class\n";
const SQL: &str =
    "CREATE PROCEDURE dbo.CreateOrder AS\nINSERT INTO dbo.Orders (Name) VALUES ('test');\n";

#[test]
fn request_follows_explicit_interface_implementation_to_database_evidence() {
    interface_trace(false);
}

#[test]
fn request_withholds_changed_interface_implementation() {
    interface_trace(true);
}

fn interface_trace(stale_implementation: bool) {
    use engram_graph::store::{InterfaceBinding, InterfaceBindings, InterfaceMemberLocator, INTERFACE_BINDINGS};
    let temporary = tempfile::tempdir().unwrap();
    let graph = Arc::new(GraphStore::open(&temporary.path().join("graph.redb")).unwrap());
    let caller_source = "Public Class Consumer\nPublic Sub Run()\n_service.Save()\nEnd Sub\nEnd Class\n";
    let contract_source = "Public Interface IStore\nSub Save()\nEnd Interface\n";
    let implementation_source = "Public Class Store\nImplements IStore\nPublic Sub Persist() Implements IStore.Save\nOrders.Save()\nEnd Sub\nEnd Class\n";
    let decoy_source = "Public Class OtherStore\nPublic Sub Persist()\nEnd Sub\nEnd Class\n";
    let mut source_set = vec![
        Source { path:"Consumer.vb",content:caller_source },
        Source { path:"IStore.vb",content:contract_source },
        Source { path:"Store.vb",content:implementation_source },
        Source { path:"OtherStore.vb",content:decoy_source },
        Source { path:"Orders.vb",content:HELPER },
        Source { path:"orders.sql",content:SQL },
    ];
    let make = |path: &str, name: &str, start, end, contract| {
        let id = engram_core::ids::NodeId::symbol("function", None, path, name, start).0;
        let mut n = node(&id, "function", name, path, start, end);
        n.metadata = Some(json!({"semantic_member_id":format!("M:{name}"),
            "semantic_span_start":start.to_string(),"semantic_span_length":"5",
            "interface_member_declaration":if contract {"true"} else {"false"}}));
        n
    };
    let caller = make("Consumer.vb","Consumer.Run",2,4,false);
    let contract = make("IStore.vb","IStore.Save",2,2,true);
    let implementation = make("Store.vb","Store.Persist",3,5,false);
    let decoy = make("OtherStore.vb","OtherStore.Persist",2,3,false);
    let save = make("Orders.vb","Orders.Save",2,4,false);
    let procedure = node("proc","stored_procedure","dbo.CreateOrder","orders.sql",1,2);
    let table = node("table","db_table","dbo.Orders","orders.sql",2,2);
    let mut nodes = vec![caller.clone(),contract.clone(),implementation.clone(),decoy.clone(),save.clone(),procedure.clone(),table.clone()];
    let mut selected = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for source in &source_set {
        let hash = blake3::hash(source.content.as_bytes()).to_hex().to_string();
        hashes.insert(source.path,hash.clone());
        let mut file = node(&format!("file:{}",source.path),"file",source.path,source.path,1,source.content.lines().count() as u32);
        file.metadata = Some(json!({"file_hash":hash,"vb_syntax_source_sha256":"c".repeat(64),"vb_parser_provenance":{"backend":"roslyn","parser":"fixture"}}));
        nodes.push(file);selected.insert(source.path.to_string(),1);
    }
    let edge = |source: &Node,target: &Node,kind| Edge {
        source_id:source.node_id.clone(),target_id:target.node_id.clone(),edge_kind:kind,weight:1,updated_at_ms:0,namespace:"memory".into(),language:"vbnet".into(),generation:1,metadata:None,
    };
    let mapping = |source: &Node,kind| {
        let mut e = edge(source,&contract,kind);
        let binding = InterfaceBinding {
            target:InterfaceMemberLocator {version:1,name:contract.name.clone(),semantic_id:"M:IStore.Save".into(),file_path:"IStore.vb".into(),start_line:2,span_start:2,span_length:5,syntax_source_sha256:"c".repeat(64),contract_type_key:"a".repeat(64),contract_type:"Global.IStore".into()},
            source_semantic_id:format!("M:{}",source.name),source_generation:1,source_file_hash:hashes[source.file_path.as_str()].clone(),target_generation:1,target_file_hash:hashes["IStore.vb"].clone(),parser:json!({"backend":"roslyn","parser":"fixture"}),reference_lines:vec![source.start_line],reference_lines_truncated:false,
        };
        e.metadata = Some(json!({INTERFACE_BINDINGS:InterfaceBindings {version:1,complete:true,bindings:vec![binding]}}));e
    };
    let edges = vec![mapping(&caller,EdgeKind::Calls),mapping(&implementation,EdgeKind::Implements),
        edge(&implementation,&save,EdgeKind::Calls),edge(&save,&procedure,EdgeKind::CallsStoredProcedure),edge(&procedure,&table,EdgeKind::StoredProcWritesTable)];
    graph.upsert_nodes_and_edges("trace",&nodes,&edges).unwrap();
    if stale_implementation {source_set[2].content = "Public Class Store\nEnd Class\n";}
    let result = execution::trace_selected(&graph,"trace",execution::empty_trace("Consumer.vb",true,false),vec![(caller,"explicit source entry".into())],true,&source_set,&selected).unwrap();
    assert!(!result.paths.iter().any(|hop| hop.to==decoy.node_id));
    if stale_implementation {
        assert!(!result.paths.iter().any(|hop| hop.to==implementation.node_id || hop.to=="table"));
        assert!(!result.gaps.is_empty());
    } else {
        let dispatch = result.paths.iter().find(|hop| hop.kind=="interface_implementation_candidate").expect("explicit implementation must be navigable");
        assert_eq!(dispatch.from,contract.node_id);assert_eq!(dispatch.to,implementation.node_id);
        assert!(dispatch.target_source_verified);
        assert!(dispatch.evidence.as_ref().unwrap()["interpretation"].as_str().unwrap().contains("not proven"));
        assert!(result.source_members.iter().any(|m|m["file"]=="Store.vb" && m["excerpt"].as_str().unwrap().contains("Orders.Save()")));
        assert!(result.paths.iter().any(|hop|hop.to=="table"));
    }
    drop(graph);temporary.close().unwrap();
}

fn sources<'a>(vb: &'a str) -> Vec<Source<'a>> {
    vec![
        Source {
            path: "Site/Global.asax",
            content: "<%@ Application %>",
        },
        Source {
            path: "Site/App_Code/RouteConfig.vb",
            content: "routes.MapPageRoute(\"buy\", \"buy/{id}\", \"~/Checkout.aspx\")",
        },
        Source {
            path: "Site/Checkout.aspx",
            content: PAGE,
        },
        Source {
            path: "Site/Checkout.aspx.vb",
            content: vb,
        },
        Source {
            path: "Site/App_Code/Orders.vb",
            content: HELPER,
        },
        Source {
            path: "db/orders.sql",
            content: SQL,
        },
    ]
}

fn node(id: &str, kind: &str, name: &str, path: &str, start: u32, end: u32) -> Node {
    Node {
        node_id: id.into(),
        node_type: kind.into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: start,
        end_line: end,
        generation: 1,
        metadata: None,
    }
}

struct Fixture {
    // Fields drop in declaration order: database handles close before TempDir.
    graph: Arc<GraphStore>,
    _temp: tempfile::TempDir,
    selected: BTreeMap<String, u64>,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let graph = Arc::new(GraphStore::open(&temp.path().join("graph.redb")).unwrap());
    let mut nodes = vec![
        node(
            "load",
            "function",
            "Checkout.Initial",
            "Site/Checkout.aspx.vb",
            2,
            4,
        ),
        node(
            "buy",
            "function",
            "Checkout.Purchase",
            "Site/Checkout.aspx.vb",
            5,
            8,
        ),
        node(
            "decoy",
            "function",
            "Checkout.Wrong",
            "Site/Checkout.aspx.vb",
            9,
            10,
        ),
        node(
            "save",
            "function",
            "Orders.Save",
            "Site/App_Code/Orders.vb",
            2,
            4,
        ),
        node(
            "sp",
            "stored_procedure",
            "dbo.CreateOrder",
            "db/orders.sql",
            1,
            2,
        ),
        node("table", "db_table", "dbo.Orders", "db/orders.sql", 2, 2),
    ];
    let mut selected = BTreeMap::new();
    for source in sources(VB) {
        let mut file = node(
            &format!("file:{}", source.path),
            "file",
            source.path,
            source.path,
            1,
            source.content.lines().count() as u32,
        );
        file.metadata =
            Some(json!({"file_hash":blake3::hash(source.content.as_bytes()).to_hex().to_string()}));
        nodes.push(file);
        selected.insert(source.path.into(), 1);
    }
    let edges: Vec<_> = [
        ("buy", "save", EdgeKind::Calls),
        ("save", "sp", EdgeKind::CallsStoredProcedure),
        ("sp", "table", EdgeKind::StoredProcWritesTable),
    ]
    .into_iter()
    .map(|(source, target, kind)| Edge {
        source_id: source.into(),
        target_id: target.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        edge_kind: kind,
        weight: 1,
        generation: 1,
        metadata: Some(json!({"confidence":1.0,"basis":"authored fixture"})),
        updated_at_ms: 1,
    })
    .collect();
    graph
        .upsert_nodes_and_edges("project", &nodes, &edges)
        .unwrap();
    Fixture {
        graph,
        _temp: temp,
        selected,
    }
}

#[test]
fn click_trace_includes_authored_load_handler_and_cross_file_database_path() {
    let fixture = fixture();
    let sources = sources(VB);
    let route = routing::resolve("POST /buy/42", &sources)
        .unwrap()
        .candidates
        .remove(0);
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("click btnBuy"),
        true,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap();
    let mut handlers: Vec<_> = trace.handlers.iter().map(|h| h.name.as_str()).collect();
    handlers.sort();
    assert_eq!(handlers, vec!["Initial", "Purchase"]);
    assert!(!trace.handlers.iter().any(|h| h.node_id == "decoy"));
    assert!(
        trace
            .paths
            .iter()
            .any(|edge| edge.from == "save" && edge.to == "sp")
    );
    assert!(
        trace
            .paths
            .iter()
            .any(|edge| edge.from == "sp" && edge.target_name == "dbo.Orders")
    );
    assert!(trace.source_members.iter().any(|member| {
        member["excerpt"]
            .as_str()
            .unwrap()
            .contains("Response.Redirect")
    }));
    assert_eq!(
        trace.trigger_coverage,
        "unknown_no_trigger_binding_in_existing_graph"
    );
}

#[test]
fn stale_source_and_wrong_physical_revision_withhold_handler_paths() {
    let fixture = fixture();
    let changed = VB.replace("Orders.Save()", "Other.Save()");
    let sources = sources(&changed);
    let route = routing::resolve("buy/42", &sources)
        .unwrap()
        .candidates
        .remove(0);
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("click btnBuy"),
        true,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap();
    assert!(trace.handlers.is_empty());
    assert!(trace.paths.is_empty());
    assert!(trace.gaps.iter().any(|gap| gap.contains("fingerprint")));
    let sources = self::sources(VB);
    let mut selected = fixture.selected.clone();
    selected.insert("Site/Checkout.aspx.vb".into(), 2);
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        None,
        true,
        false,
        &sources,
        &selected,
    )
    .unwrap();
    assert!(trace.handlers.is_empty());
}

#[test]
fn db_opt_out_and_unknown_action_do_not_invent_evidence() {
    let fixture = fixture();
    let sources = sources(VB);
    let route = routing::resolve("buy/42", &sources)
        .unwrap()
        .candidates
        .remove(0);
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("click btnBuy"),
        false,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap();
    assert_eq!(trace.paths.len(), 1);
    assert_eq!(trace.paths[0].kind, "calls");
    assert_eq!(trace.trigger_coverage, "not_requested_include_db_false");
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("unbound event"),
        true,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap();
    assert!(trace.handlers.is_empty());
    assert!(trace.paths.is_empty());
}

#[test]
fn mismatched_edge_revision_cannot_expand_an_otherwise_current_handler() {
    let fixture = fixture();
    let sources = sources(VB);
    fixture
        .graph
        .upsert_edges(
            "project",
            &[Edge {
                source_id: "buy".into(),
                target_id: "save".into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                edge_kind: EdgeKind::Calls,
                weight: 1,
                generation: 2,
                metadata: None,
                updated_at_ms: 2,
            }],
        )
        .unwrap();
    let route = routing::resolve("buy/42", &sources)
        .unwrap()
        .candidates
        .remove(0);
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("click btnBuy"),
        true,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap();
    assert!(!trace.handlers.is_empty());
    assert!(trace.paths.is_empty());
    assert!(trace.gaps.iter().any(|gap| gap.contains("edge revision")));
}

#[test]
fn unknown_target_span_is_a_reference_not_a_verified_definition() {
    let fixture = fixture();
    let sources = sources(VB);
    let mut target = fixture.graph.get_node("project", "save").unwrap().unwrap();
    target.start_line = 0;
    target.end_line = 0;
    fixture.graph.upsert_nodes("project", &[target]).unwrap();
    let route = routing::resolve("buy/42", &sources)
        .unwrap()
        .candidates
        .remove(0);
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("click btnBuy"),
        true,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap();
    let reference = trace.paths.iter().find(|edge| edge.to == "save").unwrap();
    assert!(!reference.target_source_verified);
    assert!(reference.target_line.is_none());
    assert!(
        !trace
            .source_members
            .iter()
            .any(|member| member["node_id"] == "save")
    );
    assert!(!trace.paths.iter().any(|edge| edge.from == "save"));
}

#[test]
fn reached_method_resolves_sql_source_without_fabricated_database_graph_edges() {
    let fixture = fixture();
    let mut sources = sources(VB);
    let helper = "Public Class Orders\nPublic Shared Sub Save()\nDim cmd As New SqlCommand(\"dbo.CreateOrder\")\ncmd.CommandType = System.Data.CommandType.StoredProcedure\ncmd.ExecuteNonQuery()\nEnd Sub\nEnd Class\n";
    sources
        .iter_mut()
        .find(|source| source.path == "Site/App_Code/Orders.vb")
        .unwrap()
        .content = helper;
    let mut file = fixture
        .graph
        .get_node("project", "file:Site/App_Code/Orders.vb")
        .unwrap()
        .unwrap();
    file.metadata = Some(json!({"file_hash":blake3::hash(helper.as_bytes()).to_hex().to_string()}));
    let mut method = fixture.graph.get_node("project", "save").unwrap().unwrap();
    method.end_line = 6;
    fixture
        .graph
        .upsert_nodes("project", &[file, method])
        .unwrap();
    fixture
        .graph
        .delete_edges_of_kind("project", &EdgeKind::CallsStoredProcedure)
        .unwrap();
    fixture
        .graph
        .delete_edges_of_kind("project", &EdgeKind::StoredProcWritesTable)
        .unwrap();
    let route = routing::resolve("buy/42", &sources)
        .unwrap()
        .candidates
        .remove(0);
    let trace = execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("click btnBuy"),
        true,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap();
    assert!(
        trace
            .paths
            .iter()
            .any(|edge| edge.kind == "calls_stored_procedure"
                && edge.target_name == "dbo.CreateOrder"
                && edge.target_source_verified)
    );
    assert!(
        trace
            .paths
            .iter()
            .any(|edge| edge.kind == "stored_proc_writes_table" && edge.target_name == "Orders")
    );
    assert!(
        trace
            .source_members
            .iter()
            .any(|member| member["file"] == "db/orders.sql"
                && member["recovery"]["arguments"]["project_id"] == "project")
    );
}

const ORM_BODY: &str = "Public Class Orders\nPublic Shared Function Save() As Object\nDim db As New WarehouseDataContext\nDim data = From row In db.inv_items Select row\nReturn data\nEnd Function\nEnd Class\n";

#[test]
fn reached_inline_update_is_source_bound_and_obeys_database_selection() {
    let fixture = fixture();
    let mut sources = sources(VB);
    let helper = "Public Class Orders\nPublic Shared Sub Save()\nUsing db As New WarehouseDataContext()\ndb.ExecuteCommand(\"UPDATE dbo.Orders \" & \"SET Name = {0}\", name)\nEnd Using\nEnd Sub\nEnd Class\n";
    sources.iter_mut().find(|s| s.path == "Site/App_Code/Orders.vb").unwrap().content = helper;
    let mut file = fixture.graph.get_node("project", "file:Site/App_Code/Orders.vb").unwrap().unwrap();
    file.metadata = Some(json!({"file_hash":blake3::hash(helper.as_bytes()).to_hex().to_string()}));
    let mut method = fixture.graph.get_node("project", "save").unwrap().unwrap();
    method.end_line = 6;
    fixture.graph.upsert_nodes("project", &[file, method]).unwrap();
    fixture.graph.delete_edges_of_kind("project", &EdgeKind::CallsStoredProcedure).unwrap();
    fixture.graph.delete_edges_of_kind("project", &EdgeKind::StoredProcWritesTable).unwrap();
    let route = routing::resolve("buy/42", &sources).unwrap().candidates.remove(0);
    for include_db in [true, false] {
        let trace = execution::trace_page(&fixture.graph, "project", &route, Some("click btnBuy"), include_db, true, &sources, &fixture.selected).unwrap();
        let dependencies: Vec<_> = trace.paths.iter().filter(|p| p.kind == "inline_sql_writes_table").collect();
        assert_eq!(dependencies.len(), usize::from(include_db));
        if let Some(dependency) = dependencies.first() {
            assert_eq!(dependency.target_name, "Orders");
            let evidence = dependency.evidence.as_ref().unwrap();
            assert_eq!(evidence["file"], "Site/App_Code/Orders.vb");
            assert_eq!(evidence["line"], 4);
            assert_eq!(evidence["source_blake3"], blake3::hash(helper.as_bytes()).to_hex().to_string());
        }
    }
    sources.iter_mut().find(|s| s.path == "Site/App_Code/Orders.vb").unwrap().content = "changed source";
    let stale = execution::trace_page(&fixture.graph, "project", &route, Some("click btnBuy"), true, true, &sources, &fixture.selected).unwrap();
    assert!(!stale.paths.iter().any(|p| p.kind.starts_with("inline_sql_")));
}
const DBML: &str = "<Database Class=\"WarehouseDataContext\" xmlns=\"http://schemas.microsoft.com/linqtosql/dbml/2007\">\n  <Table Name=\"dbo.Inventory\" Member=\"inv_items\"><Type Name=\"InventoryRow\" /></Table>\n</Database>";

fn orm_trace(
    helper: &str,
    mapping: &str,
    other: Option<&str>,
    state: &str,
    include_db: bool,
) -> execution::PageTrace {
    let mut fixture = fixture();
    let mut sources = sources(VB);
    sources
        .iter_mut()
        .find(|s| s.path == "Site/App_Code/Orders.vb")
        .unwrap()
        .content = helper;
    sources.push(Source {
        path: "Site/App_Code/Inventory.dbml",
        content: mapping,
    });
    if let Some(other) = other {
        sources.push(Source {
            path: "Site/App_Code/Other.dbml",
            content: other,
        });
    }
    let mut changed = Vec::new();
    for source in sources
        .iter()
        .filter(|s| s.path.ends_with(".dbml") || s.path.ends_with("Orders.vb"))
    {
        let mut file = node(
            &format!("file:{}", source.path),
            "file",
            source.path,
            source.path,
            1,
            source.content.lines().count() as u32,
        );
        file.metadata =
            Some(json!({"file_hash":blake3::hash(source.content.as_bytes()).to_hex().to_string()}));
        if source.path.ends_with("Inventory.dbml") {
            if state == "stale" {
                file.metadata = Some(json!({"file_hash":"old source"}));
            }
            if state == "revision" {
                file.generation = 2;
            }
        }
        if !(source.path.ends_with("Inventory.dbml") && state == "unselected") {
            fixture.selected.insert(source.path.into(), 1);
        }
        changed.push(file);
    }
    if state == "missing" {
        sources.retain(|s| !s.path.ends_with("Inventory.dbml"));
    }
    let mut method = fixture.graph.get_node("project", "save").unwrap().unwrap();
    method.end_line = helper.lines().count() as u32 - 1;
    changed.push(method);
    fixture.graph.upsert_nodes("project", &changed).unwrap();
    fixture
        .graph
        .delete_edges_of_kind("project", &EdgeKind::CallsStoredProcedure)
        .unwrap();
    fixture
        .graph
        .delete_edges_of_kind("project", &EdgeKind::StoredProcWritesTable)
        .unwrap();
    let query_line = helper
        .lines()
        .position(|line| line.contains("Dim data = From"))
        .unwrap()
        + 1;
    fixture
        .graph
        .upsert_edges(
            "project",
            &[Edge {
                source_id: "save".into(),
                target_id: "table:inv_items".into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                edge_kind: EdgeKind::QueriesTable,
                weight: 1,
                generation: 1,
                updated_at_ms: 1,
                metadata: Some(
                    json!({"orm":"true", "access":"read", "src_line":query_line.to_string()}),
                ),
            }],
        )
        .unwrap();
    let route = routing::resolve("buy/42", &sources)
        .unwrap()
        .candidates
        .remove(0);
    execution::trace_page(
        &fixture.graph,
        "project",
        &route,
        Some("click btnBuy"),
        include_db,
        true,
        &sources,
        &fixture.selected,
    )
    .unwrap()
    // Fixture drops graph before TempDir, including assertion-unwind paths.
}

#[test]
fn reached_orm_member_uses_its_context_mapping_and_preserves_graph_evidence() {
    let other = DBML
        .replace("WarehouseDataContext", "OtherDataContext")
        .replace("dbo.Inventory", "other.Unrelated");
    let trace = orm_trace(ORM_BODY, DBML, Some(&other), "current", true);
    let edge = trace
        .paths
        .iter()
        .find(|p| p.kind == "queries_table")
        .unwrap();
    let mapping = edge
        .table_mapping
        .as_ref()
        .expect("reached context/member mapping");
    assert_eq!(mapping["table"], "dbo.Inventory");
    assert_eq!(
        mapping["mapping_source"]["file"],
        "Site/App_Code/Inventory.dbml"
    );
    assert_eq!(mapping["mapping_source"]["line"], 2);
    assert_eq!(
        mapping["mapping_source"]["source_blake3"],
        blake3::hash(DBML.as_bytes()).to_hex().to_string()
    );
    assert_eq!(mapping["query_source"]["line"], 4);
    assert_eq!(mapping["receiver"], "db");
    assert_eq!(edge.to, "table:inv_items");
    assert_eq!(edge.evidence.as_ref().unwrap()["access"], "read");
    assert!(
        !edge.target_source_verified,
        "DBML is not proof of a physical table definition"
    );
    assert!(
        mapping["interpretation"]
            .as_str()
            .unwrap()
            .contains("Not a table definition")
    );
}

#[test]
fn bom_and_crlf_dbml_citations_keep_original_lines_and_content_hash() {
    let mapping = format!(
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\r\n{}",
        DBML.replace('\n', "\r\n")
    );
    let trace = orm_trace(ORM_BODY, &mapping, None, "current", true);
    let mapped = trace
        .paths
        .iter()
        .find_map(|p| p.table_mapping.as_ref())
        .expect("BOM DBML mapping");
    assert_eq!(mapped["mapping_source"]["line"], 3);
    assert_eq!(
        mapped["mapping_source"]["source_blake3"],
        blake3::hash(mapping.as_bytes()).to_hex().to_string()
    );
    assert!(mapping.lines().nth(2).unwrap().contains("<Table Name="));
}

#[test]
fn dbml_mapping_withholds_ambiguous_stale_missing_and_unselected_evidence() {
    for state in ["stale", "revision", "missing", "unselected"] {
        let trace = orm_trace(ORM_BODY, DBML, None, state, true);
        assert!(
            trace.paths.iter().all(|p| p.table_mapping.is_none()),
            "{state}"
        );
        assert!(trace.gaps.iter().any(|g| g.contains("DBML")), "{state}");
    }
    let conflicting = DBML.replace("dbo.Inventory", "other.Inventory");
    let trace = orm_trace(ORM_BODY, DBML, Some(&conflicting), "current", true);
    assert!(trace.paths.iter().all(|p| p.table_mapping.is_none()));
    assert!(
        trace
            .gaps
            .iter()
            .any(|g| g.contains("context is missing or ambiguous"))
    );
    let duplicate_member = DBML.replace(
        "</Database>",
        "<Table Name=\"other.Items\" Member=\"inv_items\" /></Database>",
    );
    let trace = orm_trace(ORM_BODY, &duplicate_member, None, "current", true);
    assert!(trace.paths.iter().all(|p| p.table_mapping.is_none()));
}

#[test]
fn malformed_dbml_and_xml_examples_cannot_become_table_bindings() {
    let commented = DBML
        .replace("<Table ", "<!-- <Table ")
        .replace("</Table>", "</Table> -->");
    let prefixed = DBML
        .replace("<Database ", "<d:Database ")
        .replace("xmlns=", "xmlns:d=")
        .replace("</Database>", "</d:Database>")
        .replace("<Table ", "<d:Table ")
        .replace("</Table>", "</d:Table>")
        .replace("<Type ", "<d:Type ");
    let good = orm_trace(ORM_BODY, &prefixed, None, "current", true);
    assert!(
        good.paths.iter().any(|p| p.table_mapping.is_some()),
        "namespace-qualified DBML is supported"
    );
    for mapping in [
        DBML.replace("</Database>", ""),
        DBML.replace("dbo.Inventory", "&unknown;"),
        format!("<!DOCTYPE Database SYSTEM 'file:///not-read'>{DBML}"),
        DBML.replace(
            "http://schemas.microsoft.com/linqtosql/dbml/2007",
            "urn:unrelated",
        ),
        commented,
    ] {
        let trace = orm_trace(ORM_BODY, &mapping, None, "current", true);
        assert!(
            trace.paths.iter().all(|p| p.table_mapping.is_none()),
            "{mapping}"
        );
        assert!(trace.gaps.iter().any(|g| g.contains("DBML")));
    }
}

#[test]
fn unknown_aliased_reassigned_or_chained_contexts_remain_unresolved() {
    for declaration in [
        "Dim db As New OtherDataContext",
        "Dim db = suppliedContext",
        "Dim original As New WarehouseDataContext\nDim db = original",
        "Dim db As New WarehouseDataContext\ndb = suppliedContext",
        "Dim db = New WarehouseDataContext().Other",
        "Dim db As New WarehouseDataContext\nMutate(ByRef db)",
        "' Dim db As New WarehouseDataContext",
        "Dim example = \"Dim db As New WarehouseDataContext\"",
    ] {
        let helper = ORM_BODY.replace("Dim db As New WarehouseDataContext", declaration);
        let trace = orm_trace(&helper, DBML, None, "current", true);
        assert!(
            trace.paths.iter().all(|p| p.table_mapping.is_none()),
            "{declaration}"
        );
        assert!(
            trace
                .gaps
                .iter()
                .any(|g| g.contains("ORM") || g.contains("DBML"))
        );
    }
}

#[test]
fn database_opt_out_does_not_parse_or_return_dbml_mappings() {
    let trace = orm_trace(ORM_BODY, "not XML", None, "current", false);
    assert!(
        trace
            .paths
            .iter()
            .all(|p| p.kind != "queries_table" && p.table_mapping.is_none())
    );
    assert!(!trace.gaps.iter().any(|g| g.contains("DBML")));
}
