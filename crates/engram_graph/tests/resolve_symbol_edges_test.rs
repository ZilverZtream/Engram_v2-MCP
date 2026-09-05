#![allow(clippy::unwrap_used)]
//! Tests for the HashMap-based resolve_symbol_edges rewrite.
//!
//! Verifies exact-name match, terminal-segment fallback, metadata.fqn match,
//! file_path tiebreaker for ambiguous names, and ADJ_IN/ADJ_OUT consistency.

use engram_core::RelPath;
use engram_graph::{Edge, EdgeKind, GraphStore, Node};

fn open_store(tmp: &tempfile::TempDir) -> GraphStore {
    GraphStore::open(&tmp.path().join("graph.redb")).expect("GraphStore::open")
}

fn make_node(node_id: &str, name: &str, file_path: &str) -> Node {
    Node {
        node_id: node_id.to_string(),
        node_type: "function".to_string(),
        name: name.to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        file_path: RelPath::new(file_path),
        start_line: 1,
        end_line: 10,
        generation: 1,
        metadata: None,
    }
}

fn make_node_with_fqn(node_id: &str, name: &str, file_path: &str, fqn: &str) -> Node {
    let mut m = serde_json::Map::new();
    m.insert("fqn".into(), serde_json::Value::String(fqn.to_string()));
    Node {
        node_id: node_id.to_string(),
        node_type: "function".to_string(),
        name: name.to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        file_path: RelPath::new(file_path),
        start_line: 1,
        end_line: 10,
        generation: 1,
        metadata: Some(serde_json::Value::Object(m)),
    }
}

fn make_call(source_id: &str, target_id: &str) -> Edge {
    Edge {
        source_id: source_id.to_string(),
        target_id: target_id.to_string(),
        namespace: "memory".to_string(),
        language: "vbnet".to_string(),
        edge_kind: EdgeKind::Calls,
        weight: 1,
        generation: 1,
        metadata: None,
        updated_at_ms: 1_000_000,
    }
}

fn make_api_call(source_id: &str, target_id: &str, method: &str) -> Edge {
    let mut m = serde_json::Map::new();
    m.insert(
        "ajax_target_method".into(),
        serde_json::Value::String(method.to_string()),
    );
    Edge {
        source_id: source_id.to_string(),
        target_id: target_id.to_string(),
        namespace: "memory".to_string(),
        language: "javascript".to_string(),
        edge_kind: EdgeKind::ApiCall,
        weight: 1,
        generation: 1,
        metadata: Some(serde_json::Value::Object(m)),
        updated_at_ms: 1_000_000,
    }
}

#[test]
fn api_routed_call_resolves_to_the_api_layer_handler() {
    // A client `api.ajax('iopX')` (ApiCall carrying ajax_target_method) names an
    // overloaded server function: an api-json handler `api.iopX` beside a code
    // implementation `_io.Foo.iopX`. Both are real, so the bare name is
    // ambiguous; and because real function nodes are class-QUALIFIED, the
    // candidates surface through the TERMINAL index (`iopX`), never the
    // bare-name index. The generic file/receiver tiebreak can't choose (the .ts
    // source is in neither file), which previously left a dangling `::`
    // placeholder (the ox_causal_20 finding). Api-routed calls bind to the
    // `api.`-class handler.
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-api-layer";

    let handler = make_node(
        "sym:function:Site/x/api-json/api-x.vb:api.iopX:1",
        "api.iopX",
        "Site/x/api-json/api-x.vb",
    );
    let impl_node = make_node(
        "sym:function:Site/x/code/x.vb:_io.Foo.iopX:1",
        "_io.Foo.iopX",
        "Site/x/code/x.vb",
    );
    let caller = make_node(
        "file:Site/modules/ts/panel.ts",
        "panel.ts",
        "Site/modules/ts/panel.ts",
    );
    graph
        .upsert_nodes(pid, &[handler.clone(), impl_node.clone(), caller.clone()])
        .unwrap();
    graph
        .upsert_edges(pid, &[make_api_call(&caller.node_id, "::iopX", "iopX")])
        .unwrap();

    graph.resolve_symbol_edges(pid).unwrap();

    let out = graph
        .neighbors(pid, EdgeKind::ApiCall, &caller.node_id, 10)
        .unwrap();
    let targets: Vec<String> = out.into_iter().map(|(t, _)| t).collect();
    assert!(
        targets.contains(&handler.node_id),
        "api-routed call must resolve to the api-layer handler; got {targets:?}"
    );
    assert!(
        !targets.iter().any(|t| t.starts_with("::")),
        "no dangling placeholder must remain; got {targets:?}"
    );
    assert!(
        !targets.contains(&impl_node.node_id),
        "must not bind to the code-layer impl; got {targets:?}"
    );
}

