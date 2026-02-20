use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

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
        for raw in trimmed.split('/') {
            let seg = raw.trim();
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                parts.pop();
                continue;
            }
            if seg.contains('\0') || seg.chars().any(|c| c.is_control()) {
                continue;
            }
            parts.push(seg);
        }

        Self(parts.join("/"))
    }

    /// Create a RelPath from an absolute path relative to a root.
    pub fn from_relative(root: &Path, path: &Path) -> Option<Self> {
        let rel = path.strip_prefix(root).ok()?;
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return None;
        }
        Some(Self::new(&rel.to_string_lossy()))
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
        assert_eq!(RelPath::new("../../etc/passwd").as_str(), "etc/passwd");
    }

    #[test]
    fn test_rel_path_drops_control_chars() {
        assert_eq!(RelPath::new("foo/\u{0000}bar/baz").as_str(), "foo/baz");
    }
}
