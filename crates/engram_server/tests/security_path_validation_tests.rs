#![allow(clippy::unwrap_used)]
//! Behavioral tests for the production path validation functions.
//!
//! All tests call production code directly:
//!  - `engram_core::PathContext::new()` and `PathContext::resolve_path()`
//!  - `engram_core::safe_join()`
//!  - `engram_core::safe_open_read()`
//!
//! These replace the test-local `resolve_dotdot_components()` helper that
//! was previously used. Every assertion is against the actual production
//! enforcement path, not a re-implementation of it.

use engram_core::{safe_join, safe_open_read, PathContext};
use std::path::PathBuf;

// ── PathContext construction contracts ────────────────────────────────────────

/// Empty allowed_roots must be rejected at construction time (fail-closed).
/// AUD-2026-INV-0001: PathContext::new is the production enforcer — empty
/// roots must produce an explicit Err, not a permissive PathContext.
#[test]
fn path_context_new_empty_roots_fails_closed() {
    let result = PathContext::new(vec![]);
    assert!(
        result.is_err(),
        "PathContext::new(vec![]) must return Err — empty roots must fail closed, not permit all paths"
    );
    let err = result.unwrap_err().to_string().to_lowercase();
    assert!(
        err.contains("empty") || err.contains("root") || err.contains("allowed"),
        "error must describe the empty-roots constraint; got: {err}"
    );
}

/// A non-existent root must be rejected at construction time because
/// PathContext::new canonicalizes each root and fails if it doesn't exist.
#[test]
fn path_context_new_nonexistent_root_returns_err() {
    let nonexistent = PathBuf::from(
        "/definitely_does_not_exist_engram_test_root_abc123_xyz",
    );
    let result = PathContext::new(vec![nonexistent]);
    assert!(
        result.is_err(),
        "PathContext::new with a non-existent root must return Err (canonicalize fails on missing path)"
    );
}

/// A valid existing directory must be accepted.
#[test]
fn path_context_new_valid_existing_root_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let result = PathContext::new(vec![tmp.path().to_path_buf()]);
    assert!(
        result.is_ok(),
        "PathContext::new with a valid existing directory must succeed; got: {:?}",
        result.err()
    );
}

// ── PathContext::resolve_path — production allowed-roots enforcement ───────────

/// A file that lives inside the allowed root must resolve successfully.
#[test]
fn path_context_resolves_file_inside_allowed_root() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tmp.path().join("data.txt"), b"content").expect("write test file");

    let ctx = PathContext::new(vec![tmp.path().to_path_buf()]).expect("valid root");

    let result = ctx.resolve_path(tmp.path().join("data.txt"));
    assert!(
        result.is_ok(),
        "path inside allowed root must resolve successfully; got: {:?}",
        result.err()
    );
}

/// A path that lives outside the allowed root must be denied even if it exists.
/// This is the core allowed-roots enforcement property.
#[test]
fn path_context_rejects_path_outside_allowed_root() {
    let root_dir = tempfile::TempDir::new().expect("root tmpdir");
    let outside_dir = tempfile::TempDir::new().expect("outside tmpdir");
    std::fs::write(outside_dir.path().join("secret.txt"), b"outside").expect("write");

    let ctx = PathContext::new(vec![root_dir.path().to_path_buf()]).expect("valid root");

    let result = ctx.resolve_path(outside_dir.path().join("secret.txt"));
    assert!(
        result.is_err(),
        "path outside allowed root must be denied — production enforce_path contract; got Ok"
    );
}

// ── safe_join — production lightweight traversal guard ───────────────────────

/// Parent-directory traversal (`..`) must be rejected.
/// This is the AUD-2026-INV-0001 fix: `..` in sub_path cannot escape base_dir.
#[test]
fn safe_join_rejects_dotdot_traversal() {
    let base = PathBuf::from("/project/root");

    let r1 = safe_join(&base, "../etc/passwd");
    assert!(r1.is_err(), "safe_join must reject '../etc/passwd' traversal");

    let r2 = safe_join(&base, "src/../../etc/passwd");
    assert!(r2.is_err(), "safe_join must reject nested '..' traversal via 'src/../../'");

    // Error message must be informative, not empty
    let err_msg = r1.unwrap_err().to_string();
    assert!(
        err_msg.contains("traversal") || err_msg.contains("..") || err_msg.contains("not allowed"),
        "error must describe the traversal reason; got: {err_msg}"
    );
}

/// Absolute sub_path must be rejected — cannot escape base_dir with an absolute path.
#[test]
fn safe_join_rejects_absolute_sub_path() {
    let base = PathBuf::from("/project/root");

    assert!(
        safe_join(&base, "/etc/passwd").is_err(),
        "safe_join must reject absolute path '/etc/passwd'"
    );
    assert!(
        safe_join(&base, "\\Windows\\System32\\cmd.exe").is_err(),
        "safe_join must reject backslash-absolute path"
    );
}

/// NUL byte in sub_path must be rejected.
/// A NUL byte truncates the path on many operating systems.
#[test]
fn safe_join_rejects_nul_byte_in_sub_path() {
    let base = PathBuf::from("/project/root");
    let result = safe_join(&base, "file\0evil.txt");
    assert!(
        result.is_err(),
        "safe_join must reject sub_path containing a NUL byte"
    );
}

