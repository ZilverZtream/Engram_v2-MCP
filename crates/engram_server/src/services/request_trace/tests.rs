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
