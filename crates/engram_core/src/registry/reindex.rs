//! Durable recovery reasons and revision-checked settlement.
use super::*;
use std::collections::BTreeMap;

const KEY: &str = "reindex_requirements_v1";
// Anonymous database-wide sequence. Its leading NUL cannot be a valid project
// prefix, so deleting all project data cannot make an old repair token reusable.
pub(super) const REVISION_SEQUENCE: &str = "\0reindex_revision_sequence_v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReindexReason { Source, Vector, Unknown }

/// Captured before repair. New failures, including those in the same millisecond,
/// cannot be acknowledged by completing work for an older revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexRequirementToken {
    project_id: String,
    reason: ReindexReason,
    revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Requirement { since_ms: u64, revision: u64 }

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct Requirements {
    revision: u64,
    pending: BTreeMap<ReindexReason, Requirement>,
}

impl Requirements {
    pub(super) fn since_ms(&self) -> Option<u64> {
        self.pending.values().map(|r| r.since_ms).min()
    }

    fn legacy(since_ms: Option<u64>) -> Self {
        let mut requirements = Self::default();
        if let Some(since_ms) = since_ms {
            requirements.revision = 1;
            requirements.pending.insert(ReindexReason::Unknown, Requirement { since_ms, revision: 1 });
        }
        requirements
    }

    fn token(&self, project_id: &str, reason: ReindexReason) -> Option<ReindexRequirementToken> {
        self.pending.get(&reason).map(|r| ReindexRequirementToken {
            project_id: project_id.to_owned(), reason, revision: r.revision,
        })
    }
}

fn key(project_id: &str) -> String { format!("{project_id}\0{KEY}") }

pub(super) fn read_requirements(wtx: &redb::WriteTransaction, project_id: &str) -> anyhow::Result<Option<Requirements>> {
    let table = wtx.open_table(META)?;
    table.get(key(project_id).as_str())?.map(|v| serde_json::from_slice(v.value()).map_err(Into::into)).transpose()
}

pub(super) fn next_revision(wtx: &redb::WriteTransaction, project_floor: u64) -> anyhow::Result<u64> {
    let mut meta = wtx.open_table(META)?;
    let last: u64 = meta.get(REVISION_SEQUENCE)?.map(|v| serde_json::from_slice(v.value())).transpose()?.unwrap_or(0);
    let next = last.max(project_floor).checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Recovery requirement revision exhausted"))?;
    meta.insert(REVISION_SEQUENCE, serde_json::to_vec(&next)?.as_slice())?;
    Ok(next)
}

/// Import an existing legacy flag before a metadata edit can overwrite it.
/// New callers supplying a legacy flag also receive a durable Unknown reason.
pub(super) fn requirements_for_project_write(wtx: &redb::WriteTransaction, incoming: &ProjectRecord) -> anyhow::Result<Option<Requirements>> {
    if let Some(requirements) = read_requirements(wtx, &incoming.project_id)? { return Ok(Some(requirements)); }
    let current: Option<ProjectRecord> = wtx.open_table(PROJECTS)?.get(incoming.project_id.as_str())?
        .map(|v| serde_json::from_slice(v.value())).transpose()?;
    let since = current.and_then(|record| record.reindex_required_since_ms).or(incoming.reindex_required_since_ms);
    let Some(since_ms) = since else { return Ok(None); };
    let revision = next_revision(wtx, 1)?;
    let mut requirements = Requirements { revision, pending: BTreeMap::new() };
    requirements.pending.insert(ReindexReason::Unknown, Requirement { since_ms, revision });
    wtx.open_table(META)?.insert(key(&incoming.project_id).as_str(), serde_json::to_vec(&requirements)?.as_slice())?;
    Ok(Some(requirements))
}

pub(super) fn clear_pending_on_delete(wtx: &redb::WriteTransaction, project_id: &str) -> anyhow::Result<()> {
    let project: Option<ProjectRecord> = wtx.open_table(PROJECTS)?.get(project_id)?
        .map(|v| serde_json::from_slice(v.value())).transpose()?;
    let requirements = if let Some(project) = project {
        requirements_for_project_write(wtx, &project)?
    } else { read_requirements(wtx, project_id)? };
    if let Some(mut requirements) = requirements {
        // Advance the anonymous sequence even for a map written by an earlier
        // version, then clear just its reasons without recreating PROJECTS.
        requirements.revision = next_revision(wtx, requirements.revision)?;
        requirements.pending.clear();
        wtx.open_table(META)?.insert(key(project_id).as_str(), serde_json::to_vec(&requirements)?.as_slice())?;
    }
    Ok(())
}

