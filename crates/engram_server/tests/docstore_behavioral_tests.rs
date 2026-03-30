#![allow(clippy::unwrap_used)]
//! Behavioral tests for the production DocStore (Subsystem 5 — persistence).
//!
//! Covers all previously untested DocStore methods:
//!  - `open` / `count_docs_for_project` / `list_doc_summaries_for_project`
//!  - `set_docs_for_file` / `get_docs_for_file` / `get_all_docs_for_file`
//!  - `set_fingerprints` (batch)
//!  - `list_tracked_paths`
//!  - `all_doc_ids_for_project`
//!  - `delete_namespace`

use engram_index::{DocRecord, DocStore, FileFingerprint};
use redb::{Database, TableDefinition};

// ── helpers ───────────────────────────────────────────────────────────────────

fn open_store(tmp: &tempfile::TempDir) -> DocStore {
    DocStore::open(&tmp.path().join("docs.redb")).expect("DocStore::open must succeed")
}

fn make_doc(doc_id: &str, path: &str, namespace: &str) -> DocRecord {
    DocRecord {
        doc_id: doc_id.to_string(),
        path: path.to_string(),
        start_line: 1,
        end_line: 10,
        language: "rust".to_string(),
        content: format!("// content for {doc_id}"),
        content_hash: format!("hash_{doc_id}"),
        namespace: namespace.to_string(),
        generation: 1,
    }
}

fn make_fingerprint(rel_path: &str) -> FileFingerprint {
    FileFingerprint {
        rel_path: rel_path.to_string(),
        size: 1024,
        mtime_ms: 1_000_000,
        file_hash: "abc123".to_string(),
    }
}

// ── open ──────────────────────────────────────────────────────────────────────

/// DocStore::open must succeed on a fresh tempdir path.
#[test]
fn docstore_open_on_fresh_path_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let result = DocStore::open(&tmp.path().join("docs.redb"));
    assert!(
        result.is_ok(),
        "DocStore::open must succeed on a fresh path; got: {:?}",
        result.err()
    );
}

// ── count_docs_for_project ────────────────────────────────────────────────────

/// count_docs_for_project must return the exact count of inserted docs.
#[test]
fn docstore_count_docs_for_project_matches_inserted_count() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    store
        .put_docs(
            "proj-count",
            &[
                make_doc("d1", "src/a.rs", "rust"),
                make_doc("d2", "src/b.rs", "rust"),
                make_doc("d3", "src/c.rs", "rust"),
            ],
        )
        .expect("put_docs must succeed");

    let count = store.count_docs_for_project("proj-count").expect("count_docs_for_project");
    assert_eq!(count, 3, "must count 3 docs; got {count}");
}

/// count_docs_for_project must return 0 for an empty project.
#[test]
fn docstore_count_docs_for_project_zero_for_empty_project() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let count = store.count_docs_for_project("proj-empty").expect("count");
    assert_eq!(count, 0, "empty project must have 0 docs");
}

/// count_docs_for_project must not count docs from other projects.
#[test]
fn docstore_count_docs_for_project_project_isolation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    store
        .put_docs("proj-A", &[make_doc("d1", "a.rs", "rs")])
        .expect("put proj-A");
    store
        .put_docs(
            "proj-B",
            &[
                make_doc("d2", "b.rs", "rs"),
                make_doc("d3", "c.rs", "rs"),
            ],
        )
        .expect("put proj-B");

    let count_a = store.count_docs_for_project("proj-A").expect("count A");
    let count_b = store.count_docs_for_project("proj-B").expect("count B");

    assert_eq!(count_a, 1, "proj-A must have 1 doc; got {count_a}");
    assert_eq!(count_b, 2, "proj-B must have 2 docs; got {count_b}");
}

// ── list_doc_summaries_for_project ────────────────────────────────────────────