/// A normal relative path must be accepted and joined correctly.
#[test]
fn safe_join_accepts_normal_relative_path() {
    let base = PathBuf::from("/project/root");
    let result = safe_join(&base, "src/lib.rs");
    assert!(
        result.is_ok(),
        "safe_join must accept a normal relative path 'src/lib.rs'; got: {:?}",
        result.err()
    );
    let joined = result.unwrap();
    assert!(
        joined.to_string_lossy().contains("src") && joined.to_string_lossy().contains("lib.rs"),
        "joined path must incorporate the sub_path components; got: {:?}",
        joined
    );
}

/// Nested relative paths (no traversal) must be accepted.
#[test]
fn safe_join_accepts_nested_relative_path() {
    let base = PathBuf::from("/project/root");
    let result = safe_join(&base, "a/b/c/deep.txt");
    assert!(
        result.is_ok(),
        "safe_join must accept a nested relative path without traversal; got: {:?}",
        result.err()
    );
}

// ── safe_open_read — production validated file open ───────────────────────────

/// safe_open_read must succeed for a real file inside the base directory.
#[test]
fn safe_open_read_succeeds_for_file_within_base() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(tmp.path().join("hello.txt"), b"world content").expect("write");

    let result = safe_open_read(tmp.path(), "hello.txt");
    assert!(
        result.is_ok(),
        "safe_open_read must succeed for a real file within base; got: {:?}",
        result.err()
    );
}

/// safe_open_read must reject parent-directory traversal.
#[test]
fn safe_open_read_rejects_dotdot_traversal() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let result = safe_open_read(tmp.path(), "../etc/passwd");
    assert!(
        result.is_err(),
        "safe_open_read must reject '../etc/passwd' traversal"
    );
}

/// safe_open_read must return Err for a missing file, not panic.
#[test]
fn safe_open_read_returns_err_for_missing_file() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let result = safe_open_read(tmp.path(), "nonexistent.txt");
    assert!(
        result.is_err(),
        "safe_open_read must return Err for a file that does not exist, not panic"
    );
}

/// SEC1-TOCTOU: resolve_path on a non-existent file inside an existing root
/// must succeed (ancestor walk finds the root and appends the suffix lexically).
/// This exercises the fixed ancestor-walk code path without a `exists()` gate.
#[test]
fn sec1_resolve_path_nonexistent_file_in_existing_root_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let ctx = PathContext::new(vec![tmp.path().to_path_buf()])
        .expect("PathContext::new must succeed for existing root");

    // The file does not exist, but the directory does — ancestor walk must
    // canonicalize the existing parent and re-append the file name.
    let target = tmp.path().join("newfile.txt");
    let result = ctx.resolve_path(&target);
    assert!(
        result.is_ok(),
        "SEC1-TOCTOU: resolve_path must succeed for a non-existent file inside an \
         allowed root; got: {:?}",
        result.err()
    );
}

/// SEC1-TOCTOU: structural check — `resolve_path` in security.rs must NOT contain
/// an `ancestor.exists()` call followed by `canonicalize`. After the TOCTOU fix,
/// only a single `canonicalize` call is made (no preceding `exists()` gate).
#[test]
fn sec1_source_does_not_contain_exists_plus_canonicalize_toctou_pattern() {
    let source = include_str!("../../engram_core/src/security.rs");

    // The old TOCTOU pattern: `ancestor.exists()` as a gate before `canonicalize`.
    // After the fix this must not appear — `canonicalize` is called directly.
    let toctou_pattern = source.contains("ancestor.exists()");
    assert!(
        !toctou_pattern,
        "SEC1-TOCTOU: security.rs must not contain `ancestor.exists()` as a gate \
         before `canonicalize`. This is a TOCTOU race: between exists() returning \
         true and canonicalize() executing, a symlink swap can bypass root checks. \
         Fix: call canonicalize() directly and handle Err as 'not found'."
    );

    // The fixed pattern: `match std::fs::canonicalize(ancestor)` is used directly.
    assert!(
        source.contains("canonicalize(ancestor)"),
        "SEC1-TOCTOU: security.rs must call canonicalize(ancestor) directly \
         (single-syscall pattern) to eliminate the TOCTOU window"
    );
}

/// SEC1-TOCTOU: structural check — `partial.exists()` as a gate before
/// `symlink_metadata` must not appear in the fixed source.
#[test]
fn sec1_source_does_not_contain_partial_exists_toctou_pattern() {
    let source = include_str!("../../engram_core/src/security.rs");

    // Check specifically for the guarded form `if partial.exists() {` — the
    // bare string `partial.exists()` may appear in comments explaining the fix.
    let toctou_pattern = source.contains("if partial.exists()");
    assert!(
        !toctou_pattern,
        "SEC1-TOCTOU: security.rs must not contain `if partial.exists()` as a gate \
         before `symlink_metadata`. This is a TOCTOU race: a symlink could be \
         injected between the exists() check and the metadata read. Fix: call \
         symlink_metadata() directly and treat Err(NotFound) as 'no symlink'."
    );

    // The fixed pattern: symlink_metadata is called unconditionally.
    assert!(
        source.contains("symlink_metadata(&partial)"),
        "SEC1-TOCTOU: security.rs must call symlink_metadata(&partial) directly \
         (single-syscall pattern) without a preceding exists() gate"
    );
}
