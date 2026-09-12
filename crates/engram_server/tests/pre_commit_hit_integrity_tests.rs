#![allow(clippy::unwrap_used)]
//! External audit round 2 (docs/audits/10, P1-1): a search hit whose backing
//! document returns Ok(None) was converted to empty content or skipped
//! (gates.rs). That is an INDEX-INTEGRITY failure — the gate must be
//! degraded and say so — never "empty evidence".

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use engram_core::config::Config;
use engram_index::hybrid::HybridQuery;
use engram_server::services::pre_commit_review_service::gates::hit_content;
use engram_server::services::pre_commit_review_service::{
    DiffFile, GateContext, parse_unified_diff,
};
use engram_server::services::project_service::{ensure_project_runtime, get_active_generation};
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;

async fn build() -> (tempfile::TempDir, AppState, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site/App_Code")).unwrap();
    std::fs::write(
        root.join("Site/App_Code/Auth.vb"),
        "Public Class Auth\n    Public Function Authenticate(user As String) As Boolean\n        Return True\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(50),
        max_project_bytes: Some(1 << 20),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "HitIntegrity".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, pid)
}

struct Fixture {
    project_dir: PathBuf,
    diff_files: Vec<DiffFile>,
    changed: HashSet<String>,
}

fn gate_ctx<'a>(state: &'a AppState, pid: &'a str, gen_: u64, f: &'a Fixture) -> GateContext<'a> {
    GateContext {
        state,
        graph: state.graph.clone(),
        registry: state.registry.clone(),
        project_id: pid,
        project_dir: f.project_dir.as_path(),
        generation: gen_,
        diff_files: &f.diff_files,
        changed_paths: &f.changed,
        total_commits: 0,
        repo_rules: Arc::new(Vec::new()),
        files_by_parent: Arc::new(HashMap::new()),
        audit_function: None,
        search_index_note: None,
        degraded: Mutex::new(Vec::new()),
        caps: Mutex::new(Vec::new()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hit_without_a_backing_document_degrades_the_gate_as_an_integrity_failure() {
    let (_tmp, state, pid) = build().await;
    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    let rec = state.registry.get_project(&pid).unwrap().unwrap();
    let fx = Fixture {
        project_dir: PathBuf::from(&rec.directory),
        diff_files: parse_unified_diff(""),
        changed: HashSet::new(),
    };
    let ctx = gate_ctx(&state, &pid, gen_, &fx);
    let got = hit_content(&ctx, &ps.search, "ghost-pk-that-no-document-backs");
    assert!(
        got.is_none(),
        "no content can be fabricated for a missing document: {got:?}"
    );
    let notes = ctx.degraded.lock().unwrap().clone();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("integrity") && n.contains("ghost-pk-that-no-document-backs")),
        "the gate is DEGRADED with an integrity note naming the hit: {notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hit_with_a_backing_document_is_read_without_degrading() {
    let (_tmp, state, pid) = build().await;
    let ps = ensure_project_runtime(&state, &pid).await.unwrap();
    let gen_ = get_active_generation(&state, &pid).await.unwrap();
    let rec = state.registry.get_project(&pid).unwrap().unwrap();
    let fx = Fixture {
        project_dir: PathBuf::from(&rec.directory),
        diff_files: parse_unified_diff(""),
        changed: HashSet::new(),
    };
    let ctx = gate_ctx(&state, &pid, gen_, &fx);
    let hits = ps
        .search
        .lexical_search(&HybridQuery {
            project_id: pid.clone(),
            namespace: engram_core::namespaces::NAMESPACE_MEMORY.into(),
            generation: gen_,
            text: "Authenticate".into(),
            top_k: 3,
            fts_mode: "loose".into(),
            include_path_prefixes: None,
            exclude_path_prefixes: None,
            include_path_suffixes: None,
            language_filters: None,
            author_filter: None,
            date_after: None,
            date_before: None,
            use_mmr: false,
        })
        .unwrap();
    assert!(!hits.is_empty(), "the fixture indexes Auth.vb");
    let got = hit_content(&ctx, &ps.search, &hits[0].pk);
    assert!(
        got.as_deref().is_some_and(|c| c.contains("Authenticate")),
        "a real hit reads its document: {got:?}"
    );
    assert!(
        ctx.degraded.lock().unwrap().is_empty(),
        "nothing degraded for a real hit"
    );
}
