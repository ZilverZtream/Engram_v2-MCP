//! Exact read-only ownership inventory. Missing identity and malformed receipts
//! remain evidence, never an implicit adoption or repair authorization.
use super::*;
use std::collections::BTreeMap;

const RECEIPT_PREFIX: &str = "quality_source:";
const IDENTITY_EPOCH: &str = "source_identity_epoch_v1";
const SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const VALUE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuleOwnershipSnapshot {
    project_id: String,
    project_raw: Vec<u8>,
    identity_epoch_raw: Option<Vec<u8>>,
    corpus_revision_raw: Option<Vec<u8>>,
    rules: BTreeMap<String, Vec<u8>>,
    receipts: BTreeMap<String, Vec<u8>>,
}

impl RuleOwnershipSnapshot {
    /// Keep a successful adoption inside the same limits that its next reader
    /// must satisfy, including accumulated receipts and aliases.
    pub(super) fn check_adoption_budget(
        &self,
        rows: &BTreeMap<String, Vec<u8>>,
        receipt_key: &str,
        receipt: &[u8],
    ) -> anyhow::Result<()> {
        let mut remaining = SNAPSHOT_BYTES;
        let mut admit = |key: &str, raw: &[u8]| -> anyhow::Result<()> {
            anyhow::ensure!(
                raw.len() <= VALUE_BYTES && key.len() <= remaining,
                "Resulting ownership inventory exceeds its read budget; no rows adopted"
            );
            remaining -= key.len();
            anyhow::ensure!(
                raw.len() <= remaining,
                "Resulting ownership inventory exceeds its read budget; no rows adopted"
            );
            remaining -= raw.len();
            Ok(())
        };
        admit("", &self.project_raw)?;
        if let Some(raw) = &self.identity_epoch_raw {
            admit("", raw)?;
        }
        if let Some(raw) = &self.corpus_revision_raw {
            admit("", raw)?;
        }
        for (key, raw) in &self.rules {
            admit(key, rows.get(key).unwrap_or(raw))?;
        }
        anyhow::ensure!(
            self.receipts.len() + usize::from(!self.receipts.contains_key(receipt_key)) <= 256,
            "Resulting ownership inventory exceeds receipt count budget; no rows adopted"
        );
        for (key, raw) in &self.receipts {
            if key != receipt_key {
                admit(key, raw)?;
            }
        }
        admit(receipt_key, receipt)
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }
    pub fn project_raw(&self) -> &[u8] {
        &self.project_raw
    }
    pub fn project(&self) -> anyhow::Result<ProjectRecord> {
        let project: ProjectRecord = serde_json::from_slice(&self.project_raw)?;
        anyhow::ensure!(
            project.project_id == self.project_id,
            "Ownership project identity mismatch"
        );
        Ok(project)
    }
    pub fn identity_epoch_raw(&self) -> Option<&[u8]> {
        self.identity_epoch_raw.as_deref()
    }
    pub fn corpus_revision_raw(&self) -> Option<&[u8]> {
        self.corpus_revision_raw.as_deref()
    }
    pub fn identity_epoch(&self) -> anyhow::Result<Option<u64>> {
        self.identity_epoch_raw
            .as_ref()
            .map(|value| {
                let epoch = std::str::from_utf8(value)?.parse::<u64>()?;
                anyhow::ensure!(epoch > 0, "Ownership identity epoch is invalid");
                Ok(epoch)
            })
            .transpose()
    }
    /// Exact stored bytes, including legacy fields, whitespace and unknown
    /// JSON fields. Decode separately; a failure must not drop a row.
    pub fn rules(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.rules
    }
    /// Every source-receipt key/value, including malformed or pending entries.
    /// This map is not a list of accepted or valid ownership claims.
    pub fn receipts(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.receipts
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        hash_value(b"engram:rule-ownership-snapshot:v1\0", self)
    }
}

pub(super) fn hash_value(domain: &[u8], value: &impl Serialize) -> anyhow::Result<String> {
    struct HashWriter(blake3::Hasher);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(blake3::Hasher::new());
    writer.0.update(domain);
    // Stream serialization instead of allocating a second encoded copy of
    // the complete raw inventory (Vec<u8> JSON can amplify its size).
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0.finalize().to_hex().to_string())
}

fn copy_bounded(bytes: &[u8], remaining: &mut usize) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        bytes.len() <= VALUE_BYTES && bytes.len() <= *remaining,
        "Ownership inventory byte budget exceeded; no partial inventory returned"
    );
    *remaining -= bytes.len();
    Ok(bytes.to_vec())
}

