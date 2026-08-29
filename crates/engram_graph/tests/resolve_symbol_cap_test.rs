#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 6 — the last golden miss (`ox_impact_4`,
//! "GetByID in the projekt DAL") on release 28: `resolve_symbol`'s short-name
//! step scanned only the first 50 SUBSTRING matches of the name, so on a big
//! project the `_grunddata.projekt.GetByID` candidate never entered the
//! ambiguity list (4 `_ata.*` candidates were reported) and the question's
//! qualifier had nothing to narrow. The short-name step must see EVERY
//! exact-short-name candidate.

use engram_core::RelPath;
use engram_graph::{GraphStore, Node, ResolveResult};

const PID: &str = "resolve-cap-test";

fn func(path: &str, fqn: &str) -> Node {
    Node {
        node_id: format!("sym:function:{path}:{fqn}:1"),
        node_type: "function".into(),
        name: fqn.into(),
        namespace: "memory".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 10,
        generation: 1,
        metadata: None,
    }
}

#[test]
fn every_exact_short_name_candidate_is_in_the_ambiguity_list_on_a_big_project() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = GraphStore::open(&tmp.path().join("graph.redb")).unwrap();
    let mut nodes = Vec::new();
    // 60 DAL classes with a GetByID, plus 20 substring-only near-misses that
    // sit in front of the real candidates in scan order.
    for i in 0..20 {
        nodes.push(func(
            &format!("Site/App_Code/a/near{i:02}.vb"),
            &format!("_a.near{i:02}.GetByIDs"),
        ));
    }
    for i in 0..60 {
        nodes.push(func(
            &format!("Site/App_Code/ata/code/cls{i:02}.vb"),
            &format!("_ata.cls{i:02}.GetByID"),
        ));
    }
    nodes.push(func(
        "Site/App_Code/grunddata/code/projekt.vb",
        "_grunddata.projekt.GetByID",
    ));
    store.upsert_nodes(PID, &nodes).unwrap();

    match store.resolve_symbol(PID, "GetByID", None, None).unwrap() {
        ResolveResult::Ambiguous(v) => {
            assert!(
                v.iter().any(|n| n.name == "_grunddata.projekt.GetByID"),
                "the projekt candidate is in the list ({} candidates): {:?}",
                v.len(),
                v.iter().map(|n| n.name.as_str()).collect::<Vec<_>>()
            );
            assert_eq!(
                v.len(),
                61,
                "every exact short-name match, none of the substring near-misses"
            );
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}