#[test]
fn resolves_exact_name_match() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-exact";

    let target = make_node("sym:function:a.vb:Foo:1", "Foo", "a.vb");
    let source = make_node("sym:function:b.vb:Bar:1", "Bar", "b.vb");
    graph
        .upsert_nodes(pid, &[target.clone(), source.clone()])
        .unwrap();
    graph
        .upsert_edges(pid, &[make_call(&source.node_id, "::Foo")])
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 1);

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let rewritten: Vec<_> = calls
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(rewritten.len(), 1);
    assert_eq!(rewritten[0].target_id, target.node_id);
}

#[test]
fn resolves_metadata_fqn_fallback() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-fqn";

    // Node's name doesn't match, but metadata.fqn does.
    let target = make_node_with_fqn(
        "sym:function:a.vb:DoWork:1",
        "DoWork",
        "a.vb",
        "MyModule.SpecialWork",
    );
    let source = make_node("sym:function:b.vb:Caller:1", "Caller", "b.vb");
    graph
        .upsert_nodes(pid, &[target.clone(), source.clone()])
        .unwrap();
    graph
        .upsert_edges(pid, &[make_call(&source.node_id, "::MyModule.SpecialWork")])
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 1);

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let rewritten: Vec<_> = calls
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(rewritten[0].target_id, target.node_id);
}

#[test]
fn resolves_terminal_segment_fallback() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-terminal";

    // Node name is "MyModule.Helper" — terminal segment is "Helper".
    let target = make_node(
        "sym:function:a.vb:MyModule.Helper:1",
        "MyModule.Helper",
        "a.vb",
    );
    let source = make_node("sym:function:b.vb:Caller:1", "Caller", "b.vb");
    graph
        .upsert_nodes(pid, &[target.clone(), source.clone()])
        .unwrap();
    graph
        .upsert_edges(pid, &[make_call(&source.node_id, "::Helper")])
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 1);

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let rewritten: Vec<_> = calls
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(rewritten[0].target_id, target.node_id);
}

#[test]
fn disambiguates_by_file_path() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-disambig";

    // Two nodes with same name, different files.
    let node_a = make_node("sym:function:a.vb:Create:10", "Create", "a.vb");
    let node_b = make_node("sym:function:b.vb:Create:20", "Create", "b.vb");
    // Source is in same file as node_a.
    let source = make_node("sym:function:a.vb:DoStuff:50", "DoStuff", "a.vb");
    graph
        .upsert_nodes(pid, &[node_a.clone(), node_b.clone(), source.clone()])
        .unwrap();
    graph
        .upsert_edges(pid, &[make_call(&source.node_id, "::Create")])
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 1);

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let rewritten: Vec<_> = calls
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(
        rewritten[0].target_id, node_a.node_id,
        "Expected tiebreaker to pick node_a (same file as source)"
    );
}

#[test]
fn skips_genuinely_unresolvable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-unresolvable";

    let source = make_node("sym:function:a.vb:Caller:1", "Caller", "a.vb");
    graph.upsert_nodes(pid, &[source.clone()]).unwrap();
    // Target "::NonExistent" matches nothing.
    graph
        .upsert_edges(pid, &[make_call(&source.node_id, "::NonExistent")])
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 0);

    // Original edge should still exist unchanged.
    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].target_id, "::NonExistent");
}

