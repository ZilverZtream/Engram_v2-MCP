#![allow(clippy::unwrap_used)]
//! A parameter the schema advertises must either work or say it doesn't.
//!
//! A sweep of all 131 request structs for fields no handler ever reads found
//! eight. One (`ingest_quality_gates.clear_existing`) is documented as
//! "Reserved: … currently a no-op", which is the honest form. The other
//! seven were silent.
//!
//! Three of them are FILTERS, which is the dangerous kind: a caller that
//! passes `metadata_filter` to `search_memory` gets results that do not
//! honour it, and nothing says so. Unfiltered results presented as filtered
//! are wrong data, not merely missing behaviour — so those fail closed, the
//! same way an unknown `namespace` or an invalid `freshness` already does.
//!
//! The rest only fail to opt into something (a cache bypass with no cache
//! behind it, richer evidence), so they are documented rather than rejected.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use tempfile::tempdir;

async fn setup() -> (tempfile::TempDir, engram_server::Engram, String) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn alpha() -> u8 { 1 }\n").unwrap();

    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(256 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "ParamTest".into(),
            project_type: engram_server::models::ProjectType::General,
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

fn search_request(pid: &str) -> engram_server::SearchMemoryRequest {
    engram_server::SearchMemoryRequest {
        project_id: pid.to_string(),
        query: "alpha".into(),
        ..Default::default()
    }
}

/// search_memory is the primary retrieval tool. A filter it silently
/// ignores hands the caller results that violate the constraint they asked
/// for — the worst shape of this defect.
#[tokio::test]
async fn search_memory_rejects_the_unimplemented_metadata_filter() {
    let (_tmp, engram, pid) = setup().await;

    let mut req = search_request(&pid);
    req.metadata_filter = Some(serde_json::json!({"author": "someone"}));

    let err = engram
        .search_memory(Parameters(req))
        .await
        .expect_err("an unimplemented filter must not be silently ignored");

    let msg = format!("{err}");
    assert!(
        msg.contains("metadata_filter"),
        "the error must name the parameter; got: {msg}"
    );
    assert!(
        msg.contains("include_path_prefixes")
            || msg.contains("language_filters")
            || msg.contains("exclude_path_prefixes"),
        "the error must point at the filters that DO work; got: {msg}"
    );
}

/// Omitting it must keep working — the guard must not break every caller.
#[tokio::test]
async fn search_memory_without_the_filter_still_works() {
    let (_tmp, engram, pid) = setup().await;
    engram
        .search_memory(Parameters(search_request(&pid)))
        .await
        .expect("an ordinary search must be unaffected");
}
