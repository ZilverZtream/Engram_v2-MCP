//! One reviewed ownership transition; never an inferred policy acceptance.
use super::rule_ownership::hash_value;
use super::rule_receipt::{SOURCE_PROVENANCE, source_key, valid_origin};
use super::*;
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LegacyAdoptionDecision {
    AdoptUnverified,
    TransferManual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyAdoptionBinding {
    pub rule_id: String,
    pub expected_raw_blake3: String,
    pub source_rule_id: String,
    pub source_projection_json: String,
    pub decision: LegacyAdoptionDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyAdoptionPlan {
    pub origin: String,
    pub source_blake3: String,
    pub authored: bool,
    pub bindings: Vec<LegacyAdoptionBinding>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegacyAdoptionOutcome {
    pub audit_key: String,
    pub plan_blake3: String,
    pub receipt_blake3: String,
    pub adopted_rule_ids: Vec<String>,
    pub searchable_evidence_published: bool,
}

struct RawObject(BTreeMap<String, Box<RawValue>>);
impl<'de> Deserialize<'de> for RawObject {
    fn deserialize<D: serde::Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = RawObject;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a unique-key rule object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut object = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    if object.contains_key(&key) {
                        return Err(serde::de::Error::custom("Ambiguous duplicate rule field"));
                    }
                    object.insert(key, map.next_value::<Box<RawValue>>()?);
                }
                Ok(RawObject(object))
            }
        }
        input.deserialize_map(Visitor)
    }
}

pub(super) fn merge_rule_fields(raw: &[u8], replacement: &RepoRule) -> anyhow::Result<Vec<u8>> {
    let mut stored: RawObject = serde_json::from_slice(raw)?;
    let known: RawObject = serde_json::from_slice(&serde_json::to_vec(replacement)?)?;
    stored.0.extend(known.0);
    Ok(serde_json::to_vec(&stored.0)?)
}

struct Prepared {
    rows: BTreeMap<String, Vec<u8>>,
    receipt_key: String,
    receipt: Vec<u8>,
    audit_key: String,
    audit: Vec<u8>,
    outcome: LegacyAdoptionOutcome,
}

impl LegacyAdoptionPlan {
    pub fn digest(&self, expected: &RuleOwnershipSnapshot) -> anyhow::Result<String> {
        hash_value(
            b"engram:reviewed-legacy-adoption:v1\0",
            &(expected.adoption_basis_digest()?, self),
        )
    }
}

