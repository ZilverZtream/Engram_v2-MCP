#![allow(clippy::unwrap_used)]
//! External audit round 2, item 8 (TS→API route resolution, the ImpactEngine
//! one-hop slice): a JavaScript/TypeScript API call must reach the server
//! method that serves it, not stop at the endpoint file or a name placeholder.
//!
//! Two routes exist in ASP.NET codebases:
//!   * web-method route: `xhr.open('POST', '/api.asmx/getimg')` — the extractor
//!     stores the method in `ajax_target_method`; the `.asmx` exposes a class;
//!     the VB method is `<class>.<method>`.
//!   * name route: `api.ajax('athDeleteByID', …)` — a broker `Select Case`
//!     dispatches the literal to an implementation (`Case "athDeleteByID"` →
//!     `s = DeleteChangeRequest(qry)`); the VB extractor stamps that Calls
//!     edge with `dispatch_key`.
//!
//! The post-ingest resolver adds an `ApiCall` edge from the ENCLOSING client
//! function (the callee arm walks function nodes, not file nodes) to the
//! serving method, stamped `route_method` / `route_dispatch`.
use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};

fn open_store(tmp: &tempfile::TempDir) -> GraphStore {
    GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open")
}

fn node(id: &str, node_type: &str, name: &str, path: &str, lines: (u32, u32)) -> Node {
    Node {
        node_id: id.to_string(),
        node_type: node_type.to_string(),
        name: name.to_string(),
        namespace: "memory".to_string(),
        language: if path.ends_with(".vb") {
            "vbnet"
        } else {
            "typescript"
        }
        .to_string(),
        file_path: RelPath::new(path),
        start_line: lines.0,
        end_line: lines.1,
        generation: 1,
        metadata: None,
    }
}

fn edge(kind: EdgeKind, source: &str, target: &str, meta: serde_json::Value) -> Edge {
    Edge {
        source_id: source.to_string(),
        target_id: target.to_string(),
        namespace: "memory".to_string(),
        language: "typescript".to_string(),
        edge_kind: kind,
        weight: 1,
        generation: 1,
        metadata: Some(meta),
        updated_at_ms: 1_000_000,
    }
}

const AJAX_FILE: &str = "file:Site/Q/api/ajax.ts";
const GET_IMAGE: &str = "sym:function:Site/Q/api/ajax.ts:getImage:160";
const ASMX: &str = "sym:web_service:Site/api.asmx:api.asmx:1";
const GETIMG: &str = "sym:function:Site/App_Code/api-json/api-images.vb:api.getimg:277";

fn web_method_fixture(graph: &GraphStore, pid: &str) {
    graph
        .upsert_nodes(
            pid,
            &[
                node(AJAX_FILE, "file", "ajax.ts", "Site/Q/api/ajax.ts", (0, 0)),
                node(
                    GET_IMAGE,
                    "function",
                    "getImage",
                    "Site/Q/api/ajax.ts",
                    (160, 180),
                ),
                node(ASMX, "web_service", "api.asmx", "Site/api.asmx", (1, 1)),
                node(
                    GETIMG,
                    "function",
                    "api.getimg",
                    "Site/App_Code/api-json/api-images.vb",
                    (277, 360),
                ),
            ],
        )
        .unwrap();
    graph
        .upsert_edges(
            pid,
            &[
                // the extractor's edge: file → endpoint, method only in metadata
                edge(
                    EdgeKind::ApiCall,
                    AJAX_FILE,
                    ASMX,
                    serde_json::json!({
                        "ajax_transport": "xhr",
                        "ajax_url": "/api.asmx/getimg",
                        "ajax_target_method": "getimg",
                        "src_line": "173"
                    }),
                ),
                // webforms: the .asmx exposes class `api` — a partial class spread
                // over twenty files, so the placeholder stays AMBIGUOUS/unresolved
                edge(
                    EdgeKind::ExposesWebService,
                    ASMX,
                    "::api",
                    serde_json::json!({}),
                ),
            ],
        )
        .unwrap();
}

