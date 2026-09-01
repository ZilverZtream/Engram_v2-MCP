#![allow(clippy::unwrap_used)]
//! Doc-11 P1e (round-2 audit residue): `project_health` prints
//! `tantivy_docs_total` as the sum of a HARDCODED namespace subset
//! (hybrid.rs count_docs_by_namespace) — live it printed 421,293 while the
//! store held 422,249. A doc in any namespace outside the subset
//! (business_logic, code, memory_bank, …) vanishes from the "total". The
//! label must print the project-wide count.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_index::hybrid::IndexDoc;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_total_counts_every_namespace_not_a_hardcoded_subset() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("proj");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join("a.vb"), "Public Class A\nEnd Class\n").unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: project_dir.to_string_lossy().to_string(),
            project_name: "totals".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();

    // One doc OUTSIDE the hardcoded subset.
    let ps = engram_server::services::project_service::ensure_project_runtime(&state, &pid)
        .await
        .unwrap();
    {
        let fields = ps.search.fields();
        let mut guard = ps.search.create_bulk_writer().unwrap();
        engram_index::hybrid::HybridSearchEngine::write_docs_to_writer(
            &fields,
            &mut guard,
            &pid,
            &[IndexDoc {
                generation: 1,
                chunk_id: 0,
                path: RelPath::new("rules/r1.md"),
                language: "markdown".into(),
                content: "if the order ships partially then invoice the shipped part".into(),
                namespace: "business_logic".into(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 1,
                doc_id: "bl:r1".into(),
                content_hash: "h-bl-r1".into(),
            }],
        )
        .unwrap();
        guard.finish().unwrap();
    }
    let expected = ps.search.count_docs(&pid).unwrap();

    let res = engram
        .project_health(Parameters(engram_server::ProjectIdRequest {
            project_id: pid.clone(),
        }))
        .await
        .unwrap();
    let text = match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => &t.text,
        _ => panic!("expected text"),
    };
    let printed: usize = text
        .split("tantivy_docs_total: ")
        .nth(1)
        .expect("label present")
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        printed, expected,
        "the printed total must be the project-wide count (business_logic \
         and every other namespace included), not a hardcoded subset:\n{text}"
    );
}
