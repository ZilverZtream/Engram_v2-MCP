//! Reviewed alias metadata only; real source/search publication remains separate.
use crate::{models::*, state::AppState};
use engram_core::registry::{AliasReviewAction, AliasReviewBinding, AliasReviewPlan, AliasReviewSource};
use engram_index::quality_gates::{parse_quality_source_checked, QualitySource};
use serde_json::json;
use std::path::Path;

fn observe(root: &Path, origin: &str) -> anyhow::Result<(AliasReviewSource, Option<String>)> {
    anyhow::ensure!(root.is_absolute(), "Project source root is not absolute");
    let root = std::fs::canonicalize(root)?;
    let path = engram_core::safe_join(&root, origin)?;
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((AliasReviewSource::Missing, None)),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink(),
                "Source absence cannot be inferred from an unsupported file type");
            let text = super::legacy_rule_adoption::read_source(&root, origin)?;
            Ok((AliasReviewSource::Present {
                blake3:blake3::hash(text.as_bytes()).to_hex().to_string()
            }, Some(text)))
        }
    }
}

fn expected_source(expected: &LegacyAliasSourceExpectation, actual: &AliasReviewSource) -> bool {
    match (expected, actual) {
        (LegacyAliasSourceExpectation::Missing, AliasReviewSource::Missing) => true,
        (LegacyAliasSourceExpectation::Present { blake3:expected }, AliasReviewSource::Present { blake3:actual }) =>
            expected.len() == 64 && expected.eq_ignore_ascii_case(actual),
        _ => false,
    }
}

