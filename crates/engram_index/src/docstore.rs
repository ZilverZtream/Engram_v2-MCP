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
// Delegates to shared `engram_core::validate_key_component` for consistency with the graph store.
fn validate_key_component(value: &str, name: &str) -> anyhow::Result<()> {
    engram_core::validate_key_component(name, value).map_err(|e| anyhow::anyhow!("{e}"))
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
            // DS1: skip corrupt records rather than aborting the entire scan.
            // A single malformed value must not hide all healthy siblings.
            let path = match de_bincode_or_json::<DocRecord>(v.value()) {
                Ok(rec) => rec.path,
                Err(e) => {
                    tracing::warn!(
                        key,
                        "DS1: skipping corrupt DocRecord in list_doc_summaries: {e:#}"
                    );
                    continue;
                }
            };
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
        // M-5 fix: validate and serialise ALL records BEFORE opening the write
        // transaction.  Previously, a bad record at position N caused the whole
        // transaction (including the N-1 valid records already staged) to be
        // rolled back with no way for the caller to identify which record failed
        // or recover the valid prefix.  Separating validation from the write
        // transaction means only genuinely invalid batches are rejected upfront.
        let serialized: Vec<(String, Vec<u8>)> = recs
            .iter()
            .map(|rec| {
                validate_key_component(&rec.namespace, "namespace")?;
                validate_key_component(&rec.doc_id, "doc_id")?;
                let key = format!("{}\0{}\0{}", project_id, rec.namespace, rec.doc_id);
                let val = ser_bincode(rec)?;
                Ok((key, val))
            })
            .collect::<anyhow::Result<_>>()?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(DOC_BY_ID)?;
            for (key, val) in &serialized {
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

    /// All fingerprints for a project in ONE range scan (TODO-46): the
    /// per-call freshness check was list_tracked_paths + N point reads.
    pub fn list_fingerprints(&self, project_id: &str) -> anyhow::Result<Vec<FileFingerprint>> {
        let prefix = format!("{}\0", project_id);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(FILE_FINGERPRINT)?;
        let mut out = Vec::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            out.push(de_bincode_or_json(v.value())?);
        }
        Ok(out)
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

    /// All doc_ids for a project across all namespaces.
    pub fn all_doc_ids_for_project(&self, project_id: &str) -> anyhow::Result<Vec<String>> {
        let prefix = format!("{}\0", project_id);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(DOC_BY_ID)?;
        let mut out = Vec::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, _) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let mut parts = k.value().splitn(3, '\0');
            let _ = parts.next();
            let _ = parts.next();
            let doc_id = parts.next().unwrap_or_default().to_string();
            out.push(doc_id);
        }
        Ok(out)
    }

    /// Count docs per namespace for a project.
    pub fn count_docs_by_namespace(
        &self,
        project_id: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, usize>> {
        let prefix = format!("{}\0", project_id);
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(DOC_BY_ID)?;
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, _) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            let mut parts = k.value().splitn(3, '\0');
            let _ = parts.next();
            let ns = parts.next().unwrap_or_default().to_string();
            *counts.entry(ns).or_insert(0) += 1;
        }
        Ok(counts)
    }

    /// Remove all DocStore data for a project+namespace (used when wiping old data).
    ///
    /// DS3: also clears FILE_FINGERPRINT entries for files that belonged to
    /// this namespace, preventing orphaned fingerprint rows from biasing future
    /// change-detection/copy-forward decisions.
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
        // DS3: collect rel_paths while deleting DOCS_BY_FILE so we can clear
        // the corresponding FILE_FINGERPRINT rows in the same transaction.
        let mut rel_paths: Vec<String> = Vec::new();
        {
            let mut t = wtx.open_table(DOCS_BY_FILE)?;
            let mut keys: Vec<String> = Vec::new();
            for r in t.range(prefix_doc.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix_doc) {
                    break;
                }
                // Extract the rel_path portion after "project_id\0namespace\0"
                let rel = k.value()[prefix_doc.len()..].to_string();
                rel_paths.push(rel);
                keys.push(k.value().to_string());
            }
            for k in keys {
                t.remove(k.as_str())?;
            }
        }
        // DS3: purge orphaned FILE_FINGERPRINT rows for every path that was in
        // this namespace.  A file shared across multiple namespaces will have
        // its fingerprint re-computed on the next index run — this is safe
        // (conservative) and prevents unbounded fingerprint accumulation.
        {
            let mut t = wtx.open_table(FILE_FINGERPRINT)?;
            for rel in &rel_paths {
                let fp_key = format!("{}\0{}", project_id, rel);
                t.remove(fp_key.as_str())?;
            }
        }
        wtx.commit()?;
        Ok(())
    }

    /// DS1: remove all DocRecords for a project+namespace whose `generation` is
    /// strictly less than `min_generation`, returning the count of deleted records.
    ///
    /// Old records accumulate as orphans when incremental indexing emits
    /// newer-generation docs without cleaning up stale ones.  Call this after
    /// advancing the active generation to keep storage bounded.
    pub fn purge_old_generation_docs(
        &self,
        project_id: &str,
        namespace: &str,
        min_generation: u64,
    ) -> anyhow::Result<usize> {
        validate_key_component(project_id, "project_id")?;
        validate_key_component(namespace, "namespace")?;
        let prefix = format!("{}\0{}\0", project_id, namespace);
        let wtx = self.db.begin_write()?;
        let removed = {
            let mut doc_table = wtx.open_table(DOC_BY_ID)?;
            let mut file_table = wtx.open_table(DOCS_BY_FILE)?;

            // Collect stale doc keys and, for DS1-w3f8, the (rel_path → doc_ids) pairs
            // needed to reconcile DOCS_BY_FILE.  Both tables are updated atomically in
            // the same write transaction so there is no window where DOC_BY_ID has been
            // pruned but DOCS_BY_FILE still holds references to the removed doc_ids.
            let mut stale_keys: Vec<String> = Vec::new();
            // Map from rel_path → list of stale doc_ids for that path.
            let mut stale_by_path: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();

            for r in doc_table.range(prefix.as_str()..)? {
                let (k, v) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                let rec: DocRecord = de_bincode_or_json(v.value())?;
                if rec.generation < min_generation {
                    stale_keys.push(k.value().to_string());
                    stale_by_path
                        .entry(rec.path.clone())
                        .or_default()
                        .push(rec.doc_id.clone());
                }
            }

            let count = stale_keys.len();

            // Remove stale docs from DOC_BY_ID.
            for k in &stale_keys {
                doc_table.remove(k.as_str())?;
            }

            // DS1-w3f8: reconcile DOCS_BY_FILE — for each affected rel_path, remove
            // the stale doc_ids from the newline-separated mapping.  Delete the entry
            // entirely when no live doc_ids remain, preventing orphaned file→doc links
            // from accumulating and skewing copy-forward / readback behaviors.
            for (rel_path, stale_ids) in &stale_by_path {
                let file_key = format!("{}\0{}\0{}", project_id, namespace, rel_path);
                // Copy the current value out of the AccessGuard before mutating the table —
                // redb's borrow rules forbid holding a read guard while also taking a mutable
                // reference to the same table handle.
                let current_bytes: Option<Vec<u8>> = file_table
                    .get(file_key.as_str())?
                    .map(|g| g.value().to_vec());
                if let Some(bytes) = current_bytes {
                    let live_ids: Vec<&str> = std::str::from_utf8(&bytes)
                        .unwrap_or("")
                        .lines()
                        .filter(|id| !id.is_empty() && !stale_ids.iter().any(|s| s.as_str() == *id))
                        .collect();
                    if live_ids.is_empty() {
                        file_table.remove(file_key.as_str())?;
                    } else {
                        let updated = live_ids.join("\n");
                        file_table.insert(file_key.as_str(), updated.as_bytes())?;
                    }
                }
            }

            count
        };
        wtx.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn open_temp_store() -> (tempfile::TempDir, DocStore) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("docs.redb");
        let store = DocStore::open(&db_path).expect("open DocStore");
        (dir, store)
    }

    fn make_doc(doc_id: &str, path: &str, namespace: &str) -> DocRecord {
        DocRecord {
            doc_id: doc_id.to_string(),
            path: path.to_string(),
            start_line: 1,
            end_line: 10,
            language: "rust".to_string(),
            content: format!("// content for {}", doc_id),
            content_hash: format!("hash_{}", doc_id),
            namespace: namespace.to_string(),
            generation: 1,
        }
    }

    // ── 1. open_creates_tables ────────────────────────────────────────────────

    #[test]
    fn open_creates_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("fresh.redb");
        let store = DocStore::open(&db_path);
        assert!(store.is_ok(), "DocStore::open must succeed on a fresh path");
    }

    // ── 2. upsert_and_get_doc_roundtrip ──────────────────────────────────────

    #[test]
    fn upsert_and_get_doc_roundtrip() {
        let (_dir, store) = open_temp_store();
        let rec = make_doc("doc_001", "src/main.rs", "code");
        store.put_doc("proj1", &rec).expect("put_doc");

        let got = store.get_doc("proj1", "code", "doc_001").expect("get_doc");
        assert!(got.is_some(), "get_doc must return the stored record");
        let got = got.unwrap();
        assert_eq!(got.doc_id, "doc_001");
        assert_eq!(got.path, "src/main.rs");
        assert_eq!(got.namespace, "code");
        assert_eq!(got.generation, 1);
        assert!(got.content.contains("doc_001"));
    }

    // ── 3. upsert_same_doc_is_idempotent ─────────────────────────────────────

    #[test]
    fn upsert_same_doc_is_idempotent() {
        let (_dir, store) = open_temp_store();
        let rec = make_doc("doc_idem", "src/lib.rs", "code");
        store.put_doc("proj1", &rec).expect("first put");
        // Second put with same id must not fail
        store
            .put_doc("proj1", &rec)
            .expect("second put (idempotent)");

        let got = store.get_doc("proj1", "code", "doc_idem").unwrap().unwrap();
        assert_eq!(got.doc_id, "doc_idem");
    }

    // ── 4. get_nonexistent_doc_returns_none ──────────────────────────────────

    #[test]
    fn get_nonexistent_doc_returns_none() {
        let (_dir, store) = open_temp_store();
        let got = store.get_doc("proj1", "code", "no_such_doc").unwrap();
        assert!(got.is_none(), "unknown doc_id must return None");
    }

    // ── 5. get_docs_for_file_returns_all ─────────────────────────────────────

    #[test]
    fn get_docs_for_file_returns_all() {
        let (_dir, store) = open_temp_store();
        let doc_ids: Vec<String> = vec!["d1".to_string(), "d2".to_string(), "d3".to_string()];
        store
            .set_docs_for_file("proj1", "code", "src/foo.rs", &doc_ids)
            .expect("set_docs_for_file");

        let result = store
            .get_docs_for_file("proj1", "code", "src/foo.rs")
            .unwrap();
        assert_eq!(
            result, doc_ids,
            "get_docs_for_file must return all stored ids in order"
        );
    }

    // ── 6. get_docs_for_file_empty_when_none ─────────────────────────────────

    #[test]
    fn get_docs_for_file_empty_when_none() {
        let (_dir, store) = open_temp_store();
        let result = store
            .get_docs_for_file("proj1", "code", "src/not_indexed.rs")
            .unwrap();
        assert!(result.is_empty(), "unindexed file must return empty vec");
    }

    // ── 7. upsert_fingerprint_and_get_roundtrip ──────────────────────────────

    #[test]
    fn upsert_fingerprint_and_get_roundtrip() {
        let (_dir, store) = open_temp_store();
        let fp = FileFingerprint {
            rel_path: "src/main.rs".to_string(),
            size: 1024,
            mtime_ms: 1_700_000_000_000,
            file_hash: "abc123def456".to_string(),
        };
        store
            .set_fingerprint("proj1", &fp)
            .expect("set_fingerprint");

        let got = store
            .get_fingerprint("proj1", "src/main.rs")
            .unwrap()
            .expect("fingerprint must be present");
        assert_eq!(got.rel_path, "src/main.rs");
        assert_eq!(got.size, 1024);
        assert_eq!(got.mtime_ms, 1_700_000_000_000);
        assert_eq!(got.file_hash, "abc123def456");
    }

    // ── 8. get_fingerprint_missing_returns_none ───────────────────────────────

    #[test]
    fn get_fingerprint_missing_returns_none() {
        let (_dir, store) = open_temp_store();
        let got = store.get_fingerprint("proj1", "src/missing.rs").unwrap();
        assert!(got.is_none(), "missing fingerprint must return None");
    }

    // ── 8b. list_fingerprints batch scan (TODO-46) ───────────────────────────

    #[test]
    fn list_fingerprints_returns_only_project_scoped() {
        let (_dir, store) = open_temp_store();
        let fps_a = [
            FileFingerprint {
                rel_path: "src/a.rs".into(),
                size: 1,
                mtime_ms: 10,
                file_hash: "ha".into(),
            },
            FileFingerprint {
                rel_path: "src/b.rs".into(),
                size: 2,
                mtime_ms: 20,
                file_hash: "hb".into(),
            },
        ];
        let fps_other = [FileFingerprint {
            rel_path: "src/c.rs".into(),
            size: 3,
            mtime_ms: 30,
            file_hash: "hc".into(),
        }];
        store.set_fingerprints("proj1", &fps_a).unwrap();
        store.set_fingerprints("proj2", &fps_other).unwrap();

        let mut got = store.list_fingerprints("proj1").unwrap();
        got.sort_by(|x, y| x.rel_path.cmp(&y.rel_path));
        assert_eq!(got.len(), 2, "only proj1 fingerprints, not proj2's");
        assert_eq!(got[0].rel_path, "src/a.rs");
        assert_eq!(got[1].rel_path, "src/b.rs");
        // A project with no fingerprints yields an empty list.
        assert!(store.list_fingerprints("proj3").unwrap().is_empty());
    }

    // ── 9. count_docs_by_namespace ────────────────────────────────────────────

    #[test]
    fn count_docs_by_namespace() {
        let (_dir, store) = open_temp_store();
        // 3 docs in "code", 2 in "memory"
        for i in 0..3 {
            let rec = make_doc(
                &format!("code_doc_{}", i),
                &format!("src/f{}.rs", i),
                "code",
            );
            store.put_doc("proj_ns", &rec).unwrap();
        }
        for i in 0..2 {
            let rec = make_doc(
                &format!("mem_doc_{}", i),
                &format!("notes/n{}.md", i),
                "memory",
            );
            store.put_doc("proj_ns", &rec).unwrap();
        }

        let counts = store.count_docs_by_namespace("proj_ns").unwrap();
        assert_eq!(
            counts.get("code").copied().unwrap_or(0),
            3,
            "must count 3 docs in namespace 'code'"
        );
        assert_eq!(
            counts.get("memory").copied().unwrap_or(0),
            2,
            "must count 2 docs in namespace 'memory'"
        );
    }

    // ── 10. count_docs_by_namespace_empty_returns_empty ──────────────────────

    #[test]
    fn count_docs_by_namespace_empty_returns_empty() {
        let (_dir, store) = open_temp_store();
        let counts = store.count_docs_by_namespace("empty_proj").unwrap();
        assert!(counts.is_empty(), "no docs → counts must be empty map");
    }

    // ── 11. list_doc_summaries_returns_all ────────────────────────────────────

    #[test]
    fn list_doc_summaries_returns_all() {
        let (_dir, store) = open_temp_store();
        let recs = vec![
            make_doc("s_doc_1", "src/a.rs", "code"),
            make_doc("s_doc_2", "src/b.rs", "code"),
            make_doc("s_doc_3", "notes/c.md", "memory"),
        ];
        store.put_docs("proj_sum", &recs).unwrap();

        let summaries = store.list_doc_summaries_for_project("proj_sum").unwrap();
        assert_eq!(
            summaries.len(),
            3,
            "must return summary for each stored doc"
        );

        // Every summary must have non-empty doc_id and path
        for s in &summaries {
            assert!(!s.doc_id.is_empty(), "summary doc_id must not be empty");
            assert!(!s.path.is_empty(), "summary path must not be empty");
            assert!(
                !s.namespace.is_empty(),
                "summary namespace must not be empty"
            );
        }

        // All 3 doc_ids must be present
        let ids: Vec<&str> = summaries.iter().map(|s| s.doc_id.as_str()).collect();
        assert!(ids.contains(&"s_doc_1"));
        assert!(ids.contains(&"s_doc_2"));
        assert!(ids.contains(&"s_doc_3"));
    }

    // ── 12. all_doc_ids_for_project ───────────────────────────────────────────

    #[test]
    fn all_doc_ids_for_project() {
        let (_dir, store) = open_temp_store();
        let recs: Vec<DocRecord> = (0..4)
            .map(|i| make_doc(&format!("id_{}", i), &format!("src/f{}.rs", i), "code"))
            .collect();
        store.put_docs("proj_ids", &recs).unwrap();

        let ids = store.all_doc_ids_for_project("proj_ids").unwrap();
        assert_eq!(ids.len(), 4);
        for i in 0..4 {
            assert!(ids.contains(&format!("id_{}", i)), "id_{} missing", i);
        }
    }

    // ── 13. different_projects_isolated ──────────────────────────────────────

    #[test]
    fn different_projects_isolated() {
        let (_dir, store) = open_temp_store();
        let rec_a = make_doc("shared_id", "src/lib.rs", "code");
        let rec_b = make_doc("shared_id", "src/lib.rs", "code");
        store.put_doc("project_A", &rec_a).unwrap();
        store.put_doc("project_B", &rec_b).unwrap();

        // Retrieving from project_A must not return project_B's doc (same key, different project)
        let ids_a = store.all_doc_ids_for_project("project_A").unwrap();
        let ids_b = store.all_doc_ids_for_project("project_B").unwrap();
        // Both should have exactly 1 entry and project_A must not appear in project_B's list
        assert_eq!(ids_a.len(), 1, "project_A must have exactly 1 doc");
        assert_eq!(ids_b.len(), 1, "project_B must have exactly 1 doc");

        // Docs for project_C (never written) must be empty
        let ids_c = store.all_doc_ids_for_project("project_C").unwrap();
        assert!(ids_c.is_empty(), "project_C must have no docs");
    }

    // ── 14. different_namespaces_isolated ────────────────────────────────────

    #[test]
    fn different_namespaces_isolated() {
        let (_dir, store) = open_temp_store();
        let doc_ids_code = vec!["doc_code_1".to_string()];
        let doc_ids_mem = vec!["doc_mem_1".to_string()];
        store
            .set_docs_for_file("proj_ns_iso", "code", "src/shared.rs", &doc_ids_code)
            .unwrap();
        store
            .set_docs_for_file("proj_ns_iso", "memory", "src/shared.rs", &doc_ids_mem)
            .unwrap();

        let result_code = store
            .get_docs_for_file("proj_ns_iso", "code", "src/shared.rs")
            .unwrap();
        let result_mem = store
            .get_docs_for_file("proj_ns_iso", "memory", "src/shared.rs")
            .unwrap();

        assert_eq!(
            result_code, doc_ids_code,
            "code namespace must return only code docs"
        );
        assert_eq!(
            result_mem, doc_ids_mem,
            "memory namespace must return only memory docs"
        );

        // Wrong namespace → empty
        let result_wrong = store
            .get_docs_for_file("proj_ns_iso", "other", "src/shared.rs")
            .unwrap();
        assert!(
            result_wrong.is_empty(),
            "wrong namespace must return empty vec"
        );
    }

    // ── 15. doc_generation_stored_correctly ──────────────────────────────────

    #[test]
    fn doc_generation_stored_correctly() {
        let (_dir, store) = open_temp_store();
        let mut rec = make_doc("gen_doc", "src/gen.rs", "code");
        rec.generation = 42;
        store.put_doc("proj_gen", &rec).unwrap();

        let got = store
            .get_doc("proj_gen", "code", "gen_doc")
            .unwrap()
            .unwrap();
        assert_eq!(
            got.generation, 42,
            "generation field must round-trip correctly"
        );
    }

    // ── 16. null_byte_in_doc_id_rejected ─────────────────────────────────────

    #[test]
    fn null_byte_in_doc_id_rejected() {
        let (_dir, store) = open_temp_store();
        let mut rec = make_doc("bad\0id", "src/x.rs", "code");
        rec.doc_id = "bad\0id".to_string();
        let result = store.put_doc("proj1", &rec);
        assert!(result.is_err(), "doc_id with NUL byte must be rejected");
    }

    // ── 17. null_byte_in_namespace_rejected ──────────────────────────────────

    #[test]
    fn null_byte_in_namespace_rejected() {
        let (_dir, store) = open_temp_store();
        let mut rec = make_doc("doc_nns", "src/x.rs", "bad\0ns");
        rec.namespace = "bad\0ns".to_string();
        let result = store.put_doc("proj1", &rec);
        assert!(result.is_err(), "namespace with NUL byte must be rejected");
    }

    // ── 18. null_byte_in_project_id_rejected ─────────────────────────────────

    #[test]
    fn null_byte_in_project_id_rejected() {
        let (_dir, store) = open_temp_store();
        let rec = make_doc("doc_np", "src/x.rs", "code");
        let result = store.put_doc("bad\0proj", &rec);
        assert!(result.is_err(), "project_id with NUL byte must be rejected");
    }

    // ── 19. large_content_stored_correctly ───────────────────────────────────

    #[test]
    fn large_content_stored_correctly() {
        let (_dir, store) = open_temp_store();
        let large_content = "A".repeat(10_240); // 10 KB
        let mut rec = make_doc("large_doc", "src/big.rs", "code");
        rec.content = large_content.clone();
        store.put_doc("proj_large", &rec).unwrap();

        let got = store
            .get_doc("proj_large", "code", "large_doc")
            .unwrap()
            .unwrap();
        assert_eq!(
            got.content.len(),
            10_240,
            "large content must be stored and retrieved intact"
        );
        assert_eq!(got.content, large_content);
    }

    // ── 20. multiple_files_for_project ───────────────────────────────────────

    #[test]
    fn multiple_files_for_project() {
        let (_dir, store) = open_temp_store();
        let recs: Vec<DocRecord> = (0..5)
            .map(|i| {
                make_doc(
                    &format!("mf_doc_{}", i),
                    &format!("src/file{}.rs", i),
                    "code",
                )
            })
            .collect();
        store.put_docs("proj_mf", &recs).unwrap();

        let summaries = store.list_doc_summaries_for_project("proj_mf").unwrap();
        assert_eq!(
            summaries.len(),
            5,
            "must return summaries for all 5 files, got {}",
            summaries.len()
        );

        // Each file path must appear
        for i in 0..5 {
            let expected_path = format!("src/file{}.rs", i);
            let found = summaries.iter().any(|s| s.path == expected_path);
            assert!(found, "path {} missing from summaries", expected_path);
        }
    }
}
