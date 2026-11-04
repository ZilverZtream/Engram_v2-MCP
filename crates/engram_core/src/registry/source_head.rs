//! Exact, lifecycle-aware source publication bases. A numeric generation alone
//! loses legacy absence and cannot detect a head changed away and back again.
use super::*;

const REVISION: &str = "source_head_revision_v1";
const HEAD: &str = "active_generation";
const REQUIRED: &str = "source_head_required_v1";
pub const SOURCE_SNAPSHOTS_REQUIRED_KEY: &str = "source_snapshots_required_v1";
const MANAGED: &str = SOURCE_SNAPSHOTS_REQUIRED_KEY;
const LEGACY_ATTESTED: &str = "source_legacy_absence_attested_v1";
const IDENTITY_VERIFIED: &str = "source_identity_verified_v1";
const IDENTITY_EPOCH: &str = "source_identity_epoch_v1";
const RECOVERY_HEAD_BYTES: usize = 4096;

/// Explicit recovery authority; never interprets damaged metadata as a source view.
#[derive(Debug, Clone)]
pub struct SourceRecoveryToken {
    project_id: String,
    metadata: RecoveryMetadata,
    directory: String,
    project_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryMetadata {
    raw_head: Option<Vec<u8>>,
    required: Option<Vec<u8>>,
    verified: Option<Vec<u8>>,
    legacy_attested: Option<Vec<u8>>,
    managed: Option<Vec<u8>>,
    revision: u64,
    identity_epoch: u64,
}

impl SourceRecoveryToken {
    pub fn project_id(&self) -> &str { &self.project_id }
    pub fn source_identity(&self) -> (&str, &str) { (&self.directory, &self.project_type) }
    pub fn matches_source_identity(&self, directory: &str, project_type: &str) -> bool {
        self.directory == directory && self.project_type == project_type
    }
}

/// A fresh durable allocation. Abandoning it never makes its generation reusable.
#[derive(Debug, Clone)]
pub struct SourceRecoveryReservation {
    base: SourceRecoveryToken,
    generation: u64,
}

impl SourceRecoveryReservation {
    pub fn project_id(&self) -> &str { self.base.project_id() }
    pub fn generation(&self) -> u64 { self.generation }
    pub fn source_identity(&self) -> (&str, &str) { self.base.source_identity() }
    pub fn matches_source_identity(&self, directory: &str, project_type: &str) -> bool {
        self.base.matches_source_identity(directory, project_type)
    }
}

fn recovery_metadata(
    project: &str,
    mut read: impl FnMut(&str, usize) -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<(RecoveryMetadata, u64)> {
    let raw_head = read(&head_key(project), RECOVERY_HEAD_BYTES)?;
    let required = read(&flag_key(project, REQUIRED), 1)?;
    anyhow::ensure!(required.as_deref().is_none_or(|v| v == b"1"), "Invalid source head presence metadata");
    let verified = read(&flag_key(project, IDENTITY_VERIFIED), 1)?;
    identity_verified(verified.as_deref())?;
    let legacy_attested = read(&flag_key(project, LEGACY_ATTESTED), 1)?;
    anyhow::ensure!(legacy_attested.as_deref().is_none_or(|v| v == b"1"), "Invalid legacy source head attestation");
    let managed = read(&flag_key(project, MANAGED), 1)?;
    validate_managed(managed.as_deref())?;
    let revision = read(&revision_key(project), 20)?
        .ok_or_else(|| anyhow::anyhow!("Source recovery requires an existing head revision"))?;
    let revision = self::revision(&revision)?;
    let epoch = read(&flag_key(project, IDENTITY_EPOCH), 20)?
        .ok_or_else(|| anyhow::anyhow!("Source recovery requires an existing identity epoch"))?;
    let identity_epoch = self::revision(&epoch)?;
    anyhow::ensure!(identity_epoch <= revision, "Source recovery identity epoch exceeds head revision");
    let sequence = read(super::reindex::REVISION_SEQUENCE, 20)?
        .ok_or_else(|| anyhow::anyhow!("Source recovery requires an existing durable revision allocator"))?;
    let sequence: u64 = serde_json::from_slice(&sequence)?;
    anyhow::ensure!(sequence >= revision.max(identity_epoch), "Source recovery revision allocator is behind source authority");
    Ok((RecoveryMetadata { raw_head, required, verified, legacy_attested, managed, revision, identity_epoch }, sequence))
}

fn recovery_matches(
    wtx: &redb::WriteTransaction,
    base: &SourceRecoveryToken,
) -> anyhow::Result<Option<u64>> {
    let project: Option<ProjectRecord> = wtx.open_table(PROJECTS)?.get(base.project_id())?
        .map(|value| serde_json::from_slice(value.value())).transpose()?;
    if !project.is_some_and(|p| base.matches_source_identity(&p.directory, &p.project_type)) {
        return Ok(None);
    }
    let meta = wtx.open_table(META)?;
    let (current, sequence) = recovery_metadata(base.project_id(), |key, limit| {
        meta.get(key)?.map(|value| {
            anyhow::ensure!(value.value().len() <= limit, "Source recovery metadata exceeds read budget");
            Ok(value.value().to_vec())
        }).transpose()
    })?;
    Ok((current == base.metadata).then_some(sequence))
}

#[derive(Debug, Clone)]
pub struct SourceHeadToken {
    project_id: String,
    raw_head: Option<String>,
    managed: Option<Vec<u8>>,
    generation: u64,
    revision: u64,
    identity_epoch: u64,
    directory: String,
    project_type: String,
}

impl SourceHeadToken {
    pub fn project_id(&self) -> &str {
        &self.project_id
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn generation_metadata_absent(&self) -> bool {
        self.raw_head.is_none()
    }
    pub fn matches_source_identity(&self, directory: &str, project_type: &str) -> bool {
        self.directory == directory && self.project_type == project_type
    }
    pub fn source_identity(&self) -> (&str, &str) {
        (&self.directory, &self.project_type)
    }
    /// Stable across source publications, fresh after lifecycle/identity changes.
    pub fn identity_epoch(&self) -> u64 {
        self.identity_epoch
    }
}

/// Independent layout evidence for fresh source reads, never an admission gate
/// for opening unrelated knowledge. An unproven head may still resolve a valid
/// managed Files record, but disk emptiness cannot certify a legacy layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceSnapshotRequirement {
    Managed,
    Unproven,
    AttestedLegacy,
    Unpublished,
}

fn validate_managed(value: Option<&[u8]>) -> anyhow::Result<()> {
    anyhow::ensure!(value.is_none_or(|value| value == b"1"), "Invalid source snapshot requirement metadata");
    Ok(())
}

fn revision_key(project: &str) -> String {
    format!("{project}\0{REVISION}")
}
fn head_key(project: &str) -> String {
    format!("{project}\0{HEAD}")
}
fn flag_key(project: &str, flag: &str) -> String {
    format!("{project}\0{flag}")
}

fn check_presence(raw: Option<&str>, required: Option<&[u8]>) -> anyhow::Result<()> {
    if let Some(required) = required {
        anyhow::ensure!(required == b"1", "Invalid source head presence metadata");
        anyhow::ensure!(
            raw.is_some(),
            "Authoritative source generation metadata is missing; source recovery is required"
        );
    }
    Ok(())
}

pub(super) fn mark_head_required(
    wtx: &redb::WriteTransaction,
    project: &str,
) -> anyhow::Result<()> {
    wtx.open_table(META)?
        .insert(flag_key(project, REQUIRED).as_str(), b"1".as_slice())?;
    Ok(())
}

fn generation(project: &str, raw: Option<&str>) -> anyhow::Result<u64> {
    raw.map(|value| value.parse().map_err(|_| anyhow::anyhow!(
        "Invalid active_generation metadata for project '{project}'; source generation cannot be determined"
    ))).transpose().map(|value| value.unwrap_or(1))
}

fn bounded_text(bytes: &[u8]) -> anyhow::Result<String> {
    anyhow::ensure!(
        bytes.len() <= 20,
        "Source head metadata exceeds read budget"
    );
    Ok(std::str::from_utf8(bytes)?.to_owned())
}

fn revision(bytes: &[u8]) -> anyhow::Result<u64> {
    let value = bounded_text(bytes)?.parse::<u64>()?;
    anyhow::ensure!(value != 0, "Invalid source head revision");
    Ok(value)
}

pub(super) fn advance_revision(wtx: &redb::WriteTransaction, project: &str) -> anyhow::Result<u64> {
    let key = revision_key(project);
    let previous = wtx
        .open_table(META)?
        .get(key.as_str())?
        .map(|value| revision(value.value()))
        .transpose()?
        .unwrap_or(0);
    // The anonymous sequence is not erased by project deletion. Sharing it
    // with recovery reasons does not let either token acknowledge the other.
    let epoch = wtx.open_table(META)?.get(flag_key(project, IDENTITY_EPOCH).as_str())?
        .map(|value| revision(value.value())).transpose()?.unwrap_or(0);
    let next = super::reindex::next_revision(wtx, previous.max(epoch))?;
    wtx.open_table(META)?
        .insert(key.as_str(), next.to_string().as_bytes())?;
    Ok(next)
}

pub(super) fn mark_identity_unverified(
    wtx: &redb::WriteTransaction,
    project: &str,
) -> anyhow::Result<u64> {
    // Keep the epoch at or below the current head revision. A missing epoch
    // cannot reissue an earlier value even if the anonymous sequence was lost.
    let epoch = advance_revision(wtx, project)?;
    wtx.open_table(META)?.insert(
        flag_key(project, IDENTITY_EPOCH).as_str(), epoch.to_string().as_bytes(),
    )?;
    wtx.open_table(META)?.insert(
        flag_key(project, IDENTITY_VERIFIED).as_str(),
        b"0".as_slice(),
    )?;
    Ok(epoch)
}

fn identity_verified(value: Option<&[u8]>) -> anyhow::Result<bool> {
    match value {
        None | Some(b"0") => Ok(false),
        Some(b"1") => Ok(true),
        _ => anyhow::bail!("Invalid source identity verification metadata"),
    }
}

impl Registry {
    /// Capture only a damaged head with intact lifetime and allocator authority.
    /// Healthy heads and unattested legacy absence keep the ordinary token path.
    pub fn capture_source_recovery(&self, project_id: &str) -> anyhow::Result<SourceRecoveryToken> {
        vk("project_id", project_id)?;
        let rtx = self.db.begin_read()?;
        let project: ProjectRecord = rtx.open_table(PROJECTS)?.get(project_id)?
            .map(|value| serde_json::from_slice(value.value())).transpose()?
            .ok_or_else(|| anyhow::anyhow!("Source project is not registered"))?;
        let meta = rtx.open_table(META)?;
        let (metadata, _) = recovery_metadata(project_id, |key, limit| {
            meta.get(key)?.map(|value| {
                anyhow::ensure!(value.value().len() <= limit, "Source recovery metadata exceeds read budget");
                Ok(value.value().to_vec())
            }).transpose()
        })?;
        match metadata.raw_head.as_deref() {
            None => anyhow::ensure!(metadata.legacy_attested.is_none() || metadata.required.is_some() || metadata.managed.is_some(),
                "An attested legacy source requires ordinary source publication"),
            Some(bytes) => {
                let parsed = bounded_text(bytes).and_then(|value| generation(project_id, Some(&value)));
                anyhow::ensure!(parsed.is_err() || (metadata.managed.is_some() && parsed.as_ref().is_ok_and(|value| *value == 0)),
                    "Healthy source heads require ordinary source publication");
            }
        }
        Ok(SourceRecoveryToken {
            project_id: project_id.to_owned(), metadata,
            directory: project.directory, project_type: project.project_type,
        })
    }

    /// The caller holds source mutation admission and has observed all physical
    /// and catalog generations. Reserve above that floor without publishing it.
    pub fn reserve_source_recovery(
        &self,
        base: &SourceRecoveryToken,
        observed_floor: u64,
    ) -> anyhow::Result<Option<SourceRecoveryReservation>> {
        let wtx = self.db.begin_write()?;
        let Some(sequence) = recovery_matches(&wtx, base)? else { return Ok(None); };
        let numeric_head = base.metadata.raw_head.as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|value| value.parse::<u64>().ok()).unwrap_or(0);
        let floor = sequence.max(observed_floor).max(numeric_head);
        anyhow::ensure!(floor < u64::MAX - 1, "Source recovery generation allocation exhausted");
        let generation = super::reindex::next_revision(&wtx, floor)?;
        wtx.commit()?;
        Ok(Some(SourceRecoveryReservation { base: base.clone(), generation }))
    }

    /// Publish only after the reserved source checkpoint is written and sealed.
    /// Exact raw metadata and lifetime comparison protect a concurrent winner.
    pub fn publish_source_recovery(&self, reservation: &SourceRecoveryReservation) -> anyhow::Result<bool> {
        let base = &reservation.base;
        let wtx = self.db.begin_write()?;
        let Some(sequence) = recovery_matches(&wtx, base)? else { return Ok(false); };
        anyhow::ensure!(sequence >= reservation.generation,
            "Source recovery revision allocator lost its durable reservation");
        advance_revision(&wtx, base.project_id())?;
        mark_head_required(&wtx, base.project_id())?;
        wtx.open_table(META)?.insert(flag_key(base.project_id(), MANAGED).as_str(), b"1".as_slice())?;
        {
            let mut meta = wtx.open_table(META)?;
            meta.insert(flag_key(base.project_id(), IDENTITY_VERIFIED).as_str(), b"1".as_slice())?;
            meta.insert(head_key(base.project_id()).as_str(), reservation.generation.to_string().as_bytes())?;
        }
        wtx.commit()?;
        Ok(true)
    }

    /// Initial registration is visible together with its unpublished head.
    /// It must never reset an existing project, including a legacy one.
    pub fn register_source_project(&self, project: &ProjectRecord) -> anyhow::Result<()> {
        vk("project_id", &project.project_id)?;
        let wtx = self.db.begin_write()?;
        anyhow::ensure!(
            wtx.open_table(PROJECTS)?
                .get(project.project_id.as_str())?
                .is_none(),
            "Source project is already registered"
        );
        wtx.open_table(PROJECTS)?.insert(
            project.project_id.as_str(),
            serde_json::to_vec(project)?.as_slice(),
        )?;
        wtx.open_table(META)?
            .insert(head_key(&project.project_id).as_str(), b"0".as_slice())?;
        advance_revision(&wtx, &project.project_id)?;
        mark_head_required(&wtx, &project.project_id)?;
        mark_identity_unverified(&wtx, &project.project_id)?;
        wtx.commit()?;
        Ok(())
    }

    /// Distinguish valid legacy absence from an explicit unpublished zero.
    /// The index/catalog layer must reject absence for managed source stores.
    pub fn source_generation_metadata(&self, project_id: &str) -> anyhow::Result<Option<u64>> {
        vk("project_id", project_id)?;
        let rtx = self.db.begin_read()?;
        let meta = rtx.open_table(META)?;
        let raw = meta
            .get(head_key(project_id).as_str())?
            .map(|v| bounded_text(v.value()))
            .transpose()?;
        let required = meta.get(flag_key(project_id, REQUIRED).as_str())?;
        check_presence(raw.as_deref(), required.as_ref().map(|v| v.value()))?;
        let managed = meta.get(flag_key(project_id, MANAGED).as_str())?;
        validate_managed(managed.as_ref().map(|value| value.value()))?;
        anyhow::ensure!(managed.is_none() || raw.is_some(),
            "Managed source head metadata is missing; explicit full source recovery required");
        anyhow::ensure!(managed.is_none() || generation(project_id, raw.as_deref())? != 0,
            "Managed source head is contradictory zero; explicit source recovery required");
        raw.map(|value| generation(project_id, Some(&value)))
            .transpose()
    }

    /// Read generation and independent layout evidence atomically. This does
    /// not pin source data, initialize metadata, or affect knowledge namespaces.
    pub fn source_snapshot_requirement(&self, project_id: &str) -> anyhow::Result<(u64, SourceSnapshotRequirement)> {
        vk("project_id", project_id)?;
        let tx = self.db.begin_read()?;
        anyhow::ensure!(tx.open_table(PROJECTS)?.get(project_id)?.is_some(), "Source project is not registered");
        let meta = tx.open_table(META)?;
        let raw = meta.get(head_key(project_id).as_str())?
            .map(|value| bounded_text(value.value())).transpose()?;
        let required = meta.get(flag_key(project_id, REQUIRED).as_str())?;
        check_presence(raw.as_deref(), required.as_ref().map(|value| value.value()))?;
        let managed = meta.get(flag_key(project_id, MANAGED).as_str())?;
        validate_managed(managed.as_ref().map(|value| value.value()))?;
        anyhow::ensure!(managed.is_none() || raw.is_some(),
            "Managed source head metadata is missing; explicit full source recovery required");
        let legacy = meta.get(flag_key(project_id, LEGACY_ATTESTED).as_str())?;
        anyhow::ensure!(legacy.as_ref().is_none_or(|value| value.value() == b"1"), "Invalid legacy source head attestation");
        let generation = generation(project_id, raw.as_deref())?;
        anyhow::ensure!(managed.is_none() || generation != 0,
            "Managed source head is contradictory zero; explicit source recovery required");
        let requirement = if managed.is_some() { SourceSnapshotRequirement::Managed }
            else if generation == 0 { SourceSnapshotRequirement::Unpublished }
            else if raw.is_none() && required.is_none() && legacy.is_some() { SourceSnapshotRequirement::AttestedLegacy }
            else { SourceSnapshotRequirement::Unproven };
        Ok((generation, requirement))
    }

    /// Pure read; absence retains the legacy generation-one convention.
    pub fn active_source_generation(&self, project_id: &str) -> anyhow::Result<u64> {
        Ok(self.source_generation_metadata(project_id)?.unwrap_or(1))
    }

    /// Capture project existence, exact metadata and its revision in one
    /// transaction. Only a pre-revision legacy project needs a lazy write;
    /// normal/no-op refreshes read this token without committing metadata.
    pub fn capture_source_head(&self, project_id: &str) -> anyhow::Result<SourceHeadToken> {
        vk("project_id", project_id)?;
        {
            let rtx = self.db.begin_read()?;
            let project: ProjectRecord = rtx
                .open_table(PROJECTS)?
                .get(project_id)?
                .map(|v| serde_json::from_slice(v.value()))
                .transpose()?
                .ok_or_else(|| anyhow::anyhow!("Source project is not registered"))?;
            let meta = rtx.open_table(META)?;
            let raw_head = meta
                .get(head_key(project_id).as_str())?
                .map(|value| bounded_text(value.value()))
                .transpose()?;
            let generation = generation(project_id, raw_head.as_deref())?;
            let required = meta.get(flag_key(project_id, REQUIRED).as_str())?;
            check_presence(raw_head.as_deref(), required.as_ref().map(|v| v.value()))?;
            let managed = meta.get(flag_key(project_id, MANAGED).as_str())?
                .map(|value| { validate_managed(Some(value.value()))?; Ok::<_, anyhow::Error>(value.value().to_vec()) }).transpose()?;
            anyhow::ensure!(managed.is_none() || raw_head.is_some(),
                "Managed source head metadata is missing; explicit full source recovery required");
            anyhow::ensure!(managed.is_none() || generation != 0,
                "Managed source head is contradictory zero; explicit source recovery required");
            let epoch = meta.get(flag_key(project_id, IDENTITY_EPOCH).as_str())?
                .map(|value| revision(value.value())).transpose()?;
            if let (Some(revision), Some(identity_epoch)) = (meta
                .get(revision_key(project_id).as_str())?
                .map(|value| revision(value.value()))
                .transpose()?, epoch)
            {
                return Ok(SourceHeadToken {
                    project_id: project_id.into(),
                    raw_head,
                    managed,
                    generation,
                    revision,
                    identity_epoch,
                    directory: project.directory,
                    project_type: project.project_type,
                });
            }
        }
        let wtx = self.db.begin_write()?;
        let project: ProjectRecord = wtx
            .open_table(PROJECTS)?
            .get(project_id)?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("Source project is not registered"))?;
        let (raw_head, managed, existing_revision, existing_epoch) = {
            let meta = wtx.open_table(META)?;
            let raw = meta
                .get(head_key(project_id).as_str())?
                .map(|v| bounded_text(v.value()))
                .transpose()?;
            let required = meta.get(flag_key(project_id, REQUIRED).as_str())?;
            check_presence(raw.as_deref(), required.as_ref().map(|v| v.value()))?;
            let managed = meta.get(flag_key(project_id, MANAGED).as_str())?
                .map(|value| { validate_managed(Some(value.value()))?; Ok::<_, anyhow::Error>(value.value().to_vec()) }).transpose()?;
            anyhow::ensure!(managed.is_none() || raw.is_some(),
                "Managed source head metadata is missing; explicit full source recovery required");
            (
                raw,
                managed,
                meta.get(revision_key(project_id).as_str())?
                    .map(|v| revision(v.value()))
                    .transpose()?,
                meta.get(flag_key(project_id, IDENTITY_EPOCH).as_str())?
                    .map(|v| revision(v.value())).transpose()?,
            )
        };
        let generation = generation(project_id, raw_head.as_deref())?;
        anyhow::ensure!(managed.is_none() || generation != 0,
            "Managed source head is contradictory zero; explicit source recovery required");
        let mut revision = match existing_revision {
            Some(value) => value,
            None => advance_revision(&wtx, project_id)?,
        };
        let identity_epoch = match existing_epoch {
            Some(epoch) => epoch,
            None => {
                let epoch = mark_identity_unverified(&wtx, project_id)?;
                revision = epoch;
                epoch
            }
        };
        wtx.commit()?;
        Ok(SourceHeadToken {
            project_id: project_id.into(),
            raw_head,
            managed,
            generation,
            revision,
            identity_epoch,
            directory: project.directory,
            project_type: project.project_type,
        })
    }