/// list_doc_summaries_for_project must return one summary per stored doc,
/// with correct namespace, doc_id, and path fields.
#[test]
fn docstore_list_doc_summaries_returns_correct_fields() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    store
        .put_doc("proj-sum", &make_doc("doc-001", "src/main.rs", "functions"))
        .expect("put doc");

    let summaries = store
        .list_doc_summaries_for_project("proj-sum")
        .expect("list_doc_summaries_for_project must not error");

    assert_eq!(summaries.len(), 1, "must return 1 summary for 1 doc");
    let s = &summaries[0];
    assert_eq!(s.doc_id, "doc-001", "summary doc_id must match");
    assert_eq!(s.namespace, "functions", "summary namespace must match");
    assert_eq!(s.path, "src/main.rs", "summary path must match");
}

/// list_doc_summaries_for_project must return empty for an empty project.
#[test]
fn docstore_list_doc_summaries_empty_project_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let summaries = store
        .list_doc_summaries_for_project("proj-none")
        .expect("must not error");
    assert!(summaries.is_empty(), "empty project must yield no summaries");
}

// ── set_docs_for_file / get_docs_for_file ────────────────────────────────────

/// set_docs_for_file followed by get_docs_for_file must return the same doc_ids.
#[test]
fn docstore_set_and_get_docs_for_file_round_trips() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let doc_ids = vec!["chunk-001".to_string(), "chunk-002".to_string(), "chunk-003".to_string()];
    store
        .set_docs_for_file("proj-file", "rust", "src/parser.rs", &doc_ids)
        .expect("set_docs_for_file must succeed");

    let retrieved = store
        .get_docs_for_file("proj-file", "rust", "src/parser.rs")
        .expect("get_docs_for_file must not error");

    assert_eq!(retrieved, doc_ids, "retrieved doc_ids must match stored list");
}

/// get_docs_for_file for an unknown file must return empty vec, not Err.
#[test]
fn docstore_get_docs_for_file_unknown_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let result = store
        .get_docs_for_file("proj-x", "rust", "no/such/file.rs")
        .expect("get_docs_for_file must not error");
    assert!(result.is_empty(), "unknown file must return empty vec, not Err");
}

/// set_docs_for_file must overwrite a previous mapping (idempotent update).
#[test]
fn docstore_set_docs_for_file_overwrites_previous() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let first = vec!["old-chunk".to_string()];
    store
        .set_docs_for_file("proj-upd", "rs", "lib.rs", &first)
        .expect("first set");

    let updated = vec!["new-chunk-a".to_string(), "new-chunk-b".to_string()];
    store
        .set_docs_for_file("proj-upd", "rs", "lib.rs", &updated)
        .expect("second set");

    let retrieved = store
        .get_docs_for_file("proj-upd", "rs", "lib.rs")
        .expect("get");
    assert_eq!(retrieved, updated, "second set must overwrite first");
}

// ── get_all_docs_for_file ─────────────────────────────────────────────────────

/// get_all_docs_for_file must return the full DocRecords in insertion order.
#[test]
fn docstore_get_all_docs_for_file_returns_full_records() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let doc_a = make_doc("file-doc-a", "src/types.rs", "rust");
    let doc_b = make_doc("file-doc-b", "src/types.rs", "rust");

    store
        .put_docs("proj-all", &[doc_a.clone(), doc_b.clone()])
        .expect("put_docs");
    store
        .set_docs_for_file(
            "proj-all",
            "rust",
            "src/types.rs",
            &["file-doc-a".to_string(), "file-doc-b".to_string()],
        )
        .expect("set_docs_for_file");

    let recs = store
        .get_all_docs_for_file("proj-all", "rust", "src/types.rs")
        .expect("get_all_docs_for_file must not error");

    assert_eq!(recs.len(), 2, "must return 2 DocRecords; got {}", recs.len());
    assert_eq!(recs[0].doc_id, "file-doc-a");
    assert_eq!(recs[1].doc_id, "file-doc-b");
}

