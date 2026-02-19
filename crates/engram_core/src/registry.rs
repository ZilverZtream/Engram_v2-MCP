use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// Registry tables store JSON blobs keyed by string keys.
///
/// Keying scheme:
/// - projects: "{project_id}"
/// - memory_bank: "{project_id}\0{section_id}"
/// - repo_rules: "{project_id}\0{rule_id}"
/// - watches: "{project_id}\0{watch_id}"
/// - jobs: "{job_id}"
/// - meta: "{project_id}\0{key}"
static PROJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("projects");
static MEMORY_BANK: TableDefinition<&str, &[u8]> = TableDefinition::new("memory_bank");
static REPO_RULES: TableDefinition<&str, &[u8]> = TableDefinition::new("repo_rules");
static WATCHES: TableDefinition<&str, &[u8]> = TableDefinition::new("watches");
static JOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("jobs");
static META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub project_id: String,
    pub project_name: String,
    pub project_type: String,
    pub directory: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySection {
    pub section_id: String,
    pub title: String,
    pub content: String,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRule {
    pub rule_id: String,
    pub file_pattern: String,
    pub rule_text: String,
    pub priority: i32,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchRecord {
    pub watch_id: String,
    pub directory: String,
    pub enabled: bool,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub job_id: String,
    pub kind: String,
    pub project_id: Option<String>,
    pub status: String,
    pub message: String,
    pub progress_pct: u8,
    pub estimated_time_remaining_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone)]
pub struct Registry {
    db: Arc<Database>,
}

impl Registry {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        let wtx = db.begin_write()?;
        {
            let _ = wtx.open_table(PROJECTS)?;
            let _ = wtx.open_table(MEMORY_BANK)?;
            let _ = wtx.open_table(REPO_RULES)?;
            let _ = wtx.open_table(WATCHES)?;
            let _ = wtx.open_table(JOBS)?;
            let _ = wtx.open_table(META)?;
        }
        wtx.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    // ---- Projects ----
    pub fn put_project(&self, rec: &ProjectRecord) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(rec)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(PROJECTS)?;
            t.insert(rec.project_id.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_project(&self, project_id: &str) -> anyhow::Result<Option<ProjectRecord>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(PROJECTS)?;
        if let Some(v) = t.get(project_id)? {
            let rec: ProjectRecord = serde_json::from_slice(v.value())?;
            Ok(Some(rec))
        } else {
            Ok(None)
        }
    }

    pub fn list_projects(&self) -> anyhow::Result<Vec<ProjectRecord>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(PROJECTS)?;
        let mut out: Vec<ProjectRecord> = Vec::new();
        for r in t.iter()? {
            let (_k, v) = r?;
            out.push(serde_json::from_slice(v.value())?);
        }
        out.sort_by(|a, b| a.project_name.cmp(&b.project_name));
        Ok(out)
    }

    pub fn delete_project(&self, project_id: &str) -> anyhow::Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(PROJECTS)?;
            t.remove(project_id)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn delete_all_for_project(&self, project_id: &str) -> anyhow::Result<()> {
        let prefix = format!("{project_id}\0");
        let wtx = self.db.begin_write()?;

        {
            let mut t = wtx.open_table(PROJECTS)?;
            t.remove(project_id)?;
        }

        {
            let mut t = wtx.open_table(MEMORY_BANK)?;
            let mut keys = Vec::new();
            for r in t.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                t.remove(k.as_str())?;
            }
        }

        {
            let mut t = wtx.open_table(REPO_RULES)?;
            let mut keys = Vec::new();
            for r in t.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                t.remove(k.as_str())?;
            }
        }

        {
            let mut t = wtx.open_table(WATCHES)?;
            let mut keys = Vec::new();
            for r in t.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            for k in keys {
                t.remove(k.as_str())?;
            }
        }

        {
            let mut t = wtx.open_table(JOBS)?;
            let mut job_ids = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                let job: JobRecord = serde_json::from_slice(v.value())?;
                if job.project_id.as_deref() == Some(project_id) {
                    job_ids.push(k.value().to_string());
                }
            }
            for jid in job_ids {
                t.remove(jid.as_str())?;
            }
        }

        {
            let mut t = wtx.open_table(META)?;
            let mut keys = Vec::new();
            for r in t.range(prefix.as_str()..)? {
                let (k, _) = r?;
                if !k.value().starts_with(&prefix) {
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

    // ---- Memory bank ----
    pub fn put_memory_section(&self, project_id: &str, sec: &MemorySection) -> anyhow::Result<()> {
        let key = format!("{project_id}\0{}", sec.section_id);
        let bytes = serde_json::to_vec(sec)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(MEMORY_BANK)?;
            t.insert(key.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn list_memory_sections(&self, project_id: &str) -> anyhow::Result<Vec<MemorySection>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(MEMORY_BANK)?;
        let mut out: Vec<MemorySection> = Vec::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            out.push(serde_json::from_slice(v.value())?);
        }
        out.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(out)
    }

    pub fn get_memory_section(
        &self,
        project_id: &str,
        section_id: &str,
    ) -> anyhow::Result<Option<MemorySection>> {
        let key = format!("{project_id}\0{section_id}");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(MEMORY_BANK)?;
        if let Some(v) = t.get(key.as_str())? {
            Ok(Some(serde_json::from_slice(v.value())?))
        } else {
            Ok(None)
        }
    }

    pub fn delete_memory_section(&self, project_id: &str, section_id: &str) -> anyhow::Result<()> {
        let key = format!("{project_id}\0{section_id}");
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(MEMORY_BANK)?;
            t.remove(key.as_str())?;
        }
        wtx.commit()?;
        Ok(())
    }

    // ---- Repo rules ----
    pub fn put_repo_rule(&self, project_id: &str, rule: &RepoRule) -> anyhow::Result<()> {
        let key = format!("{project_id}\0{}", rule.rule_id);
        let bytes = serde_json::to_vec(rule)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(REPO_RULES)?;
            t.insert(key.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn list_repo_rules(&self, project_id: &str) -> anyhow::Result<Vec<RepoRule>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(REPO_RULES)?;
        let mut out: Vec<RepoRule> = Vec::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            out.push(serde_json::from_slice(v.value())?);
        }
        out.sort_by(|a, b| a.file_pattern.cmp(&b.file_pattern));
        Ok(out)
    }

    pub fn delete_repo_rule(&self, project_id: &str, rule_id: &str) -> anyhow::Result<()> {
        let key = format!("{project_id}\0{rule_id}");
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(REPO_RULES)?;
            t.remove(key.as_str())?;
        }
        wtx.commit()?;
        Ok(())
    }

    // ---- Watches ----
    pub fn put_watch(&self, project_id: &str, watch: &WatchRecord) -> anyhow::Result<()> {
        let key = format!("{project_id}\0{}", watch.watch_id);
        let bytes = serde_json::to_vec(watch)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(WATCHES)?;
            t.insert(key.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn list_watches(&self, project_id: &str) -> anyhow::Result<Vec<WatchRecord>> {
        let prefix = format!("{project_id}\0");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(WATCHES)?;
        let mut out: Vec<WatchRecord> = Vec::new();
        for r in t.range(prefix.as_str()..)? {
            let (k, v) = r?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    // ---- Jobs ----
    pub fn put_job(&self, job: &JobRecord) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(job)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(JOBS)?;
            t.insert(job.job_id.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_job(&self, job_id: &str) -> anyhow::Result<Option<JobRecord>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(JOBS)?;
        if let Some(v) = t.get(job_id)? {
            Ok(Some(serde_json::from_slice(v.value())?))
        } else {
            Ok(None)
        }
    }

    pub fn list_jobs(&self, project_id: Option<&str>) -> anyhow::Result<Vec<JobRecord>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(JOBS)?;
        let mut out = Vec::new();
        for r in t.iter()? {
            let (_k, v) = r?;
            let job: JobRecord = serde_json::from_slice(v.value())?;
            if project_id.is_some() && job.project_id.as_deref() != project_id {
                continue;
            }
            out.push(job);
        }
        out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        Ok(out)
    }

    pub fn delete_job(&self, job_id: &str) -> anyhow::Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(JOBS)?;
            t.remove(job_id)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn cleanup_orphaned_jobs(&self) -> anyhow::Result<usize> {
        let mut count = 0;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(JOBS)?;
            let mut to_update = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                let mut job: JobRecord = serde_json::from_slice(v.value())?;
                if job.status == "running" {
                    job.status = "aborted".into();
                    job.message = "Job aborted due to system restart.".into();
                    job.updated_at_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis() as u64;
                    to_update.push((k.value().to_string(), job));
                }
            }
            for (k, job) in to_update {
                let bytes = serde_json::to_vec(&job)?;
                t.insert(k.as_str(), bytes.as_slice())?;
                count += 1;
            }
        }
        wtx.commit()?;
        Ok(count)
    }

    // ---- Meta ----
    pub fn set_meta(&self, project_id: &str, key: &str, value: &str) -> anyhow::Result<()> {
        let k = format!("{project_id}\0{key}");
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(META)?;
            t.insert(k.as_str(), value.as_bytes())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_meta(&self, project_id: &str, key: &str) -> anyhow::Result<Option<String>> {
        let k = format!("{project_id}\0{key}");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(META)?;
        if let Some(v) = t.get(k.as_str())? {
            Ok(Some(std::str::from_utf8(v.value())?.to_string()))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cleanup_orphaned_jobs() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("registry.redb");
        let reg = Registry::open(&path).unwrap();

        let job = JobRecord {
            job_id: "job1".into(),
            kind: "index".into(),
            project_id: None,
            status: "running".into(),
            message: "indexing".into(),
            progress_pct: 50,
            estimated_time_remaining_ms: None,
            created_at_ms: 100,
            updated_at_ms: 100,
        };
        reg.put_job(&job).unwrap();

        let count = reg.cleanup_orphaned_jobs().unwrap();
        assert_eq!(count, 1);

        let cleaned = reg.get_job("job1").unwrap().unwrap();
        assert_eq!(cleaned.status, "aborted");
        assert!(cleaned.message.contains("restart"));
    }
}