    pub fn legacy_source_head_attested(&self, project_id: &str) -> anyhow::Result<bool> {
        let flag = self.get_meta_bounded(project_id, LEGACY_ATTESTED, 1)?;
        anyhow::ensure!(
            flag.as_deref().is_none_or(|value| value == "1"),
            "Invalid legacy source head attestation"
        );
        Ok(flag.is_some())
    }

    /// Identity changes invalidate stat-cache trust, not every file's bytes.
    /// Reverify inherited fingerprints once; steady no-op scans stay cheap.
    pub fn source_identity_needs_verification(&self, project_id: &str) -> anyhow::Result<bool> {
        Ok(self.source_stat_cache_state(project_id)?.0)
    }

    /// Verification state and epoch are observed in the same read transaction.
    pub fn source_stat_cache_state(&self, project_id: &str) -> anyhow::Result<(bool, Option<u64>)> {
        vk("project_id", project_id)?;
        let tx = self.db.begin_read()?;
        let table = tx.open_table(META)?;
        let value = table.get(flag_key(project_id, IDENTITY_VERIFIED).as_str())?;
        let epoch = table.get(flag_key(project_id, IDENTITY_EPOCH).as_str())?
            .map(|v| revision(v.value())).transpose()?;
        Ok((!identity_verified(value.as_ref().map(|v| v.value()))?, epoch))
    }