/// get_all_docs_for_file for unknown file must return empty vec.
#[test]
fn docstore_get_all_docs_for_file_unknown_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let recs = store
        .get_all_docs_for_file("proj-none", "rust", "no/file.rs")
        .expect("must not error");
    assert!(recs.is_empty(), "unknown file must return empty vec");
}

// ── set_fingerprints (batch) ──────────────────────────────────────────────────

/// set_fingerprints must persist all fingerprints in one transaction;
/// each must be retrievable via get_fingerprint.
#[test]
fn docstore_set_fingerprints_batch_all_retrievable() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let fps = vec![
        make_fingerprint("src/a.rs"),
        make_fingerprint("src/b.rs"),
        make_fingerprint("src/c.rs"),
    ];
    store
        .set_fingerprints("proj-fps", &fps)
        .expect("set_fingerprints must succeed");

    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        let fp = store
            .get_fingerprint("proj-fps", path)
            .expect("get_fingerprint must not error");
        assert!(
            fp.is_some(),
            "fingerprint for '{path}' must be retrievable after batch set"
        );
        assert_eq!(fp.unwrap().rel_path, path);
    }
}

/// set_fingerprints with empty slice must succeed and not error.
#[test]
fn docstore_set_fingerprints_empty_batch_is_noop() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let result = store.set_fingerprints("proj-empty-fps", &[]);
    assert!(result.is_ok(), "set_fingerprints([]) must succeed (no-op)");
}

// ── list_tracked_paths ────────────────────────────────────────────────────────

/// list_tracked_paths must return all paths stored via set_docs_for_file
/// for the specified (project, namespace).
#[test]
fn docstore_list_tracked_paths_returns_all_files_for_namespace() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let paths = ["src/a.rs", "src/b.rs", "src/c.rs"];
    for p in paths {
        store
            .set_docs_for_file("proj-tracked", "rust", p, &["chunk-x".to_string()])
            .expect("set_docs_for_file");
    }

    let tracked = store
        .list_tracked_paths("proj-tracked", "rust")
        .expect("list_tracked_paths must not error");

    assert_eq!(tracked.len(), 3, "must list 3 tracked paths; got {}", tracked.len());
    for p in paths {
        assert!(
            tracked.contains(&p.to_string()),
            "must include '{p}'; got: {tracked:?}"
        );
    }
}

/// list_tracked_paths must return only paths for the specified namespace.
#[test]
fn docstore_list_tracked_paths_namespace_isolation() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    store
        .set_docs_for_file("proj-ns", "rust", "lib.rs", &["r1".to_string()])
        .expect("set rust");
    store
        .set_docs_for_file("proj-ns", "csharp", "Form.cs", &["c1".to_string()])
        .expect("set csharp");

    let rust_paths = store
        .list_tracked_paths("proj-ns", "rust")
        .expect("list rust");
    let cs_paths = store
        .list_tracked_paths("proj-ns", "csharp")
        .expect("list csharp");

    assert_eq!(rust_paths.len(), 1, "rust namespace must have 1 path");
    assert_eq!(cs_paths.len(), 1, "csharp namespace must have 1 path");
    assert!(rust_paths[0] == "lib.rs", "rust path must be lib.rs; got {}", rust_paths[0]);
    assert!(cs_paths[0] == "Form.cs", "csharp path must be Form.cs; got {}", cs_paths[0]);
}

/// list_tracked_paths for empty project must return empty vec.
#[test]
fn docstore_list_tracked_paths_empty_project_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let tracked = store.list_tracked_paths("proj-empty", "rust").expect("list");
    assert!(tracked.is_empty(), "empty project must have 0 tracked paths");
}

// ── all_doc_ids_for_project ───────────────────────────────────────────────────

