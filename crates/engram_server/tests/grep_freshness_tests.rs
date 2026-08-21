#![allow(clippy::unwrap_used)]
//! `grep_project`'s freshness guard has to actually detect staleness.
//!
//! The parameter defaults to `"strict"` and its whole job is to stop an
//! agent reading grep output as current when the file on disk has moved on.
//! It worked by comparing indexed fingerprints from the DocStore against
//! disk — but nothing has ever written a fingerprint to the DocStore, so it
//! compared against an empty set and could never report anything. Every
//! `docs.redb` on disk is redb's empty size to the byte.
//!
//! The fingerprints do exist: ingest records (mtime, size, file_hash) into
//! the graph's file nodes, which is what the incremental change scan already
//! trusts to decide what to re-index. The guard now reads the same source.
//!
//! Note the indexer stores mtime in SECONDS, so these tests move mtime by
//! whole seconds — a sub-second edit is genuinely below the recorded
//! resolution and is caught by the size comparison instead.

use engram_core::Config;
use engram_server::AppState;
use rmcp::handler::server::tool::Parameters;
use std::path::Path;
use tempfile::tempdir;

fn grep_request(pid: &str, pattern: &str, freshness: &str) -> engram_server::GrepProjectRequest {
    engram_server::GrepProjectRequest {
        project_id: pid.to_string(),
        pattern: pattern.to_string(),
        regex: false,
        case_sensitive: None,
        multiline: false,
        path_prefix: None,
        language: None,
        context_before: 0,
        context_after: 0,
        max_results: 200,
        freshness: freshness.to_string(),
        namespace: "memory".into(),
        output_json: false,
    }
}

/// Push a file's mtime back so the recorded second-granularity stamp and the
/// on-disk one genuinely differ, regardless of how fast the test runs.
fn age_file(path: &Path, secs: u64) {
    let meta = std::fs::metadata(path).unwrap();
    let modified = meta.modified().unwrap();
    let older = modified - std::time::Duration::from_secs(secs);
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(older).unwrap();
}

async fn setup() -> (
    tempfile::TempDir,
    engram_server::Engram,
    String,
    std::path::PathBuf,
) {
    let tmp = tempdir().unwrap();
    // data_dir lives OUTSIDE the indexed tree. Nested inside it, engram
    // indexes its own Tantivy segment files, which keep being rewritten
    // after the index run and so read as permanently stale.
    let root = tmp.path().join("repo");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/orders.rs"),
        "pub fn submit_order(id: u64) -> bool {\n    tracker_marker(id)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/stable.rs"),
        "pub fn untouched_helper() -> u8 {\n    7\n}\n",
    )
    .unwrap();

    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: data_dir.clone(),
        max_project_files: Some(50),
        max_project_bytes: Some(1024 * 1024),
        embedding_backend: "fts_only".into(),
        max_concurrent_jobs: 2,
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = engram_server::Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "FreshTest".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, engram, pid, root)
}

