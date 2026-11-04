//! Read-only evidence for an explicit legacy-ownership decision. No names,
//! matching text, receipt claims or source existence imply accepted ownership.
use crate::services::{quality_source, rule_authority};
use crate::state::AppState;
use anyhow::{Result, ensure};
use engram_core::{ProjectRecord, registry::RepoRule};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

const RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Serialize)]
struct RawEvidence {
    bytes: usize,
    blake3: String,
    utf8: Option<String>,
    hex: Option<String>,
}

fn raw(bytes: &[u8], remaining: usize) -> Result<RawEvidence> {
    ensure!(
        bytes.len() <= remaining,
        "Ownership response budget exceeded; no partial inventory returned"
    );
    let (utf8, hex) = match std::str::from_utf8(bytes) {
        Ok(text) => (Some(text.to_owned()), None),
        Err(_) => {
            ensure!(
                bytes.len() <= remaining / 2,
                "Ownership raw-byte response budget exceeded"
            );
            use std::fmt::Write;
            let mut hex = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                write!(hex, "{byte:02x}")?;
            }
            (None, Some(hex))
        }
    };
    Ok(RawEvidence {
        bytes: bytes.len(),
        blake3: blake3::hash(bytes).to_hex().to_string(),
        utf8,
        hex,
    })
}

fn charge(value: &impl Serialize, remaining: &mut usize) -> Result<()> {
    struct Counter {
        remaining: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(std::io::Error::other(
                    "Ownership response budget exceeded; no partial inventory returned",
                ));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        remaining: *remaining,
    };
    serde_json::to_writer(&mut counter, value)?;
    *remaining = counter.remaining;
    Ok(())
}

use engram_core::registry::RuleSourceReceipt as InventoryReceipt;

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

#[derive(Serialize)]
struct ReceiptEvidence {
    metadata_key: String,
    raw: RawEvidence,
    status: &'static str,
    origin: Option<String>,
    pending: Option<bool>,
    authored: Option<bool>,
    declared_source_blake3: Option<String>,
    source_fingerprint_status: &'static str,
    claimed_rule_ids: Vec<String>,
}

#[derive(Serialize)]
struct RowClaim {
    receipt_key: String,
    origin: String,
    pending: bool,
    claim_index: usize,
    matches_current_rule_fields: bool,
}

#[derive(Serialize)]
struct RowEvidence {
    rule_id: String,
    raw: RawEvidence,
    decoded: Option<RepoRule>,
    status: &'static str,
    authority: Option<rule_authority::Authority>,
    registry_enforcement: &'static str,
    ownership_status: &'static str,
    claim_status: &'static str,
    receipt_claims: Vec<RowClaim>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct SourceEvidence {
    path: String,
    status: &'static str,
    blake3: Option<String>,
    bytes: Option<usize>,
}

fn source(root: &Path, path: &str, remaining: &mut usize) -> SourceEvidence {
    let mut observation = SourceEvidence {
        path: path.to_owned(),
        status: "unavailable_or_unsafe",
        blake3: None,
        bytes: None,
    };
    if !valid_path(path) {
        observation.status = "invalid_project_relative_path";
        return observation;
    }
    if *remaining == 0 {
        observation.status = "source_byte_budget_omitted";
        return observation;
    }
    let read = (|| -> Result<_> {
        let file = engram_core::safe_open_read(root, path)?;
        let before = file.metadata()?;
        ensure!(before.is_file(), "Source is not a regular file");
        let limit = (*remaining).min(4 * 1024 * 1024);
        if before.len() > limit as u64 {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        let mut file = file;
        file.by_ref()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            *remaining = 0;
            return Ok(None);
        }
        *remaining -= bytes.len();
        let after = file.metadata()?;
        ensure!(
            before.len() == after.len() && before.modified()? == after.modified()?,
            "Source changed during fingerprinting"
        );
        Ok(Some((
            blake3::hash(&bytes).to_hex().to_string(),
            bytes.len(),
        )))
    })();
    match read {
        Ok(Some((hash, len))) => {
            observation.status = "complete_raw_fingerprint";
            observation.blake3 = Some(hash);
            observation.bytes = Some(len);
        }
        Ok(None) => observation.status = "source_byte_budget_omitted",
        Err(_) => {}
    }
    observation
}

fn sources(root: Option<&Path>, paths: &[String]) -> Vec<SourceEvidence> {
    let mut remaining = 8 * 1024 * 1024;
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            if index >= 64 {
                SourceEvidence {
                    path: path.clone(),
                    status: "source_path_budget_omitted",
                    blake3: None,
                    bytes: None,
                }
            } else if let Some(root) = root {
                source(root, path, &mut remaining)
            } else {
                SourceEvidence {
                    path: path.clone(),
                    status: "project_filesystem_root_unavailable",
                    blake3: None,
                    bytes: None,
                }
            }
        })
        .collect()
}