/// all_doc_ids_for_project must return all doc_ids across all namespaces.
#[test]
fn docstore_all_doc_ids_for_project_includes_all_namespaces() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    store
        .put_docs(
            "proj-all-ids",
            &[
                make_doc("fn-doc-1", "a.rs", "functions"),
                make_doc("fn-doc-2", "b.rs", "functions"),
                make_doc("class-doc-1", "A.cs", "classes"),
            ],
        )
        .expect("put_docs");

    let all_ids = store
        .all_doc_ids_for_project("proj-all-ids")
        .expect("all_doc_ids_for_project must not error");

    assert_eq!(all_ids.len(), 3, "must return 3 doc_ids; got {}", all_ids.len());
    assert!(all_ids.contains(&"fn-doc-1".to_string()), "must include fn-doc-1");
    assert!(all_ids.contains(&"fn-doc-2".to_string()), "must include fn-doc-2");
    assert!(all_ids.contains(&"class-doc-1".to_string()), "must include class-doc-1");
}

/// all_doc_ids_for_project must return empty for a project with no docs.
#[test]
fn docstore_all_doc_ids_for_project_empty_returns_empty() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let ids = store.all_doc_ids_for_project("proj-empty-ids").expect("must not error");
    assert!(ids.is_empty(), "empty project must yield no doc_ids");
}

// ── delete_namespace ──────────────────────────────────────────────────────────

/// delete_namespace must remove all docs and file mappings for that namespace,
/// leaving other namespaces in the same project untouched.
#[test]
fn docstore_delete_namespace_removes_only_that_namespace() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    // Insert docs into two namespaces
    store
        .put_docs(
            "proj-del-ns",
            &[
                make_doc("rust-doc-1", "lib.rs", "rust"),
                make_doc("rust-doc-2", "main.rs", "rust"),
            ],
        )
        .expect("put rust docs");
    store
        .put_doc("proj-del-ns", &make_doc("cs-doc-1", "App.cs", "csharp"))
        .expect("put csharp doc");

    store
        .set_docs_for_file("proj-del-ns", "rust", "lib.rs", &["rust-doc-1".to_string()])
        .expect("set file mapping rust");
    store
        .set_docs_for_file("proj-del-ns", "csharp", "App.cs", &["cs-doc-1".to_string()])
        .expect("set file mapping csharp");

    // Delete the rust namespace
    store
        .delete_namespace("proj-del-ns", "rust")
        .expect("delete_namespace must succeed");

    // rust namespace docs must be gone
    let rust_doc = store
        .get_doc("proj-del-ns", "rust", "rust-doc-1")
        .expect("get after delete");
    assert!(rust_doc.is_none(), "rust-doc-1 must be gone after delete_namespace");

    // rust file mapping must be gone
    let rust_paths = store
        .list_tracked_paths("proj-del-ns", "rust")
        .expect("list tracked");
    assert!(rust_paths.is_empty(), "rust tracked paths must be empty after delete_namespace");

    // csharp doc must NOT be affected
    let cs_doc = store
        .get_doc("proj-del-ns", "csharp", "cs-doc-1")
        .expect("get csharp doc");
    assert!(cs_doc.is_some(), "csharp doc must survive delete_namespace(rust)");

    let cs_paths = store
        .list_tracked_paths("proj-del-ns", "csharp")
        .expect("list csharp tracked");
    assert_eq!(cs_paths.len(), 1, "csharp tracked paths must survive delete_namespace(rust)");
}

/// delete_namespace on a non-existent namespace must succeed (idempotent).
#[test]
fn docstore_delete_namespace_nonexistent_is_idempotent() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    let result = store.delete_namespace("proj-none", "no-such-ns");
    assert!(result.is_ok(), "delete_namespace of nonexistent namespace must be idempotent");
}

// ── DS1: corruption-injection tests ──────────────────────────────────────────
//
// Verify that `de_bincode_or_json` fails closed on corrupt bytes — i.e. it
// returns Err rather than panicking or silently returning garbage data.
// We inject corrupt bytes by writing directly to the underlying Redb tables
// (same key format used by DocStore internals).