    /// Pure read. Missing epochs never authorize a derived file-stat hint.
    pub fn source_identity_epoch(&self, project_id: &str) -> anyhow::Result<Option<u64>> {
        vk("project_id", project_id)?;
        let tx = self.db.begin_read()?;
        let meta = tx.open_table(META)?;
        let epoch = meta.get(flag_key(project_id, IDENTITY_EPOCH).as_str())?
            .map(|value| revision(value.value())).transpose()?;
        Ok(epoch)
    }

    /// Read-only validation for derived observations, not source attestation.
    pub fn source_head_is_current(&self, base: &SourceHeadToken) -> anyhow::Result<bool> {
        let tx = self.db.begin_read()?;
        let project: Option<ProjectRecord> = tx.open_table(PROJECTS)?
            .get(base.project_id.as_str())?
            .map(|v| serde_json::from_slice(v.value())).transpose()?;
        if !project.is_some_and(|p| base.matches_source_identity(&p.directory, &p.project_type)) {
            return Ok(false);
        }
        let meta = tx.open_table(META)?;
        let current = meta.get(head_key(&base.project_id).as_str())?;
        Ok(meta.get(revision_key(&base.project_id).as_str())?
            .map(|v| revision(v.value())).transpose()? == Some(base.revision)
            && meta.get(flag_key(&base.project_id, IDENTITY_EPOCH).as_str())?
                .map(|v| revision(v.value())).transpose()? == Some(base.identity_epoch)
            && current.as_ref().map(|v| v.value()) == base.raw_head.as_deref().map(str::as_bytes)
            && meta.get(flag_key(&base.project_id, MANAGED).as_str())?.as_ref().map(|v| v.value()) == base.managed.as_deref())
    }

