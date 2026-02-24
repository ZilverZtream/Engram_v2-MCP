//! Durable checkpoint store for crash-safe job orchestration.
//!
//! Stores job progress checkpoints in Redb so that indexing/update jobs
//! can resume from the last successful checkpoint after a crash.
//! Each checkpoint records the job_id, phase, last processed item,
//! and an opaque state blob for phase-specific resume data.

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

static CHECKPOINTS: TableDefinition<&str, &[u8]> = TableDefinition::new("checkpoints");

/// Phases of an indexing job, used to track resume points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobPhase {
    /// Scanning filesystem for changed files.
    Scanning,
    /// Parsing and chunking files.
    Parsing,
    /// Indexing chunks into Tantivy.
    TantivyIndexing,
    /// Indexing vectors into LanceDB.
    VectorIndexing,
    /// Building graph nodes and edges.
    GraphBuilding,
    /// Post-processing (linking, enrichment).
    PostProcessing,
    /// Completed successfully.
    Completed,
    /// Failed with error.
    Failed,
}

impl std::fmt::Display for JobPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Scanning => write!(f, "scanning"),
            Self::Parsing => write!(f, "parsing"),
            Self::TantivyIndexing => write!(f, "tantivy_indexing"),
            Self::VectorIndexing => write!(f, "vector_indexing"),
            Self::GraphBuilding => write!(f, "graph_building"),
            Self::PostProcessing => write!(f, "post_processing"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// A durable checkpoint for a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique job identifier.
    pub job_id: String,
    /// Project this job belongs to.
    pub project_id: String,
    /// Current phase of the job.
    pub phase: JobPhase,
    /// Number of items processed so far in the current phase.
    pub items_processed: u64,
    /// Total items expected in the current phase (0 if unknown).
    pub items_total: u64,
    /// Generation being built.
    pub generation: u64,
    /// Idempotency key: blake3 hash of (project_id, directory, generation).
    /// Prevents duplicate work if the same job is submitted twice.
    pub idempotency_key: String,
    /// Opaque phase-specific state for resume (e.g., list of remaining files).
    /// Serialized as JSON string.
    pub resume_state: Option<String>,
    /// Timestamp when this checkpoint was written.
    pub updated_at_ms: u64,
    /// Error message if phase == Failed.
    pub error: Option<String>,
}

impl Checkpoint {
    /// Compute an idempotency key from project + directory + generation.
    pub fn compute_idempotency_key(project_id: &str, directory: &str, generation: u64) -> String {
        let input = format!("{project_id}\0{directory}\0{generation}");
        blake3::hash(input.as_bytes()).to_hex().to_string()
    }

    /// Check if this checkpoint is resumable (not completed or failed).
    pub fn is_resumable(&self) -> bool {
        !matches!(self.phase, JobPhase::Completed | JobPhase::Failed)
    }
}

/// Redb-backed checkpoint store.
#[derive(Clone)]
pub struct CheckpointStore {
    db: Arc<Database>,
}

