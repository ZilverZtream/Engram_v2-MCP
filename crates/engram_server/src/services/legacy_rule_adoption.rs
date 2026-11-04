//! Explicit source-bound ownership review; no native engine or authored file writes.
use crate::{models::*, state::AppState};
use engram_core::registry::{LegacyAdoptionBinding, LegacyAdoptionDecision, LegacyAdoptionPlan};
use engram_index::quality_gates::{QualityRule, QualitySource, parse_quality_source_checked};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::Path,
};

pub(crate) fn source_origin(root: &Path, source: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(
        root.is_absolute(),
        "Quality source requires an absolute project directory"
    );
    let root = std::fs::canonicalize(root)?;
    let source = std::fs::canonicalize(source)?;
    let relative = source.strip_prefix(&root).map_err(|_| {
        anyhow::anyhow!("Cannot establish project-relative quality source identity")
    })?;
    let origin = relative
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Quality source path must be lossless UTF-8"))?
        .replace('\\', "/");
    engram_core::registry::RuleSourceReceipt::validate_origin(&origin)?;
    Ok(origin)
}

pub(crate) fn read_source(root: &Path, origin: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        root.is_absolute(),
        "Quality source requires an absolute project directory"
    );
    let mut file = engram_core::safe_open_read(root, origin)?;
    let before = file.metadata()?;
    anyhow::ensure!(
        before.is_file() && before.len() <= 4 * 1024 * 1024,
        "Quality source must be a regular file of at most 4 MiB"
    );
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    anyhow::ensure!(
        bytes.len() <= 4 * 1024 * 1024
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "Quality source changed while reading"
    );
    Ok(String::from_utf8(bytes)?)
}

pub(crate) fn projection(rule: &QualityRule) -> anyhow::Result<String> {
    Ok(serde_json::to_string(rule)?)
}

/// Positional parser identities are insufficient to authorize a changed policy.
/// An adopted entry is reusable only with its complete reviewed projection.
pub(crate) fn resolve_aliases(
    rules: &[QualityRule],
    previous: Option<&super::quality_source::SourceReceipt>,
) -> anyhow::Result<std::collections::BTreeMap<String, engram_core::registry::RepoRule>> {
    let mut resolved = std::collections::BTreeMap::new();
    let Some(previous) = previous else {
        return Ok(resolved);
    };
    for alias in &previous.legacy_aliases {
        let matches: Vec<_> = rules
            .iter()
            .filter(|rule| rule.id == alias.source_rule_id)
            .collect();
        anyhow::ensure!(
            matches.len() == 1,
            "Adopted rule `{}` source entry `{}` is missing or ambiguous; explicit source-binding review is required",
            alias.rule_id,
            alias.source_rule_id
        );
        let current = projection(matches[0])?;
        let hash = blake3::hash(current.as_bytes()).to_hex().to_string();
        if current != alias.source_projection_json || hash != alias.source_projection_blake3 {
            let before: Value = serde_json::from_str(&alias.source_projection_json)?;
            let after: Value = serde_json::from_str(&current)?;
            let changed: Vec<_> = ["id", "text", "category", "severity", "path_scope", "source"]
                .into_iter()
                .filter(|field| before[*field] != after[*field])
                .collect();
            anyhow::bail!(
                "Adopted rule `{}` source projection changed (fields: {}; before BLAKE3 {}; after BLAKE3 {}). Explicit source-binding review is required",
                alias.rule_id,
                changed.join(", "),
                alias.source_projection_blake3,
                hash
            );
        }
        let owned = previous
            .rules
            .iter()
            .find(|rule| rule.rule_id == alias.rule_id)
            .ok_or_else(|| anyhow::anyhow!("Adopted alias has no owned row"))?;
        anyhow::ensure!(
            resolved
                .insert(alias.source_rule_id.clone(), owned.clone())
                .is_none(),
            "Duplicate adopted source entry"
        );
    }
    Ok(resolved)
}

