#![allow(clippy::unwrap_used)]
//! External audit round 2, item 8 — end to end: "which server API functions
//! does <file>.ts call?" must reach the VB implementation through the name
//! route (`api.ajax('ordGetLines')` → broker `Case "ordGetLines"` →
//! `GetOrderLines`). Live r43 found the callee hop's two items and then let
//! concept chunks evict them: a hop from the file the question NAMES is
//! direct evidence, not a 0.6 guess — and the hop must also read the file
//! node's own ApiCall edges, since a call outside any function body is still
//! that file calling the server.
use engram_core::config::Config;
use engram_server::models::AskCodebaseRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::{Value, json};

async fn build() -> (tempfile::TempDir, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    for d in [
        "Site/ts/orders",
        "Site/ts/misc",
        "Site/Q/api",
        "Site/App_Code/api-json",
        "Site/App_Code/orders/api-json",
        "Site/App_Code/noise",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("Site/ts/orders/orderPanel.ts"),
        "namespace orders {\n    export class orderPanel {\n        private _id: number;\n        public load(): void {\n            new api.ajax('ordGetLines', { ord_id: this._id }, null, (ret) => {\n                this.render(ret);\n            });\n        }\n        private render(ret: any): void {\n        }\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/api-json/api-broker.vb"),
        "Public Class api\n    Public Shared Function action(ByVal qry As JSONqry) As JSONreturn\n        Dim s As JSONreturn = Nothing\n        Select Case qry.func\n            Case \"ordGetLines\"\n                s = GetOrderLines(qry)\n            Case \"ordGetNoiseA\"
                s = ordGetNoiseA(qry)
            Case \"ordGetNoiseB\"
                s = ordGetNoiseB(qry)
            Case \"ordGetNoiseC\"
                s = ordGetNoiseC(qry)
        End Select\n        Return s\n    End Function\nEnd Class\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/orders/api-json/api-orders.vb"),
        "Partial Class api\n    Public Shared Function GetOrderLines(ByVal qry As JSONqry) As JSONreturn\n        Dim lines = orderLines.LoadByOrder(qry.ord_id)\n        Return Nothing\n    End Function\nEnd Class\n",
    )
    .unwrap();
    // The wrapper class: TS reaches /api.asmx/getimg through api.ajax().getImage.
    std::fs::write(
        root.join("Site/Q/api/ajax.ts"),
        "namespace api {
    export class ajax {
        public getImage(module: string, id: number, imageName: string): void {
            let req = new XMLHttpRequest();
            req.open('POST', '/api.asmx/getimg', true);
            req.send(JSON.stringify({ module: module, id: id, name: imageName }));
        }
    }
}
",
    )
    .unwrap();
    // A LEGACY client twin of the API name (live r45: `athDeleteByID` is also
    // a caw.js function, and the resolver bound the mention to it instead of
    // the broker's implementation).
    std::fs::write(
        root.join("Site/ts/misc/legacy.js"),
        "function ordGetLines(orderId) {
    return null;
}
",
    )
    .unwrap();
    // A decoy getImage so only the receiver (`new api.ajax()`) disambiguates.
    std::fs::write(
        root.join("Site/ts/misc/thumbs.ts"),
        "export function getImage(cacheKey: any): void {
}
",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/api.asmx"),
        "<%@ WebService Language=\"VB\" CodeBehind=\"api.vb\" Class=\"api\" %>
",
    )
    .unwrap();
    std::fs::write(
        root.join("Site/App_Code/api-json/api-images.vb"),
        "Partial Class api
    Public Function getimg(ByVal o As imgData) As String
        Return \"\"
    End Function
End Class
",
    )
    .unwrap();
    // A SECOND family member: "order info panel" now matches two distinct
    // stems, so no unique compound file exists — the FAMILY seeds the hop
    // (cycle 32, owner-approved) instead of one entity being minted.
    std::fs::write(
        root.join("Site/ts/orders/ataOrderInfoPanel.ts"),
        "namespace orders {
    export class ataOrderInfoPanel {
        private _id: number;
        public loadImages(): void {
            new api.ajax().getImage('ata', this._id, 'Img.1');
        }
    }
}
",
    )
    .unwrap();
    // Live r49: the real family has FIVE members (ata/io/permit/pl/vehicle
    // MarkerInfowindow) and the 2..=4 gate rejected it — a family is a family
    // whatever its size; the seed list is what gets capped.
    for extra in [
        "vehicleOrderInfoPanel",
        "permitOrderInfoPanel",
        "plOrderInfoPanel",
    ] {
        std::fs::write(
            root.join(format!("Site/ts/orders/{extra}.ts")),
            format!(
                "namespace orders {{
    export class {extra} {{
        private _id: number;
        public loadImages(): void {{
            new api.ajax().getImage('x', this._id, 'Img.1');
        }}
    }}
}}
"
            ),
        )
        .unwrap();
    }
    // The compiled twin: same stem, so distinct-stem uniqueness must still
    // pick the .ts source (live r46: ioMarkerInfowindow.{ts,js}).
    std::fs::write(
        root.join("Site/ts/orders/orderInfoPanel.js"),
        "var orders;
(function (orders) {
    var orderInfoPanel = (function () {
        function orderInfoPanel() { }
        orderInfoPanel.prototype.loadImages = function () {
            new api.ajax().getImage('orders', this._id, 'Img.1');
        };
        return orderInfoPanel;
    })();
})(orders || (orders = {}));
",
    )
    .unwrap();
    // Three more name-routed calls whose implementations live in three files:
    // without the named-seed cap raise they fill the hop before the wrapper.
    for (name, file) in [
        ("ordGetNoiseA", "api-noisea.vb"),
        ("ordGetNoiseB", "api-noiseb.vb"),
        ("ordGetNoiseC", "api-noisec.vb"),
    ] {
        std::fs::write(
            root.join(format!("Site/App_Code/orders/api-json/{file}")),
            format!(
                "Partial Class api
    Public Shared Function {name}(ByVal qry As JSONqry) As JSONreturn
        Return Nothing
    End Function
End Class
"
            ),
        )
        .unwrap();
    }
    // The info panel: a COMPOUND name ("order info panel" -> orderInfoPanel.ts)
    // whose images arrive through the wrapper, two hops from this file.
    std::fs::write(
        root.join("Site/ts/orders/orderInfoPanel.ts"),
        "namespace orders {
    export class orderInfoPanel {
        private _id: number;
        public loadImages(): void {
            new api.ajax('ordGetNoiseA', { o: this._id }, null, null);
            new api.ajax('ordGetNoiseB', { o: this._id }, null, null);
            new api.ajax('ordGetNoiseC', { o: this._id }, null, null);
            new api.ajax().getImage('orders', this._id, 'Img.1');
        }
    }
}
",
    )
    .unwrap();
    // Enough chunks about "server api functions" and "order panel" to fill the
    // evidence cap on their own.
    for i in 0..30 {
        std::fs::write(
            root.join(format!("Site/App_Code/noise/order_panel_server_api{i:02}.vb")),
            format!(
                "Public Class order_panel_server_api{i:02}\n    ' the order panel and the server API functions it depends on\n    Public Function ServerApiFunction{i:02}() As String\n        Return \"order panel server api functions\"\n    End Function\nEnd Class\n"
            ),
        )
        .unwrap();
    }
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(200),
        max_project_bytes: Some(4 * 1024 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "FileCallee".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid)
}

