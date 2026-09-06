#![allow(clippy::unwrap_used)]
//! External audit round 2, item 8 (TS→API route resolution): the extractors
//! must carry BOTH ends of a name-routed API call.
//!
//! Client side — a broker-style API takes the server function's name as a
//! string literal: `new api.ajax('athDeleteByID', …)`, `new api.ajax().call("x", …)`,
//! `new api.jsonAdapter('x', …)`. Nothing in the URL names the function, so the
//! literal is the only route evidence; it must become an `api_call` edge whose
//! target is the function name (kind `api_function`).
//!
//! Server side — the broker dispatches with `Select Case`: `Case "athDeleteByID"`
//! followed by `s = DeleteChangeRequest(qry)`. The Calls edge of that arm must
//! carry `dispatch_key = "athDeleteByID"` so the resolver can join the two.
use std::path::Path;

use engram_index::ExtractedEdge;
use engram_index::js_extractor::extract_js;
use engram_index::vb_extractor::extract_vb;

fn api_name_edges(edges: &[ExtractedEdge]) -> Vec<&ExtractedEdge> {
    edges
        .iter()
        .filter(|e| e.kind == "api_call" && e.target_kind.as_deref() == Some("api_function"))
        .collect()
}

#[test]
fn api_ajax_name_literal_is_an_api_call_edge_to_the_function_name() {
    let ts = r#"
namespace caw {
    export class manager {
        private del(id: number) {
            new api.ajax('athDeleteByID', { ath_id: id }, null, (ret) => {
                this.reload();
            });
        }
    }
}
"#;
    let (_, edges) = extract_js(Path::new("Site/modules/dashboard/ts/caw/caw/caw.ts"), ts);
    let api = api_name_edges(&edges);
    assert_eq!(api.len(), 1, "one name-routed call expected, got {api:?}");
    let e = api[0];
    assert_eq!(e.target_name, "athDeleteByID");
    assert_eq!(e.source_start_line, 5);
    let meta = e.metadata.as_ref().expect("metadata");
    assert_eq!(
        meta.get("ajax_target_method").map(String::as_str),
        Some("athDeleteByID")
    );
    assert_eq!(
        meta.get("ajax_transport").map(String::as_str),
        Some("api_name")
    );
}

#[test]
fn call_and_json_adapter_shapes_are_name_routes_too() {
    let ts = r#"
new api.ajax().call("iopGetAvailableImages", { io_pr_id: this._id }, null, (ret) => { });
let adapter = new api.jsonAdapter('fjGet', { fj_id: id }, null, (data) => { });
let ajax = new api.ajax();
ajax.call(apiFunctionName, apiParameters, apiData, onSuccess); // variable: no route
"#;
    let (_, edges) = extract_js(Path::new("Site/Q/api/jsonAdapter.ts"), ts);
    let mut names: Vec<&str> = api_name_edges(&edges)
        .iter()
        .map(|e| e.target_name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["fjGet", "iopGetAvailableImages"]);
}

#[test]
fn a_url_route_is_not_reported_as_a_name_route() {
    let ts = r#"
let req = new XMLHttpRequest();
req.open('POST', '/api.asmx/getimg', true);
"#;
    let (_, edges) = extract_js(Path::new("Site/Q/api/ajax.ts"), ts);
    // A URL route is NOT an api.ajax('name') NAME route: nothing carries the
    // `api_name` transport (that is the distinction this test guards).
    assert!(!edges.iter().any(|e| {
        e.metadata
            .as_ref()
            .and_then(|m| m.get("ajax_transport"))
            .map(String::as_str)
            == Some("api_name")
    }));
    // It keeps its web_service endpoint edge (migration + endpoint-class use)...
    let url = edges
        .iter()
        .find(|e| e.kind == "api_call" && e.target_name == "api.asmx")
        .expect("the web-method route keeps its endpoint edge");
    assert_eq!(
        url.metadata
            .as_ref()
            .and_then(|m| m.get("ajax_target_method"))
            .map(String::as_str),
        Some("getimg")
    );
    // ...and (round-7 WebMethod-fetch fix) ALSO a distinct method-function edge
    // so the served method resolves and multi-method-per-service calls don't
    // collide — but via the ORIGINAL transport, not `api_name`.
    assert!(
        edges
            .iter()
            .any(|e| e.target_name == "getimg" && e.target_kind.as_deref() == Some("api_function"))
    );
}