    /// Caller has compared the complete eligible file set and hashes using
    /// this source identity. No source generation or chunk needs rewriting if
    /// all bytes match. The already-verified path commits no metadata at all.
    pub fn attest_unchanged_source_identity(&self, base: &SourceHeadToken) -> anyhow::Result<bool> {
        {
            let tx = self.db.begin_read()?;
            let project: Option<ProjectRecord> = tx
                .open_table(PROJECTS)?
                .get(base.project_id.as_str())?
                .map(|v| serde_json::from_slice(v.value()))
                .transpose()?;
            if !project.is_some_and(|p| base.matches_source_identity(&p.directory, &p.project_type))
            {
                return Ok(false);
            }
            let meta = tx.open_table(META)?;
            let current = meta.get(head_key(&base.project_id).as_str())?;
            let rev = meta
                .get(revision_key(&base.project_id).as_str())?
                .map(|v| revision(v.value()))
                .transpose()?;
            if meta.get(flag_key(&base.project_id, IDENTITY_EPOCH).as_str())?
                .map(|v| revision(v.value())).transpose()? != Some(base.identity_epoch)
                || rev != Some(base.revision)
                || meta.get(flag_key(&base.project_id, MANAGED).as_str())?.as_ref().map(|v| v.value()) != base.managed.as_deref()
                || current.as_ref().map(|v| v.value())
                    != base.raw_head.as_deref().map(str::as_bytes)
            {
                return Ok(false);
            }
            let verified = meta.get(flag_key(&base.project_id, IDENTITY_VERIFIED).as_str())?;
            if identity_verified(verified.as_ref().map(|v| v.value()))? {
                return Ok(true);
            }
        }
        let tx = self.db.begin_write()?;
        let project: Option<ProjectRecord> = tx
            .open_table(PROJECTS)?
            .get(base.project_id.as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?;
        if !project.is_some_and(|p| base.matches_source_identity(&p.directory, &p.project_type)) {
            return Ok(false);
        }
        {
            let meta = tx.open_table(META)?;
            let current = meta.get(head_key(&base.project_id).as_str())?;
            let rev = meta
                .get(revision_key(&base.project_id).as_str())?
                .map(|v| revision(v.value()))
                .transpose()?;
            if meta.get(flag_key(&base.project_id, IDENTITY_EPOCH).as_str())?
                .map(|v| revision(v.value())).transpose()? != Some(base.identity_epoch)
                || rev != Some(base.revision)
                || meta.get(flag_key(&base.project_id, MANAGED).as_str())?.as_ref().map(|v| v.value()) != base.managed.as_deref()
                || current.as_ref().map(|v| v.value())
                    != base.raw_head.as_deref().map(str::as_bytes)
            {
                return Ok(false);
            }
            let verified = meta.get(flag_key(&base.project_id, IDENTITY_VERIFIED).as_str())?;
            if identity_verified(verified.as_ref().map(|v| v.value()))? {
                return Ok(true);
            }
        }
        advance_revision(&tx, &base.project_id)?;
        tx.open_table(META)?.insert(
            flag_key(&base.project_id, IDENTITY_VERIFIED).as_str(),
            b"1".as_slice(),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The index layer calls this only after observing a genuine legacy store
    /// before staging. The proof survives an abandoned first migration, but
    /// never overrides a later explicit/published head that has gone missing.
    pub fn attest_legacy_source_head(&self, base: &SourceHeadToken) -> anyhow::Result<bool> {
        anyhow::ensure!(
            base.raw_head.is_none(),
            "Legacy absence attestation needs an absent head"
        );
        let wtx = self.db.begin_write()?;
        let project: Option<ProjectRecord> = wtx
            .open_table(PROJECTS)?
            .get(base.project_id.as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?;
        if !project.is_some_and(|p| base.matches_source_identity(&p.directory, &p.project_type)) {
            return Ok(false);
        }
        {
            let meta = wtx.open_table(META)?;
            if meta.get(flag_key(&base.project_id, IDENTITY_EPOCH).as_str())?
                .map(|v| revision(v.value())).transpose()? != Some(base.identity_epoch)
                || meta.get(flag_key(&base.project_id, MANAGED).as_str())?.as_ref().map(|v| v.value()) != base.managed.as_deref()
                || base.managed.is_some()
                || meta.get(head_key(&base.project_id).as_str())?.is_some()
                || meta
                    .get(flag_key(&base.project_id, REQUIRED).as_str())?
                    .is_some()
                || meta
                    .get(revision_key(&base.project_id).as_str())?
                    .map(|v| revision(v.value()))
                    .transpose()?
                    != Some(base.revision)
            {
                return Ok(false);
            }
            if let Some(flag) = meta.get(flag_key(&base.project_id, LEGACY_ATTESTED).as_str())? {
                anyhow::ensure!(
                    flag.value() == b"1",
                    "Invalid legacy source head attestation"
                );
                return Ok(true);
            }
        }
        wtx.open_table(META)?.insert(
            flag_key(&base.project_id, LEGACY_ATTESTED).as_str(),
            b"1".as_slice(),
        )?;
        wtx.commit()?;
        Ok(true)
    }

    /// Publish only against the exact captured head and registration lifetime.
    /// A stale writer changes neither the head nor the revision. Callers must
    /// finish/seal all required stores before attempting this commit.
    pub fn publish_source_head(&self, base: &SourceHeadToken, next: u64) -> anyhow::Result<bool> {
        anyhow::ensure!(
            next > base.generation,
            "Source generation must advance its base"
        );
        let wtx = self.db.begin_write()?;
        let project: Option<ProjectRecord> = wtx
            .open_table(PROJECTS)?
            .get(base.project_id.as_str())?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?;
        if !project.is_some_and(|p| base.matches_source_identity(&p.directory, &p.project_type)) {
            return Ok(false);
        }
        {
            let meta = wtx.open_table(META)?;
            let current_revision = meta
                .get(revision_key(&base.project_id).as_str())?
                .map(|v| revision(v.value()))
                .transpose()?;
            let current = meta.get(head_key(&base.project_id).as_str())?;
            if meta.get(flag_key(&base.project_id, IDENTITY_EPOCH).as_str())?
                .map(|v| revision(v.value())).transpose()? != Some(base.identity_epoch)
                || current_revision != Some(base.revision)
                || meta.get(flag_key(&base.project_id, MANAGED).as_str())?.as_ref().map(|v| v.value()) != base.managed.as_deref()
                || current.as_ref().map(|v| v.value())
                    != base.raw_head.as_deref().map(str::as_bytes)
            {
                return Ok(false);
            }
        }
        advance_revision(&wtx, &base.project_id)?;
        mark_head_required(&wtx, &base.project_id)?;
        wtx.open_table(META)?.insert(flag_key(&base.project_id, MANAGED).as_str(), b"1".as_slice())?;
        wtx.open_table(META)?.insert(
            flag_key(&base.project_id, IDENTITY_VERIFIED).as_str(),
            b"1".as_slice(),
        )?;
        wtx.open_table(META)?.insert(
            head_key(&base.project_id).as_str(),
            next.to_string().as_bytes(),
        )?;
        wtx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn project() -> ProjectRecord {
        ProjectRecord {
            project_id: "project".into(),
            project_name: "Source publication".into(),
            project_type: "general".into(),
            directory: "source".into(),
            created_at_ms: 1,
            updated_at_ms: 1,
            reindex_required_since_ms: None,
        }
    }

    #[test]
    fn legacy_absent_head_can_publish_without_first_inventing_a_stored_generation() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        let base = registry.capture_source_head("project").unwrap();
        assert_eq!(base.generation(), 1);
        assert_eq!(registry.get_meta("project", HEAD).unwrap(), None);
        assert!(registry.publish_source_head(&base, 2).unwrap());
        assert_eq!(registry.active_source_generation("project").unwrap(), 2);
        assert!(!registry.publish_source_head(&base, 3).unwrap());
        assert_eq!(registry.active_source_generation("project").unwrap(), 2);
    }

    #[test]
    fn deletion_and_identical_recreation_never_accept_an_old_publication_token() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        let old = registry.capture_source_head("project").unwrap();
        registry.delete_all_for_project("project").unwrap();
        assert!(!registry.publish_source_head(&old, 2).unwrap());
        assert_eq!(registry.get_meta("project", HEAD).unwrap(), None);
        registry.put_project(&project()).unwrap();
        let recreated = registry.capture_source_head("project").unwrap();
        assert!(!registry.publish_source_head(&old, 2).unwrap());
        assert!(registry.publish_source_head(&recreated, 2).unwrap());
    }

    #[test]
    fn head_changed_away_and_back_rejects_old_work_across_registry_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        let old = {
            let registry = Registry::open(&path).unwrap();
            registry.put_project(&project()).unwrap();
            registry.set_meta("project", HEAD, "1").unwrap();
            let token = registry.capture_source_head("project").unwrap();
            registry.set_meta("project", HEAD, "2").unwrap();
            assert!(
                registry
                    .compare_exchange_meta("project", HEAD, Some("2"), "1")
                    .unwrap()
            );
            token
        };
        let registry = Registry::open(&path).unwrap();
        assert!(!registry.publish_source_head(&old, 2).unwrap());
        assert_eq!(registry.active_source_generation("project").unwrap(), 1);
        let current = registry.capture_source_head("project").unwrap();
        assert!(registry.publish_source_head(&current, 2).unwrap());
    }

    #[test]
    fn competing_publications_have_one_winner_and_unrelated_edits_do_not_invalidate_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        registry.set_meta("project", HEAD, "0").unwrap();
        let base = registry.capture_source_head("project").unwrap();
        registry
            .set_meta("project", "unrelated", "human note")
            .unwrap();
        let mut renamed = project();
        renamed.project_name = "Human rename".into();
        registry.put_project(&renamed).unwrap();
        let first = registry.clone();
        let old = base.clone();
        let second = registry.clone();
        let a = std::thread::spawn(move || first.publish_source_head(&old, 1).unwrap());
        let b = std::thread::spawn(move || second.publish_source_head(&base, 1).unwrap());
        assert_eq!(u8::from(a.join().unwrap()) + u8::from(b.join().unwrap()), 1);
        assert_eq!(
            registry
                .get_project("project")
                .unwrap()
                .unwrap()
                .project_name,
            "Human rename"
        );
        assert_eq!(
            registry
                .get_meta("project", "unrelated")
                .unwrap()
                .as_deref(),
            Some("human note")
        );
    }

    #[test]
    fn invalid_head_is_neither_guessed_nor_overwritten_by_token_capture() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        registry
            .set_meta("project", HEAD, "not-a-generation")
            .unwrap();
        assert!(registry.capture_source_head("project").is_err());
        assert!(registry.active_source_generation("project").is_err());
        assert_eq!(
            registry.get_meta("project", HEAD).unwrap().as_deref(),
            Some("not-a-generation")
        );
        assert!(registry.capture_source_head("missing").is_err());
        assert_eq!(registry.get_meta("missing", REVISION).unwrap(), None);
    }