pub(crate) fn execute(state: &AppState, req: &IngestQualityGatesRequest,
    source: QualitySource) -> anyhow::Result<String> {
    let review = req.alias_review.as_ref().ok_or_else(|| anyhow::anyhow!("Missing alias review"))?;
    anyhow::ensure!(req.adoption.is_none() && !req.clear_existing && !req.replace_source,
        "Alias review is exclusive with adoption, corpus clearing and ingestion replacement");
    anyhow::ensure!(!review.bindings.is_empty() && review.bindings.len() <= 32, "Select 1..32 alias dispositions");
    engram_core::registry::RuleSourceReceipt::validate_origin(&req.source_path)?;
    let snapshot = state.registry.snapshot_rule_ownership(&req.project_id)?;
    snapshot.adoption_basis_digest()?;
    let project = snapshot.project()?;
    let root = Path::new(&project.directory);
    let origin = req.source_path.clone();
    let (observed, content) = observe(root, &origin)?;
    anyhow::ensure!(expected_source(&review.source, &observed),
        "Source evidence changed or missing-source assertion is false; recollect before review");
    let previous = snapshot.receipts().get(&super::quality_source::receipt_key(&origin))
        .ok_or_else(|| anyhow::anyhow!("Reviewed source receipt is missing"))?;
    let receipt = engram_core::registry::RuleSourceReceipt::decode(
        &super::quality_source::receipt_key(&origin), previous)?;
    anyhow::ensure!(receipt.authored == matches!(source,QualitySource::CopilotInstructions | QualitySource::CodingRulesMd),
        "Source kind cannot change during alias review");
    let parsed = match content {
        Some(text) => parse_quality_source_checked(&text, source, &origin).map_err(anyhow::Error::msg)?,
        None => Vec::new(),
    };
    anyhow::ensure!(parsed.len() <= 512, "Source entry count exceeds the alias review budget");
    let projections = parsed.iter().map(super::legacy_rule_adoption::projection)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let bindings = review.bindings.iter().map(|binding| -> anyhow::Result<AliasReviewBinding> {
        let expected_alias_blake3 = match &binding.expected_alias_blake3 {
            Some(hash) => hash.clone(),
            None => {
                let alias = receipt.legacy_aliases.iter().find(|alias| alias.rule_id==binding.rule_id)
                    .ok_or_else(|| anyhow::anyhow!("Selected row has no adopted alias"))?;
                blake3::hash(&serde_json::to_vec(alias)?).to_hex().to_string()
            }
        };
        Ok(AliasReviewBinding {
            rule_id:binding.rule_id.clone(),expected_raw_blake3:binding.expected_raw_blake3.clone(),
            expected_alias_blake3,
            disposition:match &binding.disposition {
                LegacyAliasDisposition::RebindOnlyUnchanged { source_rule_id } =>
                    AliasReviewAction::RebindOnlyUnchanged { source_rule_id:source_rule_id.clone() },
                LegacyAliasDisposition::Retire => AliasReviewAction::Retire,
            },
        })
    }).collect::<anyhow::Result<Vec<_>>>()?;
    let plan = AliasReviewPlan { origin,source:observed.clone(),current_projections:projections,bindings };
    let preview = state.registry.preview_legacy_alias_review(&snapshot,&plan,&review.decision_ref)?;
    if matches!(observed, AliasReviewSource::Present { .. }) {
        // A reviewed mapping must have a supported next import. Validate all
        // unselected aliases too, and refuse promotion-ID collisions before
        // creating another durable pending state.
        let next = engram_core::registry::RuleSourceReceipt::decode(
            &super::quality_source::receipt_key(&plan.origin),
            preview["after_receipt_utf8"].as_str().unwrap().as_bytes())?;
        let aliases = super::legacy_rule_adoption::resolve_aliases(&parsed,Some(&next))?;
        let claims = snapshot.receipts().iter().map(|(key,raw)|
            engram_core::registry::RuleSourceReceipt::decode(key,raw))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for rule in parsed.iter().filter(|rule| !aliases.contains_key(&rule.id)
            && crate::handlers::quality_gate_tools::source_promotion_candidate(rule,source))
            .take(crate::handlers::quality_gate_tools::SOURCE_PROMOTION_LIMIT)
        {
            let id = format!("qg-{}",rule.id);
            anyhow::ensure!(claims.iter().all(|claim| claim.origin==plan.origin
                || !claim.rules.iter().any(|owned| owned.rule_id==id)),
                "Current source promotion has a competing source claim; no lifecycle change applied");
            anyhow::ensure!(!receipt.legacy_aliases.iter().any(|alias| alias.rule_id==id),
                "Current source promotion would collide with or resurrect a legacy ID; review the source mapping first");
            if snapshot.rules().contains_key(&id) {
                anyhow::ensure!(receipt.rules.iter().any(|owned| owned.rule_id==id),
                    "Current source promotion collides with an unselected/manual row; no lifecycle change applied");
            }
        }
    }
    let apply = matches!(review.mode,LegacyRuleAdoptionMode::Apply);
    let value = if apply {
        anyhow::ensure!(review.expected_plan_blake3.as_deref() == preview["plan_blake3"].as_str(),
            "Exact preview plan hash is required; no alias lifecycle changes applied");
        anyhow::ensure!(observe(root,&plan.origin)?.0 == observed,
            "Source changed before apply; no alias lifecycle changes applied");
        let outcome = state.registry.apply_legacy_alias_review(&snapshot,&plan,&review.decision_ref)?;
        let postcommit = match observe(root,&plan.origin) {
            Ok((now,_)) if now == observed => "matches_postcommit_read",
            Ok(_) => "changed_after_commit_recheck_required",
            Err(_) => "unavailable_after_commit_recheck_required",
        };
        json!({"status":"alias_review_applied","outcome":outcome,"source_status":postcommit,
            "source_reimport_required":true,"authored_files_written":0,"searchable_evidence_published":false,
            "instruction":"Reviewed metadata is committed. Old managed rules and search documents remain withheld by the pending receipt until normal source reimport completes. A missing source cannot be reimported while absent."})
    } else {
        json!({"status":"ready_for_explicit_alias_apply","review":preview,
            "source_reimport_required":true,"authored_files_written":0,"searchable_evidence_published":false,
            "instruction":"Review the complete row, alias, source and receipt evidence. Apply requires the exact plan hash and caller decision reference. This does not authenticate human authority or accept changed policy."})
    };
    let rendered = serde_json::to_string(&value)?;
    anyhow::ensure!(rendered.len() <= 4*1024*1024,"Complete alias review exceeds response budget");
    Ok(rendered)
}
