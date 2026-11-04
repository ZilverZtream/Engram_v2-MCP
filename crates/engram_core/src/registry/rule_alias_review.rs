//! Explicit lifecycle review; never acceptance of changed source policy.
use super::*;
use super::rule_ownership::hash_value;
use super::rule_receipt::{source_key, SOURCE_PROVENANCE};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum AliasReviewSource {
    Present { blake3: String },
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum AliasReviewAction {
    RebindOnlyUnchanged { source_rule_id: String },
    Retire,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasReviewBinding {
    pub rule_id: String,
    pub expected_raw_blake3: String,
    pub expected_alias_blake3: String,
    pub disposition: AliasReviewAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasReviewPlan {
    pub origin: String,
    pub source: AliasReviewSource,
    /// Complete parser observations, supplied by the source-bound service.
    pub current_projections: Vec<String>,
    pub bindings: Vec<AliasReviewBinding>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AliasReviewOutcome {
    pub plan_blake3: String,
    pub audit_key: String,
    pub retired_rule_ids: Vec<String>,
    pub rebound_rule_ids: Vec<String>,
    pub source_reimport_required: bool,
    pub searchable_evidence_published: bool,
}

struct Prepared {
    receipt_key: String,
    receipt: Vec<u8>,
    remove: BTreeSet<String>,
    audit: Vec<u8>,
    review: serde_json::Value,
    outcome: AliasReviewOutcome,
}

fn policy(raw: &str, origin: &str) -> anyhow::Result<serde_json::Value> {
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    let object = value.as_object_mut().ok_or_else(|| anyhow::anyhow!("Invalid source projection"))?;
    anyhow::ensure!(object.get("id").is_some_and(|v| v.is_string())
        && object.get("source").and_then(|v| v.as_str()) == Some(origin),
        "Source projection identity differs from reviewed origin");
    object.remove("id");
    Ok(value)
}

fn prepare(expected: &RuleOwnershipSnapshot, plan: &AliasReviewPlan, decision_ref: &str) -> anyhow::Result<Prepared> {
    expected.adoption_basis_digest()?;
    RuleSourceReceipt::validate_origin(&plan.origin)?;
    anyhow::ensure!(!decision_ref.trim().is_empty() && decision_ref.len() <= 2048,
        "An explicit bounded caller decision reference is required");
    anyhow::ensure!(!plan.bindings.is_empty() && plan.bindings.len() <= 32,
        "Select 1..32 alias dispositions");
    anyhow::ensure!(plan.current_projections.len() <= 512
        && plan.current_projections.iter().try_fold(0usize, |n, p| n.checked_add(p.len()))
            .is_some_and(|bytes| bytes <= 4 * 1024 * 1024),
        "Current source projections exceed the review budget");
    match &plan.source {
        AliasReviewSource::Present { blake3 } => anyhow::ensure!(
            blake3.len() == 64 && blake3.bytes().all(|b| b.is_ascii_hexdigit()), "Invalid current source fingerprint"),
        AliasReviewSource::Missing => anyhow::ensure!(plan.current_projections.is_empty()
            && plan.bindings.iter().all(|b| matches!(b.disposition, AliasReviewAction::Retire)),
            "An absent source permits retirement only"),
    }
    let key = source_key(&plan.origin);
    let before = expected.receipts().get(&key).ok_or_else(|| anyhow::anyhow!("Source ownership receipt is missing"))?;
    let mut receipt = RuleSourceReceipt::decode(&key, before)?;
    // Existing aliases may be reviewed while publication is pending. Exact
    // row/receipt CAS is still required, and this operation never clears it.
    let provenance = format!("{SOURCE_PROVENANCE}{}", plan.origin);
    for owned in &receipt.rules {
        let raw = expected.rules().get(&owned.rule_id).ok_or_else(|| anyhow::anyhow!("Source-owned row is missing"))?;
        let actual: RepoRule = serde_json::from_slice(raw)?;
        anyhow::ensure!(actual == *owned && actual.provenance.as_deref() == Some(provenance.as_str()),
            "Source-owned row changed; recollect exact ownership evidence");
    }
    let all_receipts = expected.receipts().iter().map(|(key,raw)|
        Ok((key,RuleSourceReceipt::decode(key,raw)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut current = BTreeMap::new();
    for raw in &plan.current_projections {
        let value: serde_json::Value = serde_json::from_str(raw)?;
        let id = value.get("id").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Source entry has no ID"))?.to_owned();
        let semantic = policy(raw, &plan.origin)?;
        anyhow::ensure!(current.insert(id, (raw, semantic)).is_none(), "Current parser identities are ambiguous");
    }
    let mut selected = BTreeSet::new();
    let mut remove = BTreeSet::new();
    let mut rebound = Vec::new();
    let mut replacements = Vec::new();
    let mut changes = Vec::new();
    for binding in &plan.bindings {
        vk("rule_id", &binding.rule_id)?;
        anyhow::ensure!(selected.insert(binding.rule_id.clone()), "Duplicate alias disposition");
        let raw = expected.rules().get(&binding.rule_id).ok_or_else(|| anyhow::anyhow!("Selected row is missing"))?;
        anyhow::ensure!(blake3::hash(raw).to_hex().as_str() == binding.expected_raw_blake3,
            "Selected raw row changed");
        let alias = receipt.legacy_aliases.iter().find(|a| a.rule_id == binding.rule_id)
            .ok_or_else(|| anyhow::anyhow!("Selected row has no adopted alias"))?;
        anyhow::ensure!(blake3::hash(&serde_json::to_vec(alias)?).to_hex().as_str() == binding.expected_alias_blake3,
            "Selected alias changed");
        for (other_key, other) in &all_receipts {
            anyhow::ensure!(*other_key == &key || !other.rules.iter().any(|r| r.rule_id == binding.rule_id),
                "Selected row has a competing source claim");
        }
        let old_policy = policy(&alias.source_projection_json, &plan.origin)?;
        let after = match &binding.disposition {
            AliasReviewAction::RebindOnlyUnchanged { source_rule_id } => {
                let (projection, semantic) = current.get(source_rule_id)
                    .ok_or_else(|| anyhow::anyhow!("Reviewed target parser entry is missing"))?;
                anyhow::ensure!(semantic == &old_policy,
                    "Rebinding cannot change policy text, scope, category, severity, origin or any other projection field");
                anyhow::ensure!(source_rule_id != &alias.source_rule_id,
                    "Parser identity is unchanged; no lifecycle mutation is needed");
                let next = LegacyRuleAlias {
                    rule_id: alias.rule_id.clone(), source_rule_id: source_rule_id.clone(),
                    source_projection_json: projection.to_string(),
                    source_projection_blake3: blake3::hash(projection.as_bytes()).to_hex().to_string(),
                };
                replacements.push(next.clone());
                rebound.push(binding.rule_id.clone());
                Some(next)
            }
            AliasReviewAction::Retire => {
                anyhow::ensure!(!current.values().any(|(_, value)| value == &old_policy),
                    "The unchanged policy still exists in the source; rebind it rather than retire its ownership");
                remove.insert(binding.rule_id.clone());
                None
            }
        };
        changes.push(serde_json::json!({"rule_id":binding.rule_id,"disposition":binding.disposition,
            "before_raw_utf8":std::str::from_utf8(raw)?,"before_raw_blake3":binding.expected_raw_blake3,
            "before_alias":alias,"before_alias_blake3":binding.expected_alias_blake3,"after_alias":after,"row_deleted":remove.contains(&binding.rule_id)}));
    }
    // Remove all selected mappings first: a two-entry positional swap is one
    // atomic final mapping, never a transient collision or inferred reassignment.
    receipt.legacy_aliases.retain(|a| !selected.contains(&a.rule_id));
    receipt.legacy_aliases.extend(replacements);
    receipt.rules.retain(|r| !remove.contains(&r.rule_id));
    // Keep source hash, document IDs, cleanup and corpus identity exactly as
    // observed. Old enforcement/search evidence is withheld until real import.
    receipt.pending = true;
    receipt.validate(&key)?;
    let next_receipt = receipt.encode()?.into_bytes();
    expected.check_adoption_budget(&BTreeMap::new(), &key, &next_receipt)?;
    let plan_hash = hash_value(b"engram:reviewed-alias-lifecycle:v1\0",
        &(expected.digest()?, plan, decision_ref))?;
    let outcome = AliasReviewOutcome { plan_blake3:plan_hash.clone(),
        audit_key:format!("quality_alias_review:{plan_hash}"),
        retired_rule_ids:remove.iter().cloned().collect(), rebound_rule_ids:rebound,
        source_reimport_required:true, searchable_evidence_published:false };
    let review = serde_json::json!({"plan_blake3":plan_hash,"ownership_snapshot_blake3":expected.digest()?,
        "project":expected.project()?,"identity_epoch":expected.identity_epoch()?,"origin":plan.origin,"source":plan.source,"decision_ref":decision_ref,
        "decision_reference_origin":"Caller-supplied reference; Engram does not authenticate human identity or policy acceptance",
        "before_receipt_utf8":std::str::from_utf8(before)?,
        "after_receipt_utf8":std::str::from_utf8(&next_receipt)?,
        "current_source_projections":plan.current_projections,"changes":changes,
        "source_reimport_required":true,"searchable_evidence_published":false,"authored_files_written":0});
    let audit = serde_json::to_vec(&review)?;
    anyhow::ensure!(audit.len() <= 256 * 1024,
        "Complete lifecycle evidence exceeds the 256 KiB audit budget; no partial review or mutation");
    Ok(Prepared { receipt_key:key,receipt:next_receipt,remove,audit,review,outcome })
}

impl Registry {
    pub fn preview_legacy_alias_review(&self, expected: &RuleOwnershipSnapshot,
        plan: &AliasReviewPlan, decision_ref: &str) -> anyhow::Result<serde_json::Value> {
        Ok(prepare(expected, plan, decision_ref)?.review)
    }

    pub fn apply_legacy_alias_review(&self, expected: &RuleOwnershipSnapshot,
        plan: &AliasReviewPlan, decision_ref: &str) -> anyhow::Result<AliasReviewOutcome> {
        let prepared = prepare(expected, plan, decision_ref)?;
        let tx = self.db.begin_write()?;
        {
            let projects = tx.open_table(PROJECTS)?;
            let rows = tx.open_table(REPO_RULES)?;
            let meta = tx.open_table(META)?;
            let current = RuleOwnershipSnapshot::from_tables(expected.project_id(), &projects, &rows, &meta)?;
            anyhow::ensure!(current.digest()? == expected.digest()?,
                "Ownership snapshot changed; no aliases reviewed, recollect before apply");
            anyhow::ensure!(meta.get(format!("{}\0{}",expected.project_id(),prepared.outcome.audit_key).as_str())?.is_none(),
                "This lifecycle decision was already recorded");
        }
        let affected = plan.bindings.iter().map(|b| b.rule_id.as_str()).collect();
        rule_receipt::guard_mutation(&tx, expected.project_id(), &affected, Some(&prepared.receipt_key))?;
        {
            let mut rows = tx.open_table(REPO_RULES)?;
            for id in &prepared.remove {
                rows.remove(format!("{}\0{id}",expected.project_id()).as_str())?;
            }
        }
        {
            let mut meta = tx.open_table(META)?;
            meta.insert(format!("{}\0{}",expected.project_id(),prepared.receipt_key).as_str(),prepared.receipt.as_slice())?;
            meta.insert(format!("{}\0{}",expected.project_id(),prepared.outcome.audit_key).as_str(),prepared.audit.as_slice())?;
        }
        {
            let projects = tx.open_table(PROJECTS)?;
            let rows = tx.open_table(REPO_RULES)?;
            let meta = tx.open_table(META)?;
            RuleOwnershipSnapshot::from_tables(expected.project_id(), &projects, &rows, &meta)?;
        }
        tx.commit()?;
        Ok(prepared.outcome)
    }
}