/// DS1: get_doc returns Err (not panic, not garbage) when the stored value is corrupt.
#[test]
fn docstore_get_doc_fails_closed_on_corrupt_bytes() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("docs.redb");

    // First, insert a valid doc so the table exists.
    {
        let store = DocStore::open(&db_path).expect("open");
        store
            .put_doc("proj-corrupt", &make_doc("doc-bad", "src/bad.rs", "rust"))
            .expect("put_doc");
    }

    // Now overwrite the stored value with corrupt bytes via raw Redb API.
    {
        static DOC_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("doc_by_id");
        let db = Database::open(&db_path).expect("raw open");
        let wtx = db.begin_write().expect("write tx");
        {
            let mut t = wtx.open_table(DOC_BY_ID).expect("open table");
            let corrupt: &[u8] = b"\xff\xfe\xfd corrupt garbage bytes \x00\x01\x02";
            t.insert("proj-corrupt\0rust\0doc-bad", corrupt)
                .expect("raw insert corrupt bytes");
        }
        wtx.commit().expect("commit");
    }

    // Re-open through DocStore and verify get_doc fails closed.
    let store = DocStore::open(&db_path).expect("reopen");
    let result = store.get_doc("proj-corrupt", "rust", "doc-bad");
    assert!(
        result.is_err(),
        "DS1: get_doc must return Err on corrupt bytes, got: {:?}",
        result
    );
}

/// DS1: get_fingerprint returns Err (not panic, not garbage) when the stored value is corrupt.
#[test]
fn docstore_get_fingerprint_fails_closed_on_corrupt_bytes() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("docs.redb");

    // Insert a valid fingerprint first.
    {
        let store = DocStore::open(&db_path).expect("open");
        store
            .set_fingerprints("proj-fp-corrupt", &[make_fingerprint("src/lib.rs")])
            .expect("set_fingerprints");
    }

    // Overwrite with corrupt bytes.
    {
        static FILE_FINGERPRINT: TableDefinition<&str, &[u8]> =
            TableDefinition::new("file_fingerprint");
        let db = Database::open(&db_path).expect("raw open");
        let wtx = db.begin_write().expect("write tx");
        {
            let mut t = wtx.open_table(FILE_FINGERPRINT).expect("open table");
            let corrupt: &[u8] = b"\xde\xad\xbe\xef not a fingerprint";
            t.insert("proj-fp-corrupt\0src/lib.rs", corrupt)
                .expect("raw insert corrupt bytes");
        }
        wtx.commit().expect("commit");
    }

    // Re-open and verify get_fingerprint fails closed.
    let store = DocStore::open(&db_path).expect("reopen");
    let result = store.get_fingerprint("proj-fp-corrupt", "src/lib.rs");
    assert!(
        result.is_err(),
        "DS1: get_fingerprint must return Err on corrupt bytes, got: {:?}",
        result
    );
}

/// count_docs_for_project must reflect the correct count after delete_namespace.
#[test]
fn docstore_count_after_delete_namespace_reflects_removal() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    store
        .put_docs(
            "proj-recount",
            &[
                make_doc("r1", "a.rs", "rust"),
                make_doc("r2", "b.rs", "rust"),
                make_doc("c1", "A.cs", "csharp"),
            ],
        )
        .expect("put");

    let before = store.count_docs_for_project("proj-recount").expect("count");
    assert_eq!(before, 3);

    store
        .delete_namespace("proj-recount", "rust")
        .expect("delete rust namespace");

    let after = store.count_docs_for_project("proj-recount").expect("count after");
    assert_eq!(after, 1, "must have 1 doc remaining after deleting rust namespace; got {after}");
}

