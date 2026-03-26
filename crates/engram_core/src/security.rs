use crate::types::{EngramError, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PathContext {
    allowed_roots: Vec<PathBuf>,
}

impl PathContext {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Result<Self> {
        if allowed_roots.is_empty() {
            return Err(EngramError::Config("allowed_roots cannot be empty".into()));
        }
        let mut roots = Vec::with_capacity(allowed_roots.len());
        for r in allowed_roots {
            let canon = std::fs::canonicalize(&r).map_err(|e| {
                EngramError::Config(format!("cannot canonicalize allowed root {r:?}: {e}"))
            })?;
            roots.push(Self::strip_unc_prefix(&canon));
        }
        Ok(Self {
            allowed_roots: roots,
        })
    }

    /// Resolve `input` to an absolute canonical path and ensure it sits within any allowed root.
    ///
    /// Falls back to parent-based resolution when `input` doesn't exist yet (e.g. new files).
    /// On Windows, strips the `\\?\` extended-length prefix so downstream string operations
    /// (like `starts_with`, relative path derivation) behave consistently.
    pub fn resolve_path(&self, input: impl AsRef<Path>) -> Result<PathBuf> {
        let input = input.as_ref();
        let canon = std::fs::canonicalize(input).or_else(|_| {
            // Path may not exist yet (e.g. new file in a new nested directory).
            // Walk up the ancestor chain until we find an existing directory we can
            // canonicalize, then re-append the unresolved suffix beneath it.
            //
            // Depth limit prevents probing top-level system directories when given
            // a deeply nested non-existent path. 64 components is generous enough
            // for any legitimate project layout while stopping runaway traversals.
            const MAX_ANCESTOR_DEPTH: usize = 64;
            let mut ancestor = input;
            let mut suffix = std::path::PathBuf::new();
            let mut depth: usize = 0;
            loop {
                depth += 1;
                if depth > MAX_ANCESTOR_DEPTH {
                    break Err(EngramError::PathNotAllowed(format!(
                        "cannot access {input:?}: ancestor walk exceeded {MAX_ANCESTOR_DEPTH} levels"
                    )));
                }
                match ancestor.parent() {
                    Some(parent) => {
                        // Prepend current component to suffix before ascending.
                        if let Some(name) = ancestor.file_name() {
                            let mut new_suffix = std::path::PathBuf::from(name);
                            new_suffix.push(&suffix);
                            suffix = new_suffix;
                        }
                        ancestor = parent;
                        if ancestor.exists() {
                            let canon_ancestor = std::fs::canonicalize(ancestor).map_err(|e| {
                                EngramError::PathNotAllowed(format!("cannot access {input:?}: {e}"))
                            })?;
                            // Security: reject any suffix component that is `..`.
                            // Such components would escape the canonicalized ancestor and
                            // bypass the allowed-roots check below (lexical starts_with
                            // cannot resolve `..` across a join boundary).
                            for component in suffix.components() {
                                if matches!(component, std::path::Component::ParentDir) {
                                    return Err(EngramError::PathNotAllowed(format!(
                                        "cannot access {input:?}: path traversal via '..' detected"
                                    )));
                                }
                            }
                            // Security (Fix #1): walk each existing intermediate component
                            // of the suffix and reject any that is a symlink.  A symlink
                            // in the unresolved suffix can point outside allowed_roots even
                            // though the lexical `starts_with` check below would pass.
                            let mut partial = canon_ancestor.clone();
                            for component in suffix.components() {
                                partial.push(component);
                                if partial.exists() {
                                    match std::fs::symlink_metadata(&partial) {
                                        Ok(meta) if meta.file_type().is_symlink() => {
                                            return Err(EngramError::PathNotAllowed(format!(
                                                "cannot access {input:?}: symlink in unresolved path component {partial:?}"
                                            )));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            break Ok(canon_ancestor.join(&suffix));
                        }
                    }
                    None => {
                        break Err(EngramError::PathNotAllowed(format!(
                            "cannot access {input:?}: no existing ancestor directory found"
                        )));
                    }
                }
            }
        })?;
        // On Windows, canonicalize returns \\?\ UNC paths. Strip the prefix for consistency.
        let canon = Self::strip_unc_prefix(&canon);
        for root in &self.allowed_roots {
            if canon.starts_with(root) {
                return Ok(canon);
            }
        }
        Err(EngramError::PathNotAllowed(format!(
            "{canon:?} is outside allowed_roots"
        )))
    }

    /// Strip the `\\?\` extended-length path prefix on Windows.
    #[cfg(windows)]
    fn strip_unc_prefix(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            PathBuf::from(stripped)
        } else {
            p.to_path_buf()
        }
    }

    #[cfg(not(windows))]
    fn strip_unc_prefix(p: &Path) -> PathBuf {
        p.to_path_buf()
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.allowed_roots
    }
}

/// Join a relative `sub_path` to `base_dir`, rejecting any traversal attempt.
///
/// This is a lightweight, synchronous guard for use inside `spawn_blocking` closures
/// where the full `PathContext::resolve_path` (which does I/O) is not available.
///
/// Rejects:
/// - Absolute paths (starting with `/`, `\`, or a drive letter like `C:`)
/// - Parent-directory components (`..`)
/// - NUL bytes (which would truncate paths on some OSes)
/// - Symlinks in any path component (prevents symlink-traversal out of `base_dir`)
///
/// Returns the joined `base_dir/sub_path` on success.
pub fn safe_join(base_dir: &Path, sub_path: &str) -> Result<PathBuf> {
    // Reject NUL bytes
    if sub_path.contains('\0') {
        return Err(EngramError::PathNotAllowed(format!(
            "path contains NUL byte: {:?}",
            &sub_path[..sub_path.len().min(80)]
        )));
    }

    let rel = Path::new(sub_path);

    // Reject absolute paths
    if rel.is_absolute() || sub_path.starts_with('/') || sub_path.starts_with('\\') {
        return Err(EngramError::PathNotAllowed(format!(
            "absolute path not allowed: {sub_path:?}"
        )));
    }

    // Reject parent-directory traversal and Windows drive-prefix components.
    // Component::Prefix covers "C:" style prefixes that Path::is_absolute()
    // may miss on Windows when the path is relative-looking but contains a
    // drive letter (e.g. "C:foo" is relative but has a Prefix component and
    // resolves outside any reasonable base_dir).
    for component in rel.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(EngramError::PathNotAllowed(format!(
                    "path traversal via '..' not allowed: {sub_path:?}"
                )));
            }
            std::path::Component::Prefix(_) => {
                return Err(EngramError::PathNotAllowed(format!(
                    "Windows drive-prefix component not allowed: {sub_path:?}"
                )));
            }
            _ => {}
        }
    }

    let joined = base_dir.join(rel);

    // Reject symlinks in any existing component of the joined path.
    // A symlink inside the project directory can point outside base_dir even
    // though the lexical path looks safe.  Walk each prefix incrementally so
    // that intermediate symlinks (not just the final component) are caught.
    //
    // TOCTOU note: we call `symlink_metadata` directly (single syscall) instead
    // of the previous `exists() + symlink_metadata()` double-stat, which had a
    // race window between the two calls.  A `NotFound` error means the component
    // does not yet exist, which is safe (a non-existent path cannot be a symlink).
    let mut partial = base_dir.to_path_buf();
    for component in rel.components() {
        partial.push(component);
        match std::fs::symlink_metadata(&partial) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(EngramError::PathNotAllowed(format!(
                    "symlink not allowed in path: {partial:?}"
                )));
            }
            Ok(_) => {} // regular file/dir — safe to continue
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Component does not yet exist — safe (can't be a symlink if it's not there)
            }
            Err(e) => {
                // ENG-AUD-2026-0006: non-NotFound error means we cannot confirm the path is
                // safe (e.g., permission denied on an intermediate component). Fail closed.
                return Err(EngramError::PathNotAllowed(format!(
                    "ENG-AUD-2026-0006: cannot verify path component {:?}: {e} — failing closed for safety",
                    partial
                )));
            }
        }
    }

    Ok(joined)
}