async fn grep(engram: &engram_server::Engram, req: engram_server::GrepProjectRequest) -> String {
    let res = engram
        .grep_project(Parameters(req))
        .await
        .expect("grep_project must succeed");
    res.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A freshly indexed project must report no staleness — the guard must not
/// become a permanent false alarm.
#[tokio::test]
async fn freshly_indexed_project_reports_no_stale_files() {
    let (_tmp, engram, pid, _root) = setup().await;
    let out = grep(&engram, grep_request(&pid, "tracker_marker", "strict")).await;
    assert!(
        !out.contains("match the indexed fingerprint"),
        "a just-indexed project must be clean; got:\n{out}"
    );
}

/// The finding. Editing a tracked file behind the index must be reported.
#[tokio::test]
async fn editing_a_file_behind_the_index_is_reported_as_stale() {
    let (_tmp, engram, pid, root) = setup().await;
    let edited = root.join("src/orders.rs");

    std::fs::write(
        &edited,
        "pub fn submit_order(id: u64) -> bool {\n    tracker_marker(id) && audit(id)\n}\n\
         pub fn audit(_id: u64) -> bool { true }\n",
    )
    .unwrap();
    age_file(&edited, 5);

    let out = grep(&engram, grep_request(&pid, "tracker_marker", "strict")).await;

    assert!(
        out.contains("match the indexed fingerprint"),
        "an edit behind the index must be surfaced, or the guard is decorative; got:\n{out}"
    );
    // Check the WARNING names it, not merely that the string appears
    // somewhere — orders.rs is also a match line here, which would pass
    // vacuously.
    assert!(
        out.contains("> - `src/orders.rs`"),
        "the warning must name the file that drifted; got:\n{out}"
    );
    assert!(
        !out.contains("> - `src/stable.rs`"),
        "an untouched file must not be reported as stale; got:\n{out}"
    );
}

/// A deleted file is stale too — its indexed content can no longer be
/// verified against anything.
#[tokio::test]
async fn deleting_a_tracked_file_is_reported_as_stale() {
    let (_tmp, engram, pid, root) = setup().await;
    std::fs::remove_file(root.join("src/orders.rs")).unwrap();

    let out = grep(&engram, grep_request(&pid, "untouched_helper", "strict")).await;
    assert!(
        out.contains("match the indexed fingerprint") && out.contains("orders.rs"),
        "a deleted tracked file must be surfaced; got:\n{out}"
    );
}

/// `freshness=off` must skip the check — it exists for callers who know the
/// index is being rebuilt and do not want the stat sweep.
#[tokio::test]
async fn freshness_off_skips_the_check() {
    let (_tmp, engram, pid, root) = setup().await;
    let edited = root.join("src/orders.rs");
    std::fs::write(&edited, "pub fn submit_order() {}\n").unwrap();
    age_file(&edited, 5);

    let out = grep(&engram, grep_request(&pid, "submit_order", "off")).await;
    assert!(
        !out.contains("match the indexed fingerprint"),
        "freshness=off must not run the check; got:\n{out}"
    );
}

/// A check that has nothing to compare against must SAY so. An empty
/// fingerprint set produces an empty stale list, which is indistinguishable
/// from a clean bill of health — that silence is exactly what let this guard
/// pass as working while it validated nothing.
#[tokio::test]
async fn a_check_with_no_fingerprints_says_it_could_not_verify() {
    let (_tmp, engram, pid, _root) = setup().await;

    // Drop the graph, keeping the search index: the fingerprints are gone
    // but the corpus is still searchable.
    engram
        .state
        .graph
        .delete_project_data(&pid)
        .expect("purge graph");

    let out = grep(&engram, grep_request(&pid, "tracker_marker", "strict")).await;
    assert!(
        out.contains("could NOT be verified"),
        "with no fingerprints the guard must admit it proved nothing; got:\n{out}"
    );
    assert!(
        !out.contains("match the indexed fingerprint"),
        "it must not also claim files drifted; got:\n{out}"
    );
}

/// The full-scan tier must actually scan. It is reached by patterns the
/// trigram index cannot serve, and it used to read the same unwritten store
/// as the freshness guard — so it scanned zero chunks and reported zero
/// matches for text plainly present in the corpus.
#[tokio::test]
async fn full_scan_tier_finds_matches_the_term_index_cannot_serve() {
    let (_tmp, engram, pid, _root) = setup().await;

    // A two-character literal is below the trigram minimum, so this cannot
    // be served by the term index and falls through to the full scan.
    let out = grep(&engram, grep_request(&pid, "u8", "strict")).await;

    assert!(
        out.contains("full_scan"),
        "precondition: a 2-char literal must reach the full-scan tier; got:\n{out}"
    );
    assert!(
        out.contains("stable.rs"),
        "the full scan must find `u8` in stable.rs; got:\n{out}"
    );
    assert!(
        !out.contains("UNCONFIRMED coverage"),
        "a scan that covered the corpus must not warn about zero coverage; got:\n{out}"
    );
}

/// The J5 fix: grep_project must find a string in a file added since the last
/// index — by scanning the working tree, not just the index — instead of
/// returning "no matches" and letting the agent conclude the code is absent.
#[tokio::test]
async fn grep_finds_a_new_unindexed_file_via_disk_fallback() {
    let (_tmp, engram, pid, root) = setup().await;
    // A brand-new file the index has never seen.
    std::fs::write(
        root.join("src/newly_added.rs"),
        "pub fn frobnicate_widget() -> u8 {\n    42\n}\n",
    )
    .unwrap();
    let out = grep(&engram, grep_request(&pid, "frobnicate_widget", "warn")).await;
    assert!(
        out.contains("newly_added.rs"),
        "disk fallback must find the string in the new file:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("not in the index"),
        "output must note the match came from the working tree:\n{out}"
    );
}