fn prepare(
    expected: &RuleOwnershipSnapshot,
    plan: &LegacyAdoptionPlan,
    decision_ref: &str,
) -> anyhow::Result<Prepared> {
    expected.adoption_basis_digest()?;
    anyhow::ensure!(
        valid_origin(&plan.origin)
            && plan.source_blake3.len() == 64
            && plan.source_blake3.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid adoption source identity"
    );
    anyhow::ensure!(
        !decision_ref.trim().is_empty() && decision_ref.len() <= 2048,
        "An explicit bounded decision reference is required"
    );
    anyhow::ensure!(
        !plan.bindings.is_empty() && plan.bindings.len() <= 32,
        "Adoption requires 1..32 explicit row mappings"
    );
    let key = source_key(&plan.origin);
    let mut receipts = BTreeMap::new();
    for (receipt_key, raw) in expected.receipts() {
        let receipt = RuleSourceReceipt::decode(receipt_key, raw)?;
        anyhow::ensure!(
            !receipt.pending,
            "Pending source `{}` must be resolved before adoption",
            receipt.origin
        );
        receipts.insert(receipt_key.clone(), receipt);
    }
    let corpus = expected
        .corpus_revision_raw()
        .map(std::str::from_utf8)
        .transpose()?
        .unwrap_or("")
        .to_owned();
    let mut receipt = receipts
        .get(&key)
        .cloned()
        .unwrap_or_else(|| RuleSourceReceipt {
            origin: plan.origin.clone(),
            source_hash: plan.source_blake3.clone(),
            authored: plan.authored,
            pending: false,
            doc_ids: Vec::new(),
            rules: Vec::new(),
            cleanup: Vec::new(),
            corpus_revision: corpus.clone(),
            legacy_aliases: Vec::new(),
        });
    anyhow::ensure!(
        receipt.authored == plan.authored && receipt.source_hash == plan.source_blake3,
        "Existing source identity or fingerprint differs; refresh/review that source before adoption"
    );
    anyhow::ensure!(
        receipt.corpus_revision == corpus,
        "Search corpus changed since source publication; reconcile before adoption"
    );
    for owned in &receipt.rules {
        let actual: RepoRule = serde_json::from_slice(
            expected
                .rules()
                .get(&owned.rule_id)
                .ok_or_else(|| anyhow::anyhow!("Source-owned row is missing"))?,
        )?;
        anyhow::ensure!(
            actual == *owned,
            "Source-owned row `{}` was changed; resolve before adoption",
            owned.rule_id
        );
    }
    let mut ids = BTreeSet::new();
    let mut sources: BTreeSet<_> = receipt
        .legacy_aliases
        .iter()
        .map(|a| a.source_rule_id.clone())
        .collect();
    let mut rows = BTreeMap::new();
    let mut changes = Vec::new();
    let mut projection_budget = 2 * 1024 * 1024;
    for binding in &plan.bindings {
        anyhow::ensure!(
            binding.source_projection_json.len() <= projection_budget,
            "Complete adoption projections exceed 2 MiB; no rows adopted"
        );
        projection_budget -= binding.source_projection_json.len();
        vk("rule_id", &binding.rule_id)?;
        anyhow::ensure!(
            ids.insert(binding.rule_id.clone()) && sources.insert(binding.source_rule_id.clone()),
            "Duplicate adoption mapping"
        );
        anyhow::ensure!(
            !receipt
                .legacy_aliases
                .iter()
                .any(|a| a.rule_id == binding.rule_id),
            "Rule already has a legacy source binding; review its update separately"
        );
        let raw = expected
            .rules()
            .get(&binding.rule_id)
            .ok_or_else(|| anyhow::anyhow!("Adoption row is missing"))?;
        anyhow::ensure!(
            blake3::hash(raw).to_hex().as_str() == binding.expected_raw_blake3,
            "Adoption row fingerprint changed"
        );
        let old: RepoRule = serde_json::from_slice(raw)?;
        anyhow::ensure!(
            old.rule_id == binding.rule_id,
            "Rule key/value identity mismatch"
        );
        if let Some(origin) = old
            .provenance
            .as_deref()
            .and_then(|p| p.strip_prefix(SOURCE_PROVENANCE))
        {
            anyhow::ensure!(
                origin == plan.origin,
                "A declared foreign source cannot be overridden by legacy adoption"
            );
            anyhow::ensure!(
                !receipts
                    .get(&key)
                    .is_some_and(|r| r.rules.iter().any(|r| r.rule_id == old.rule_id)),
                "Rule already has a source ownership claim; review its lifecycle separately"
            );
            // A prefix with no receipt is only an unverified claim. Explicit
            // same-source adoption can establish its missing receipt/alias.
        }
        let manual = matches!(
            old.provenance.as_deref(),
            Some("manual" | "user-authored" | "manual repository rule")
        );
        anyhow::ensure!(
            manual == (binding.decision == LegacyAdoptionDecision::TransferManual),
            "Manual ownership requires its explicit transfer decision"
        );
        for (other_key, other) in &receipts {
            anyhow::ensure!(
                other_key == &key || !other.rules.iter().any(|r| r.rule_id == old.rule_id),
                "Rule `{}` is claimed by foreign source `{}`; no rules adopted",
                old.rule_id,
                other.origin
            );
        }
        let mut object: RawObject = serde_json::from_slice(raw)?;
        let provenance = format!("{SOURCE_PROVENANCE}{}", plan.origin);
        object.0.insert(
            "provenance".into(),
            RawValue::from_string(serde_json::to_string(&provenance)?)?,
        );
        let replacement = serde_json::to_vec(&object.0)?;
        anyhow::ensure!(
            replacement.len() <= 4 * 1024 * 1024,
            "Adopted row exceeds storage budget"
        );
        let new: RepoRule = serde_json::from_slice(&replacement)?;
        let mut unchanged = new.clone();
        unchanged.provenance = old.provenance.clone();
        anyhow::ensure!(unchanged == old, "Adoption may change only provenance");
        let projection_hash = blake3::hash(binding.source_projection_json.as_bytes())
            .to_hex()
            .to_string();
        receipt.rules.retain(|r| r.rule_id != new.rule_id);
        receipt.rules.push(new);
        receipt.legacy_aliases.push(LegacyRuleAlias {
            rule_id: old.rule_id.clone(),
            source_rule_id: binding.source_rule_id.clone(),
            source_projection_json: binding.source_projection_json.clone(),
            source_projection_blake3: projection_hash.clone(),
        });
        changes.push(serde_json::json!({"rule_id":old.rule_id,"before_blake3":binding.expected_raw_blake3,
            "after_blake3":blake3::hash(&replacement).to_hex().to_string(),"source_rule_id":binding.source_rule_id,
            "source_projection_blake3":projection_hash,"decision":binding.decision}));
        rows.insert(binding.rule_id.clone(), replacement);
    }
    receipt.validate(&key)?;
    let receipt = receipt.encode()?.into_bytes();
    expected.check_adoption_budget(&rows, &key, &receipt)?;
    anyhow::ensure!(
        receipt.len() <= 4 * 1024 * 1024,
        "Adoption receipt exceeds storage budget"
    );
    let plan_hash = plan.digest(expected)?;
    let audit_key = format!("quality_adoption:{plan_hash}");
    let audit = serde_json::to_vec(&serde_json::json!({"version":1,"plan_blake3":plan_hash,
        "ownership_basis_blake3":expected.adoption_basis_digest()?,"source":plan.origin,"source_blake3":plan.source_blake3,
        "decision_ref":decision_ref,"changes":changes,"searchable_evidence_published":false}))?;
    anyhow::ensure!(
        audit.len() <= 256 * 1024,
        "Adoption audit exceeds storage budget"
    );
    let outcome = LegacyAdoptionOutcome {
        audit_key: audit_key.clone(),
        plan_blake3: plan_hash,
        receipt_blake3: blake3::hash(&receipt).to_hex().to_string(),
        adopted_rule_ids: rows.keys().cloned().collect(),
        searchable_evidence_published: false,
    };
    Ok(Prepared {
        rows,
        receipt_key: key,
        receipt,
        audit_key,
        audit,
        outcome,
    })
}