#[test]
fn adj_in_updated_after_resolve() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-adj";

    let target = make_node("sym:function:a.vb:SafeRedirect:1", "SafeRedirect", "a.vb");
    let source = make_node("sym:function:b.vb:Page_Load:1", "Page_Load", "b.vb");
    graph
        .upsert_nodes(pid, &[target.clone(), source.clone()])
        .unwrap();
    graph
        .upsert_edges(pid, &[make_call(&source.node_id, "::SafeRedirect")])
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 1);

    // ADJ_IN should now have an entry for the target node pointing back to source.
    let incoming = graph
        .find_incoming_edges_with_kind(pid, Some(EdgeKind::Calls), &target.node_id, 100)
        .unwrap();
    assert!(
        !incoming.is_empty(),
        "ADJ_IN should have the resolved incoming edge"
    );
    assert_eq!(incoming[0].0, source.node_id);
}

#[test]
fn mixed_resolution_strategies() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-mixed";

    // Node resolved by exact name
    let n1 = make_node("n1", "ExactMatch", "a.vb");
    // Node resolved by metadata.fqn
    let n2 = make_node_with_fqn("n2", "InternalName", "b.vb", "Special.FqnTarget");
    // Node resolved by terminal segment
    let n3 = make_node("n3", "Namespace.TerminalHit", "c.vb");
    // Source
    let src = make_node("src", "Caller", "d.vb");

    graph
        .upsert_nodes(pid, &[n1.clone(), n2.clone(), n3.clone(), src.clone()])
        .unwrap();
    graph
        .upsert_edges(
            pid,
            &[
                make_call("src", "::ExactMatch"),
                make_call("src", "::Special.FqnTarget"),
                make_call("src", "::TerminalHit"),
                make_call("src", "::NoSuchThing"),
            ],
        )
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 3, "3 of 4 edges should resolve");

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let targets: Vec<&str> = calls.iter().map(|e| e.target_id.as_str()).collect();
    assert!(targets.contains(&"n1"), "ExactMatch should resolve to n1");
    assert!(targets.contains(&"n2"), "FqnTarget should resolve to n2");
    assert!(targets.contains(&"n3"), "TerminalHit should resolve to n3");
    assert!(
        targets.contains(&"::NoSuchThing"),
        "Unresolvable should stay as ::NoSuchThing"
    );
}

#[test]
fn resolves_via_edge_metadata_fqn_when_name_is_ambiguous_across_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-edge-fqn";

    // Two handlers share the short name "btnSave_Click" in DIFFERENT files;
    // the edge source (a control) lives in a third file (the .aspx page), so
    // the same-file tiebreak cannot disambiguate.
    let a = make_node_with_fqn(
        "sym:function:admin/a.aspx.vb:btnSave_Click:10",
        "btnSave_Click",
        "admin/a.aspx.vb",
        "_admin.PageA.btnSave_Click",
    );
    let b = make_node_with_fqn(
        "sym:function:public/b.aspx.vb:btnSave_Click:20",
        "btnSave_Click",
        "public/b.aspx.vb",
        "_public.PageB.btnSave_Click",
    );
    let source = make_node("control:admin/a.aspx:btnSave", "btnSave", "admin/a.aspx");
    graph
        .upsert_nodes(pid, &[a.clone(), b.clone(), source.clone()])
        .unwrap();

    // event_wiring-style edge: placeholder is the short name, the edge's own
    // metadata carries the precise handler FQN.
    let mut edge = make_call(&source.node_id, "::btnSave_Click");
    edge.edge_kind = EdgeKind::Insight; // any kind; resolution is kind-agnostic
    let mut m = serde_json::Map::new();
    m.insert(
        "fqn".into(),
        serde_json::Value::String("_admin.PageA.btnSave_Click".into()),
    );
    edge.metadata = Some(serde_json::Value::Object(m));
    graph.upsert_edges(pid, &[edge]).unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 1, "edge metadata fqn should disambiguate");

    let edges = graph.list_edges(pid, Some(EdgeKind::Insight)).unwrap();
    let rewritten: Vec<_> = edges
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(rewritten.len(), 1);
    assert_eq!(
        rewritten[0].target_id, a.node_id,
        "must pick the admin handler named by the edge fqn, not the public one"
    );
    // TODO-12: the rewrite must record how it was resolved.
    let meta = rewritten[0].metadata.as_ref().expect("stamped metadata");
    assert_eq!(
        meta.get("resolution").and_then(|v| v.as_str()),
        Some("post_edge_fqn")
    );
    assert_eq!(
        meta.get("confidence").and_then(|v| v.as_str()),
        Some("0.90")
    );
}

