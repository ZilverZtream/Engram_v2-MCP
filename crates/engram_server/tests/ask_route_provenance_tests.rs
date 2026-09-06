#![allow(clippy::unwrap_used)]
//! Round-8 P0-3: a wrapper/broker-mediated api_call must be labelled with its
//! route PROVENANCE, never rendered as a direct call. The callee walk used to
//! emit "<src> calls <target>" identically for a direct edge and a
//! getImage-wrapper edge, and the report called the overall answer "direct".

use engram_core::RelPath;
use engram_core::config::Config;
use engram_graph::{Edge, EdgeKind, Node};
use engram_server::services::ask_engine::providers::exhaustive_callee_set;
use engram_server::state::AppState;
use serde_json::json;

const PID: &str = "route-provenance-test";

fn state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
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

fn node(id: &str, ty: &str, name: &str, path: &str) -> Node {
    Node {
        node_id: id.into(),
        node_type: ty.into(),
        name: name.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 20,
        generation: 1,
        metadata: None,
    }
}

#[test]
fn wrapper_mediated_api_call_is_labelled_via_not_direct() {
    let (_tmp, state) = state();
    let g = &state.graph;
    let getimg = node(
        "sym:function:api-images.vb:api.getimg:1",
        "function",
        "api.getimg",
        "Site/App_Code/api-json/api-images.vb",
    );
    let caller_file = node(
        "file:Site/ts/imgHandler.ts",
        "file",
        "imgHandler.ts",
        "Site/ts/imgHandler.ts",
    );
    g.upsert_nodes(PID, &[getimg.clone(), caller_file.clone()])
        .unwrap();
    // A wrapper-mediated api_call: imgHandler.ts → getimg VIA the getImage wrapper.
    g.upsert_edges(
        PID,
        &[Edge {
            source_id: "file:Site/ts/imgHandler.ts".into(),
            target_id: "sym:function:api-images.vb:api.getimg:1".into(),
            namespace: "memory".into(),
            language: "javascript".into(),
            edge_kind: EdgeKind::ApiCall,
            weight: 1,
            generation: 1,
            metadata: Some(json!({"via": "getImage_wrapper", "ajax_url": "/api.asmx/getimg"})),
            updated_at_ms: 0,
        }],
    )
    .unwrap();

    let mut id = 0usize;
    let (items, members, _cov, _proof) = exhaustive_callee_set(
        g,
        None,
        PID,
        "Site/ts/imgHandler.ts",
        &[EdgeKind::ApiCall],
        &mut id,
    );

    let m = members
        .iter()
        .find(|m| m.display_name == "api.getimg")
        .expect("getimg must be a callee member");
    assert_eq!(
        m.via.as_deref(),
        Some("getImage_wrapper (→ /api.asmx/getimg)"),
        "the mediated route provenance must be carried on the member"
    );
    assert!(
        m.relation.contains("via_wrapper"),
        "the relation must mark the hop as wrapper-mediated, not a bare api_call: {}",
        m.relation
    );
    let text = items
        .iter()
        .map(|i| i.content.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("VIA getImage_wrapper") && text.contains("NOT a direct call"),
        "the rendered evidence must say the call is mediated, not direct:\n{text}"
    );
}

#[test]
fn a_direct_api_call_has_no_via_and_reads_plainly() {
    let (_tmp, state) = state();
    let g = &state.graph;
    let svc = node(
        "sym:function:svc.vb:svc.DoThing:1",
        "function",
        "svc.DoThing",
        "Site/App_Code/svc.vb",
    );
    let caller = node("file:Site/ts/x.ts", "file", "x.ts", "Site/ts/x.ts");
    g.upsert_nodes(PID, &[svc, caller]).unwrap();
    g.upsert_edges(
        PID,
        &[Edge {
            source_id: "file:Site/ts/x.ts".into(),
            target_id: "sym:function:svc.vb:svc.DoThing:1".into(),
            namespace: "memory".into(),
            language: "javascript".into(),
            edge_kind: EdgeKind::ApiCall,
            weight: 1,
            generation: 1,
            metadata: None, // a direct call — no `via`
            updated_at_ms: 0,
        }],
    )
    .unwrap();
    let mut id = 0usize;
    let (_items, members, _cov, _proof) =
        exhaustive_callee_set(g, None, PID, "Site/ts/x.ts", &[EdgeKind::ApiCall], &mut id);
    let m = members
        .iter()
        .find(|m| m.display_name == "svc.DoThing")
        .expect("callee present");
    assert!(
        m.via.is_none(),
        "a direct call must carry no via provenance"
    );
    assert_eq!(m.relation, "api_call");
}