impl Registry {
    /// Validate the complete reviewed transition without opening a write transaction.
    pub fn preview_legacy_repo_rule_adoption(
        &self,
        expected: &RuleOwnershipSnapshot,
        plan: &LegacyAdoptionPlan,
        decision_ref: &str,
    ) -> anyhow::Result<LegacyAdoptionOutcome> {
        Ok(prepare(expected, plan, decision_ref)?.outcome)
    }

    pub fn adopt_legacy_repo_rules(
        &self,
        expected: &RuleOwnershipSnapshot,
        plan: &LegacyAdoptionPlan,
        decision_ref: &str,
    ) -> anyhow::Result<LegacyAdoptionOutcome> {
        self.commit_legacy_adoption(expected, prepare(expected, plan, decision_ref)?, false)
    }

    fn commit_legacy_adoption(
        &self,
        expected: &RuleOwnershipSnapshot,
        prepared: Prepared,
        fail_after_rows: bool,
    ) -> anyhow::Result<LegacyAdoptionOutcome> {
        let transaction = self.db.begin_write()?;
        {
            let projects = transaction.open_table(PROJECTS)?;
            let rows = transaction.open_table(REPO_RULES)?;
            let meta = transaction.open_table(META)?;
            let current =
                RuleOwnershipSnapshot::from_tables(expected.project_id(), &projects, &rows, &meta)?;
            anyhow::ensure!(
                current.adoption_basis_digest()? == expected.adoption_basis_digest()?,
                "Ownership basis changed; no rules adopted, recollect and review"
            );
            anyhow::ensure!(
                meta.get(format!("{}\0{}", expected.project_id(), prepared.audit_key).as_str())?
                    .is_none(),
                "This adoption decision was already recorded; no audit overwritten"
            );
        }
        {
            let mut rows = transaction.open_table(REPO_RULES)?;
            for (id, raw) in &prepared.rows {
                rows.insert(
                    format!("{}\0{id}", expected.project_id()).as_str(),
                    raw.as_slice(),
                )?;
            }
        }
        anyhow::ensure!(
            !fail_after_rows,
            "Injected interruption before adoption receipt/audit publication"
        );
        {
            let mut metadata = transaction.open_table(META)?;
            metadata.insert(
                format!("{}\0{}", expected.project_id(), prepared.receipt_key).as_str(),
                prepared.receipt.as_slice(),
            )?;
            metadata.insert(
                format!("{}\0{}", expected.project_id(), prepared.audit_key).as_str(),
                prepared.audit.as_slice(),
            )?;
        }
        // Project display metadata may change without invalidating a policy
        // decision. Recheck the actual final aggregate before committing too.
        {
            let projects = transaction.open_table(PROJECTS)?;
            let rows = transaction.open_table(REPO_RULES)?;
            let metadata = transaction.open_table(META)?;
            RuleOwnershipSnapshot::from_tables(expected.project_id(), &projects, &rows, &metadata)?;
        }
        transaction.commit()?;
        Ok(prepared.outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Registry, RepoRule) {
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
        registry.capture_source_head("project").unwrap();
        let rule = RepoRule {
            rule_id: "legacy-id".into(),
            file_pattern: "Site/**".into(),
            rule_text: "Å policy\r\nException: read-only probes do not prompt.".into(),
            priority: 19,
            updated_at_ms: 42,
            introduced_at: Some("2025-09-12".into()),
            provenance: None,
        };
        registry.put_repo_rule("project", &rule).unwrap();
        (temporary, registry, rule)
    }

    fn plan(
        snapshot: &RuleOwnershipSnapshot,
        origin: &str,
        decision: LegacyAdoptionDecision,
    ) -> LegacyAdoptionPlan {
        LegacyAdoptionPlan { origin: origin.into(), source_blake3: blake3::hash(b"accepted source").to_hex().to_string(), authored: true,
            bindings: vec![LegacyAdoptionBinding { rule_id: "legacy-id".into(),
                expected_raw_blake3: blake3::hash(&snapshot.rules()["legacy-id"]).to_hex().to_string(),
                source_rule_id: "source-entry-0".into(), source_projection_json: serde_json::json!({
                    "id":"source-entry-0", "source":origin, "text":"Complete source evidence", "path_scope":"Site/**"
                }).to_string(), decision }] }
    }

    #[test]
    fn adoption_changes_only_provenance_and_commits_receipt_alias_and_audit_together() {
        let (temporary, registry, old) = fixture();
        let raw = format!(
            "{{\"rule_id\":\"legacy-id\",\"file_pattern\":\"Site/**\",\"rule_text\":{},\"priority\":19,\"updated_at_ms\":42,\"introduced_at\":\"2025-09-12\",\"provenance\":null,\"unknown_integer\":18446744073709551616000000000001,\"unknown_payload\":{{ \"keep\" : [1,2,3] }}}}",
            serde_json::to_string(&old.rule_text).unwrap()
        );
        {
            let tx = registry.db.begin_write().unwrap();
            tx.open_table(REPO_RULES)
                .unwrap()
                .insert("project\0legacy-id", raw.as_bytes())
                .unwrap();
            tx.commit().unwrap();
        }
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        let plan = plan(
            &snapshot,
            "rules.md",
            LegacyAdoptionDecision::AdoptUnverified,
        );
        let outcome = registry
            .adopt_legacy_repo_rules(&snapshot, &plan, "reviewed-decision:owned-fixture")
            .unwrap();
        let mut actual = registry.list_repo_rules("project").unwrap().remove(0);
        assert_eq!(
            actual.provenance.as_deref(),
            Some("quality-gate source: rules.md")
        );
        actual.provenance = None;
        assert_eq!(
            actual, old,
            "Ownership adoption must preserve all existing enforcement fields and dates"
        );
        let after = registry.snapshot_rule_ownership("project").unwrap();
        let before_object: RawObject = serde_json::from_str(&raw).unwrap();
        let after_object: RawObject = serde_json::from_slice(&after.rules()["legacy-id"]).unwrap();
        assert_eq!(
            before_object.0["unknown_integer"].get(),
            after_object.0["unknown_integer"].get()
        );
        assert_eq!(
            before_object.0["unknown_payload"].get(),
            after_object.0["unknown_payload"].get()
        );
        let receipt = RuleSourceReceipt::decode(
            &source_key("rules.md"),
            &after.receipts()[&source_key("rules.md")],
        )
        .unwrap();
        assert_eq!(receipt.legacy_aliases[0].rule_id, "legacy-id");
        assert!(
            receipt.doc_ids.is_empty() && !outcome.searchable_evidence_published,
            "Registry adoption must not invent ownership of historical search documents"
        );
        assert!(
            registry
                .get_meta("project", &outcome.audit_key)
                .unwrap()
                .unwrap()
                .contains("reviewed-decision:owned-fixture")
        );
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn later_manual_or_foreign_publishers_cannot_overwrite_or_delete_adopted_rules() {
        let (temporary, registry, old) = fixture();
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        registry
            .adopt_legacy_repo_rules(
                &snapshot,
                &plan(
                    &snapshot,
                    "rules.md",
                    LegacyAdoptionDecision::AdoptUnverified,
                ),
                "review:fixture",
            )
            .unwrap();
        let adopted = registry.list_repo_rules("project").unwrap()[0].clone();
        assert!(registry.put_repo_rule("project", &old).is_err());
        assert!(registry.delete_repo_rule("project", "legacy-id").is_err());
        let foreign_key = source_key("foreign.md");
        let mut pending = RuleSourceReceipt {
            origin: "foreign.md".into(),
            source_hash: blake3::hash(b"foreign").to_hex().to_string(),
            authored: true,
            pending: true,
            doc_ids: Vec::new(),
            rules: Vec::new(),
            cleanup: Vec::new(),
            corpus_revision: String::new(),
            legacy_aliases: Vec::new(),
        };
        let pending_raw = pending.encode().unwrap();
        registry
            .set_meta("project", &foreign_key, &pending_raw)
            .unwrap();
        let mut foreign = old.clone();
        foreign.provenance = Some("quality-gate source: foreign.md".into());
        pending.pending = false;
        pending.rules = vec![foreign.clone()];
        let before = registry.snapshot_rule_ownership("project").unwrap();
        for expected_row in [old, adopted.clone()] {
            assert!(
                registry
                    .publish_repo_rule_source(
                        "project",
                        &[expected_row],
                        &["legacy-id".into()],
                        &[foreign.clone()],
                        &foreign_key,
                        &pending_raw,
                        &pending.encode().unwrap()
                    )
                    .is_err(),
                "A stale or freshly reread foreign publisher must still respect current ownership"
            );
        }
        assert_eq!(registry.snapshot_rule_ownership("project").unwrap(), before);
        assert_eq!(registry.list_repo_rules("project").unwrap(), vec![adopted]);
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn claims_for_absent_rows_and_unreadable_claims_block_generic_reuse() {
        let (temporary, registry, mut old) = fixture();
        old.rule_id = "absent-owned-id".into();
        old.provenance = Some("quality-gate source: rules.md".into());
        let receipt = RuleSourceReceipt {
            origin: "rules.md".into(),
            source_hash: blake3::hash(b"source").to_hex().to_string(),
            authored: true,
            pending: false,
            doc_ids: Vec::new(),
            rules: vec![old.clone()],
            cleanup: Vec::new(),
            corpus_revision: String::new(),
            legacy_aliases: Vec::new(),
        };
        registry
            .set_meta(
                "project",
                &source_key("rules.md"),
                &receipt.encode().unwrap(),
            )
            .unwrap();
        assert!(registry.put_repo_rule("project", &old).is_err());
        assert!(registry.delete_repo_rule("project", &old.rule_id).is_err());
        registry
            .set_meta("project", "quality_source:unreadable", "{unreadable claims")
            .unwrap();
        old.rule_id = "otherwise-unclaimed".into();
        old.provenance = None;
        assert!(
            registry.put_repo_rule("project", &old).is_err(),
            "Unknown claims must not become unowned IDs"
        );
        assert_eq!(registry.list_repo_rules("project").unwrap().len(), 1);
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn changed_rows_claims_corpus_and_project_identity_refuse_stale_adoption_without_writes() {
        for change in ["row", "claim", "corpus", "identity"] {
            let (temporary, registry, mut old) = fixture();
            let snapshot = registry.snapshot_rule_ownership("project").unwrap();
            let proposal = plan(
                &snapshot,
                "rules.md",
                LegacyAdoptionDecision::AdoptUnverified,
            );
            match change {
                "row" => {
                    old.rule_text.push_str("Human revision");
                    registry.put_repo_rule("project", &old).unwrap();
                }
                "claim" => registry
                    .set_meta("project", "quality_source:unknown", "new unknown claim")
                    .unwrap(),
                "corpus" => registry
                    .set_meta("project", "quality_corpus_revision", "reset")
                    .unwrap(),
                _ => {
                    let mut p = registry.get_project("project").unwrap().unwrap();
                    p.project_type = "changed".into();
                    registry.put_project(&p).unwrap();
                }
            }
            let current = registry.snapshot_rule_ownership("project").unwrap();
            assert!(
                registry
                    .adopt_legacy_repo_rules(&snapshot, &proposal, "review:fixture")
                    .is_err(),
                "Stale {change} basis committed"
            );
            assert_eq!(
                registry.snapshot_rule_ownership("project").unwrap(),
                current
            );
            drop(registry);
            temporary.close().unwrap();
        }
    }

    #[test]
    fn interruption_after_row_writes_rolls_back_rows_receipt_and_audit() {
        let (temporary, registry, _) = fixture();
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        let prepared = prepare(
            &snapshot,
            &plan(
                &snapshot,
                "rules.md",
                LegacyAdoptionDecision::AdoptUnverified,
            ),
            "review:fixture",
        )
        .unwrap();
        let audit_key = prepared.audit_key.clone();
        assert!(
            registry
                .commit_legacy_adoption(&snapshot, prepared, true)
                .is_err()
        );
        assert_eq!(
            registry.snapshot_rule_ownership("project").unwrap(),
            snapshot
        );
        assert_eq!(registry.get_meta("project", &audit_key).unwrap(), None);
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn competing_reviewed_adoptions_have_one_atomic_winner() {
        let (temporary, registry, _) = fixture();
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["a.md", "b.md"]
            .into_iter()
            .map(|origin| {
                let registry = registry.clone();
                let snapshot = snapshot.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let proposal = plan(&snapshot, origin, LegacyAdoptionDecision::AdoptUnverified);
                    barrier.wait();
                    registry
                        .adopt_legacy_repo_rules(&snapshot, &proposal, "review:fixture")
                        .is_ok()
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|v| *v)
            .count();
        assert_eq!(winners, 1);
        let current = registry.snapshot_rule_ownership("project").unwrap();
        assert_eq!(current.receipts().len(), 1);
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn existing_manual_authority_needs_its_explicit_transfer_and_missing_epoch_never_initializes() {
        let (temporary, registry, mut old) = fixture();
        old.provenance = Some("manual".into());
        registry.put_repo_rule("project", &old).unwrap();
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        assert!(
            registry
                .adopt_legacy_repo_rules(
                    &snapshot,
                    &plan(
                        &snapshot,
                        "rules.md",
                        LegacyAdoptionDecision::AdoptUnverified
                    ),
                    "review:fixture"
                )
                .is_err()
        );
        {
            let tx = registry.db.begin_write().unwrap();
            tx.open_table(META)
                .unwrap()
                .remove("project\0source_identity_epoch_v1")
                .unwrap();
            tx.commit().unwrap();
        }
        let legacy = registry.snapshot_rule_ownership("project").unwrap();
        assert!(
            registry
                .adopt_legacy_repo_rules(
                    &legacy,
                    &plan(&legacy, "rules.md", LegacyAdoptionDecision::TransferManual),
                    "review:fixture"
                )
                .is_err()
        );
        assert_eq!(registry.source_identity_epoch("project").unwrap(), None);
        registry.capture_source_head("project").unwrap();
        let current = registry.snapshot_rule_ownership("project").unwrap();
        registry
            .adopt_legacy_repo_rules(
                &current,
                &plan(&current, "rules.md", LegacyAdoptionDecision::TransferManual),
                "review:fixture",
            )
            .unwrap();
        let mut after = registry.list_repo_rules("project").unwrap().remove(0);
        after.provenance = old.provenance.clone();
        assert_eq!(after, old);
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn display_name_and_activity_timestamp_do_not_invalidate_an_unchanged_ownership_decision() {
        let (temporary, registry, _) = fixture();
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        let mut project = registry.get_project("project").unwrap().unwrap();
        project.project_name = "Updated display name".into();
        project.updated_at_ms = 100;
        registry.put_project(&project).unwrap();
        let now = registry.snapshot_rule_ownership("project").unwrap();
        assert_ne!(snapshot.digest().unwrap(), now.digest().unwrap());
        assert_eq!(
            snapshot.adoption_basis_digest().unwrap(),
            now.adoption_basis_digest().unwrap()
        );
        registry
            .adopt_legacy_repo_rules(
                &snapshot,
                &plan(
                    &snapshot,
                    "rules.md",
                    LegacyAdoptionDecision::AdoptUnverified,
                ),
                "review:fixture",
            )
            .unwrap();
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn adoption_that_would_exceed_the_next_readers_aggregate_budget_is_refused() {
        let (temporary, registry, mut first) = fixture();
        first.rule_text = "x".repeat(3 * 1024 * 1024);
        registry.put_repo_rule("project", &first).unwrap();
        let mut second = first.clone();
        second.rule_id = "another-large-policy".into();
        registry.put_repo_rule("project", &second).unwrap();
        let snapshot = registry.snapshot_rule_ownership("project").unwrap();
        let plan = plan(
            &snapshot,
            "rules.md",
            LegacyAdoptionDecision::AdoptUnverified,
        );
        let preview =
            registry.preview_legacy_repo_rule_adoption(&snapshot, &plan, "review:fixture");
        assert!(preview.unwrap_err().to_string().contains("read budget"));
        assert!(
            registry
                .adopt_legacy_repo_rules(&snapshot, &plan, "review:fixture")
                .is_err()
        );
        assert_eq!(
            registry.snapshot_rule_ownership("project").unwrap(),
            snapshot,
            "A locally bounded receipt must not make the combined ownership state unreadable"
        );
        drop(registry);
        temporary.close().unwrap();
    }

    #[test]
    fn a_source_prefix_without_a_receipt_requires_reviewed_same_source_recovery() {
        for origin in ["rules.md", "foreign.md"] {
            let (temporary, registry, mut original) = fixture();
            original.provenance = Some(format!("{SOURCE_PROVENANCE}{origin}"));
            registry.put_repo_rule("project", &original).unwrap();
            let snapshot = registry.snapshot_rule_ownership("project").unwrap();
            let selected = plan(
                &snapshot,
                "rules.md",
                LegacyAdoptionDecision::AdoptUnverified,
            );
            let outcome = registry.adopt_legacy_repo_rules(&snapshot, &selected, "review:fixture");
            if origin == "rules.md" {
                outcome.unwrap();
                assert_eq!(
                    registry.list_repo_rules("project").unwrap(),
                    vec![original.clone()],
                    "Recovering the missing ownership receipt must preserve every policy field"
                );
                let receipt = RuleSourceReceipt::decode(
                    &source_key(origin),
                    &registry
                        .snapshot_rule_ownership("project")
                        .unwrap()
                        .receipts()[&source_key(origin)],
                )
                .unwrap();
                assert_eq!(receipt.legacy_aliases.len(), 1);
                assert!(registry.put_repo_rule("project", &original).is_err());
            } else {
                assert!(
                    outcome.is_err(),
                    "An unverified prefix is not permission to take over another source"
                );
                assert_eq!(
                    registry.snapshot_rule_ownership("project").unwrap(),
                    snapshot
                );
            }
            drop(registry);
            temporary.close().unwrap();
        }
    }
}