#[test]
fn web_method_route_reaches_the_vb_method_from_the_enclosing_function() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "route-web-method";
    web_method_fixture(&graph, pid);

    graph.resolve_symbol_edges(pid).unwrap();

    let callees = graph
        .neighbors(pid, EdgeKind::ApiCall, GET_IMAGE, 10)
        .unwrap();
    assert!(
        callees.iter().any(|(t, _)| t == GETIMG),
        "getImage must reach api.getimg through /api.asmx/getimg; got {callees:?}"
    );
    // the endpoint edge is kept: the client still calls the .asmx
    let from_file = graph
        .neighbors(pid, EdgeKind::ApiCall, AJAX_FILE, 10)
        .unwrap();
    assert!(
        from_file.iter().any(|(t, _)| t == ASMX),
        "endpoint edge dropped: {from_file:?}"
    );
    // the new edge says how it was bound
    let incoming = graph
        .find_incoming_edges_with_kind(pid, Some(EdgeKind::ApiCall), GETIMG, 10)
        .unwrap();
    assert_eq!(
        incoming.len(),
        1,
        "exactly one route edge into api.getimg: {incoming:?}"
    );
}

#[test]
fn web_method_route_is_idempotent_across_resolver_runs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "route-idempotent";
    web_method_fixture(&graph, pid);

    graph.resolve_symbol_edges(pid).unwrap();
    graph.resolve_symbol_edges(pid).unwrap();

    let incoming = graph
        .find_incoming_edges_with_kind(pid, Some(EdgeKind::ApiCall), GETIMG, 10)
        .unwrap();
    assert_eq!(
        incoming.len(),
        1,
        "a second run must not duplicate: {incoming:?}"
    );
}

const CAW_FILE: &str = "file:Site/modules/dashboard/ts/caw/caw/caw.ts";
const ATH_DEL: &str = "sym:function:Site/modules/dashboard/ts/caw/caw/caw.ts:athDel:50";
const BROKER: &str = "sym:function:Site/App_Code/api-json/api-broker.vb:api.dispatch:30";
const DELETE_CR: &str =
    "sym:function:Site/App_Code/ata/api-json/api-atahuvud.vb:api.DeleteChangeRequest:197";

#[test]
fn name_route_reaches_the_implementation_through_the_broker_dispatch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "route-dispatch";
    graph
        .upsert_nodes(
            pid,
            &[
                node(
                    CAW_FILE,
                    "file",
                    "caw.ts",
                    "Site/modules/dashboard/ts/caw/caw/caw.ts",
                    (0, 0),
                ),
                node(
                    ATH_DEL,
                    "function",
                    "athDel",
                    "Site/modules/dashboard/ts/caw/caw/caw.ts",
                    (50, 70),
                ),
                node(
                    BROKER,
                    "function",
                    "api.dispatch",
                    "Site/App_Code/api-json/api-broker.vb",
                    (30, 400),
                ),
                node(
                    DELETE_CR,
                    "function",
                    "api.DeleteChangeRequest",
                    "Site/App_Code/ata/api-json/api-atahuvud.vb",
                    (197, 230),
                ),
            ],
        )
        .unwrap();
    graph
        .upsert_edges(
            pid,
            &[
                // `new api.ajax('athDeleteByID', …)` at caw.ts:58 — no VB symbol
                // carries that name, so by-name resolution cannot bind it
                edge(
                    EdgeKind::ApiCall,
                    CAW_FILE,
                    "::athDeleteByID",
                    serde_json::json!({
                        "ajax_transport": "api_name",
                        "ajax_target_method": "athDeleteByID",
                        "src_line": "58"
                    }),
                ),
                // broker: `Case "athDeleteByID"` → `s = DeleteChangeRequest(qry)`
                edge(
                    EdgeKind::Calls,
                    BROKER,
                    DELETE_CR,
                    serde_json::json!({ "dispatch_key": "athDeleteByID" }),
                ),
            ],
        )
        .unwrap();

    graph.resolve_symbol_edges(pid).unwrap();

    let callees = graph
        .neighbors(pid, EdgeKind::ApiCall, ATH_DEL, 10)
        .unwrap();
    assert!(
        callees.iter().any(|(t, _)| t == DELETE_CR),
        "athDel must reach api.DeleteChangeRequest through the broker's Case arm; got {callees:?}"
    );
    let callers = graph
        .find_incoming_edges_with_kind(pid, Some(EdgeKind::ApiCall), DELETE_CR, 10)
        .unwrap();
    assert!(
        callers.iter().any(|(s, _, _)| s == ATH_DEL),
        "who-calls DeleteChangeRequest must list the TS function; got {callers:?}"
    );
}