#[test]
fn signature_shaped_target_resolves_after_param_strip() {
    // Roslyn sidecar builds emitted call targets WITH the parameter list
    // ("_x.Cls.Save(Integer, String)") while node names are bare
    // ("_x.Cls.Save") — the resolver must strip the signature before
    // deriving name/terminal keys or the edge stays a :: placeholder.
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-signature-strip";

    let target = make_node("sym:function:x.vb:_x.Cls.Save:5", "_x.Cls.Save", "x.vb");
    let source = make_node("sym:function:y.vb:Caller:1", "Caller", "y.vb");
    graph
        .upsert_nodes(pid, &[target.clone(), source.clone()])
        .unwrap();
    graph
        .upsert_edges(
            pid,
            &[make_call(&source.node_id, "::_x.Cls.Save(Integer, String)")],
        )
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(resolved, 1, "signature-shaped target must resolve");

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let rewritten: Vec<_> = calls
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(rewritten[0].target_id, target.node_id);
    let meta = rewritten[0].metadata.as_ref().expect("stamped metadata");
    assert_eq!(
        meta.get("resolution").and_then(|v| v.as_str()),
        Some("post_exact_name")
    );
}

#[test]
fn signature_shaped_target_resolves_via_suffix_step() {
    // Same signature-shaped target, but the node carries an extra
    // namespace-alias prefix — after the param strip the .-anchored
    // suffix-qualified step (not the bare terminal fallback) must bind it.
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-signature-suffix";

    let target = make_node(
        "sym:function:x.vb:App._x.Cls.Save:5",
        "App._x.Cls.Save",
        "x.vb",
    );
    let source = make_node("sym:function:y.vb:Caller:1", "Caller", "y.vb");
    graph
        .upsert_nodes(pid, &[target.clone(), source.clone()])
        .unwrap();
    graph
        .upsert_edges(
            pid,
            &[make_call(&source.node_id, "::_x.Cls.Save(Integer, String)")],
        )
        .unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(
        resolved, 1,
        "signature-shaped target must resolve via suffix"
    );

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let rewritten: Vec<_> = calls
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(rewritten[0].target_id, target.node_id);
    let meta = rewritten[0].metadata.as_ref().expect("stamped metadata");
    assert_eq!(
        meta.get("resolution").and_then(|v| v.as_str()),
        Some("post_suffix_qualified")
    );
}

#[test]
fn edge_metadata_fqn_misses_fall_through_to_terminal_segment() {
    let tmp = tempfile::TempDir::new().unwrap();
    let graph = open_store(&tmp);
    let pid = "test-edge-fqn-miss";

    // Single node whose dotted name ends in the placeholder's short name.
    let t = make_node(
        "sym:function:x.vb:Ns.Helper.DoWork:5",
        "Ns.Helper.DoWork",
        "x.vb",
    );
    let source = make_node("sym:function:y.vb:Caller:1", "Caller", "y.vb");
    graph
        .upsert_nodes(pid, &[t.clone(), source.clone()])
        .unwrap();

    // Edge fqn points at something that does not exist — must not block
    // the terminal-segment fallback.
    let mut edge = make_call(&source.node_id, "::DoWork");
    let mut m = serde_json::Map::new();
    m.insert(
        "fqn".into(),
        serde_json::Value::String("Ghost.Class.DoWork".into()),
    );
    edge.metadata = Some(serde_json::Value::Object(m));
    graph.upsert_edges(pid, &[edge]).unwrap();

    let resolved = graph.resolve_symbol_edges(pid).unwrap();
    assert_eq!(
        resolved, 1,
        "terminal fallback still applies after fqn miss"
    );

    let calls = graph.list_edges(pid, Some(EdgeKind::Calls)).unwrap();
    let rewritten: Vec<_> = calls
        .iter()
        .filter(|e| e.source_id == source.node_id)
        .collect();
    assert_eq!(rewritten[0].target_id, t.node_id);
}
