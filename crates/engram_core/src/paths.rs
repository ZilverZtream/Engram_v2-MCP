use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// TODO-47: canonicalizing a root is a syscall; roots are stable per project,
/// so memoize. Maps a root path -> its canonical form (None = canonicalize
/// failed, e.g. root doesn't exist; we then skip the symlink fallback).
static CANON_ROOT_CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<PathBuf>>>> =
    LazyLock::new(Default::default);

fn canonical_root(root: &Path) -> Option<PathBuf> {
    if let Ok(cache) = CANON_ROOT_CACHE.lock()
        && let Some(hit) = cache.get(root)
    {
        return hit.clone();
    }
    let canon = std::fs::canonicalize(root).ok();
    if let Ok(mut cache) = CANON_ROOT_CACHE.lock() {
        cache.insert(root.to_path_buf(), canon.clone());
    }
    canon
}

/// A project-relative path that is guaranteed to use `/` as a separator.
/// It is normalized upon creation to remove leading/trailing slashes and convert `\` to `/`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RelPath(String);

impl RelPath {
    /// Create a new RelPath from a string, normalizing separators to `/` and trimming leading/trailing slashes.
    pub fn new(path: &str) -> Self {
        // Security hardening:
        // - force slash separators
        // - strip accidental absolute prefixes
        // - collapse empty / `.` segments
        // - prevent parent-traversal (`..`) from escaping virtual root
        // - drop NUL/control chars that can poison downstream stores/logs
        let normalized = path.replace('\\', "/");
        let trimmed = normalized.trim_matches('/');

        let mut parts: Vec<&str> = Vec::new();
        let mut attempted_root_escape = false;
        for raw in trimmed.split('/') {
            let seg = raw.trim();
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                if parts.pop().is_none() {
                    attempted_root_escape = true;
                }
                continue;
            }
            if seg.contains('\0') || seg.chars().any(|c| c.is_control()) {
                continue;
            }
            parts.push(seg);
        }

        if attempted_root_escape {
            // Preserve safety invariant: a RelPath must never silently remap an
            // out-of-root traversal attempt into an apparently safe in-root path.
            // Emit an empty path sentinel so callers can reject/skip explicitly.
            Self(String::new())
        } else {
            Self(parts.join("/"))
        }
    }

    /// Create a RelPath from an absolute path relative to a root.
    ///
    /// TODO-47: lexical `strip_prefix` fails when the root is a symlink and
    /// the walker yields canonicalized paths (or vice versa), silently
    /// dropping every file. We try the fast lexical strip first, then fall
    /// back to stripping against the canonicalized root, then against both
    /// sides canonicalized — paying the syscall cost only on the rare
    /// symlinked-root path, never on the common case.
    pub fn from_relative(root: &Path, path: &Path) -> Option<Self> {
        fn finalize(rel: &Path) -> Option<RelPath> {
            if rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return None;
            }
            Some(RelPath::new(&rel.to_string_lossy()))
        }

        // Fast path: pure lexical prefix (no syscalls).
        if let Ok(rel) = path.strip_prefix(root) {
            return finalize(rel);
        }

        // Symlink fallback: strip against the canonical root.
        let canon_root = canonical_root(root)?;
        if let Ok(rel) = path.strip_prefix(&canon_root) {
            return finalize(rel);
        }

        // Last resort: canonicalize the path too (handles a symlinked path
        // under a canonical root). Bounded to the failure case only.
        let canon_path = std::fs::canonicalize(path).ok()?;
        canon_path.strip_prefix(&canon_root).ok().and_then(finalize)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn file_name(&self) -> Option<&str> {
        self.0.rsplit('/').next()
    }

    /// TODO-47: the internal form is always `/`-separated for portability.
    /// For user-facing error/log messages about a real on-disk path, render
    /// with the platform separator so Windows users see familiar `\`.
    pub fn to_native_string(&self) -> String {
        if std::path::MAIN_SEPARATOR == '/' {
            self.0.clone()
        } else {
            self.0.replace('/', std::path::MAIN_SEPARATOR_STR)
        }
    }
}

impl From<&str> for RelPath {
    fn from(s: &str) -> Self {
        RelPath::new(s)
    }
}

impl From<String> for RelPath {
    fn from(s: String) -> Self {
        RelPath::new(&s)
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for RelPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rel_path_normalization() {
        assert_eq!(RelPath::new(r"foo\bar").as_str(), "foo/bar");
        assert_eq!(RelPath::new("/foo/bar/").as_str(), "foo/bar");
        assert_eq!(RelPath::new(r"\foo\bar\").as_str(), "foo/bar");
        assert_eq!(RelPath::new("foo/bar").as_str(), "foo/bar");
    }

    #[test]
    fn from_relative_recovers_under_canonical_mismatch() {
        // TODO-47: canonicalize() yields a form (Windows extended-length
        // prefix, macOS /private/tmp) that does NOT lexically strip against
        // the plain root, exercising the symlink/canonical fallback without
        // needing real symlinks.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let file = root.join("sub/file.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();

        let canon_file = std::fs::canonicalize(&file).unwrap();
        let rel = RelPath::from_relative(root, &canon_file)
            .expect("canonical fallback must recover the rel path");
        assert_eq!(rel.as_str(), "sub/file.rs");
    }

    #[test]
    fn to_native_string_uses_platform_separator() {
        let rel = RelPath::new("a/b/c.rs");
        assert_eq!(rel.as_str(), "a/b/c.rs", "internal form stays slash");
        let sep = std::path::MAIN_SEPARATOR;
        let expected = format!("a{sep}b{sep}c.rs");
        assert_eq!(rel.to_native_string(), expected);
    }

    #[test]
    fn test_from_relative() {
        let root = Path::new("/base/dir");
        let path = Path::new("/base/dir/src/lib.rs");
        let rel = RelPath::from_relative(root, path).unwrap();
        assert_eq!(rel.as_str(), "src/lib.rs");
    }

    #[test]
    fn test_empty_rel_path() {
        assert_eq!(RelPath::new("/").as_str(), "");
        assert!(RelPath::new("/").is_empty());
    }

    #[test]
    fn test_rel_path_traversal_normalized() {
        assert_eq!(RelPath::new("src/../lib.rs").as_str(), "lib.rs");
        assert_eq!(RelPath::new("../../etc/passwd").as_str(), "");
    }

    #[test]
    fn test_rel_path_drops_control_chars() {
        assert_eq!(RelPath::new("foo/\u{0000}bar/baz").as_str(), "foo/baz");
    }
}