pub(super) fn collect_prefix(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    prefix: &str,
    retained_prefix: &str,
    max_entries: usize,
    remaining: &mut usize,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in table.range(prefix..)? {
        let (key, value) = entry?;
        let Some(suffix) = key.value().strip_prefix(prefix) else {
            break;
        };
        anyhow::ensure!(
            result.len() < max_entries && suffix.len() <= 2048,
            "Ownership inventory entry budget exceeded; no partial inventory returned"
        );
        let key_bytes = retained_prefix.len() + suffix.len();
        anyhow::ensure!(
            key_bytes <= *remaining,
            "Ownership inventory key byte budget exceeded"
        );
        *remaining -= key_bytes;
        let bytes = copy_bounded(value.value(), remaining)?;
        result.insert(format!("{retained_prefix}{suffix}"), bytes);
    }
    Ok(result)
}

impl Registry {
    /// One read transaction, no metadata initialization or corpus/index opens.
    /// At most 2048 rule rows, 256 receipts, 4MiB per value and 8MiB total raw
    /// bytes/keys. Exceeding a bound refuses the whole inventory; malformed
    /// values within bounds remain exact raw evidence for classification.
    pub fn snapshot_rule_ownership(
        &self,
        project_id: &str,
    ) -> anyhow::Result<RuleOwnershipSnapshot> {
        vk("project_id", project_id)?;
        let transaction = self.db.begin_read()?;
        let projects = transaction.open_table(PROJECTS)?;
        let rules = transaction.open_table(REPO_RULES)?;
        let meta = transaction.open_table(META)?;
        RuleOwnershipSnapshot::from_tables(project_id, &projects, &rules, &meta)
    }
}

impl RuleOwnershipSnapshot {
    pub(super) fn from_tables(
        project_id: &str,
        projects: &impl ReadableTable<&'static str, &'static [u8]>,
        rules: &impl ReadableTable<&'static str, &'static [u8]>,
        meta: &impl ReadableTable<&'static str, &'static [u8]>,
    ) -> anyhow::Result<Self> {
        let mut remaining = SNAPSHOT_BYTES;
        let project = projects
            .get(project_id)?
            .ok_or_else(|| anyhow::anyhow!("Ownership inventory project is missing"))?;
        let project_raw = copy_bounded(project.value(), &mut remaining)?;
        let identity = meta.get(format!("{project_id}\0{IDENTITY_EPOCH}").as_str())?;
        let identity_epoch_raw = identity
            .map(|v| copy_bounded(v.value(), &mut remaining))
            .transpose()?;
        let corpus_revision =
            meta.get(format!("{project_id}\0quality_corpus_revision").as_str())?;
        let corpus_revision_raw = corpus_revision
            .map(|v| copy_bounded(v.value(), &mut remaining))
            .transpose()?;
        let rules = collect_prefix(rules, &format!("{project_id}\0"), "", 2048, &mut remaining)?;
        let receipts = collect_prefix(
            meta,
            &format!("{project_id}\0{RECEIPT_PREFIX}"),
            RECEIPT_PREFIX,
            256,
            &mut remaining,
        )?;
        Ok(Self {
            project_id: project_id.to_owned(),
            project_raw,
            identity_epoch_raw,
            corpus_revision_raw,
            rules,
            receipts,
        })
    }