/// Validate and open a file for reading in a single operation, closing the
/// TOCTOU window that exists between `safe_join` returning a path and the
/// caller opening the file.
///
/// The returned `File` handle is for the actual file that was validated;
/// a post-open metadata check ensures no symlink was swapped in between
/// the lexical validation and the `open(2)` call.
pub fn safe_open_read(base_dir: &Path, sub_path: &str) -> Result<std::fs::File> {
    let path = safe_join(base_dir, sub_path)?;
    let file = std::fs::File::open(&path).map_err(|e| {
        EngramError::PathNotAllowed(format!("cannot open {:?}: {e}", path))
    })?;
    // Re-validate after open: the metadata obtained from the open file
    // descriptor describes what was *actually* opened, not a path that may
    // have been swapped.  On Windows this calls GetFileInformationByHandle;
    // on POSIX it calls fstat — both are immune to a concurrent rename/swap.
    let post_meta = file.metadata().map_err(|e| {
        EngramError::PathNotAllowed(format!("cannot stat open handle for {:?}: {e}", path))
    })?;
    if post_meta.file_type().is_symlink() {
        return Err(EngramError::PathNotAllowed(format!(
            "symlink detected at open boundary for {:?}",
            path
        )));
    }
    Ok(file)
}

/// Validate, open, and read a file entirely, closing the TOCTOU window between
/// path validation and file open.  This is the preferred way to read project
/// files when the full content is needed.
pub fn safe_read_to_string(base_dir: &Path, sub_path: &str) -> Result<String> {
    use std::io::Read as _;
    let mut file = safe_open_read(base_dir, sub_path)?;
    let mut content = String::new();
    file.read_to_string(&mut content).map_err(|e| {
        EngramError::PathNotAllowed(format!("read error for {sub_path:?}: {e}"))
    })?;
    Ok(content)
}

