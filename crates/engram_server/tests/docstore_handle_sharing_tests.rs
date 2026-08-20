#![allow(clippy::unwrap_used)]
//! Concurrent readers must not fight over a project's DocStore file lock.
//!
//! Live incident 2026-08-20: three `grep_project` calls that arrived in the
//! same millisecond all failed with
//! `-32603 "Database already open. Cannot acquire lock."`.
//!
//! Root cause: the handler opened its own `DocStore` per request. redb takes
//! an EXCLUSIVE whole-file lock (`LockFile` on Windows, `flock` on unix) for
//! the lifetime of a `Database` handle — its MVCC guarantees are per-handle,
//! not per-file. So a *read-only* tool could not run concurrently with itself,
//! nor with any other component that had the same file open.
//!
//! The fix is one shared handle per project, cached on `AppState`.

use engram_core::Config;
use engram_index::DocStore;
use engram_server::state::AppState;

fn make_cfg(data_dir: &std::path::Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        embedding_backend: "fts_only".into(),
        allowed_roots: vec![data_dir.to_path_buf()],
        ..Default::default()
    }
}

/// Characterisation of the underlying redb behaviour that caused the
/// incident. This is the mechanism the production fix must avoid — if redb
/// ever starts allowing multiple handles, this test tells us the constraint
/// is gone.
#[test]
fn two_handles_on_one_docstore_file_cannot_coexist() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("docs.redb");

    let first = DocStore::open(&path).expect("first open must succeed");

    let second = DocStore::open(&path);
    assert!(
        second.is_err(),
        "redb is expected to refuse a second handle on the same file — \
         a per-request DocStore::open therefore cannot be concurrency-safe"
    );
    let msg = format!("{}", second.err().unwrap());
    assert!(
        msg.contains("already open"),
        "expected redb's DatabaseAlreadyOpen, got: {msg}"
    );

    drop(first);
}

/// The production accessor must serve every concurrent caller from ONE
/// handle. Twelve threads racing on a cold cache reproduce the incident's
/// shape (several requests landing in the same millisecond).
#[test]
fn concurrent_docstore_callers_all_succeed_and_share_one_handle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let pid = "11111111-2222-3333-4444-555555555555";

    type OpenResult = Result<std::sync::Arc<DocStore>, String>;
    let results: Vec<OpenResult> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..12)
            .map(|_| {
                let state = state.clone();
                s.spawn(move || state.docstore_blocking(pid).map_err(|e| e.to_string()))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let failures: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(
        failures.is_empty(),
        "every concurrent caller must get a handle; failures: {failures:?}"
    );

    let stores: Vec<_> = results.into_iter().map(|r| r.unwrap()).collect();
    for other in &stores[1..] {
        assert!(
            std::sync::Arc::ptr_eq(&stores[0], other),
            "all callers must share ONE handle, not open their own"
        );
    }
}

/// Deleting a project removes its data directory. On Windows an open redb
/// handle keeps the file locked, so the cache must be evictable — otherwise
/// caching the handle would trade a read failure for an undeletable project.
#[test]
fn closing_a_project_docstore_releases_the_file_lock() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let pid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let store = state.docstore_blocking(pid).expect("open");
    drop(store);

    state.close_docstore(pid);

    let path = data_dir.join("projects").join(pid).join("docs.redb");
    assert!(
        DocStore::open(&path).is_ok(),
        "after close_docstore the file lock must be released"
    );
}
