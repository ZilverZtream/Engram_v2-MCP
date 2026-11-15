//! Compose existing scope discovery with complete, evidence-bound maintenance.
use super::*;
use crate::services::project_policy::{self, PolicyBudget};
use serde_json::{Value, json};

pub(super) struct Selection {
    pub instructions: Vec<String>,
    pub evidence: Vec<String>,
    pub context: Value,
}

pub(super) fn validate(req: &MaintenanceRequest) -> Result<Vec<String>> {
    ensure!(
        req.instruction_paths.is_empty()
            && req.area_context.is_none()
            && req.new_instruction_targets.is_empty(),
        "changed_paths discovers existing instructions; do not mix it with instruction_paths, area_context or new_instruction_targets"
    );
    let changed = paths(&req.changed_paths, 16)?;
    let mut evidence: BTreeSet<_> = changed.into_iter().collect();
    if !req.evidence_paths.is_empty() {
        evidence.extend(paths(&req.evidence_paths, 16)?);
    }
    ensure!(
        evidence.len() <= 16,
        "changed_paths and evidence_paths exceed 16 distinct files"
    );
    Ok(evidence.into_iter().collect())
}

pub(super) fn select(root: &Path, req: &MaintenanceRequest) -> Result<Selection> {
    let evidence = validate(req)?;
    let changed = paths(&req.changed_paths, 16)?;
    let policy = project_policy::resolve_with_budget(
        root,
        &changed,
        &[],
        PolicyBudget {
            max_sources: 8,
            max_characters_per_source: FILE_LIMIT,
            max_characters_total: PACKET_LIMIT / 2,
        },
    );
    let mut instructions = Vec::new();
    let mut sources = Vec::new();
    let mut unavailable = Vec::new();
    for source in &policy.sources {
        if source.truncated || source.content.len() > FILE_LIMIT {
            unavailable.push(json!({"path":source.path,"blake3":source.blake3,
                "reason":"Complete guidance exceeds the maintenance file limit; not submitted as a partial instruction."}));
            continue;
        }
        instructions.push(source.path.clone());
        sources.push(
            json!({"path":source.path,"blake3":source.blake3,"applicability":source.applicability}),
        );
    }
    instructions.sort();
    let incomplete =
        policy.coverage != "complete" || !policy.omissions.is_empty() || !unavailable.is_empty();
    let context = json!({
        "basis":"current_guidance_discovery_for_explicit_changed_paths",
        "changed_paths":changed,
        "status":if incomplete {"instruction_selection_incomplete"} else if instructions.is_empty() {"no_current_instruction_candidates_observed"} else {"current_instruction_candidates_collected"},
        "discovery_status":policy.status,"discovery_coverage":policy.coverage,
        "sources":sources,"omissions":policy.omissions,"unavailable_complete_sources":unavailable,
        "notes":policy.notes,
        "interpretation":"Changed paths are caller-declared inputs, not an exhaustive Git diff. Discovery uses current conventional guidance locations and scope metadata; it does not inventory instructions removed or formerly applicable at the base commit, infer semantic impact, accept policy or establish that no guidance change is warranted. Each selected instruction and evidence file retains exact base-commit and current snapshots in this packet; linked-reference coverage is reported by instruction_manifest. Explicit instruction_paths remain available for historical or otherwise undiscovered guidance. No index or authored file is changed."
    });
    Ok(Selection {
        instructions,
        evidence,
        context,
    })
}
