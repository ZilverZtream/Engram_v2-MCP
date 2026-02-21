use serde::{Deserialize, Serialize};

/// Stable identifier for a project.
///
/// In v1 this was a string UUID. Keep it stringy so we can migrate state without re-keying.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

/// Legacy numeric DocId - kept for backward compat only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocId(pub u64);

/// Legacy numeric ChunkId - kept for backward compat only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChunkId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub String);

impl NodeId {
    /// Canonical ID for a file node.
    pub fn file(rel_path: &str) -> Self {
        // Hard guard: reject absolute paths. They break graph traversal and portability.
        debug_assert!(
            !std::path::Path::new(rel_path).is_absolute(),
            "NodeId::file received absolute path: {}",
            rel_path
        );
        Self(format!("file:{}", rel_path))
    }

    /// Canonical ID for a symbol node.
    ///
    /// Uses FQN if available for stability across edits and cross-file resolution.
    /// Falls back to location-based ID if FQN is absent.
    pub fn symbol(
        kind: &str,
        fqn: Option<&str>,
        rel_path: &str,
        name: &str,
        start_line: u32,
    ) -> Self {
        if let Some(fqn) = fqn {
            Self(format!("sym:{}:{}", kind, fqn))
        } else {
            Self(format!("sym:{}:{}:{}:{}", kind, rel_path, name, start_line))
        }
    }

    /// Canonical ID for a SQL node.
    pub fn sql(kind: &str, normalized_content: &str) -> Self {
        Self(format!("sql:{}:{}", kind, normalized_content))
    }

    /// Canonical ID for a WebForms page node.
    pub fn page(rel_path: &str) -> Self {
        Self(format!("page:{}", rel_path))
    }

    /// Canonical ID for a WebForms control node.
    pub fn control(page_rel_path: &str, control_id: &str) -> Self {
        Self(format!("control:{}:{}", page_rel_path, control_id))
    }

    /// Canonical ID for a GIS configuration node (API keys, zoom, center point).
    pub fn gis_config(page_rel_path: &str, config_key: &str) -> Self {
        Self(format!("gis_config:{}:{}", page_rel_path, config_key))
    }

    /// Canonical ID for a database table node.
    pub fn table(table_name: &str) -> Self {
        Self(format!("table:{}", table_name.to_lowercase()))
    }

    /// Canonical ID for a database column node.
    pub fn column(table_name: &str, column_name: &str) -> Self {
        Self(format!(
            "column:{}:{}",
            table_name.to_lowercase(),
            column_name.to_lowercase()
        ))
    }

    /// Canonical ID for a global state node (Session, ViewState, etc.).
    pub fn state(state_type: &str, key: &str) -> Self {
        Self(format!("state:{}:{}", state_type, key))
    }

    /// Canonical ID for a data-binding field node (Eval/Bind expression target).
    pub fn binding_field(field_name: &str) -> Self {
        Self(format!("binding_field:{}", field_name))
    }

    /// Canonical ID for a UI container node (Panel, Table, GroupBox, div, etc.).
    pub fn ui_container(page_rel_path: &str, container_id: &str) -> Self {
        Self(format!("ui_container:{}:{}", page_rel_path, container_id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Instance-level identity: unique per (path + location + content).
///
/// doc_id = blake3(rel_path + NUL + start_line + NUL + end_line + NUL + content_hash) as hex.
/// Same file + same range + same content → same doc_id.
/// Identical content in different files → different doc_id but same content_hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocIdStr(pub String);

/// Content-level identity: deduplicated hash of the actual bytes.
///
/// content_hash = blake3(content_bytes) as hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub String);

impl DocIdStr {
    /// Compute a stable doc_id for a given (rel_path, start_line, end_line, content_hash).
    pub fn compute(
        rel_path: &str,
        start_line: u32,
        end_line: u32,
        content_hash: &ContentHash,
    ) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(rel_path.as_bytes());
        h.update(&[0u8]); // NUL separator
        h.update(&start_line.to_le_bytes());
        h.update(&[0u8]);
        h.update(&end_line.to_le_bytes());
        h.update(&[0u8]);
        h.update(content_hash.0.as_bytes());
        Self(h.finalize().to_hex().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ContentHash {
    /// Compute blake3 hash of raw content bytes.
    ///
    /// P3.2: Normalizes line endings (\r\n -> \n) before hashing to ensure
    /// deterministic hashes across platforms (Windows vs Linux).
    pub fn compute(content: &[u8]) -> Self {
        // Optimization: only normalize if it looks like it might have \r
        if content.contains(&b'\r') {
            let mut normalized = Vec::with_capacity(content.len());
            let mut i = 0;
            while i < content.len() {
                if content[i] == b'\r' {
                    if i + 1 < content.len() && content[i + 1] == b'\n' {
                        normalized.push(b'\n');
                        i += 2;
                    } else {
                        // Isolated \r - keep it or convert to \n?
                        // For determinism, let's treat any \r as part of potential \r\n or just \n.
                        normalized.push(b'\n');
                        i += 1;
                    }
                } else {
                    normalized.push(content[i]);
                    i += 1;
                }
            }
            Self(blake3::hash(&normalized).to_hex().to_string())
        } else {
            Self(blake3::hash(content).to_hex().to_string())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Build the canonical Tantivy/Lance primary key.
///
/// Format: `{project_id}:{namespace}:{generation}:{doc_id}`
pub fn build_pk(project_id: &str, namespace: &str, generation: u64, doc_id: &str) -> String {
    let effective_gen = if let Ok(policy) = crate::namespaces::get_policy(namespace) {
        if policy.versioning == crate::namespaces::NamespaceVersioning::GlobalMutable {
            0
        } else {
            generation
        }
    } else {
        generation
    };
    format!("{project_id}:{namespace}:{effective_gen}:{doc_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelPath;

    #[test]
    fn test_content_hash_line_endings() {
        let windows = b"line1\r\nline2\r\n";
        let linux = b"line1\nline2\n";
        let mac = b"line1\rline2\r";

        let hash_win = ContentHash::compute(windows);
        let hash_lin = ContentHash::compute(linux);
        let hash_mac = ContentHash::compute(mac);

        assert_eq!(
            hash_win.0, hash_lin.0,
            "Windows and Linux line endings should produce same hash"
        );
        assert_eq!(
            hash_lin.0, hash_mac.0,
            "Linux and old Mac line endings should produce same hash"
        );
    }

    #[test]
    fn test_doc_id_path_normalization() {
        let path_win = r"src\lib.rs";
        let path_lin = "src/lib.rs";

        let rel_win = RelPath::new(path_win);
        let rel_lin = RelPath::new(path_lin);

        let ch = ContentHash::compute(b"content");

        let id_win = DocIdStr::compute(rel_win.as_str(), 1, 10, &ch);
        let id_lin = DocIdStr::compute(rel_lin.as_str(), 1, 10, &ch);

        assert_eq!(
            id_win.0, id_lin.0,
            "Paths with different separators should produce same doc_id"
        );
    }
}
