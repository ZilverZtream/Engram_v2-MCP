#![allow(clippy::unwrap_used)]
//! Nothing in the request path may open a redb database per call.
//!
//! Live incident 2026-08-20: three `grep_project` calls that arrived in the
//! same millisecond all failed with
//! `-32603 "Database already open. Cannot acquire lock."`.
//!
//! Root cause: the handler opened its own `DocStore` per request. redb takes
//! an EXCLUSIVE whole-file lock (`LockFile` on Windows, `flock` on unix) for
//! the lifetime of a `Database` handle — its MVCC guarantees are per-handle,
//! not per-file. So a *read-only* tool could not run concurrently with
//! itself.
//!
//! The store it was reading turned out to be one nothing ever writes, so the
//! resolution was to stop opening it at all: freshness now reads the code
//! graph's file-node fingerprints, and the full-scan tier reads Tantivy's
//! stored chunk text. These tests pin both halves — the redb constraint that
//! caused the failure, and the absence of any per-request open.

use engram_core::Config;
use engram_index::DocStore;
use engram_server::AppState;

fn make_cfg(data_dir: &std::path::Path) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        embedding_backend: "fts_only".into(),
        allowed_roots: vec![data_dir.to_path_buf()],
        ..Default::default()
    }
}

/// Characterisation of the redb behaviour behind the incident. If redb ever
/// starts allowing multiple handles, this test tells us the constraint is
/// gone — until then, no code path may open one of these per request.
#[test]
fn two_handles_on_one_redb_file_cannot_coexist() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("docs.redb");

    let first = DocStore::open(&path).expect("first open must succeed");

    let second = DocStore::open(&path);
    assert!(
        second.is_err(),
        "redb is expected to refuse a second handle on the same file — \
         a per-request open therefore cannot be concurrency-safe"
    );
    let msg = format!("{}", second.err().unwrap());
    assert!(
        msg.contains("already open"),
        "expected redb's DatabaseAlreadyOpen, got: {msg}"
    );

    drop(first);
}

/// The per-project `docs.redb` must not be created or locked as a side
/// effect of serving requests — that is what put a request-path file lock
/// there in the first place.
#[test]
fn serving_a_project_does_not_open_a_per_project_redb() {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let (state, _rx) = AppState::new(make_cfg(&data_dir)).unwrap();

    let pid = "11111111-2222-3333-4444-555555555555";
    let path = data_dir.join("projects").join(pid).join("docs.redb");
    assert!(!path.exists(), "precondition: nothing there yet");

    drop(state);

    assert!(
        !path.exists(),
        "a per-project redb appeared without anyone asking for it"
    );

    // An explicit open still works, and releases its lock on drop — so the
    // file stays deletable. Windows refuses to unlink a locked file, which
    // is why a cached handle would have to be evicted before delete_project.
    {
        let _store = DocStore::open(&path).expect("explicit open still works");
    }
    std::fs::remove_file(&path).expect("the file must be deletable once the handle is dropped");
}