    #[test]
    fn legacy_revision_initializes_once_and_idempotent_head_writes_keep_valid_work() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        // A real pre-revision record, as written by an older binary.
        let tx = registry.db.begin_write().unwrap();
        tx.open_table(PROJECTS)
            .unwrap()
            .insert(
                "project",
                serde_json::to_vec(&project()).unwrap().as_slice(),
            )
            .unwrap();
        tx.commit().unwrap();
        let first = registry.capture_source_head("project").unwrap();
        let next = registry.capture_source_head("project").unwrap();
        assert_eq!(first.revision, next.revision);
        registry.set_meta("project", HEAD, "1").unwrap();
        let base = registry.capture_source_head("project").unwrap();
        registry.set_meta("project", HEAD, "1").unwrap();
        assert!(registry.publish_source_head(&base, 2).unwrap());
    }

    #[test]
    fn initial_registration_failure_leaves_no_project_or_partial_head_and_conflicts_preserve_existing_data()
     {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        // Exhaust the persisted allocator to fail before the registration
        // transaction can commit. No partially visible project may remain.
        let tx = registry.db.begin_write().unwrap();
        tx.open_table(META)
            .unwrap()
            .insert(
                "\0reindex_revision_sequence_v1",
                u64::MAX.to_string().as_bytes(),
            )
            .unwrap();
        tx.commit().unwrap();
        assert!(registry.register_source_project(&project()).is_err());
        assert!(registry.get_project("project").unwrap().is_none());
        assert_eq!(registry.get_meta("project", HEAD).unwrap(), None);
        assert_eq!(registry.get_meta("project", REVISION).unwrap(), None);
        let tx = registry.db.begin_write().unwrap();
        tx.open_table(META)
            .unwrap()
            .insert("\0reindex_revision_sequence_v1", b"0".as_slice())
            .unwrap();
        tx.commit().unwrap();
        registry.register_source_project(&project()).unwrap();
        assert_eq!(
            registry
                .capture_source_head("project")
                .unwrap()
                .generation(),
            0
        );
        registry.set_meta("project", HEAD, "7").unwrap();
        assert!(registry.register_source_project(&project()).is_err());
        assert_eq!(registry.active_source_generation("project").unwrap(), 7);
    }

    #[test]
    fn valid_noncanonical_legacy_value_is_compared_exactly_without_guessing_its_stored_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        registry.set_meta("project", HEAD, "01").unwrap();
        let base = registry.capture_source_head("project").unwrap();
        assert_eq!(base.generation(), 1);
        assert!(registry.publish_source_head(&base, 2).unwrap());
        assert_eq!(registry.active_source_generation("project").unwrap(), 2);
    }

    #[test]
    fn missing_explicit_or_published_head_never_reverts_to_legacy_default_even_with_old_attestation()
     {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.redb");
        {
            let registry = Registry::open(&path).unwrap();
            registry.put_project(&project()).unwrap();
            let base = registry.capture_source_head("project").unwrap();
            assert!(registry.attest_legacy_source_head(&base).unwrap());
            assert!(registry.publish_source_head(&base, 2).unwrap());
            let tx = registry.db.begin_write().unwrap();
            tx.open_table(META)
                .unwrap()
                .remove(head_key("project").as_str())
                .unwrap();
            tx.commit().unwrap();
        }
        let registry = Registry::open(&path).unwrap();
        assert!(registry.legacy_source_head_attested("project").unwrap());
        assert!(registry.source_generation_metadata("project").is_err());
        assert!(registry.capture_source_head("project").is_err());
        assert_eq!(registry.get_meta("project", HEAD).unwrap(), None);
    }

    #[test]
    fn source_directory_and_type_changes_invalidate_work_but_not_human_names() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        let base = registry.capture_source_head("project").unwrap();
        let mut edited = project();
        edited.directory = "different-source".into();
        registry.put_project(&edited).unwrap();
        registry.put_project(&project()).unwrap();
        assert!(!registry.publish_source_head(&base, 2).unwrap());
        let base = registry.capture_source_head("project").unwrap();
        edited = project();
        edited.project_type = "rust".into();
        registry.put_project(&edited).unwrap();
        assert!(!registry.publish_source_head(&base, 2).unwrap());
    }

    #[test]
    fn identity_verification_is_conditional_and_repeated_noops_do_not_invalidate_current_work() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.register_source_project(&project()).unwrap();
        let initial = registry.capture_source_head("project").unwrap();
        assert!(registry.publish_source_head(&initial, 1).unwrap());
        assert!(
            !registry
                .source_identity_needs_verification("project")
                .unwrap()
        );
        let base = registry.capture_source_head("project").unwrap();
        assert!(registry.attest_unchanged_source_identity(&base).unwrap());
        assert!(registry.attest_unchanged_source_identity(&base).unwrap());
        assert!(
            registry.publish_source_head(&base, 2).unwrap(),
            "a verified no-op must not invalidate pending work"
        );
        let old = registry.capture_source_head("project").unwrap();
        let mut moved = project();
        moved.directory = "replacement-source".into();
        registry.put_project(&moved).unwrap();
        assert!(
            registry
                .source_identity_needs_verification("project")
                .unwrap()
        );
        assert!(!registry.attest_unchanged_source_identity(&old).unwrap());
        assert!(
            registry
                .source_identity_needs_verification("project")
                .unwrap()
        );
        let current = registry.capture_source_head("project").unwrap();
        assert!(registry.attest_unchanged_source_identity(&current).unwrap());
        assert!(
            !registry
                .source_identity_needs_verification("project")
                .unwrap()
        );
        assert_eq!(registry.active_source_generation("project").unwrap(), 2);
    }

    #[test]
    fn source_identity_epoch_survives_publication_but_not_retarget_or_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.register_source_project(&project()).unwrap();
        let initial = registry.capture_source_head("project").unwrap();
        assert!(registry.publish_source_head(&initial, 1).unwrap());
        let published = registry.capture_source_head("project").unwrap();
        assert_eq!(initial.identity_epoch(), published.identity_epoch());
        assert!(registry.publish_source_head(&published, 2).unwrap());
        let next = registry.capture_source_head("project").unwrap();
        assert_eq!(initial.identity_epoch(), next.identity_epoch());
        let mut renamed = project();
        renamed.project_name = "Human label".into();
        registry.put_project(&renamed).unwrap();
        assert!(registry.source_head_is_current(&next).unwrap());
        let mut moved = project();
        moved.directory = "another-source".into();
        registry.put_project(&moved).unwrap();
        registry.put_project(&project()).unwrap();
        let returned = registry.capture_source_head("project").unwrap();
        assert_ne!(returned.identity_epoch(), initial.identity_epoch());
        assert!(!registry.source_head_is_current(&next).unwrap());
        registry.delete_all_for_project("project").unwrap();
        registry.put_project(&project()).unwrap();
        let recreated = registry.capture_source_head("project").unwrap();
        assert_ne!(returned.identity_epoch(), recreated.identity_epoch());
        assert!(!registry.source_head_is_current(&returned).unwrap());
    }

    #[test]
    fn lost_source_epoch_is_readonly_until_capture_and_cannot_reissue_prior_trust() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.register_source_project(&project()).unwrap();
        let before = registry.capture_source_head("project").unwrap();
        assert!(registry.publish_source_head(&before, 1).unwrap());
        let current = registry.capture_source_head("project").unwrap();
        let tx = registry.db.begin_write().unwrap();
        {
            let mut meta = tx.open_table(META).unwrap();
            meta.remove(flag_key("project", IDENTITY_EPOCH).as_str()).unwrap();
            // Surviving project-local floors must also cover sequence loss.
            meta.remove("\0reindex_revision_sequence_v1").unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(registry.source_identity_epoch("project").unwrap(), None);
        assert!(!registry.source_head_is_current(&current).unwrap());
        assert_eq!(registry.source_identity_epoch("project").unwrap(), None);
        let restored = registry.capture_source_head("project").unwrap();
        assert!(restored.identity_epoch() > current.identity_epoch());
        assert!(registry.source_identity_needs_verification("project").unwrap());
        assert!(!registry.publish_source_head(&current, 2).unwrap());
        registry.set_meta("project", IDENTITY_EPOCH, "broken").unwrap();
        assert!(registry.capture_source_head("project").is_err());
        assert!(registry.source_stat_cache_state("project").is_err());
        assert_eq!(registry.get_meta("project", IDENTITY_EPOCH).unwrap().as_deref(), Some("broken"));
    }

    #[test]
    fn missing_epoch_does_not_strand_an_attested_legacy_migration() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(&dir.path().join("registry.redb")).unwrap();
        registry.put_project(&project()).unwrap();
        let initial = registry.capture_source_head("project").unwrap();
        assert!(registry.attest_legacy_source_head(&initial).unwrap());
        let tx = registry.db.begin_write().unwrap();
        tx.open_table(META).unwrap().remove(flag_key("project", IDENTITY_EPOCH).as_str()).unwrap();
        tx.commit().unwrap();
        let resumed = registry.capture_source_head("project").unwrap();
        assert!(registry.legacy_source_head_attested("project").unwrap());
        assert!(registry.attest_legacy_source_head(&resumed).unwrap());
        assert!(!registry.publish_source_head(&initial, 2).unwrap());
        assert!(registry.publish_source_head(&resumed, 2).unwrap());
    }
}

