use crate::security::validate_key_component as validate_key_raw;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

mod reindex;
mod rule_ownership;
mod rule_receipt;
mod rule_adoption;
mod rule_alias_review;
mod source_head;
mod change_card_cache;
mod review_discussion;
pub use review_discussion::{ReviewDiscussionRecord,ReviewDiscussionWrite,ReviewDiscussionReference,ReviewDiscussionPage,
    REVIEW_DISCUSSION_RECORD_BYTES,REVIEW_DISCUSSION_BATCH_BYTES,REVIEW_DISCUSSION_BATCH_ROWS,
    REVIEW_DISCUSSION_PROJECT_ROWS,REVIEW_DISCUSSION_PROJECT_BYTES};
pub use change_card_cache::{ChangeCardCacheWrite, CHANGE_CARD_CACHE_BATCH, CHANGE_CARD_CACHE_ROWS,
    CHANGE_CARD_CACHE_BYTES, CHANGE_CARD_CACHE_RECORD_BYTES};
pub use reindex::{ReindexReason, ReindexRequirementToken};
pub use rule_ownership::RuleOwnershipSnapshot;
pub use rule_receipt::{LegacyRuleAlias, RuleSourceReceipt};
pub use rule_adoption::{LegacyAdoptionBinding, LegacyAdoptionDecision, LegacyAdoptionOutcome, LegacyAdoptionPlan};
pub use rule_alias_review::{AliasReviewAction, AliasReviewBinding, AliasReviewOutcome, AliasReviewPlan, AliasReviewSource};
pub use source_head::{SourceHeadToken, SourceRecoveryReservation, SourceRecoveryToken, SourceSnapshotRequirement, SOURCE_SNAPSHOTS_REQUIRED_KEY};

