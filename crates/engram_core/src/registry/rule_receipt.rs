//! Shared, strict receipt schema and transactional rule-owner checks.
use super::*;
use std::collections::BTreeSet;

pub(super) const SOURCE_PROVENANCE: &str = "quality-gate source: ";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LegacyRuleAlias {
    pub rule_id: String,
    pub source_rule_id: String,
    pub source_projection_json: String,
    pub source_projection_blake3: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptRule {
    rule_id: String,
    file_pattern: String,
    rule_text: String,
    priority: i32,
    updated_at_ms: u64,
    #[serde(default)]
    introduced_at: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
}

fn strict_rules<'de, D: serde::Deserializer<'de>>(input: D) -> Result<Vec<RepoRule>, D::Error> {
    Ok(Vec::<ReceiptRule>::deserialize(input)?
        .into_iter()
        .map(|r| RepoRule {
            rule_id: r.rule_id,
            file_pattern: r.file_pattern,
            rule_text: r.rule_text,
            priority: r.priority,
            updated_at_ms: r.updated_at_ms,
            introduced_at: r.introduced_at,
            provenance: r.provenance,
        })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSourceReceipt {
    pub origin: String,
    pub source_hash: String,
    pub authored: bool,
    pub pending: bool,
    pub doc_ids: Vec<String>,
    #[serde(deserialize_with = "strict_rules")]
    pub rules: Vec<RepoRule>,
    #[serde(default)]
    pub cleanup: Vec<String>,
    #[serde(default)]
    pub corpus_revision: String,
    #[serde(default)]
    pub legacy_aliases: Vec<LegacyRuleAlias>,
}

pub(super) fn source_key(origin: &str) -> String {
    format!(
        "quality_source:{}",
        blake3::hash(origin.as_bytes()).to_hex()
    )
}

pub(super) fn valid_origin(origin: &str) -> bool {
    !origin.is_empty()
        && origin.len() <= 512
        && !origin.contains(['\\', ':'])
        && !origin.chars().any(char::is_control)
        && origin.split('/').all(|p| !matches!(p, "" | "." | ".."))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

impl RuleSourceReceipt {
    pub fn validate_origin(origin: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            valid_origin(origin),
            "Invalid normalized quality source identity (maximum 512 bytes)"
        );
        Ok(())
    }

    pub fn validate(&self, key: &str) -> anyhow::Result<()> {
        Self::validate_origin(&self.origin)?;
        anyhow::ensure!(
            source_key(&self.origin) == key,
            "Quality-source receipt key/origin mismatch"
        );
        anyhow::ensure!(
            valid_hash(&self.source_hash),
            "Invalid quality-source fingerprint"
        );
        let mut ids = BTreeSet::new();
        for row in &self.rules {
            vk("rule_id", &row.rule_id)?;
            anyhow::ensure!(
                ids.insert(&row.rule_id),
                "Duplicate source rule claim `{}`",
                row.rule_id
            );
        }
        let mut targets = BTreeSet::new();
        let mut sources = BTreeSet::new();
        for alias in &self.legacy_aliases {
            anyhow::ensure!(
                targets.insert(&alias.rule_id) && sources.insert(&alias.source_rule_id),
                "Ambiguous legacy source aliases"
            );
            anyhow::ensure!(
                ids.contains(&alias.rule_id),
                "Legacy alias is outside source-owned rules"
            );
            anyhow::ensure!(
                alias.source_projection_json.len() <= 4 * 1024 * 1024
                    && valid_hash(&alias.source_projection_blake3)
                    && blake3::hash(alias.source_projection_json.as_bytes())
                        .to_hex()
                        .as_str()
                        == alias.source_projection_blake3,
                "Legacy source alias fingerprint mismatch"
            );
            let projection: serde_json::Value =
                serde_json::from_str(&alias.source_projection_json)?;
            anyhow::ensure!(
                projection.get("id").and_then(|v| v.as_str())
                    == Some(alias.source_rule_id.as_str())
                    && projection.get("source").and_then(|v| v.as_str())
                        == Some(self.origin.as_str()),
                "Legacy source alias projection identity mismatch"
            );
        }
        Ok(())
    }

    pub fn decode(key: &str, raw: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            raw.len() <= 4 * 1024 * 1024,
            "Quality-source receipt exceeds 4 MiB budget"
        );
        let receipt: Self = serde_json::from_slice(raw)?;
        receipt.validate(key)?;
        Ok(receipt)
    }

    pub fn encode(&self) -> anyhow::Result<String> {
        struct Buffer(Vec<u8>);
        impl std::io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let size = self
                    .0
                    .len()
                    .checked_add(bytes.len())
                    .filter(|n| *n <= 4 * 1024 * 1024)
                    .ok_or_else(|| {
                        std::io::Error::other("Quality-source receipt exceeds 4 MiB budget")
                    })?;
                if size > self.0.capacity() {
                    self.0
                        .try_reserve_exact(
                            size.next_power_of_two().min(4 * 1024 * 1024) - self.0.len(),
                        )
                        .map_err(std::io::Error::other)?;
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut buffer = Buffer(Vec::new());
        serde_json::to_writer(&mut buffer, self)?;
        Ok(String::from_utf8(buffer.0)?)
    }
}

/// Row guards and mutations share the caller's write transaction. Metadata
/// claims count even when the current row is absent; unknown claims fail closed.
pub(super) fn guard_mutation(
    transaction: &redb::WriteTransaction,
    project: &str,
    ids: &BTreeSet<&str>,
    owner_key: Option<&str>,
) -> anyhow::Result<()> {
    let metadata = transaction.open_table(META)?;
    let mut remaining = 8 * 1024 * 1024;
    let receipts = super::rule_ownership::collect_prefix(
        &metadata,
        &format!("{project}\0quality_source:"),
        "quality_source:",
        256,
        &mut remaining,
    )?;
    let mut owner_receipt = None;
    for (key, raw) in &receipts {
        let receipt = RuleSourceReceipt::decode(key, raw).map_err(|e| {
            anyhow::anyhow!("Unresolved ownership receipt `{key}`; no rules changed: {e}")
        })?;
        for row in &receipt.rules {
            if ids.contains(row.rule_id.as_str()) {
                anyhow::ensure!(
                    Some(key.as_str()) == owner_key,
                    "Repository rule `{}` is claimed by `{}`; explicit ownership resolution is required",
                    row.rule_id,
                    receipt.origin
                );
            }
        }
        if Some(key.as_str()) == owner_key {
            owner_receipt = Some(receipt);
        }
    }
    if owner_key.is_some_and(|key| key.starts_with("quality_source:")) {
        anyhow::ensure!(
            owner_receipt.is_some(),
            "Source publisher has no validated ownership receipt"
        );
    }
    let rows = transaction.open_table(REPO_RULES)?;
    for id in ids {
        if let Some(raw) = rows.get(format!("{project}\0{id}").as_str())? {
            let row: RepoRule = serde_json::from_slice(raw.value())?;
            if let Some(origin) = row
                .provenance
                .as_deref()
                .and_then(|p| p.strip_prefix(SOURCE_PROVENANCE))
            {
                anyhow::ensure!(
                    owner_receipt.as_ref().map(|r| r.origin.as_str()) == Some(origin),
                    "Repository rule `{id}` belongs to source `{origin}`; edit or explicitly transfer that source-owned rule"
                );
                if let Some(key) = owner_key {
                    if key.starts_with("quality_source:") {
                        let receipt = owner_receipt
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("Source publisher has no receipt"))?;
                        anyhow::ensure!(
                            receipt.rules.iter().any(|owned| *owned == row),
                            "Existing rule `{id}` has no matching source ownership claim; reviewed adoption is required"
                        );
                    }
                }
            } else if owner_key.is_some_and(|key| key.starts_with("quality_source:")) {
                anyhow::bail!(
                    "Existing rule `{id}` is unverified/manual; explicit adoption is required before source publication"
                );
            }
        }
    }
    Ok(())
}