#[cfg(test)]
mod managed_layout_tests {
    use super::*;

    fn project() -> ProjectRecord {
        ProjectRecord { project_id: "layout".into(), project_name: "Owned layout".into(),
            project_type: "general".into(), directory: "owned-source".into(), created_at_ms: 1,
            updated_at_ms: 1, reindex_required_since_ms: None }
    }

    fn remove_marker(registry: &Registry) {
        let tx = registry.db.begin_write().unwrap();
        tx.open_table(META).unwrap().remove(flag_key("layout", MANAGED).as_str()).unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn source_publication_atomically_installs_layout_and_marker_edits_invalidate_exact_authority() -> anyhow::Result<()> {
        let owned = tempfile::tempdir()?;
        let result = (|| -> anyhow::Result<()> {
            let registry = Registry::open(&owned.path().join("registry.redb"))?;
            registry.register_source_project(&project())?;
            assert_eq!(registry.source_snapshot_requirement("layout")?,
                (0, SourceSnapshotRequirement::Unpublished));
            let initial = registry.capture_source_head("layout")?;
            assert!(registry.publish_source_head(&initial, 1)?);
            assert_eq!(registry.source_snapshot_requirement("layout")?,
                (1, SourceSnapshotRequirement::Managed));
            let stale = registry.capture_source_head("layout")?;
            remove_marker(&registry);
            assert!(!registry.source_head_is_current(&stale)?);
            assert!(!registry.attest_unchanged_source_identity(&stale)?);
            assert!(!registry.publish_source_head(&stale, 2)?);
            assert_eq!(registry.active_source_generation("layout")?, 1);
            assert_eq!(registry.source_snapshot_requirement("layout")?,
                (1, SourceSnapshotRequirement::Unproven));
            // A reservation also captures the exact independent mode marker.
            registry.set_meta("layout", MANAGED, "1")?;
            registry.set_meta("layout", HEAD, "damaged-owned-head")?;
            let recovery = registry.capture_source_recovery("layout")?;
            let reservation = registry.reserve_source_recovery(&recovery, 90)?.unwrap();
            remove_marker(&registry);
            assert!(registry.reserve_source_recovery(&recovery, 90)?.is_none());
            assert!(!registry.publish_source_recovery(&reservation)?);
            assert_eq!(registry.get_meta("layout", HEAD)?.as_deref(), Some("damaged-owned-head"));
            // Existing durable allocator/identity authority permits explicit
            // recovery of unproven absence, never a guessed legacy generation.
            let tx = registry.db.begin_write()?;
            tx.open_table(META)?.remove(head_key("layout").as_str())?;
            tx.open_table(META)?.remove(flag_key("layout", REQUIRED).as_str())?;
            tx.commit()?;
            let absent = registry.capture_source_recovery("layout")?;
            let reserved = registry.reserve_source_recovery(&absent, reservation.generation())?.unwrap();
            assert!(reserved.generation() > reservation.generation());
            assert!(registry.publish_source_recovery(&reserved)?);
            assert_eq!(registry.source_snapshot_requirement("layout")?,
                (reserved.generation(), SourceSnapshotRequirement::Managed));
            registry.set_meta("layout", LEGACY_ATTESTED, "1")?;
            let tx = registry.db.begin_write()?;
            tx.open_table(META)?.remove(head_key("layout").as_str())?;
            tx.open_table(META)?.remove(flag_key("layout", REQUIRED).as_str())?;
            tx.commit()?;
            assert!(registry.capture_source_head("layout").is_err());
            assert!(registry.source_generation_metadata("layout").is_err());
            assert!(registry.source_snapshot_requirement("layout").is_err());
            let damaged = registry.capture_source_recovery("layout")?;
            let safe = registry.reserve_source_recovery(&damaged, 10_000)?.unwrap();
            assert!(safe.generation() > 10_000);
            assert!(registry.publish_source_recovery(&safe)?);
            registry.set_meta("layout", HEAD, "0")?;
            assert!(registry.capture_source_head("layout").is_err());
            assert!(registry.source_generation_metadata("layout").is_err());
            assert!(registry.source_snapshot_requirement("layout").is_err());
            let contradictory = registry.capture_source_recovery("layout")?;
            let repaired_zero = registry.reserve_source_recovery(&contradictory, 20_000)?.unwrap();
            assert!(repaired_zero.generation() > 20_000);
            assert!(registry.publish_source_recovery(&repaired_zero)?);
            drop(registry);
            Ok(())
        })();
        let cleanup = owned.close(); result?; cleanup?; Ok(())
    }
}