fn write_requirements(wtx: &redb::WriteTransaction, project: &mut ProjectRecord, requirements: &Requirements) -> anyhow::Result<()> {
    project.reindex_required_since_ms = requirements.since_ms();
    let reasons = serde_json::to_vec(requirements)?;
    let record = serde_json::to_vec(project)?;
    // Both representations become visible in the same database commit.
    wtx.open_table(META)?.insert(key(&project.project_id).as_str(), reasons.as_slice())?;
    wtx.open_table(PROJECTS)?.insert(project.project_id.as_str(), record.as_slice())?;
    Ok(())
}

impl Registry {
    /// Current pending reasons, read in one snapshot without writing metadata.
    pub fn reindex_reasons(&self, project_id: &str) -> anyhow::Result<Vec<ReindexReason>> {
        vk("project_id", project_id)?;
        let rtx = self.db.begin_read()?;
        let project: Option<ProjectRecord> = rtx.open_table(PROJECTS)?.get(project_id)?
            .map(|v| serde_json::from_slice(v.value())).transpose()?;
        let Some(project) = project else { return Ok(Vec::new()); };
        let requirements = rtx.open_table(META)?.get(key(project_id).as_str())?
            .map(|v| serde_json::from_slice::<Requirements>(v.value())).transpose()?
            .unwrap_or_else(|| Requirements::legacy(project.reindex_required_since_ms));
        Ok(requirements.pending.keys().copied().collect())
    }

    /// No-op for a missing project, matching the legacy flag setter.
    pub fn require_reindex(&self, project_id: &str, reason: ReindexReason, since_ms: u64) -> anyhow::Result<Option<ReindexRequirementToken>> {
        Ok(self.record_reindex_requirement(project_id, reason, since_ms, false)?.map(|(token, _)| token))
    }

    /// Ensure a repair has a durable pending reason before work starts. Reuse
    /// an existing revision; return whether this call introduced the reason so
    /// a refused-before-work operation can dismiss only its own marker.
    pub fn begin_reindex_repair(&self, project_id: &str, reason: ReindexReason, since_ms: u64) -> anyhow::Result<Option<(ReindexRequirementToken, bool)>> {
        self.record_reindex_requirement(project_id, reason, since_ms, true)
    }

    fn record_reindex_requirement(&self, project_id: &str, reason: ReindexReason, since_ms: u64, only_if_absent: bool) -> anyhow::Result<Option<(ReindexRequirementToken, bool)>> {
        vk("project_id", project_id)?;
        let wtx = self.db.begin_write()?;
        let project: Option<ProjectRecord> = wtx.open_table(PROJECTS)?.get(project_id)?
            .map(|v| serde_json::from_slice(v.value())).transpose()?;
        let Some(mut project) = project else { return Ok(None); };
        let mut requirements = requirements_for_project_write(&wtx, &project)?.unwrap_or_default();
        if only_if_absent && let Some(token) = requirements.token(project_id, reason) {
            // This can be the first durable import of a legacy warning.
            wtx.commit()?;
            return Ok(Some((token, false)));
        }
        requirements.revision = next_revision(&wtx, requirements.revision)?;
        let since_ms = requirements.pending.get(&reason).map_or(since_ms, |r| r.since_ms.min(since_ms));
        requirements.pending.insert(reason, Requirement { since_ms, revision: requirements.revision });
        project.updated_at_ms = project.updated_at_ms.max(since_ms);
        write_requirements(&wtx, &mut project, &requirements)?;
        let token = requirements.token(project_id, reason);
        wtx.commit()?;
        Ok(token.map(|token| (token, true)))
    }

    /// Read-only: health/repair observation causes no registry writes.
    pub fn reindex_requirement_token(&self, project_id: &str, reason: ReindexReason) -> anyhow::Result<Option<ReindexRequirementToken>> {
        vk("project_id", project_id)?;
        let rtx = self.db.begin_read()?;
        let project: Option<ProjectRecord> = rtx.open_table(PROJECTS)?.get(project_id)?
            .map(|v| serde_json::from_slice(v.value())).transpose()?;
        let Some(project) = project else { return Ok(None); };
        let requirements = rtx.open_table(META)?.get(key(project_id).as_str())?
            .map(|v| serde_json::from_slice::<Requirements>(v.value())).transpose()?
            .unwrap_or_else(|| Requirements::legacy(project.reindex_required_since_ms));
        Ok(requirements.token(project_id, reason))
    }