#[derive(Serialize)]
struct Report {
    status: &'static str,
    registry_snapshot_blake3: String,
    project_id: String,
    project_raw: RawEvidence,
    project: Option<ProjectRecord>,
    identity_epoch_raw: Option<RawEvidence>,
    identity_epoch: Option<u64>,
    identity_status: &'static str,
    corpus_revision_raw: Option<RawEvidence>,
    rows: Vec<RowEvidence>,
    receipts: Vec<ReceiptEvidence>,
    sources: Vec<SourceEvidence>,
    enforcement_notes: Vec<String>,
    enforcement_basis: &'static str,
    unresolved_claim_keys: Vec<String>,
    coverage: &'static str,
    adoption_performed: bool,
    authored_files_written: usize,
}

pub fn inventory(state: &AppState, pid: &str, source_paths: &[String]) -> Result<String> {
    ensure!(
        source_paths.len() <= 16,
        "At most16 explicit ownership source paths are allowed"
    );
    let mut selected = BTreeSet::new();
    for path in source_paths {
        ensure!(
            valid_path(path),
            "Use normalized project-relative ownership source paths"
        );
        ensure!(
            selected.insert(path.clone()),
            "Duplicate ownership source candidate"
        );
    }
    let snapshot = state.registry.snapshot_rule_ownership(pid)?;
    let project = snapshot.project().ok();
    let filesystem_root = project.as_ref().and_then(|p| {
        let root = Path::new(&p.directory);
        root.is_absolute().then_some(root)
    });
    let epoch = snapshot.identity_epoch();
    let identity_status = if project.is_none() {
        "unreadable_project_identity"
    } else {
        match &epoch {
            Ok(Some(_)) => "existing_epoch_observed",
            Ok(None) => "missing_epoch_adoption_requires_explicit_identity_establishment",
            Err(_) => "invalid_epoch_adoption_blocked",
        }
    };
    let mut response_remaining = 3 * 1024 * 1024;
    let project_raw = raw(snapshot.project_raw(), response_remaining)?;
    charge(&project_raw, &mut response_remaining)?;
    charge(&project, &mut response_remaining)?;
    let identity_epoch_raw = snapshot
        .identity_epoch_raw()
        .map(|v| raw(v, response_remaining))
        .transpose()?;
    charge(&identity_epoch_raw, &mut response_remaining)?;
    let corpus_revision_raw = snapshot
        .corpus_revision_raw()
        .map(|v| raw(v, response_remaining))
        .transpose()?;
    charge(&corpus_revision_raw, &mut response_remaining)?;
    let mut receipts = Vec::new();
    let mut claims = BTreeMap::new();
    let mut unknown_claims = Vec::new();
    let mut paths: Vec<_> = selected.iter().cloned().collect();
    for (key, bytes) in snapshot.receipts() {
        let parsed = serde_json::from_slice::<InventoryReceipt>(bytes).ok();
        let status = match &parsed {
            None => "unreadable_claims",
            Some(r) if !valid_path(&r.origin) => "invalid_origin_path",
            Some(r)
                if r.source_hash.len() != 64
                    || !r.source_hash.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                "invalid_declared_source_hash"
            }
            Some(r) if quality_source::receipt_key(&r.origin) != *key => {
                "receipt_key_origin_mismatch"
            }
            Some(r) if r.pending => "pending_claims_unresolved",
            Some(r) if r.validate(key).is_err() => "invalid_or_conflicting_claims",
            Some(_) => "metadata_claims_not_ownership_proof",
        };
        if status != "metadata_claims_not_ownership_proof" {
            unknown_claims.push(key.clone());
        }
        let evidence = ReceiptEvidence {
            metadata_key: key.clone(),
            raw: raw(bytes, response_remaining)?,
            status,
            origin: parsed.as_ref().map(|r| r.origin.clone()),
            pending: parsed.as_ref().map(|r| r.pending),
            authored: parsed.as_ref().map(|r| r.authored),
            declared_source_blake3: parsed.as_ref().map(|r| r.source_hash.clone()),
            source_fingerprint_status: "not_observed",
            claimed_rule_ids: parsed
                .as_ref()
                .map(|r| r.rules.iter().map(|row| row.rule_id.clone()).collect())
                .unwrap_or_default(),
        };
        charge(&evidence, &mut response_remaining)?;
        if let Some(r) = &parsed {
            if valid_path(&r.origin) && selected.insert(r.origin.clone()) {
                paths.push(r.origin.clone());
            }
        }
        claims.insert(key.clone(), parsed);
        receipts.push(evidence);
    }
    let decoded: BTreeMap<_, _> = snapshot
        .rules()
        .iter()
        .map(|(key, bytes)| {
            let row = serde_json::from_slice::<RepoRule>(bytes)
                .ok()
                .filter(|row| row.rule_id == *key);
            (key.clone(), row)
        })
        .collect();
    let observed_sources = sources(filesystem_root, &paths);
    for receipt in &mut receipts {
        receipt.source_fingerprint_status = match (&receipt.origin, &receipt.declared_source_blake3)
        {
            (Some(origin), Some(expected)) => match observed_sources
                .iter()
                .find(|s| s.path == *origin)
                .and_then(|s| s.blake3.as_ref())
            {
                Some(actual) if actual == expected => "matches_declared_bytes_not_ownership",
                Some(_) => "differs_from_declared_source",
                None => "source_unavailable_or_omitted",
            },
            _ => "claim_unreadable",
        };
    }
    let effective = match &project {
        Some(project) if decoded.values().all(Option::is_some) => {
            quality_source::effective_repo_rules_from_rows(
                state,
                pid,
                project,
                decoded.values().filter_map(Clone::clone).collect(),
                None,
            )
        }
        _ => Err(anyhow::anyhow!(
            "Stored identity or rule rows cannot be decoded; current enforcement observation unavailable"
        )),
    };
    let (enforced, notes) = match effective {
        Ok(effective) => (
            Some(
                effective
                    .rules
                    .into_iter()
                    .map(|r| r.rule_id)
                    .collect::<BTreeSet<_>>(),
            ),
            effective.notes,
        ),
        Err(error) => (
            None,
            vec![format!("Enforcement observation unavailable: {error:#}")],
        ),
    };
    let mut rows = Vec::new();
    for (id, bytes) in snapshot.rules() {
        let decoded = decoded[id].clone();
        let mut receipt_claims = Vec::new();
        let mut claim_budget = response_remaining;
        for (key, claim) in &claims {
            if let Some(claim) = claim {
                for (claim_index, row) in claim
                    .rules
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row.rule_id == *id)
                {
                    let entry = RowClaim {
                        receipt_key: key.clone(),
                        origin: claim.origin.clone(),
                        pending: claim.pending,
                        claim_index,
                        matches_current_rule_fields: decoded
                            .as_ref()
                            .is_some_and(|current| current == row),
                    };
                    charge(&entry, &mut claim_budget)?;
                    receipt_claims.push(entry);
                }
            }
        }
        let authority = decoded
            .as_ref()
            .map(|r| rule_authority::classify(r.provenance.as_deref()));
        let claim_status = if receipt_claims.len() > 1 {
            if receipt_claims.iter().any(|c| c.receipt_key != receipt_claims[0].receipt_key) { "ambiguous_multiple_receipt_claims" }
            else if receipt_claims.iter().all(|c| c.matches_current_rule_fields) { "duplicate_claim_occurrences" }
            else { "conflicting_claim_occurrences" }
        } else if receipt_claims.is_empty() { "no_parsed_claim_occurrences" }
        else { "one_declared_claim_not_ownership_proof" };
        let ownership_status = if decoded.is_none() {
            "unreadable_rule_ownership_unresolved"
        } else if !unknown_claims.is_empty() {
            "unresolved_global_receipt_evidence"
        } else if receipt_claims.len() > 1 {
            if receipt_claims
                .iter()
                .any(|c| c.receipt_key != receipt_claims[0].receipt_key)
            {
                "ambiguous_multiple_receipt_claims"
            } else if receipt_claims.iter().all(|c| c.matches_current_rule_fields) {
                "duplicate_claim_occurrences"
            } else {
                "conflicting_claim_occurrences"
            }
        } else if authority == Some(rule_authority::Authority::AcceptedManual)
            && !receipt_claims.is_empty()
        {
            "manual_row_claim_conflict"
        } else if receipt_claims.is_empty() {
            "no_receipt_claim_explicit_decision_required"
        } else {
            "single_claim_requires_source_and_row_review"
        };
        let evidence = RowEvidence {
            rule_id: id.clone(),
            raw: raw(bytes, response_remaining)?,
            status: if decoded.is_some() {
                "decoded_without_rewriting_raw_row"
            } else {
                "unreadable_or_rule_id_mismatch"
            },
            authority,
            claim_status,
            ownership_status,
            registry_enforcement: match &enforced {
                None => "unknown",
                Some(ids) if ids.contains(id) => "retained_unfiltered_registry_constraint",
                Some(_) => "withheld_by_current_source_verification",
            },
            decoded,
            receipt_claims,
        };
        charge(&evidence, &mut response_remaining)?;
        rows.push(evidence);
    }
    charge(&observed_sources, &mut response_remaining)?;
    charge(&notes, &mut response_remaining)?;
    ensure!(
        observed_sources == sources(filesystem_root, &paths),
        "Ownership source fingerprints changed during inventory; recollect"
    );
    ensure!(
        snapshot == state.registry.snapshot_rule_ownership(pid)?,
        "Ownership evidence changed during inventory; recollect"
    );
    let report = Report {
        status: "ownership_inventory_only_requires_explicit_decisions",
        registry_snapshot_blake3: snapshot.digest()?,
        project_id: pid.into(),
        project_raw,
        project,
        identity_epoch_raw,
        identity_epoch: epoch.ok().flatten(),
        identity_status,
        corpus_revision_raw,
        rows,
        receipts,
        sources: observed_sources,
        enforcement_notes: notes,
        enforcement_basis: "Shared normal evaluator over the complete captured typed row set in normal file-pattern/rule-ID order; receipts/corpus/source verification are separate bounded live reads. Endpoint recapture is not an atomic cross-store enforcement receipt.",
        unresolved_claim_keys: unknown_claims,
        coverage: "Exact bounded raw registry inventory; receipt claims, IDs, basenames, matching text and candidate file existence never establish accepted ownership. Strict known receipt/rule shapes reject unknown fields and duplicate keys as unresolved. Enforcement is an unfiltered observation, not applicability to every change. Unknown/manual/foreign/pending/malformed claims require explicit review. Source fingerprints have two passes, each bounded to64paths/8MiB/4MiBfile; the shared enforcement verifier separately allows8MiB combined receipt JSON and source bytes. These bounds are not a whole-request physical-I/O claim or an atomic filesystem+Registry snapshot. Inventory never initializes identity, adopts ownership, edits policy or opens a native project runtime. Limits are explicit; no partial raw inventory returned.",
        adoption_performed: false,
        authored_files_written: 0,
    };
    struct Buffer {
        bytes: Vec<u8>,
    }
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let size = self
                .bytes
                .len()
                .checked_add(bytes.len())
                .filter(|s| *s <= RESPONSE_BYTES)
                .ok_or_else(|| {
                    std::io::Error::other(
                        "Ownership response exceeds4MiB; no partial inventory returned",
                    )
                })?;
            if size > self.bytes.capacity() {
                let target = size.next_power_of_two().min(RESPONSE_BYTES);
                self.bytes
                    .try_reserve_exact(target - self.bytes.len())
                    .map_err(std::io::Error::other)?;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer { bytes: Vec::new() };
    serde_json::to_writer(&mut buffer, &report)?;
    Ok(String::from_utf8(buffer.bytes)?)
}