/// DS3: `delete_namespace` must also remove FILE_FINGERPRINT entries for every
/// file that belonged to the deleted namespace.
///
/// Without this fix, orphaned fingerprints accumulate indefinitely and can bias
/// copy-forward change detection (e.g. a file deleted from the project may still
/// look "unchanged" because its old fingerprint survives namespace deletion).
#[test]
fn ds3_delete_namespace_clears_file_fingerprints() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    // Populate DOCS_BY_FILE via set_docs_for_file so delete_namespace can
    // enumerate which files belong to the namespace.
    store
        .set_docs_for_file("proj-ds3", "functions", "src/main.rs", &["d1".to_string()])
        .expect("set docs for main.rs");
    store
        .set_docs_for_file("proj-ds3", "functions", "src/lib.rs", &["d2".to_string()])
        .expect("set docs for lib.rs");

    // Plant fingerprints for those files.
    let fp1 = FileFingerprint {
        rel_path: "src/main.rs".into(),
        file_hash: "hash-main".into(),
        size: 42,
        mtime_ms: 0,
    };
    let fp2 = FileFingerprint {
        rel_path: "src/lib.rs".into(),
        file_hash: "hash-lib".into(),
        size: 100,
        mtime_ms: 0,
    };
    store
        .set_fingerprints("proj-ds3", &[fp1, fp2])
        .expect("set fingerprints");

    // Verify fingerprints exist before deletion.
    assert!(
        store.get_fingerprint("proj-ds3", "src/main.rs").unwrap().is_some(),
        "DS3: fingerprint for src/main.rs must exist before delete_namespace"
    );
    assert!(
        store.get_fingerprint("proj-ds3", "src/lib.rs").unwrap().is_some(),
        "DS3: fingerprint for src/lib.rs must exist before delete_namespace"
    );

    // Delete the namespace — this should also clear the fingerprints.
    store
        .delete_namespace("proj-ds3", "functions")
        .expect("delete_namespace must succeed");

    // After deletion, fingerprints must be gone — no stale row accumulation.
    assert!(
        store.get_fingerprint("proj-ds3", "src/main.rs").unwrap().is_none(),
        "DS3: delete_namespace must purge the FILE_FINGERPRINT row for src/main.rs; \
         stale fingerprint will bias copy-forward change detection"
    );
    assert!(
        store.get_fingerprint("proj-ds3", "src/lib.rs").unwrap().is_none(),
        "DS3: delete_namespace must purge the FILE_FINGERPRINT row for src/lib.rs; \
         stale fingerprint will bias copy-forward change detection"
    );
}

/// DS3: fingerprints for files in a *different* namespace must not be affected
/// by deleting another namespace.  This proves the fingerprint cleanup is scoped
/// to files that actually belonged to the deleted namespace, not a project-wide wipe.
#[test]
fn ds3_delete_namespace_preserves_other_namespace_fingerprints() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store = open_store(&tmp);

    // Two namespaces, one file each — use set_docs_for_file to populate DOCS_BY_FILE.
    store
        .set_docs_for_file("proj-ds3b", "ns-a", "a.rs", &["x1".to_string()])
        .expect("set ns-a docs");
    store
        .set_docs_for_file("proj-ds3b", "ns-b", "b.rs", &["y1".to_string()])
        .expect("set ns-b docs");

    let fp_a = FileFingerprint {
        rel_path: "a.rs".into(),
        file_hash: "hash-a".into(),
        size: 1,
        mtime_ms: 0,
    };
    let fp_b = FileFingerprint {
        rel_path: "b.rs".into(),
        file_hash: "hash-b".into(),
        size: 2,
        mtime_ms: 0,
    };
    store.set_fingerprints("proj-ds3b", &[fp_a, fp_b]).expect("set fps");

    // Delete ns-a — only a.rs fingerprint should be removed.
    store.delete_namespace("proj-ds3b", "ns-a").expect("delete ns-a");

    assert!(
        store.get_fingerprint("proj-ds3b", "a.rs").unwrap().is_none(),
        "DS3: a.rs fingerprint must be removed when ns-a is deleted"
    );
    assert!(
        store.get_fingerprint("proj-ds3b", "b.rs").unwrap().is_some(),
        "DS3: b.rs fingerprint must survive deletion of ns-a (belongs to ns-b)"
    );
}

