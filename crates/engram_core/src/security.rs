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
            roots.push(canon);
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
            let mut ancestor = input;
            let mut suffix = std::path::PathBuf::new();
            loop {
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
