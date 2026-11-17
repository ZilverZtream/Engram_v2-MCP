//! Prior applicability supplies evidence and review candidates, not current authority.
use super::*;
use crate::services::project_policy::{PolicyContext, resolve_at_revision_with_budget};

pub(super) struct Prior {
    pub context: PolicyContext,
    pub candidates: Vec<references::Candidate>,
    pub observations: Vec<Value>,
    pub retained_bytes: usize,
    pub requires_review: bool,
}

pub(super) fn discover(
    root: &Path,
    req: &MaintenanceRequest,
    changed: &[String],
    selected: &[String],
) -> Result<Prior> {
    let context = resolve_at_revision_with_budget(
        root,
        git2::Oid::from_str(&req.base_commit)?,
        changed,
        &[],
        PolicyBudget {
            max_sources: 8,
            max_characters_per_source: FILE_LIMIT,
            max_characters_total: PACKET_LIMIT / 2,
        },
    )?;
    let retained_bytes = context
        .sources
        .iter()
        .chain(&context.linked_sources)
        .map(|source| source.content.len())
        .sum();
    let mut result = Prior {
        context,
        candidates: Vec::new(),
        observations: Vec::new(),
        retained_bytes,
        requires_review: false,
    };
    let mut remaining = PACKET_LIMIT / 2;
    for source in &result.context.sources {
        if selected.contains(&source.path) {
            continue;
        }
        result.requires_review = true;
        if source.truncated || source.content.len() > FILE_LIMIT {
            result.observations.push(json!({"path":source.path,"before_blake3":source.blake3,
                "status":"complete_prior_instruction_not_supplied",
                "detail":"Prior source exceeds maintenance complete-file or selection limits; recover it from the stated Git revision. No partial instruction was selected."}));
            continue;
        }
        match disk(root, &source.path, &mut remaining, FILE_LIMIT) {
            Ok(Some(current)) => {
                result.observations.push(json!({"path":source.path,"before_blake3":source.blake3,
                    "current_blake3":current.blake3,"status":"current_instruction_available_for_prior_applicability_review"}));
                result.candidates.push(references::Candidate {
                    path: source.path.clone(), current_blake3: current.blake3,
                    current_bytes: current.content.len(),
                    matches: vec![json!({"kind":"prior_applicability","source_revision":req.base_commit,
                        "before_blake3":source.blake3,"applicability_at_base":source.applicability})],
                    selection_basis: "guidance_applicable_at_base_commit",
                });
            }
            Ok(None) => result.observations.push(json!({"path":source.path,"before_blake3":source.blake3,
                "status":"absent_now_historical_evidence_only",
                "detail":"The guide applied at the base revision and is absent now. Its removal requires review; it was not accepted or selected as an editable current instruction."})),
            Err(error) => result.observations.push(json!({"path":source.path,"before_blake3":source.blake3,
                "status":"current_instruction_unavailable_historical_evidence_only","detail":format!("{error:#}")})),
        }
    }
    Ok(result)
}
