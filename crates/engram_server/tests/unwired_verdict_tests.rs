#![allow(clippy::unwrap_used)]
//! Row-3 audit (docs/audits/05-pre-commit-gates.md) A5: the `unwired`
//! gate reported a NEW definition as "unwired" whenever its graph lookup
//! returned nothing — including when the lookup FAILED. Since slice 2 a
//! failure degrades the gate, but the candidate was still judged on an
//! empty result. A failed lookup is no evidence either way: the candidate
//! must be skipped (and the gate degraded), not reported.

use engram_core::RelPath;
use engram_graph::Node;
use engram_server::services::pre_commit_review_service::gates::{UnwiredVerdict, unwired_verdict};

fn node(name: &str) -> Node {
    Node {
        node_id: format!("sym:function:Site/x.vb:api.{name}:1"),
        node_type: "function".into(),
        name: name.into(),
        namespace: "api".into(),
        language: "vbnet".into(),
        file_path: RelPath::new("Site/x.vb"),
        start_line: 1,
        end_line: 3,
        generation: 1,
        metadata: None,
    }
}

#[test]
fn a_failed_lookup_is_skipped_not_reported_as_unwired() {
    let failed: anyhow::Result<Vec<Node>> = Err(anyhow::anyhow!("redb: database locked"));
    assert_eq!(
        unwired_verdict("Helper", failed, &[]),
        UnwiredVerdict::Unknown,
        "no evidence either way"
    );
}

#[test]
fn an_empty_lookup_is_unwired_and_a_hit_with_a_caller_is_wired() {
    let empty: anyhow::Result<Vec<Node>> = Ok(Vec::new());
    assert_eq!(
        unwired_verdict("Helper", empty, &[]),
        UnwiredVerdict::Unwired
    );
    // A same-named node that HAS a caller wires the candidate …
    let n = node("Helper");
    let id = n.node_id.clone();
    let hit: anyhow::Result<Vec<Node>> = Ok(vec![n]);
    assert_eq!(
        unwired_verdict("Helper", hit, &[(id.clone(), 1)]),
        UnwiredVerdict::Wired
    );
    // … a same-named node WITHOUT callers does not.
    let hit: anyhow::Result<Vec<Node>> = Ok(vec![node("Helper")]);
    assert_eq!(
        unwired_verdict("Helper", hit, &[(id, 0)]),
        UnwiredVerdict::Unwired
    );
}