    /// Return false if the requirement is absent or a newer failure superseded it.
    pub fn settle_reindex_requirement(&self, token: &ReindexRequirementToken) -> anyhow::Result<bool> {
        vk("project_id", &token.project_id)?;
        let wtx = self.db.begin_write()?;
        let project: Option<ProjectRecord> = wtx.open_table(PROJECTS)?.get(token.project_id.as_str())?
            .map(|v| serde_json::from_slice(v.value())).transpose()?;
        let Some(mut project) = project else { return Ok(false); };
        let mut requirements = read_requirements(&wtx, &token.project_id)?
            .unwrap_or_else(|| Requirements::legacy(project.reindex_required_since_ms));
        if requirements.token(&token.project_id, token.reason).as_ref() != Some(token) { return Ok(false); }
        requirements.pending.remove(&token.reason);
        // Retain the revision even after the final reason is settled: an old
        // token must never match a later, newly created requirement.
        write_requirements(&wtx, &mut project, &requirements)?;
        wtx.commit()?;
        Ok(true)
    }

    pub(super) fn dismiss_reindex_requirements(&self, project_id: &str) -> anyhow::Result<()> {
        vk("project_id", project_id)?;
        let wtx = self.db.begin_write()?;
        let project: Option<ProjectRecord> = wtx.open_table(PROJECTS)?.get(project_id)?
            .map(|v| serde_json::from_slice(v.value())).transpose()?;
        let Some(mut project) = project else { return Ok(()); };
        let mut requirements = read_requirements(&wtx, project_id)?
            .unwrap_or_else(|| Requirements::legacy(project.reindex_required_since_ms));
        if requirements.pending.is_empty() { return Ok(()); }
        requirements.pending.clear();
        write_requirements(&wtx, &mut project, &requirements)?;
        wtx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> ProjectRecord {
        ProjectRecord { project_id: "project".into(), project_name: "User project".into(),
            project_type: "general".into(), directory: "source".into(), created_at_ms: 1,
            updated_at_ms: 1, reindex_required_since_ms: None }
    }

    #[test]
    fn repairing_vectors_preserves_source_requirements_and_unrelated_metadata_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let token = {
            let registry = Registry::open(&path).unwrap();
            registry.put_project(&project()).unwrap();
            registry.set_meta("project", "active_generation", "7").unwrap();
            registry.require_reindex("project", ReindexReason::Source, 20).unwrap();
            registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap()
        };
        let registry = Registry::open(&path).unwrap();
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(10));
        assert!(registry.settle_reindex_requirement(&token).unwrap());
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(20));
        assert_eq!(registry.get_meta("project", "active_generation").unwrap().as_deref(), Some("7"));
        assert!(!registry.settle_reindex_requirement(&token).unwrap());
    }

    #[test]
    fn same_millisecond_failure_and_later_failure_after_success_survive_old_repair() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        let old = registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap();
        let new = registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap();
        assert!(!registry.settle_reindex_requirement(&old).unwrap());
        assert!(registry.settle_reindex_requirement(&new).unwrap());
        registry.require_reindex("project", ReindexReason::Vector, 10).unwrap();
        assert!(!registry.settle_reindex_requirement(&new).unwrap());
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(10));
    }

    #[test]
    fn source_completion_cannot_clear_vectors_and_vector_completion_cannot_clear_legacy_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        let mut legacy = project();
        legacy.reindex_required_since_ms = Some(3);
        registry.put_project(&legacy).unwrap();
        let vector = registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap();
        let source = registry.require_reindex("project", ReindexReason::Source, 20).unwrap().unwrap();
        assert!(registry.settle_reindex_requirement(&source).unwrap());
        assert!(registry.reindex_requirement_token("project", ReindexReason::Vector).unwrap().is_some());
        assert!(registry.settle_reindex_requirement(&vector).unwrap());
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(3));
        assert!(registry.reindex_requirement_token("project", ReindexReason::Unknown).unwrap().is_some());
    }

    #[test]
    fn stale_project_edits_neither_erase_live_warning_nor_resurrect_settled_warning() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        let mut old_healthy = project();
        registry.put_project(&old_healthy).unwrap();
        let token = registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap();
        let mut old_degraded = registry.get_project("project").unwrap().unwrap();
        old_healthy.project_name = "Renamed".into();
        registry.put_project(&old_healthy).unwrap();
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(10));
        assert!(registry.settle_reindex_requirement(&token).unwrap());
        old_degraded.project_name = "Renamed again".into();
        registry.put_project(&old_degraded).unwrap();
        let project = registry.get_project("project").unwrap().unwrap();
        assert_eq!(project.project_name, "Renamed again");
        assert_eq!(project.reindex_required_since_ms, None);
    }

    #[test]
    fn concurrent_subsystem_failures_both_remain_repairable() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        std::thread::scope(|scope| {
            for reason in [ReindexReason::Vector, ReindexReason::Source] {
                let registry = &registry;
                scope.spawn(move || registry.require_reindex("project", reason, 10).unwrap());
            }
        });
        let vector = registry.reindex_requirement_token("project", ReindexReason::Vector).unwrap().unwrap();
        assert!(registry.settle_reindex_requirement(&vector).unwrap());
        assert!(registry.reindex_requirement_token("project", ReindexReason::Source).unwrap().is_some());
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(10));
    }

    #[test]
    fn refused_repair_can_dismiss_only_the_marker_it_introduced() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        let (own, created) = registry.begin_reindex_repair("project", ReindexReason::Vector, 10).unwrap().unwrap();
        assert!(created);
        let (same, created) = registry.begin_reindex_repair("project", ReindexReason::Vector, 20).unwrap().unwrap();
        assert!(!created);
        assert_eq!(own, same);
        registry.require_reindex("project", ReindexReason::Vector, 10).unwrap();
        assert!(!registry.settle_reindex_requirement(&own).unwrap(), "refusal must preserve a failure that arrived after admission");
        let (current, created) = registry.begin_reindex_repair("project", ReindexReason::Vector, 30).unwrap().unwrap();
        assert!(!created);
        assert!(registry.settle_reindex_requirement(&current).unwrap());
        assert!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms.is_none());
    }

    #[test]
    fn unreadable_reason_metadata_cannot_be_reported_or_settled_as_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        let token = registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap();
        registry.set_meta("project", KEY, "{interrupted metadata").unwrap();
        assert!(registry.reindex_reasons("project").is_err());
        assert!(registry.settle_reindex_requirement(&token).is_err());
        assert!(registry.put_project(&project()).is_err());
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(10));
    }

    #[test]
    fn deleting_and_recreating_a_project_neither_inherits_warnings_nor_reuses_old_repair_tokens() {
        for delete_all in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
            registry.put_project(&project()).unwrap();
            let old = registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap();
            if delete_all { registry.delete_all_for_project("project").unwrap(); }
            else { registry.delete_project("project").unwrap(); }
            assert!(registry.get_project("project").unwrap().is_none());
            registry.put_project(&project()).unwrap();
            assert!(registry.reindex_reasons("project").unwrap().is_empty());
            assert!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms.is_none());
            let new = registry.require_reindex("project", ReindexReason::Vector, 10).unwrap().unwrap();
            assert!(!registry.settle_reindex_requirement(&old).unwrap());
            assert!(registry.settle_reindex_requirement(&new).unwrap());
        }
    }

    #[test]
    fn stale_project_metadata_cannot_erase_a_legacy_warning_before_its_first_import() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        let mut legacy = project();
        legacy.reindex_required_since_ms = Some(3);
        // Actual pre-reason registry layout: only the project JSON has a flag.
        let transaction = registry.db.begin_write().unwrap();
        transaction.open_table(PROJECTS).unwrap().insert("project", serde_json::to_vec(&legacy).unwrap().as_slice()).unwrap();
        transaction.commit().unwrap();
        let mut stale = project();
        stale.project_name = "Metadata edit from before the failure".into();
        registry.put_project(&stale).unwrap();
        assert_eq!(registry.get_project("project").unwrap().unwrap().reindex_required_since_ms, Some(3));
        assert_eq!(registry.reindex_reasons("project").unwrap(), vec![ReindexReason::Unknown]);
    }
}