async fn ask(engram: &Engram, pid: &str, question: &str) -> Value {
    let req: AskCodebaseRequest = serde_json::from_value(json!({
        "project_id": pid,
        "question": question,
        "output_format": "json",
        "depth": "standard"
    }))
    .unwrap();
    let res = engram.handle_ask_codebase(req).await.unwrap();
    let t = res.content[0].as_text().unwrap().text.clone();
    let start = t.find('{').unwrap_or(0);
    serde_json::from_str(&t[start..]).unwrap_or_else(|e| panic!("not JSON ({e}):\n{t}"))
}

fn paths(v: &Value) -> Vec<String> {
    v["evidence"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| e["path"].as_str().unwrap_or("").to_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn what_a_named_ts_file_calls_reaches_the_vb_implementation_through_the_name_route() {
    let (_tmp, engram, pid) = build().await;
    let v = ask(
        &engram,
        &pid,
        "Which server API functions does orderPanel.ts call?",
    )
    .await;
    let ps = paths(&v);
    assert!(
        ps.iter().any(|p| p.ends_with("api-orders.vb")),
        "the served implementation must be cited; got {ps:?}"
    );
    assert!(
        ps.iter().any(|p| p.ends_with("orderpanel.ts")),
        "the asked file itself must be cited; got {ps:?}"
    );
}

#[tokio::test]
async fn who_calls_the_implementation_lists_the_ts_client() {
    let (_tmp, engram, pid) = build().await;
    let v = ask(&engram, &pid, "What calls GetOrderLines?").await;
    let ps = paths(&v);
    assert!(
        ps.iter().any(|p| p.ends_with("orderpanel.ts")),
        "the TS client reached through the broker arm must be cited; got {ps:?}"
    );
}

#[tokio::test]
async fn an_api_name_literal_binds_to_the_implementation_the_broker_dispatches_it_to() {
    // Live r44 (ox_causal_1): `athDeleteByID` names no symbol — only the broker's
    // arm knows it is served by DeleteChangeRequest — so "which VB function
    // handles it?" never cited the implementation file.
    let (_tmp, engram, pid) = build().await;
    let v = ask(
        &engram,
        &pid,
        "Which VB function handles the ordGetLines API?",
    )
    .await;
    let ps = paths(&v);
    assert!(
        ps.iter().any(|p| p.ends_with("api-orders.vb")),
        "the dispatched implementation must be cited; got {ps:?}"
    );
    // Live r46: the legacy twin + the served implementation were called
    // AMBIGUOUS. They are the two ends of one route, not competing symbols.
    assert_eq!(
        v["status"].as_str(),
        Some("answered"),
        "a name with a legacy twin and one served implementation is not ambiguous"
    );
}

#[tokio::test]
async fn a_compound_name_reaches_the_implementation_through_the_wrapper_route() {
    // The golden ox_multi_4 shape: "marker info window" names no token the
    // planner sees, and the image fetch is TWO hops (panel → api.ajax().getImage
    // → /api.asmx/getimg → api-images.vb).
    let (_tmp, engram, pid) = build().await;
    let v = ask(
        &engram,
        &pid,
        "How does the order info panel fetch its images?",
    )
    .await;
    let ps = paths(&v);
    // Cycle 32 (owner-approved): with TWO *OrderInfoPanel families no entity
    // is minted — a wrong guess and an ambiguity status are both worse — but
    // the FAMILY seeds the hop, so the served implementation still arrives.
    assert!(
        ps.iter().any(|p| p.ends_with("api-images.vb")),
        "the served implementation two hops away must be cited; got {ps:?}"
    );
}
