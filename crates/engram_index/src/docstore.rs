//! Redb-backed persistent store for chunk content and file fingerprints.
//!
//! Lives per-project at `{data_dir}/projects/{project_id}/docs.redb`.
//! Enables copy-forward indexing: unchanged files can be re-emitted into new
//! generations without re-reading from disk.

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Serialize a value to bincode (compact binary, ~3-5x smaller than JSON for
/// typical DocRecords). Falls back gracefully on deserialization — if bincode
/// decode fails we try JSON, enabling rolling migration from the previous
/// JSON-only format without a manual data wipe.
fn ser_bincode<T: Serialize>(val: &T) -> anyhow::Result<Vec<u8>> {
    bincode::serialize(val).map_err(|e| anyhow::anyhow!("bincode serialize: {e}"))
}

fn de_bincode_or_json<T: serde::de::DeserializeOwned>(data: &[u8]) -> anyhow::Result<T> {
    // Fast path: bincode (new format)
    if let Ok(v) = bincode::deserialize::<T>(data) {
        return Ok(v);
    }
    // Fallback: JSON (legacy format from before Phase 20)
    serde_json::from_slice(data).map_err(|e| anyhow::anyhow!("deserialize: {e}"))
}

// Fix #4: key components must not contain the delimiter bytes used for
// composite key construction ('\0' for field separator, '\n' for list items).
// Reject such values early so they can never silently corrupt the database.
fn validate_key_component(value: &str, name: &str) -> anyhow::Result<()> {
    if value.contains('\0') || value.contains('\n') {
        anyhow::bail!(
            "DocStore key component `{name}` must not contain '\\0' or '\\n' (got {:?})",
            value
        );
    }
    Ok(())
}

// ---- Table definitions ----

/// key = "{project_id}\0{namespace}\0{doc_id}"
/// value = DocRecord as JSON
static DOC_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("doc_by_id");

/// key = "{project_id}\0{namespace}\0{rel_path}"
/// value = newline-separated list of doc_ids in order
static DOCS_BY_FILE: TableDefinition<&str, &[u8]> = TableDefinition::new("docs_by_file");

/// key = "{project_id}\0{rel_path}"
/// value = FileFingerprint as JSON
static FILE_FINGERPRINT: TableDefinition<&str, &[u8]> = TableDefinition::new("file_fingerprint");

// ---- Data types ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocRecord {
    pub doc_id: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub language: String,
    pub content: String,
    pub content_hash: String,
    pub namespace: String,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct DocSummary {
    pub namespace: String,
    pub doc_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub rel_path: String,
    /// File size in bytes.
    pub size: u64,
    /// Last-modified time as unix ms.
    pub mtime_ms: u64,
    /// blake3 hex hash of file content.
    pub file_hash: String,
}

// ---- DocStore ----

#[derive(Clone)]
pub struct DocStore {
    db: Arc<Database>,
}

impl DocStore {
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(db_path)?;
        let wtx = db.begin_write()?;
        {
            let _ = wtx.open_table(DOC_BY_ID)?;
            let _ = wtx.open_table(DOCS_BY_FILE)?;
            let _ = wtx.open_table(FILE_FINGERPRINT)?;
        }
        wtx.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Count all docs for a project across namespaces.
    pub fn count_docs_for_project(&self, project_id: &str) -> anyhow::Result<usize> {
        let prefix = format!("{}\0", project_id);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(DOC_BY_ID)?;
        let mut count = 0usize;
        for r in t.range(prefix.as_str()..)? {
            let (k, _) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    /// Return lightweight per-doc metadata for a project.
    pub fn list_doc_summaries_for_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Vec<DocSummary>> {
        let prefix = format!("{}\0", project_id);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(DOC_BY_ID)?;
        let mut out = Vec::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, v) = r?;
            let key = k.value();
            if !key.starts_with(&prefix) {
                break;
            }

            let mut parts = key.splitn(3, '\0');
            let _ = parts.next();
            let namespace = parts.next().unwrap_or_default().to_string();
            let doc_id = parts.next().unwrap_or_default().to_string();
            let path = de_bincode_or_json::<DocRecord>(v.value())?.path;
            out.push(DocSummary {
                namespace,
                doc_id,
                path,
            });
        }
        Ok(out)
    }

    /// Persist a DocRecord.
    pub fn put_doc(&self, project_id: &str, rec: &DocRecord) -> anyhow::Result<()> {
        validate_key_component(project_id, "project_id")?;
        validate_key_component(&rec.namespace, "namespace")?;
        validate_key_component(&rec.doc_id, "doc_id")?;
        let key = format!("{}\0{}\0{}", project_id, rec.namespace, rec.doc_id);
        let val = ser_bincode(rec)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(DOC_BY_ID)?;
            t.insert(key.as_str(), val.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Persist a batch of DocRecords in one transaction.
    pub fn put_docs(&self, project_id: &str, recs: &[DocRecord]) -> anyhow::Result<()> {
        if recs.is_empty() {
            return Ok(());
        }
        validate_key_component(project_id, "project_id")?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(DOC_BY_ID)?;
            for rec in recs {
                validate_key_component(&rec.namespace, "namespace")?;
                validate_key_component(&rec.doc_id, "doc_id")?;
                let key = format!("{}\0{}\0{}", project_id, rec.namespace, rec.doc_id);
                let val = ser_bincode(rec)?;
                t.insert(key.as_str(), val.as_slice())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    /// Retrieve a DocRecord by doc_id.
    pub fn get_doc(
        &self,
        project_id: &str,
        namespace: &str,
        doc_id: &str,
    ) -> anyhow::Result<Option<DocRecord>> {
        let key = format!("{}\0{}\0{}", project_id, namespace, doc_id);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(DOC_BY_ID)?;
        let Some(v) = t.get(key.as_str())? else {
            return Ok(None);
        };
        Ok(Some(de_bincode_or_json(v.value())?))
    }

    /// Update the file-to-docs mapping for a given file.
    pub fn set_docs_for_file(
        &self,
        project_id: &str,
        namespace: &str,
        rel_path: &str,
        doc_ids: &[String],
    ) -> anyhow::Result<()> {
        validate_key_component(project_id, "project_id")?;
        validate_key_component(namespace, "namespace")?;
        validate_key_component(rel_path, "rel_path")?;
        for did in doc_ids {
            validate_key_component(did, "doc_id")?;
        }
        let key = format!("{}\0{}\0{}", project_id, namespace, rel_path);
        let val = doc_ids.join("\n").into_bytes();
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(DOCS_BY_FILE)?;
            t.insert(key.as_str(), val.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Retrieve doc_ids for a file.
    pub fn get_docs_for_file(
        &self,
        project_id: &str,
        namespace: &str,
        rel_path: &str,
    ) -> anyhow::Result<Vec<String>> {
        let key = format!("{}\0{}\0{}", project_id, namespace, rel_path);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(DOCS_BY_FILE)?;
        let Some(v) = t.get(key.as_str())? else {
            return Ok(vec![]);
        };
        let raw = String::from_utf8_lossy(v.value()).to_string();
        if raw.is_empty() {
            return Ok(vec![]);
        }
        Ok(raw.split('\n').map(|s| s.to_string()).collect())
    }

    /// Retrieve all DocRecords for a file (ordered).
    pub fn get_all_docs_for_file(
        &self,
        project_id: &str,
        namespace: &str,
        rel_path: &str,
    ) -> anyhow::Result<Vec<DocRecord>> {
        let doc_ids = self.get_docs_for_file(project_id, namespace, rel_path)?;
        let mut out = Vec::with_capacity(doc_ids.len());
        for did in &doc_ids {
            if let Some(rec) = self.get_doc(project_id, namespace, did)? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Store or update a file fingerprint.
    pub fn set_fingerprint(&self, project_id: &str, fp: &FileFingerprint) -> anyhow::Result<()> {
        validate_key_component(project_id, "project_id")?;
        validate_key_component(&fp.rel_path, "rel_path")?;
        let key = format!("{}\0{}", project_id, fp.rel_path);
        let val = ser_bincode(fp)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(FILE_FINGERPRINT)?;
            t.insert(key.as_str(), val.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Retrieve a file fingerprint.
    pub fn get_fingerprint(
        &self,
        project_id: &str,
        rel_path: &str,
    ) -> anyhow::Result<Option<FileFingerprint>> {
        let key = format!("{}\0{}", project_id, rel_path);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(FILE_FINGERPRINT)?;
        let Some(v) = t.get(key.as_str())? else {
            return Ok(None);
        };
        Ok(Some(de_bincode_or_json(v.value())?))
    }

    /// Batch-store fingerprints.
    pub fn set_fingerprints(
        &self,
        project_id: &str,
        fps: &[FileFingerprint],
    ) -> anyhow::Result<()> {
        if fps.is_empty() {
            return Ok(());
        }
        validate_key_component(project_id, "project_id")?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(FILE_FINGERPRINT)?;
            for fp in fps {
                validate_key_component(&fp.rel_path, "rel_path")?;
                let key = format!("{}\0{}", project_id, fp.rel_path);
                let val = ser_bincode(fp)?;
                t.insert(key.as_str(), val.as_slice())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    /// List all tracked rel_paths for a (project, namespace).
    pub fn list_tracked_paths(
        &self,
        project_id: &str,
        namespace: &str,
    ) -> anyhow::Result<Vec<String>> {
        let prefix = format!("{}\0{}\0", project_id, namespace);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(DOCS_BY_FILE)?;
        let mut out = Vec::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, _) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let path = k.value()[prefix.len()..].to_string();
            out.push(path);
        }
        Ok(out)
    }

    /// Remove all DocStore data for a project+namespace (used when wiping old data).
    pub fn delete_namespace(&self, project_id: &str, namespace: &str) -> anyhow::Result<()> {
        let prefix_doc = format!("{}\0{}\0", project_id, namespace);
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(DOC_BY_ID)?;
            let mut keys: Vec<String> = Vec::new();
            for r in t.range(prefix_doc.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix_doc) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                t.remove(k.as_str())?;
            }
        }
        {
            let mut t = wtx.open_table(DOCS_BY_FILE)?;
            let mut keys: Vec<String> = Vec::new();
            for r in t.range(prefix_doc.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix_doc) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                t.remove(k.as_str())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }
}