#[test]
fn vb_select_case_arm_stamps_its_call_with_the_dispatch_key() {
    let vb = r#"
Imports System

Public Class api

    Public Shared Function dispatch(ByVal qry As JSONqry) As JSONreturn
        Dim s As JSONreturn = Nothing
        Select Case qry.func
            ' ÄTA - (Change Request)
            Case "athDeleteByID"
                s = DeleteChangeRequest(qry)
            Case "athGet"
                s = GetChangeRequest(qry)
            Case Else
                s = Nothing
        End Select
        LogCall(qry)
        Return s
    End Function

End Class
"#;
    let (_, edges) = extract_vb(Path::new("Site/App_Code/api-json/api-broker.vb"), vb);
    let key_of = |callee: &str| -> Option<String> {
        edges
            .iter()
            .find(|e| e.kind == "calls" && e.target_name == callee)
            .unwrap_or_else(|| panic!("no calls edge to {callee}: {edges:?}"))
            .metadata
            .as_ref()
            .and_then(|m| m.get("dispatch_key"))
            .cloned()
    };
    assert_eq!(
        key_of("DeleteChangeRequest").as_deref(),
        Some("athDeleteByID")
    );
    assert_eq!(key_of("GetChangeRequest").as_deref(), Some("athGet"));
    // `LogCall(qry)` sits outside the arms: whatever edge it gets, no key.
    let keyed_outside = edges.iter().any(|e| {
        e.kind == "calls"
            && e.target_name == "LogCall"
            && e.metadata
                .as_ref()
                .is_some_and(|m| m.contains_key("dispatch_key"))
    });
    assert!(
        !keyed_outside,
        "a call outside the Case arms carries no key"
    );
}

#[test]
fn dispatch_arm_edges_come_from_a_pass_both_extraction_paths_share() {
    // Live r43: the Roslyn sidecar path never ran the arm scan (it lived in
    // the regex fallback only), so production carried no dispatch keys.
    let vb = r#"
Public Class api
    Public Shared Function dispatch(ByVal qry As JSONqry) As JSONreturn
        Dim s As JSONreturn = Nothing
        Select Case qry.func
            Case "athDeleteByID"
                s = DeleteChangeRequest(qry)
            Case "athGet"
                s = GetChangeRequest(qry)
            Case Else
                s = Nothing
        End Select
        LogCall(qry)
        Return s
    End Function
End Class
"#;
    let ranges = vec![(3u32, 16u32, "api.dispatch".to_string())];
    let edges = engram_index::vb_extractor::vb_dispatch_arm_edges(vb, &ranges);
    let key_of = |callee: &str| -> Option<String> {
        edges
            .iter()
            .find(|e| e.kind == "calls" && e.target_name == callee)
            .and_then(|e| e.metadata.as_ref())
            .and_then(|m| m.get("dispatch_key"))
            .cloned()
    };
    assert_eq!(
        key_of("DeleteChangeRequest").as_deref(),
        Some("athDeleteByID")
    );
    assert_eq!(key_of("GetChangeRequest").as_deref(), Some("athGet"));
    assert!(key_of("LogCall").is_none(), "{edges:?}");
    assert!(
        edges
            .iter()
            .all(|e| e.source_name == "api.dispatch" && e.source_kind == "function"),
        "arm edges hang off the enclosing function: {edges:?}"
    );
    assert_eq!(
        edges[0].source_start_line, 7,
        "the arm's call line, not the function's"
    );
}

#[test]
fn a_member_call_carries_its_receiver() {
    // `new api.ajax().getImage(…)` — the property name alone is ambiguous
    // across the project; the receiver text lets the resolver bind it.
    let ts = r#"
namespace orders {
    export class orderInfoPanel {
        public loadImages(): void {
            new api.ajax().getImage('orders', 1, 'Img.1');
        }
    }
}
"#;
    let extractor = engram_index::SymbolExtractor::new();
    let (_, edges) =
        extractor.extract(std::path::Path::new("Site/ts/orders/orderInfoPanel.ts"), ts);
    let call = edges
        .iter()
        .find(|e| e.kind == "calls" && e.target_name.contains("getImage"))
        .unwrap_or_else(|| panic!("no calls edge to getImage: {edges:?}"));
    let receiver = call
        .metadata
        .as_ref()
        .and_then(|m| m.get("receiver"))
        .cloned()
        .unwrap_or_default();
    assert!(
        receiver.contains("api.ajax"),
        "receiver must name the wrapper class; got {receiver:?} in {call:?}"
    );
}