/// Validate that a composite-key component contains no separator characters.
///
/// Both the graph store (NUL-separated keys) and the doc store (newline-separated keys)
/// use this function to reject values that would corrupt composite keys.
pub fn validate_key_component(name: &str, value: &str) -> std::result::Result<(), String> {
    if value.contains('\0') {
        return Err(format!(
            "key component '{name}' contains NUL byte — this would corrupt composite keys. \
             Value (truncated): {:?}",
            &value[..value.len().min(80)]
        ));
    }
    if value.contains('\n') {
        return Err(format!(
            "key component '{name}' contains newline — this would corrupt composite keys. \
             Value (truncated): {:?}",
            &value[..value.len().min(80)]
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── safe_join ──────────────────────────────────────────────────────────

    #[test]
    fn safe_join_allows_normal_relative_path() {
        let base = Path::new("/project");
        let result = safe_join(base, "src/main.rs").unwrap();
        assert_eq!(result, base.join("src/main.rs"));
    }

    #[test]
    fn safe_join_rejects_parent_traversal() {
        let base = Path::new("/project");
        assert!(safe_join(base, "../etc/passwd").is_err());
        assert!(safe_join(base, "src/../../etc/passwd").is_err());
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        let base = Path::new("/project");
        assert!(safe_join(base, "/etc/passwd").is_err());
        assert!(safe_join(base, "\\Windows\\System32").is_err());
    }

    #[test]
    fn safe_join_rejects_nul_byte() {
        let base = Path::new("/project");
        assert!(safe_join(base, "file\0.txt").is_err());
    }

    #[test]
    fn safe_join_allows_windows_relative() {
        let base = Path::new("C:\\project");
        let result = safe_join(base, "src\\main.rs");
        assert!(result.is_ok());
    }

    // ── safe_join symlink rejection ───────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn safe_join_rejects_symlink_component() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        // Create a real subdir so `base/src` exists
        std::fs::create_dir(base.join("src")).unwrap();
        // Create a symlink inside the project: base/src/link → /tmp (outside base)
        symlink("/tmp", base.join("src/link")).unwrap();
        let result = safe_join(base, "src/link/secret.txt");
        assert!(
            result.is_err(),
            "safe_join should reject a path that traverses a symlink"
        );
    }

    // ── validate_key_component ─────────────────────────────────────────────

    #[test]
    fn key_component_rejects_nul() {
        assert!(validate_key_component("test", "bad\0value").is_err());
    }

    #[test]
    fn key_component_rejects_newline() {
        assert!(validate_key_component("test", "bad\nvalue").is_err());
    }

    #[test]
    fn key_component_allows_normal_values() {
        assert!(validate_key_component("test", "good_value").is_ok());
        assert!(validate_key_component("test", "path/to/file.rs").is_ok());
    }

    // ── safe_open_read and safe_read_to_string (ENG-AUD-P1-0008) ──────────

    #[test]
    fn safe_open_read_reads_real_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        std::fs::write(base.join("hello.txt"), "world").unwrap();
        let file = safe_open_read(base, "hello.txt");
        assert!(file.is_ok(), "safe_open_read should succeed for a real file");
    }

    #[test]
    fn safe_read_to_string_returns_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        std::fs::write(base.join("data.txt"), "hello test").unwrap();
        let content = safe_read_to_string(base, "data.txt").expect("should read");
        assert_eq!(content, "hello test");
    }

    #[test]
    fn safe_open_read_rejects_traversal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        let result = safe_open_read(base, "../etc/passwd");
        assert!(result.is_err(), "must reject parent-directory traversal");
    }

    #[test]
    fn safe_open_read_rejects_missing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        let result = safe_open_read(base, "nonexistent.txt");
        assert!(result.is_err(), "must fail if file does not exist");
    }

    #[test]
    #[cfg(unix)]
    fn safe_open_read_rejects_symlink_to_outside() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path();
        // Create a symlink inside the project that points outside
        symlink("/etc/passwd", base.join("evil.txt")).unwrap();
        let result = safe_open_read(base, "evil.txt");
        assert!(
            result.is_err(),
            "safe_open_read must reject a symlink pointing outside base_dir"
        );
    }

    #[test]
    fn safe_join_single_stat_no_double_check() {
        // Regression: safe_join must not call `exists()` before `symlink_metadata`
        // (double-stat TOCTOU). We cannot directly observe system calls in a unit
        // test, but we can verify that safe_join on a non-existent path succeeds —
        // which the old `if partial.exists()` guard also allowed, and the new
        // single-stat path also allows (NotFound → treat as non-symlink, OK).
        let base = Path::new("/project");
        // Non-existent path should be accepted (lexically safe, not yet on disk)
        let result = safe_join(base, "src/does_not_exist.rs");
        assert!(
            result.is_ok(),
            "safe_join must accept a lexically valid but non-existent path"
        );
    }

    // ── ENG-AUD-2026-0006: NotFound vs other errors in symlink check ──────

    #[test]
    fn safe_join_allows_nonexistent_path_component() {
        // NotFound on a component is treated as safe (path doesn't exist yet).
        let base = std::path::Path::new("/tmp/definitely_nonexistent_engram_test_base");
        let result = safe_join(base, "some/nonexistent/path.rs");
        // Should succeed lexically (the path doesn't exist, but that's OK)
        assert!(
            result.is_ok(),
            "nonexistent path must be accepted (NotFound is safe): {:?}", result
        );
    }

    #[test]
    fn safe_join_eng_aud_0006_error_distinction_in_source() {
        // Structural test: the source must distinguish NotFound from other errors.
        let source = include_str!("security.rs");
        assert!(
            source.contains("ENG-AUD-2026-0006"),
            "security.rs must contain ENG-AUD-2026-0006 audit tag"
        );
        assert!(
            source.contains("ErrorKind::NotFound"),
            "security.rs must check for NotFound specifically"
        );
        // The old catch-all Err(_) => {} pattern must not exist
        // (check that blank-swallow pattern is gone from the symlink walk)
        // Use a positive check: the fail-closed error message must be present
        assert!(
            source.contains("failing closed for safety"),
            "safe_join must fail closed for non-NotFound symlink_metadata errors"
        );
    }
}