fn vk(name: &str, value: &str) -> anyhow::Result<()> {
    validate_key_raw(name, value).map_err(|e| anyhow::anyhow!("{e}"))
}

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
    /// Earliest outstanding source, vector, or legacy recovery requirement.
    /// Subsystem repairs settle only the requirement revision they verified;
    /// a successful source index alone does not prove vector/history recovery.
    #[serde(default)]
    pub reindex_required_since_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySection {
    pub section_id: String,
    pub title: String,
    pub content: String,
    pub updated_at_ms: u64,
    /// When the section was first written. `0` for legacy sections written
    /// before this field existed (age unknown). All fields below are
    /// `#[serde(default)]` so those legacy JSON records still deserialize.
    #[serde(default)]
    pub created_at_ms: u64,
    /// The agent / session that last wrote it. Free-form; with several
    /// concurrent sessions this is how a note's origin is known.
    #[serde(default)]
    pub author: Option<String>,
    /// A small controlled vocabulary — preference / decision / gotcha /
    /// reference / note — so recall can distinguish a standing preference
    /// from a scratch note. Validated at the write boundary.
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional review-by date (unix ms). A hint that the note should be
    /// re-checked after this time; nothing enforces it yet.
    #[serde(default)]
    pub review_after_ms: Option<u64>,
    /// Free-form tags for grouping.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Files (or symbol node_ids) this memory is about. Lets the note declare
    /// its subject so staleness can be judged: if a referenced file is
    /// re-indexed more recently than this memory was written, the note may no
    /// longer match the code.
    #[serde(default)]
    pub related_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoRule {
    pub rule_id: String,
    pub file_pattern: String,
    pub rule_text: String,
    pub priority: i32,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub introduced_at: Option<String>,
    #[serde(default)]
    pub provenance: Option<String>,
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

/// A bounded observation of matching project metadata. `has_more` and omitted
/// values disclose incomplete coverage; neither proves anything about other keys.
#[derive(Debug)]
pub struct MetaPrefixPage {
    pub entries: Vec<(String, String)>,
    pub inspected: usize,
    pub omitted_values: usize,
    pub has_more: bool,
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
        let db = crate::storage::open_database(path, crate::storage::DatabaseCache::Metadata)?;
        // Validate every existing table's type without committing an unchanged
        // schema. Release the read snapshot before initializing missing tables.
        let needs_init = {
            let rtx = db.begin_read()?;
            let mut missing = false;
            for table in [PROJECTS, MEMORY_BANK, REPO_RULES, WATCHES, JOBS, META] {
                match rtx.open_table(table) {
                    Ok(_) => {}
                    Err(redb::TableError::TableDoesNotExist(_)) => missing = true,
                    Err(error) => return Err(error.into()),
                }
            }
            missing
        };
        if needs_init {
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
        }
        Ok(Self { db: Arc::new(db) })
    }

    // ---- Projects ----
    pub fn put_project(&self, rec: &ProjectRecord) -> anyhow::Result<()> {
        vk("project_id", rec.project_id.as_str())?;
        let wtx = self.db.begin_write()?;
        let changes_source_identity = {
            let projects = wtx.open_table(PROJECTS)?;
            let previous: Option<ProjectRecord> = projects.get(rec.project_id.as_str())?
                .map(|v| serde_json::from_slice(v.value())).transpose()?;
            previous.is_none_or(|old| old.directory != rec.directory || old.project_type != rec.project_type)
        };
        if changes_source_identity {
            source_head::advance_revision(&wtx, &rec.project_id)?;
            source_head::mark_identity_unverified(&wtx, &rec.project_id)?;
        }
        {
            // Metadata edits often start from an earlier project snapshot.
            // Once reasons exist, their transactional aggregate is authoritative.
            let mut rec = rec.clone();
            if let Some(requirements) = reindex::requirements_for_project_write(&wtx, &rec)? {
                rec.reindex_required_since_ms = requirements.since_ms();
            }
            let bytes = serde_json::to_vec(&rec)?;
            let mut t = wtx.open_table(PROJECTS)?;
            t.insert(rec.project_id.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_project(&self, project_id: &str) -> anyhow::Result<Option<ProjectRecord>> {
        vk("project_id", project_id)?;
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(PROJECTS)?;
        if let Some(v) = t.get(project_id)? {
            let rec: ProjectRecord = serde_json::from_slice(v.value())?;
            Ok(Some(rec))
        } else {
            Ok(None)
        }
    }

    /// Record a recovery requirement whose subsystem is unknown (legacy API).
    /// Source-only or vector-only repair cannot dismiss this requirement.
    pub fn set_reindex_required(&self, project_id: &str, since_ms: u64) -> anyhow::Result<()> {
        self.require_reindex(project_id, ReindexReason::Unknown, since_ms)?;
        Ok(())
    }

    /// Explicit administrative disposition of all recovery requirements.
    /// Never call from subsystem repair/index completion: use a captured
    /// `ReindexRequirementToken` and `settle_reindex_requirement` instead.
    pub fn clear_reindex_required(&self, project_id: &str) -> anyhow::Result<()> {
        self.dismiss_reindex_requirements(project_id)
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
        vk("project_id", project_id)?;
        let wtx = self.db.begin_write()?;
        reindex::clear_pending_on_delete(&wtx, project_id)?;
        {
            let mut t = wtx.open_table(PROJECTS)?;
            t.remove(project_id)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn delete_all_for_project(&self, project_id: &str) -> anyhow::Result<()> {
        vk("project_id", project_id)?;
        let prefix = format!("{project_id}\0");
        let wtx = self.db.begin_write()?;
        reindex::clear_pending_on_delete(&wtx, project_id)?;

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
        vk("project_id", project_id)?;
        vk("section_id", &sec.section_id)?;
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
        vk("project_id", project_id)?;
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
        vk("project_id", project_id)?;
        vk("section_id", section_id)?;
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
        vk("project_id", project_id)?;
        vk("section_id", section_id)?;
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
        vk("project_id", project_id)?;
        vk("rule_id", &rule.rule_id)?;
        let key = format!("{project_id}\0{}", rule.rule_id);
        let bytes = serde_json::to_vec(rule)?;
        let wtx = self.db.begin_write()?;
        rule_receipt::guard_mutation(&wtx, project_id, &std::collections::BTreeSet::from([rule.rule_id.as_str()]), None)?;
        {
            let mut t = wtx.open_table(REPO_RULES)?;
            t.insert(key.as_str(), bytes.as_slice())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn list_repo_rules(&self, project_id: &str) -> anyhow::Result<Vec<RepoRule>> {
        vk("project_id", project_id)?;
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
        vk("project_id", project_id)?;
        vk("rule_id", rule_id)?;
        let key = format!("{project_id}\0{rule_id}");
        let wtx = self.db.begin_write()?;
        rule_receipt::guard_mutation(&wtx, project_id, &std::collections::BTreeSet::from([rule_id]), None)?;
        {
            let mut t = wtx.open_table(REPO_RULES)?;
            t.remove(key.as_str())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Publish source-managed rules and their receipt together. Every affected
    /// row is compared with the caller's snapshot so concurrent manual edits
    /// cannot be overwritten or retired by an import.
    pub fn publish_repo_rule_source(
        &self, project_id: &str, expected: &[RepoRule], remove: &[String],
        upsert: &[RepoRule], receipt_key: &str, expected_receipt: &str, receipt: &str,
    ) -> anyhow::Result<()> {
        vk("project_id", project_id)?;
        vk("key", receipt_key)?;
        let meta_key = format!("{project_id}\0{receipt_key}");
        let wtx = self.db.begin_write()?;
        let mut affected: std::collections::BTreeSet<&str> = remove.iter().map(String::as_str)
            .chain(upsert.iter().map(|r| r.rule_id.as_str())).collect();
        let published = receipt_key.starts_with("quality_source:").then(|| RuleSourceReceipt::decode(receipt_key, receipt.as_bytes())).transpose()?;
        if let Some(published) = &published {
            affected.extend(published.rules.iter().map(|r| r.rule_id.as_str()));
            anyhow::ensure!(!published.pending, "Cannot publish a pending source receipt");
            let provenance = format!("{}{}", rule_receipt::SOURCE_PROVENANCE, published.origin);
            for row in &published.rules {
                anyhow::ensure!(row.provenance.as_deref() == Some(provenance.as_str()), "Source publication cannot invent legacy ownership");
            }
            for row in upsert {
                anyhow::ensure!(published.rules.iter().any(|r| r == row), "Source publication does not own its replacement rule");
            }
        }
        rule_receipt::guard_mutation(&wtx, project_id, &affected, Some(receipt_key))?;
        if let Some(published) = &published {
            let table = wtx.open_table(REPO_RULES)?;
            for claim in &published.rules {
                let next = if let Some(row) = upsert.iter().find(|r| r.rule_id == claim.rule_id) { Some(row.clone()) }
                    else if remove.contains(&claim.rule_id) { None }
                    else { table.get(format!("{project_id}\0{}", claim.rule_id).as_str())?
                        .map(|raw| serde_json::from_slice::<RepoRule>(raw.value())).transpose()? };
                anyhow::ensure!(next.as_ref() == Some(claim), "Published ownership claim does not match the resulting rule state");
            }
        }
        {
            let mut meta = wtx.open_table(META)?;
            anyhow::ensure!(meta.get(meta_key.as_str())?.as_ref().map(|v| v.value())
                == Some(expected_receipt.as_bytes()), "Source receipt changed during import; retry");
            let mut table = wtx.open_table(REPO_RULES)?;
            let affected: std::collections::BTreeSet<&str> = remove.iter().map(String::as_str)
                .chain(upsert.iter().map(|r| r.rule_id.as_str())).collect();
            for id in affected {
                vk("rule_id", id)?;
                let key = format!("{project_id}\0{id}");
                let snapshot = expected.iter().find(|r| r.rule_id == id);
                let stored: Option<RepoRule> = table.get(key.as_str())?
                    .map(|value| serde_json::from_slice(value.value())).transpose()?;
                anyhow::ensure!(stored.as_ref() == snapshot,
                    "Repository rule `{id}` changed during import; no rules published");
            }
            for id in remove {
                if !upsert.iter().any(|row| &row.rule_id == id) { table.remove(format!("{project_id}\0{id}").as_str())?; }
            }
            for rule in upsert {
                let key = format!("{project_id}\0{}", rule.rule_id);
                let raw = {
                    match table.get(key.as_str())? {
                        Some(stored) if serde_json::from_slice::<RepoRule>(stored.value())? == *rule => None,
                        Some(stored) => Some(rule_adoption::merge_rule_fields(stored.value(), rule)?),
                        None => Some(serde_json::to_vec(rule)?),
                    }
                };
                if let Some(raw) = raw { table.insert(key.as_str(), raw.as_slice())?; }
            }
            meta.insert(meta_key.as_str(), receipt.as_bytes())?;
        }
        wtx.commit()?;
        Ok(())
    }

    // ---- Watches ----
    pub fn put_watch(&self, project_id: &str, watch: &WatchRecord) -> anyhow::Result<()> {
        vk("project_id", project_id)?;
        vk("watch_id", &watch.watch_id)?;
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

    /// Disable every stored watch atomically, preserving its configuration and
    /// unknown fields. Repeated disable requests do not rewrite unchanged rows.
    pub fn disable_project_watches(&self, project_id: &str, updated_at_ms: u64) -> anyhow::Result<()> {
        vk("project_id", project_id)?;
        let prefix = format!("{project_id}\0");
        let wtx = self.db.begin_write()?;
        let mut changes = Vec::new();
        {
            let mut table = wtx.open_table(WATCHES)?;
            for entry in table.range(prefix.as_str()..)? {
                let (key, value) = entry?;
                if !key.value().starts_with(&prefix) { break; }
                let mut row: serde_json::Value = serde_json::from_slice(value.value())?;
                let object = row.as_object_mut().ok_or_else(|| anyhow::anyhow!("Watch configuration is not an object"))?;
                if object.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) { continue; }
                object.insert("enabled".into(), serde_json::Value::Bool(false));
                object.insert("updated_at_ms".into(), serde_json::json!(updated_at_ms));
                changes.push((key.value().to_owned(), serde_json::to_vec(&row)?));
            }
            for (key, bytes) in &changes { table.insert(key.as_str(), bytes.as_slice())?; }
        }
        if !changes.is_empty() { wtx.commit()?; }
        Ok(())
    }

    pub fn list_watches(&self, project_id: &str) -> anyhow::Result<Vec<WatchRecord>> {
        vk("project_id", project_id)?;
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
        vk("job_id", job.job_id.as_str())?;
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
        vk("job_id", job_id)?;
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(JOBS)?;
        if let Some(v) = t.get(job_id)? {
            Ok(Some(serde_json::from_slice(v.value())?))
        } else {
            Ok(None)
        }
    }

    pub fn list_jobs(&self, project_id: Option<&str>) -> anyhow::Result<Vec<JobRecord>> {
        if let Some(pid) = project_id {
            vk("project_id", pid)?;
        }
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
        vk("job_id", job_id)?;
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(JOBS)?;
            t.remove(job_id)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn cleanup_orphaned_jobs(&self) -> anyhow::Result<usize> {
        // A startup with no running jobs must not commit an unchanged table.
        // Parse every record so malformed persisted jobs still fail cleanup.
        let has_orphans = {
            let rtx = self.db.begin_read()?;
            let t = rtx.open_table(JOBS)?;
            let mut found = false;
            for row in t.iter()? {
                let (_, value) = row?;
                let job: JobRecord = serde_json::from_slice(value.value())?;
                found |= job.status == "running";
            }
            found
        };
        if !has_orphans {
            return Ok(0);
        }

        // Re-read under the existing write transaction: another caller may
        // have completed cleanup after the read preflight was released.
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
        if count > 0 {
            wtx.commit()?;
        }
        Ok(count)
    }

    // ---- Meta ----
    // ---- Global (system-scoped) meta ----
    //
    // ADP1: Persist process-global flags (e.g. kill-switch) that must survive
    // restarts. Uses the reserved prefix `"__global__"` which cannot collide
    // with real project_ids (those are UUIDs and never start with `__`).

    /// Store a global (non-project-scoped) flag under `key`.
    pub fn set_global_flag(&self, key: &str, value: &str) -> anyhow::Result<()> {
        vk("key", key)?;
        // `__global__` contains no NUL or newline, so it passes vk() directly.
        let k = format!("__global__\0{key}");
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(META)?;
            t.insert(k.as_str(), value.as_bytes())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Retrieve a global flag stored with [`set_global_flag`].
    pub fn get_global_flag(&self, key: &str) -> anyhow::Result<Option<String>> {
        vk("key", key)?;
        let k = format!("__global__\0{key}");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(META)?;
        if let Some(v) = t.get(k.as_str())? {
            Ok(Some(std::str::from_utf8(v.value())?.to_string()))
        } else {
            Ok(None)
        }
    }

    /// ADP1: Persist the ADP kill-switch state to the registry so it survives
    /// process restarts.  When `enabled` is true the kill-switch fires on every
    /// call to `apply_rollout_policy`, regardless of the config-file setting.
    pub fn set_adp_kill_switch(&self, enabled: bool) -> anyhow::Result<()> {
        self.set_global_flag("adp_kill_switch", if enabled { "true" } else { "false" })
    }

    /// ADP1: Read the persisted ADP kill-switch state.  Returns `false` if not
    /// yet persisted (i.e. the config-file value is the sole source of truth).
    pub fn get_adp_kill_switch(&self) -> anyhow::Result<bool> {
        Ok(self
            .get_global_flag("adp_kill_switch")?
            .map(|v| v == "true")
            .unwrap_or(false))
    }

    pub fn set_meta(&self, project_id: &str, key: &str, value: &str) -> anyhow::Result<()> {
        vk("project_id", project_id)?;
        vk("key", key)?;
        let k = format!("{project_id}\0{key}");
        let wtx = self.db.begin_write()?;
        let changes_source_head = key == "active_generation" && {
            let table = wtx.open_table(META)?;
            table.get(k.as_str())?.as_ref().map(|v| v.value()) != Some(value.as_bytes())
        };
        if changes_source_head {
            source_head::advance_revision(&wtx, project_id)?;
            source_head::mark_identity_unverified(&wtx, project_id)?;
        }
        if key == "active_generation" { source_head::mark_head_required(&wtx, project_id)?; }
        {
            let mut t = wtx.open_table(META)?;
            t.insert(k.as_str(), value.as_bytes())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Atomically replace metadata only if its current value matches the snapshot.
    pub fn compare_exchange_meta(&self, project_id: &str, key: &str, expected: Option<&str>, value: &str) -> anyhow::Result<bool> {
        vk("project_id", project_id)?;
        vk("key", key)?;
        let k = format!("{project_id}\0{key}");
        let wtx = self.db.begin_write()?;
        {
            let table = wtx.open_table(META)?;
            let matches = table.get(k.as_str())?.as_ref().map(|v| v.value()) == expected.map(str::as_bytes);
            if !matches { return Ok(false); }
        }
        if key == "active_generation" && expected != Some(value) {
            source_head::advance_revision(&wtx, project_id)?;
            source_head::mark_identity_unverified(&wtx, project_id)?;
        }
        if key == "active_generation" { source_head::mark_head_required(&wtx, project_id)?; }
        wtx.open_table(META)?.insert(k.as_str(), value.as_bytes())?;
        wtx.commit()?;
        Ok(true)
    }

    /// Compare the exact original bytes and publish a small identity companion
    /// in the same transaction. Refuse a conflicting existing companion. This
    /// helper cannot bypass the special source-head publication protocol.
    pub fn compare_exchange_meta_with_companion(
        &self, project_id: &str, key: &str, expected: Option<&str>, value: &str,
        companion_key: &str, companion_value: &str,
    ) -> anyhow::Result<bool> {
        vk("project_id", project_id)?;
        vk("key", key)?;
        vk("companion_key", companion_key)?;
        anyhow::ensure!(key != companion_key && key != "active_generation"
            && companion_key != "active_generation", "invalid companion metadata keys");
        anyhow::ensure!(value.len() <= 1024 * 1024 && companion_value.len() <= 4096,
            "companion metadata write exceeds budget");
        let k = format!("{project_id}\0{key}");
        let companion = format!("{project_id}\0{companion_key}");
        let wtx = self.db.begin_write()?;
        {
            let mut table = wtx.open_table(META)?;
            if table.get(k.as_str())?.as_ref().map(|v| v.value()) != expected.map(str::as_bytes) {
                return Ok(false);
            }
            if table.get(companion.as_str())?.as_ref()
                .is_some_and(|v| v.value() != companion_value.as_bytes()) {
                return Ok(false);
            }
            table.insert(k.as_str(), value.as_bytes())?;
            table.insert(companion.as_str(), companion_value.as_bytes())?;
        }
        wtx.commit()?;
        Ok(true)
    }

    /// Inspect at most `max_entries` matching rows. Check each borrowed key and
    /// value and cumulative retained bytes BEFORE cloning. One extra borrowed
    /// row establishes `has_more`; no count-all scan or value-body copy occurs.
    pub fn list_meta_prefix_bounded(
        &self, project_id: &str, key_prefix: &str, max_entries: usize,
        max_value_bytes: usize, max_total_bytes: usize,
    ) -> anyhow::Result<MetaPrefixPage> {
        vk("project_id", project_id)?;
        vk("key_prefix", key_prefix)?;
        anyhow::ensure!(project_id.len() <= 512 && key_prefix.len() <= 512
            && (1..=256).contains(&max_entries) && max_value_bytes > 0
            && max_value_bytes <= max_total_bytes && max_total_bytes <= 1024 * 1024,
            "invalid metadata prefix inspection budget");
        let project_prefix = format!("{project_id}\0");
        let prefix = format!("{project_prefix}{key_prefix}");
        let rtx = self.db.begin_read()?;
        let table = rtx.open_table(META)?;
        let mut page = MetaPrefixPage {
            entries: Vec::new(), inspected: 0, omitted_values: 0, has_more: false,
        };
        let mut retained_bytes = 0usize;
        for entry in table.range(prefix.as_str()..)? {
            let (key, value) = entry?;
            let full_key = key.value();
            if !full_key.starts_with(&prefix) { break; }
            if page.inspected == max_entries {
                page.has_more = true;
                break;
            }
            page.inspected += 1;
            let local_key = &full_key[project_prefix.len()..];
            let bytes = value.value();
            let admitted = local_key.len() <= 1024 && bytes.len() <= max_value_bytes
                && local_key.len().checked_add(bytes.len())
                    .and_then(|n| retained_bytes.checked_add(n))
                    .is_some_and(|n| n <= max_total_bytes);
            if !admitted {
                page.omitted_values += 1;
                continue;
            }
            let Ok(value) = std::str::from_utf8(bytes) else {
                page.omitted_values += 1;
                continue;
            };
            retained_bytes += local_key.len() + bytes.len();
            page.entries.push((local_key.to_owned(), value.to_owned()));
        }
        Ok(page)
    }

    pub fn get_meta(&self, project_id: &str, key: &str) -> anyhow::Result<Option<String>> {
        self.get_meta_bounded(project_id, key, usize::MAX)
    }

    /// Check the stored length before copying a metadata value into memory.
    pub fn get_meta_bounded(&self, project_id: &str, key: &str, max_bytes: usize) -> anyhow::Result<Option<String>> {
        self.map_meta_bounded(project_id, key, max_bytes, |bytes| Ok(std::str::from_utf8(bytes)?.to_owned()))
    }

    /// Inspect an admitted borrowed value while its read transaction is alive.
    /// Length is checked before the callback; parsing/accounting can happen
    /// without an intermediate byte or UTF-8 string copy.
    pub fn map_meta_bounded<T>(&self, project_id: &str, key: &str, max_bytes: usize,
        inspect: impl FnOnce(&[u8]) -> anyhow::Result<T>) -> anyhow::Result<Option<T>> {
        vk("project_id", project_id)?;
        vk("key", key)?;
        let k = format!("{project_id}\0{key}");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(META)?;
        if let Some(v) = t.get(k.as_str())? {
            anyhow::ensure!(v.value().len() <= max_bytes, "metadata value exceeds read budget");
            inspect(v.value()).map(Some)
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
    fn companion_publication_rejects_stale_events_and_prefix_reads_bound_real_metadata() -> anyhow::Result<()> {
        let temporary = tempdir()?;
        let result = (|| -> anyhow::Result<()> {
            let registry = Registry::open(&temporary.path().join("registry.redb"))?;
            assert!(registry.compare_exchange_meta_with_companion("p","events",None,"first","identity","review-1")?);
            assert!(!registry.compare_exchange_meta_with_companion("p","events",None,"lost-update","identity","review-1")?);
            assert_eq!(registry.get_meta("p","events")?.as_deref(),Some("first"));
            assert!(!registry.compare_exchange_meta_with_companion("p","events",Some("first"),"second","identity","other-review")?);
            assert_eq!(registry.get_meta("p","events")?.as_deref(),Some("first"));
            assert_eq!(registry.get_meta("p","identity")?.as_deref(),Some("review-1"));
            assert!(registry.compare_exchange_meta_with_companion("p","events",Some("first"),"second","identity","review-1")?);
            registry.set_meta("p","decision_identity:0","small")?;
            registry.set_meta("p","decision_identity:1",&"x".repeat(4096))?;
            registry.set_meta("p","decision_identity:2","uninspected")?;
            registry.set_meta("p-other","decision_identity:0","another project")?;
            let page = registry.list_meta_prefix_bounded("p","decision_identity:",2,256,1024)?;
            assert_eq!(page.entries,vec![("decision_identity:0".to_string(),"small".to_string())]);
            assert_eq!(page.inspected,2);
            assert_eq!(page.omitted_values,1);
            assert!(page.has_more);
            Ok(())
        })();
        let cleanup = temporary.close();
        result?;
        cleanup?;
        Ok(())
    }

    #[test]
    fn ready_store_reopen_preserves_registry_payload_and_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        {
            let registry = Registry::open(&path).unwrap();
            registry.set_meta("project", "receipt", "verified source publication").unwrap();
            registry.set_adp_kill_switch(true).unwrap();
        }
        let fingerprint = || {
            let bytes = std::fs::read(&path).unwrap();
            (bytes.len(), blake3::hash(&bytes))
        };
        let before = fingerprint();
        for _ in 0..2 {
            {
                let registry = Registry::open(&path).unwrap();
                assert_eq!(registry.get_meta("project", "receipt").unwrap().as_deref(), Some("verified source publication"));
                assert!(registry.get_adp_kill_switch().unwrap());
            }
            assert_eq!(fingerprint(), before, "Reading a ready registry must preserve its closed file");
        }
    }

    #[test]
    fn ready_store_reopen_initializes_partial_registry_without_losing_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        {
            let db = crate::storage::open_database(&path, crate::storage::DatabaseCache::Metadata).unwrap();
            let wtx = db.begin_write().unwrap();
            { wtx.open_table(META).unwrap().insert("project\0receipt", b"existing publication".as_slice()).unwrap(); }
            wtx.commit().unwrap();
        }
        let registry = Registry::open(&path).unwrap();
        assert_eq!(registry.get_meta("project", "receipt").unwrap().as_deref(), Some("existing publication"));
        assert!(registry.list_projects().unwrap().is_empty());
        registry.set_adp_kill_switch(true).unwrap();
        assert!(registry.get_adp_kill_switch().unwrap());
    }

    #[test]
    fn ready_store_reopen_rejects_wrong_registry_table_type() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let wrong = TableDefinition::<u64, u64>::new("meta");
        {
            let db = crate::storage::open_database(&path, crate::storage::DatabaseCache::Metadata).unwrap();
            let wtx = db.begin_write().unwrap();
            for table in [PROJECTS, MEMORY_BANK, REPO_RULES, WATCHES, JOBS] {
                let _ = wtx.open_table(table).unwrap();
            }
            { wtx.open_table(wrong).unwrap().insert(7, 42).unwrap(); }
            wtx.commit().unwrap();
        }
        let error = Registry::open(&path).err().expect("Wrong table type must be rejected");
        assert!(matches!(error.downcast_ref::<redb::TableError>(), Some(redb::TableError::TableTypeMismatch { .. })));
        let db = crate::storage::open_database(&path, crate::storage::DatabaseCache::Metadata).unwrap();
        let rtx = db.begin_read().unwrap();
        assert!(rtx.open_table(PROJECTS).unwrap().iter().unwrap().next().is_none());
        assert_eq!(rtx.open_table(wrong).unwrap().get(7).unwrap().unwrap().value(), 42);
    }

    #[test]
    fn bounded_metadata_read_rejects_oversized_value_without_changing_it() {
        let dir = tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.set_meta("project", "receipt", "complete metadata").unwrap();
        assert!(registry.get_meta_bounded("project", "receipt", 3).is_err());
        let invoked = std::cell::Cell::new(false);
        let result = registry.map_meta_bounded("project", "receipt", 3, |_| {
            invoked.set(true);
            Ok(())
        });
        assert!(result.is_err() && !invoked.get(),
            "Oversized metadata must be refused before invoking its parser");
        assert_eq!(registry.get_meta("project", "receipt").unwrap().as_deref(), Some("complete metadata"));
        assert_eq!(registry.get_meta_bounded("project", "missing", 0).unwrap(), None);
    }

    #[test]
    fn source_publication_preserves_concurrent_manual_edits_and_commits_nothing_on_conflict() {
        let dir = tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        let original = RepoRule { rule_id: "source-row".into(), file_pattern: "Site/**".into(),
            rule_text: "Original policy".into(), priority: 5, updated_at_ms: 1,
            introduced_at: None, provenance: Some("source".into()) };
        registry.put_repo_rule("project", &original).unwrap();
        registry.set_meta("project", "receipt", "pending").unwrap();
        let mut manual = original.clone();
        manual.rule_text = "Manual revision made while import was running".into();
        registry.put_repo_rule("project", &manual).unwrap();
        let mut added = original.clone();
        added.rule_id = "new-row".into();
        let result = registry.publish_repo_rule_source("project", &[original.clone()],
            &[original.rule_id.clone()], &[added], "receipt", "pending", "published");
        assert!(result.is_err());
        let rules = registry.list_repo_rules("project").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_text, manual.rule_text);
        assert_eq!(registry.get_meta("project", "receipt").unwrap().as_deref(), Some("pending"));
    }

    #[test]
    fn legacy_rule_without_optional_fields_can_be_refreshed() {
        let dir = tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        let legacy = br#"{"rule_id":"old-row","file_pattern":"Site/**","rule_text":"Legacy policy","priority":5,"updated_at_ms":1,"provenance":"quality-gate finding"}"#;
        {
            let transaction = registry.db.begin_write().unwrap();
            { transaction.open_table(REPO_RULES).unwrap().insert("project\0old-row", legacy.as_slice()).unwrap(); }
            transaction.commit().unwrap();
        }
        let snapshot = registry.list_repo_rules("project").unwrap();
        let mut refreshed = snapshot[0].clone();
        refreshed.file_pattern = "Site/maps/**".into();
        registry.set_meta("project", "receipt", "pending").unwrap();
        registry.publish_repo_rule_source("project", &snapshot, &[], &[refreshed],
            "receipt", "pending", "published").unwrap();
        assert_eq!(registry.list_repo_rules("project").unwrap()[0].file_pattern, "Site/maps/**");
        assert_eq!(registry.get_meta("project", "receipt").unwrap().as_deref(), Some("published"));
    }

    #[test]
    fn metadata_compare_exchange_has_one_concurrent_winner() {
        let tmp = tempdir().unwrap();
        let registry = Registry::open(&tmp.path().join("registry.redb")).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["worker-a", "worker-b"].into_iter().map(|value| {
            let registry = registry.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                registry.compare_exchange_meta("project", "session", None, value).unwrap()
            })
        }).collect();
        assert_eq!(handles.into_iter().map(|h| usize::from(h.join().unwrap())).sum::<usize>(), 1);
        let current = registry.get_meta("project", "session").unwrap().unwrap();
        assert!(!registry.compare_exchange_meta("project", "session", Some("stale"), "lost").unwrap());
        assert!(registry.compare_exchange_meta("project", "session", Some(&current), "consumed").unwrap());
        assert_eq!(registry.get_meta("project", "session").unwrap().as_deref(), Some("consumed"));
    }

    fn cleanup_job(job_id: &str, status: &str) -> JobRecord {
        JobRecord {
            job_id: job_id.into(),
            kind: "index".into(),
            project_id: Some("project".into()),
            status: status.into(),
            message: format!("Index progress: {status}"),
            progress_pct: 50,
            estimated_time_remaining_ms: Some(500),
            created_at_ms: 100,
            updated_at_ms: 100,
        }
    }

    fn closed_registry_fingerprint(path: &Path) -> (usize, blake3::Hash) {
        let bytes = std::fs::read(path).unwrap();
        (bytes.len(), blake3::hash(&bytes))
    }

    #[test]
    fn startup_noop_job_cleanup_preserves_an_empty_registry_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        { let _registry = Registry::open(&path).unwrap(); }
        let before = closed_registry_fingerprint(&path);
        {
            let registry = Registry::open(&path).unwrap();
            assert_eq!(registry.cleanup_orphaned_jobs().unwrap(), 0);
            assert!(registry.list_jobs(None).unwrap().is_empty());
        }
        assert_eq!(closed_registry_fingerprint(&path), before, "Empty cleanup must preserve closed file bytes and length");
    }

    #[test]
    fn startup_noop_job_cleanup_preserves_terminal_jobs_and_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let jobs: Vec<_> = ["completed", "failed", "aborted", "cancelled"].into_iter()
            .map(|status| cleanup_job(status, status)).collect();
        {
            let registry = Registry::open(&path).unwrap();
            for job in &jobs { registry.put_job(job).unwrap(); }
        }
        let before = closed_registry_fingerprint(&path);
        {
            let registry = Registry::open(&path).unwrap();
            assert_eq!(registry.cleanup_orphaned_jobs().unwrap(), 0);
            for job in &jobs {
                assert_eq!(serde_json::to_value(registry.get_job(&job.job_id).unwrap().unwrap()).unwrap(),
                    serde_json::to_value(job).unwrap());
            }
        }
        assert_eq!(closed_registry_fingerprint(&path), before, "Terminal-only cleanup must preserve closed file bytes and length");
    }

    #[test]
    fn startup_noop_job_cleanup_aborts_only_running_jobs_then_stays_stable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let jobs = [cleanup_job("orphan-a", "running"), cleanup_job("orphan-b", "running"),
            cleanup_job("finished", "completed"), cleanup_job("pending", "queued"),
            cleanup_job("different-case", "Running")];
        let cleaned_jobs;
        {
            let registry = Registry::open(&path).unwrap();
            for job in &jobs { registry.put_job(job).unwrap(); }
            assert_eq!(registry.cleanup_orphaned_jobs().unwrap(), 2);
            for job in &jobs {
                let loaded = registry.get_job(&job.job_id).unwrap().unwrap();
                let mut expected = job.clone();
                if job.status == "running" {
                    expected.status = "aborted".into();
                    expected.message = "Job aborted due to system restart.".into();
                    assert!(loaded.updated_at_ms > job.updated_at_ms);
                    expected.updated_at_ms = loaded.updated_at_ms;
                }
                assert_eq!(serde_json::to_value(loaded).unwrap(), serde_json::to_value(expected).unwrap());
            }
            cleaned_jobs = serde_json::to_value(registry.list_jobs(None).unwrap()).unwrap();
        }
        let before_noop = closed_registry_fingerprint(&path);
        {
            let registry = Registry::open(&path).unwrap();
            assert_eq!(registry.cleanup_orphaned_jobs().unwrap(), 0);
            assert_eq!(serde_json::to_value(registry.list_jobs(None).unwrap()).unwrap(), cleaned_jobs);
        }
        assert_eq!(closed_registry_fingerprint(&path), before_noop, "Completed orphan cleanup must become a stable no-op");
    }

    #[test]
    fn startup_noop_job_cleanup_rejects_malformed_records_without_partial_updates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let job = cleanup_job("a-orphan", "running");
        {
            let registry = Registry::open(&path).unwrap();
            registry.put_job(&job).unwrap();
            let wtx = registry.db.begin_write().unwrap();
            { wtx.open_table(JOBS).unwrap().insert("z-malformed", b"{invalid JSON".as_slice()).unwrap(); }
            wtx.commit().unwrap();
        }
        let before = closed_registry_fingerprint(&path);
        {
            let registry = Registry::open(&path).unwrap();
            assert!(registry.cleanup_orphaned_jobs().is_err());
            assert_eq!(serde_json::to_value(registry.get_job(&job.job_id).unwrap().unwrap()).unwrap(),
                serde_json::to_value(&job).unwrap());
            let rtx = registry.db.begin_read().unwrap();
            assert_eq!(rtx.open_table(JOBS).unwrap().get("z-malformed").unwrap().unwrap().value(), b"{invalid JSON");
        }
        assert_eq!(closed_registry_fingerprint(&path), before, "Malformed jobs must not trigger partial cleanup or mutate the file");
    }

    /// ADP1: Global flag must persist across separate registry opens (simulating
    /// process restart). The kill-switch set in one "run" must be visible in the next.
    #[test]
    fn adp_kill_switch_survives_registry_reopen() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("registry.redb");

        // First "run": set the kill-switch.
        {
            let reg = Registry::open(&path).unwrap();
            assert!(
                !reg.get_adp_kill_switch().unwrap(),
                "must be false before any set"
            );
            reg.set_adp_kill_switch(true).unwrap();
            assert!(reg.get_adp_kill_switch().unwrap(), "must be true after set");
        }

        // Second "run": open the same DB and verify persistence.
        {
            let reg = Registry::open(&path).unwrap();
            assert!(
                reg.get_adp_kill_switch().unwrap(),
                "ADP1: kill-switch must survive registry reopen (process restart)"
            );
        }

        // Clear the kill-switch and verify it resets.
        {
            let reg = Registry::open(&path).unwrap();
            reg.set_adp_kill_switch(false).unwrap();
            assert!(
                !reg.get_adp_kill_switch().unwrap(),
                "must be false after clear"
            );
        }
    }

    /// ADP1: Global flag key must not conflict with project-scoped meta keys.
    /// Storing a project meta under a normal project_id must not affect global flags.
    #[test]
    fn global_flag_does_not_conflict_with_project_meta() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("registry.redb");
        let reg = Registry::open(&path).unwrap();

        // Store something under a fake project_id that looks like the global prefix.
        // NUL is the composite-key separator, so "__global__" as project_id must be
        // rejected by vk() since NUL/newline are the only prohibited chars, but
        // "__global__" itself is valid and therefore WOULD be accepted by set_meta.
        // The global flag uses the literal key `"__global__\0adp_kill_switch"`,
        // which differs from any `set_meta("__global__", "adp_kill_switch", …)` call
        // because `set_meta` also produces `"__global__\0adp_kill_switch"`. This is
        // the SAME key — which is fine: the test just ensures the separate methods
        // round-trip correctly without corrupting each other.
        reg.set_adp_kill_switch(true).unwrap();

        // Project-scoped meta under a different project_id must not affect the flag.
        let proj = ProjectRecord {
            project_id: "proj-abc".into(),
            project_name: "Test".into(),
            project_type: "general".into(),
            directory: "/tmp".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        };
        reg.put_project(&proj).unwrap();
        reg.set_meta("proj-abc", "adp_kill_switch", "false")
            .unwrap();

        // Global flag must still be true.
        assert!(
            reg.get_adp_kill_switch().unwrap(),
            "ADP1: project-scoped meta must not shadow global flag"
        );
    }
}