    /// Identity and complete ownership state, excluding project display name
    /// and activity timestamps. Unrelated source indexing must not invalidate
    /// an otherwise unchanged policy decision.
    pub fn adoption_basis_digest(&self) -> anyhow::Result<String> {
        let project = self.project()?;
        anyhow::ensure!(
            self.identity_epoch()?.is_some(),
            "A valid existing project identity epoch is required before adoption; establish identity then recollect"
        );
        hash_value(
            b"engram:rule-adoption-basis:v1\0",
            &(
                &self.project_id,
                &project.directory,
                &project.project_type,
                &self.identity_epoch_raw,
                &self.corpus_revision_raw,
                &self.rules,
                &self.receipts,
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Registry) {
        let temporary = tempfile::tempdir().unwrap();
        let registry = Registry::open(&temporary.path().join("registry.redb")).unwrap();
        registry
            .put_project(&ProjectRecord {
                project_id: "project".into(),
                project_name: "Ownership behavior".into(),
                project_type: "general".into(),
                directory: temporary.path().to_string_lossy().into_owned(),
                created_at_ms: 1,
                updated_at_ms: 1,
                reindex_required_since_ms: None,
            })
            .unwrap();
        (temporary, registry)
    }

    #[test]
    fn inventory_retains_exact_legacy_rows_and_malformed_claims_without_creating_identity() {
        let (temporary, registry) = fixture();
        let raw = "{ \"rule_id\":\"legacy\", \"file_pattern\":\"Site/**\", \"rule_text\":\"Å policy\\r\\nKeep exception.\", \"priority\":7, \"updated_at_ms\":42, \"unknown_legacy_field\":true }";
        let malformed = b"{incomplete ownership receipt";
        {
            let tx = registry.db.begin_write().unwrap();
            tx.open_table(REPO_RULES)
                .unwrap()
                .insert("project\0legacy", raw.as_bytes())
                .unwrap();
            {
                let mut meta = tx.open_table(META).unwrap();
                meta.remove("project\0source_identity_epoch_v1").unwrap();
                meta.insert("project\0quality_source:broken", malformed.as_slice())
                    .unwrap();
            }
            tx.commit().unwrap();
        }
        let before = registry.list_repo_rules("project").unwrap();
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        assert_eq!(snapshot.project().unwrap().project_type, "general");
        assert_eq!(
            snapshot.identity_epoch().unwrap(),
            None,
            "Read-only inventory must preserve a missing stable identity"
        );
        assert_eq!(snapshot.rules()["legacy"], raw.as_bytes());
        assert_eq!(
            snapshot.receipts()["quality_source:broken"],
            malformed.as_slice()
        );
        assert_eq!(registry.source_identity_epoch("project").unwrap(), None);
        assert_eq!(registry.list_repo_rules("project").unwrap(), before);
        assert_eq!(
            registry.snapshot_rule_ownership("project").unwrap(),
            snapshot,
            "Inventory must not rewrite legacy rows, unknown fields or receipt evidence"
        );
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn all_source_receipts_affect_the_basis_but_another_project_does_not() {
        let (temporary, registry) = fixture();
        let before = registry.snapshot_rule_ownership("project").unwrap();
        registry
            .set_meta("other-project", "quality_source:foreign", "foreign bytes")
            .unwrap();
        assert_eq!(
            registry
                .snapshot_rule_ownership("project")
                .unwrap()
                .digest()
                .unwrap(),
            before.digest().unwrap()
        );
        registry
            .set_meta(
                "project",
                "quality_source:unrelated-destination",
                "unknown claims must not be skipped",
            )
            .unwrap();
        let after = registry.snapshot_rule_ownership("project").unwrap();
        assert_ne!(
            after.digest().unwrap(),
            before.digest().unwrap(),
            "An added receipt can change ownership even while all rule rows stay unchanged"
        );
        assert_eq!(after.rules(), before.rules());
        registry
            .set_meta(
                "project",
                "quality_source:unrelated-destination",
                "changed malformed claims",
            )
            .unwrap();
        assert_ne!(
            registry
                .snapshot_rule_ownership("project")
                .unwrap()
                .digest()
                .unwrap(),
            after.digest().unwrap()
        );
        let before_reset = registry.snapshot_rule_ownership("project").unwrap();
        registry
            .set_meta(
                "project",
                "quality_corpus_revision",
                "explicit corpus reset",
            )
            .unwrap();
        let after_reset = registry.snapshot_rule_ownership("project").unwrap();
        assert_ne!(
            before_reset.digest().unwrap(),
            after_reset.digest().unwrap(),
            "A corpus reset must invalidate an ownership basis even if rows and receipts are unchanged"
        );
        assert_eq!(
            after_reset.corpus_revision_raw(),
            Some(b"explicit corpus reset".as_slice())
        );
        assert_eq!(before_reset.rules(), after_reset.rules());
        assert_eq!(before_reset.receipts(), after_reset.receipts());
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn receipt_budget_refuses_an_incomplete_inventory_without_removing_claims() {
        let (temporary, registry) = fixture();
        {
            let tx = registry.db.begin_write().unwrap();
            {
                let mut meta = tx.open_table(META).unwrap();
                for n in 0..257 {
                    meta.insert(
                        format!("project\0quality_source:{n:04}").as_str(),
                        b"unknown".as_slice(),
                    )
                    .unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let error = registry.snapshot_rule_ownership("project").unwrap_err();
        assert!(error.to_string().contains("no partial inventory"));
        assert_eq!(
            registry
                .get_meta("project", "quality_source:0256")
                .unwrap()
                .as_deref(),
            Some("unknown"),
            "Overflow must preserve even claims beyond the inspection budget"
        );
        drop(registry);
        temporary.close().unwrap();
    }
}
