#![allow(clippy::unwrap_used)]
//! Round-6: the pre_commit_review tool description must not drift from the
//! registered gate set. The round-5 audit caught it advertising "eleven gates"
//! while nineteen were registered. This binds the advertised count to
//! all_gates() so the next gate added or removed fails a test instead of
//! silently misleading callers about what was checked.

use engram_core::config::Config;
use engram_server::services::pre_commit_review_service::gates::all_gates;
use engram_server::state::AppState;
use engram_server::tools::Engram;

fn engram() -> (tempfile::TempDir, Engram) {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("proj")).unwrap();
    let cfg = Config {
        allowed_roots: vec![tmp.path().join("proj")],
        data_dir: tmp.path().join("data"),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    (tmp, Engram::new(state))
}

#[test]
fn pre_commit_review_description_gate_count_matches_registry() {
    let (_t, engram) = engram();
    let tools = engram.tool_router.list_all();
    let desc = tools
        .iter()
        .find(|t| t.name == "pre_commit_review")
        .and_then(|t| t.description.as_deref())
        .expect("pre_commit_review tool must exist with a description")
        .to_string();
    let n = all_gates().len();
    let needle = format!("{n} graph-backed");
    assert!(
        desc.contains(&needle),
        "pre_commit_review description must advertise the real gate count \
         ({n} graph-backed gates); it has drifted. Update the description in \
         crates/engram_server/src/tools.rs. Current description:\n{desc}"
    );
}