/// Bound the encoded response before growing its allocation; exact raw JSON is
/// returned as a UTF-8 string so unknown numeric values are never rounded.
fn response(value: &Value) -> anyhow::Result<String> {
    struct Limited(Vec<u8>);
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            const LIMIT: usize = 4 * 1024 * 1024;
            if bytes.len() > LIMIT.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other(
                    "Adoption response exceeds 4 MiB; select fewer rows",
                ));
            }
            if self.0.capacity() - self.0.len() < bytes.len() {
                let capacity = (self.0.len() + bytes.len()).next_power_of_two().min(LIMIT);
                self.0
                    .try_reserve_exact(capacity - self.0.len())
                    .map_err(std::io::Error::other)?;
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited(Vec::new());
    serde_json::to_writer(&mut output, value)?;
    Ok(String::from_utf8(output.0)?)
}

pub(crate) fn execute(
    state: &AppState,
    req: &IngestQualityGatesRequest,
    source: QualitySource,
) -> anyhow::Result<String> {
    let review = req
        .adoption
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Missing adoption request"))?;
    let apply = matches!(review.mode, LegacyRuleAdoptionMode::Apply);
    anyhow::ensure!(
        !req.clear_existing && !req.replace_source,
        "Ownership review cannot be combined with corpus clearing or replacement"
    );
    anyhow::ensure!(
        review.bindings.len() <= 32,
        "Select at most 32 adoption rows"
    );
    anyhow::ensure!(
        !review.decision_ref.trim().is_empty() && review.decision_ref.len() <= 2048,
        "An explicit bounded decision reference is required"
    );
    anyhow::ensure!(
        review.expected_source_blake3.len() == 64
            && review
                .expected_source_blake3
                .bytes()
                .all(|b| b.is_ascii_hexdigit()),
        "Invalid source BLAKE3"
    );
    // Snapshot identity must exist already. Neither discovery nor apply establishes it.
    let snapshot = state.registry.snapshot_rule_ownership(&req.project_id)?;
    let project = snapshot.project()?;
    let root = Path::new(&project.directory);
    engram_core::registry::RuleSourceReceipt::validate_origin(&req.source_path)?;
    let origin = req.source_path.clone();
    let origin = source_origin(root, &engram_core::safe_join(root, &origin)?)?;
    let content = read_source(root, &origin)?;
    let source_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    anyhow::ensure!(
        source_hash.eq_ignore_ascii_case(&review.expected_source_blake3),
        "Source changed; recollect and review before adoption"
    );
    let parsed =
        parse_quality_source_checked(&content, source, &origin).map_err(anyhow::Error::msg)?;
    if review.bindings.is_empty() {
        anyhow::ensure!(
            !apply,
            "Apply requires explicit row mappings and a preview plan hash"
        );
        anyhow::ensure!(
            review.source_entry_offset <= parsed.len(),
            "Source discovery offset is out of range"
        );
        let end = review
            .source_entry_offset
            .saturating_add(128)
            .min(parsed.len());
        return response(
            &json!({"status":"selection_required", "adoption_performed":false,
            "source":origin, "source_blake3":source_hash, "source_entries":&parsed[review.source_entry_offset..end],
            "next_source_entry_offset":if end < parsed.len() {Some(end)} else {None},
            "identity_status":if snapshot.identity_epoch()?.is_some() {"established"} else {"missing_epoch_explicit_identity_establishment_required"},
            "authored_files_written":0,"searchable_evidence_published":false}),
        );
    }
    anyhow::ensure!(
        review.source_entry_offset == 0,
        "Entry paging applies only to discovery"
    );
    let mut bindings = Vec::new();
    let mut changes = Vec::new();
    let mut budget = 1024 * 1024;
    for selected in &review.bindings {
        let mut matches = parsed
            .iter()
            .filter(|rule| rule.id == selected.source_rule_id);
        let rule = matches
            .next()
            .ok_or_else(|| anyhow::anyhow!("Selected source entry is missing"))?;
        anyhow::ensure!(matches.next().is_none(), "Ambiguous source entry identity");
        let projection = projection(rule)?;
        let raw = snapshot
            .rules()
            .get(&selected.rule_id)
            .ok_or_else(|| anyhow::anyhow!("Selected rule is missing"))?;
        let bytes = raw
            .len()
            .checked_add(projection.len())
            .ok_or_else(|| anyhow::anyhow!("Preview budget overflow"))?;
        anyhow::ensure!(
            bytes <= budget,
            "Selected rows and projections exceed 1 MiB; select fewer rows"
        );
        budget -= bytes;
        let decision = match selected.decision {
            LegacyRuleAdoptionDecision::AdoptUnverified => LegacyAdoptionDecision::AdoptUnverified,
            LegacyRuleAdoptionDecision::TransferManual => LegacyAdoptionDecision::TransferManual,
        };
        changes.push(json!({"rule_id":selected.rule_id,"before_raw_utf8":std::str::from_utf8(raw)?,
            "before_blake3":blake3::hash(raw).to_hex().to_string(),"source_projection_json":projection,
            "decision":decision,"proposed_provenance":format!("quality-gate source: {origin}")}));
        bindings.push(LegacyAdoptionBinding {
            rule_id: selected.rule_id.clone(),
            expected_raw_blake3: selected.expected_raw_blake3.clone(),
            source_rule_id: selected.source_rule_id.clone(),
            source_projection_json: projection,
            decision,
        });
    }
    let plan = LegacyAdoptionPlan {
        origin: origin.clone(),
        source_blake3: source_hash.clone(),
        authored: matches!(
            source,
            QualitySource::CopilotInstructions | QualitySource::CodingRulesMd
        ),
        bindings,
    };
    let preview = match state.registry.preview_legacy_repo_rule_adoption(
        &snapshot,
        &plan,
        &review.decision_ref,
    ) {
        Ok(preview) => preview,
        Err(error) if !apply => {
            return response(&json!({"status":"blocked","reason":error.to_string(),
            "changes":changes,"adoption_performed":false,"authored_files_written":0,"searchable_evidence_published":false}));
        }
        Err(error) => return Err(error),
    };
    // Ensure a bounded reviewable response can be rendered before any write.
    let preview_response = response(
        &json!({"status":"ready_for_explicit_apply","plan_blake3":preview.plan_blake3,
        "ownership_basis_blake3":snapshot.adoption_basis_digest()?,"source":origin,"source_blake3":source_hash,
        "changes":changes,"adoption_performed":false,"authored_files_written":0,"searchable_evidence_published":false,
        "scope":"Ownership and provenance only; rule IDs, content, scope, priority and dates are preserved. Search evidence requires a separate import."}),
    )?;
    if !apply {
        return Ok(preview_response);
    }
    anyhow::ensure!(
        review.expected_plan_blake3.as_deref() == Some(preview.plan_blake3.as_str()),
        "Adoption plan changed or preview hash missing; no rows adopted, recollect and review"
    );
    anyhow::ensure!(
        blake3::hash(read_source(root, &origin)?.as_bytes())
            .to_hex()
            .as_str()
            == source_hash,
        "Source changed before adoption; no rows adopted"
    );
    let outcome = state
        .registry
        .adopt_legacy_repo_rules(&snapshot, &plan, &review.decision_ref)?;
    // Filesystem and Registry are separate stores. A postcommit change is reported
    // as such; an already committed adoption must never be reported as rolled back.
    let status = match read_source(root, &origin) {
        Ok(now) if blake3::hash(now.as_bytes()).to_hex().as_str() == source_hash => {
            "matches_postcommit_read"
        }
        Ok(_) => "changed_after_precommit_read_recheck_required",
        Err(_) => "unavailable_after_commit_recheck_required",
    };
    response(
        &json!({"status":"adopted","adoption_performed":true,"outcome":outcome,
        "source_verification":status,"cross_store_atomic":false,"authored_files_written":0,
        "searchable_evidence_published":false}),
    )
}