impl CheckpointStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let wtx = db.begin_write()?;
        {
            let _ = wtx.open_table(CHECKPOINTS)?;
        }
        wtx.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Write or update a checkpoint atomically.
    pub fn put(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(cp)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(CHECKPOINTS)?;
            t.insert(cp.job_id.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        crate::metrics::metrics().checkpoints_written.inc();
        Ok(())
    }

    /// Retrieve a checkpoint by job_id.
    pub fn get(&self, job_id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(CHECKPOINTS)?;
        if let Some(v) = t.get(job_id)? {
            let cp: Checkpoint = serde_json::from_slice(v.value())?;
            Ok(Some(cp))
        } else {
            Ok(None)
        }
    }

    /// Find the most recent resumable checkpoint for a project.
    pub fn find_resumable(&self, project_id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(CHECKPOINTS)?;
        let mut best: Option<Checkpoint> = None;
        for r in t.iter()? {
            let (_k, v) = r?;
            let cp: Checkpoint = serde_json::from_slice(v.value())?;
            if cp.project_id == project_id && cp.is_resumable()
                && best
                    .as_ref()
                    .map(|b| cp.updated_at_ms > b.updated_at_ms)
                    .unwrap_or(true)
                {
                    best = Some(cp);
                }
        }
        Ok(best)
    }

    /// Find a checkpoint by idempotency key (prevents duplicate jobs).
    pub fn find_by_idempotency_key(&self, key: &str) -> anyhow::Result<Option<Checkpoint>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(CHECKPOINTS)?;
        for r in t.iter()? {
            let (_k, v) = r?;
            let cp: Checkpoint = serde_json::from_slice(v.value())?;
            if cp.idempotency_key == key {
                return Ok(Some(cp));
            }
        }
        Ok(None)
    }

    /// Remove a checkpoint (after successful completion or cleanup).
    pub fn remove(&self, job_id: &str) -> anyhow::Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(CHECKPOINTS)?;
            t.remove(job_id)?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// List all checkpoints (for diagnostics).
    pub fn list_all(&self) -> anyhow::Result<Vec<Checkpoint>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(CHECKPOINTS)?;
        let mut out: Vec<Checkpoint> = Vec::new();
        for r in t.iter()? {
            let (_k, v) = r?;
            out.push(serde_json::from_slice(v.value())?);
        }
        out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        Ok(out)
    }

    /// Clean up old completed/failed checkpoints older than `max_age_ms`.
    pub fn cleanup_old(&self, max_age_ms: u64) -> anyhow::Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        let cutoff = now.saturating_sub(max_age_ms);

        let wtx = self.db.begin_write()?;
        let mut count = 0;
        {
            let mut t = wtx.open_table(CHECKPOINTS)?;
            let mut to_remove = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                let cp: Checkpoint = serde_json::from_slice(v.value())?;
                if matches!(cp.phase, JobPhase::Completed | JobPhase::Failed)
                    && cp.updated_at_ms < cutoff
                {
                    to_remove.push(k.value().to_string());
                }
            }
            for k in to_remove {
                t.remove(k.as_str())?;
                count += 1;
            }
        }
        wtx.commit()?;
        Ok(count)
    }
}

/// Resume state for the parsing phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseResumeState {
    /// Files already successfully parsed (relative paths).
    pub completed_files: Vec<String>,
    /// Total file count for this phase.
    pub total_files: u64,
}

/// Resume state for the vector indexing phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorResumeState {
    /// Chunk IDs already indexed into LanceDB.
    pub indexed_chunk_ids: Vec<u64>,
    /// Total chunks for this phase.
    pub total_chunks: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn checkpoint_round_trip() {
        let tmp = tempdir().unwrap();
        let store = CheckpointStore::open(&tmp.path().join("cp.redb")).unwrap();

        let cp = Checkpoint {
            job_id: "job-1".into(),
            project_id: "proj-1".into(),
            phase: JobPhase::Parsing,
            items_processed: 42,
            items_total: 100,
            generation: 3,
            idempotency_key: Checkpoint::compute_idempotency_key("proj-1", "/some/dir", 3),
            resume_state: Some(
                serde_json::to_string(&ParseResumeState {
                    completed_files: vec!["a.rs".into(), "b.rs".into()],
                    total_files: 100,
                })
                .unwrap(),
            ),
            updated_at_ms: 1234567890,
            error: None,
        };

        store.put(&cp).unwrap();
        let loaded = store.get("job-1").unwrap().unwrap();
        assert_eq!(loaded.project_id, "proj-1");
        assert_eq!(loaded.items_processed, 42);
        assert!(loaded.is_resumable());
    }

    #[test]
    fn find_resumable_ignores_completed() {
        let tmp = tempdir().unwrap();
        let store = CheckpointStore::open(&tmp.path().join("cp.redb")).unwrap();

        let cp_done = Checkpoint {
            job_id: "j1".into(),
            project_id: "p1".into(),
            phase: JobPhase::Completed,
            items_processed: 100,
            items_total: 100,
            generation: 1,
            idempotency_key: "k1".into(),
            resume_state: None,
            updated_at_ms: 1000,
            error: None,
        };
        let cp_active = Checkpoint {
            job_id: "j2".into(),
            project_id: "p1".into(),
            phase: JobPhase::VectorIndexing,
            items_processed: 50,
            items_total: 200,
            generation: 2,
            idempotency_key: "k2".into(),
            resume_state: None,
            updated_at_ms: 2000,
            error: None,
        };

        store.put(&cp_done).unwrap();
        store.put(&cp_active).unwrap();

        let found = store.find_resumable("p1").unwrap().unwrap();
        assert_eq!(found.job_id, "j2");
    }

    #[test]
    fn idempotency_key_prevents_duplicates() {
        let k1 = Checkpoint::compute_idempotency_key("p1", "/dir", 1);
        let k2 = Checkpoint::compute_idempotency_key("p1", "/dir", 1);
        let k3 = Checkpoint::compute_idempotency_key("p1", "/dir", 2);
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }
}
