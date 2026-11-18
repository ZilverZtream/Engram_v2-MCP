//! Compose existing scope discovery with complete, evidence-bound maintenance.
use super::*;
use crate::services::project_policy::{self, PolicyBudget};
use serde_json::{Value, json};
mod git_changes;
mod prior_guidance;
mod references;
mod symbols;

const GIT_EVIDENCE_PATH_LIMIT: usize = 128;

pub(super) struct Selection {
    pub instructions: Vec<String>,
    pub evidence: Vec<String>,
    pub context: Value,
    pub prior_source_bytes: usize,
}

pub(super) fn validate(req: &MaintenanceRequest) -> Result<Vec<String>> {
    ensure!(
        req.instruction_paths.is_empty()
            && req.area_context.is_none()
            && req.new_instruction_targets.is_empty(),
        "Changed-file selection discovers existing instructions; do not mix it with instruction_paths, area_context or new_instruction_targets"
    );
    ensure!(
        !req.use_git_changes || req.changed_paths.is_empty(),
        "use_git_changes and explicit changed_paths are alternatives; select one"
    );
    let changed = if req.use_git_changes {
        Vec::new()
    } else {
        paths(&req.changed_paths, 16)?
    };
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
    let mut evidence: BTreeSet<_> = validate(req)?.into_iter().collect();
    let changed = if req.use_git_changes {
        git_changes::paths_since(root, &req.base_commit)?
    } else {
        paths(&req.changed_paths, 16)?
    };
    evidence.extend(changed.iter().cloned());
    let evidence_limit = if req.use_git_changes {
        GIT_EVIDENCE_PATH_LIMIT
    } else {
        16
    };
    ensure!(
        evidence.len() <= evidence_limit,
        "Changes and evidence_paths exceed{evidence_limit} distinct files; use explicit changed_paths for a bounded review slice or reduce the comparison. No files were silently omitted."
    );
    let evidence: Vec<_> = evidence.into_iter().collect();
    let basis = if req.use_git_changes {
        "git_base_to_current_worktree"
    } else {
        "current_guidance_discovery_for_explicit_changed_paths"
    };
    let interpretation = if req.use_git_changes {
        "Changed paths are Git's net comparison from the exact base commit to current disk, including committed-since-base, staged/unstaged and nonignored untracked files. Staged contents are not a separate after snapshot; after evidence is current disk. Renames may appear as removal/addition; no semantic rename is inferred. Git ignores exclude untracked files, not tracked changes. The listing and subsequent captures are separate observations, not an atomic worktree snapshot or proof of exhaustive semantic impact."
    } else {
        "Changed paths are caller-declared inputs, not an exhaustive Git diff."
    };
    if changed.is_empty() {
        return Ok(Selection {
            instructions: Vec::new(),
            evidence,
            prior_source_bytes: 0,
            context: json!({"basis":basis,"changed_paths":[],"status":"no_git_changes_observed",
                "sources":[],"omissions":[],"notes":[],"interpretation":format!("{interpretation} No current instruction candidates were selected. This does not establish that guidance is semantically correct or needs no maintenance. No index or authored file is changed.")}),
        });
    }
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
    let prior = prior_guidance::discover(root, req, &changed, &instructions)?;
    let mut complete_bytes = policy
        .sources
        .iter()
        .map(|source| source.content.len())
        .sum::<usize>();
    let mut referenced = references::discover(root, req, &changed, &instructions)?;
    for candidate in prior.candidates.into_iter().chain(referenced.candidates) {
        if instructions.contains(&candidate.path) {
            continue;
        }
        if instructions.len() >= 8
            || complete_bytes.saturating_add(candidate.current_bytes) > PACKET_LIMIT / 2
        {
            referenced.omissions.push(json!({"path":candidate.path,"blake3":candidate.current_blake3,"reason":"complete_instruction_selection_budget","reference_matches":candidate.matches}));
            continue;
        }
        complete_bytes += candidate.current_bytes;
        instructions.push(candidate.path.clone());
        sources.push(json!({"path":candidate.path,"blake3":candidate.current_blake3,
            "selection_basis":candidate.selection_basis,
            "applicability":["Maintenance review candidate only; no authority over changed files outside the instruction's own scope."],
            "reference_matches":candidate.matches}));
    }
    instructions.sort();
    let incomplete = policy.coverage != "complete"
        || !policy.omissions.is_empty()
        || !unavailable.is_empty()
        || !referenced.omissions.is_empty()
        || !referenced.notes.is_empty()
        || prior.context.coverage != "complete"
        || !prior.context.omissions.is_empty()
        || prior.observations.iter().any(|row| row["status"].as_str().is_some_and(|status| status.contains("unavailable") || status.contains("not_supplied")));
    let context = json!({
        "basis":basis,
        "changed_paths":changed,
        "status":if incomplete {"instruction_selection_incomplete"} else if prior.requires_review {"historical_guidance_requires_review"} else if instructions.is_empty() {"no_current_instruction_candidates_observed"} else {"current_instruction_candidates_collected"},
        "prior_guidance":prior.context,
        "prior_guidance_current_observations":prior.observations,
        "prior_guidance_interpretation":"Prior guidance and linked bodies come from base_commit only. Prior applicability does not grant current authority, accept a scope change or authorize removal. Surviving documents are review candidates with their complete current/before snapshots; absent documents remain historical evidence, not editable current instructions. Complete-source budgets include the retained historical bodies. No semantic policy decision or authored write is performed.",
        "discovery_status":policy.status,"discovery_coverage":policy.coverage,
        "sources":sources,"omissions":policy.omissions,"unavailable_complete_sources":unavailable,
        "notes":policy.notes,
        "reference_discovery":{"coverage":if referenced.omissions.is_empty() && referenced.notes.is_empty(){"bounded_candidates_examined"}else{"incomplete"},
            "omissions":referenced.omissions,"notes":referenced.notes,
            "limits":{"documents":64,"complete_source_read_bytes":2097152,"links_per_snapshot":256,"reference_chain_depth":3,"matches_per_instruction":16,"reference_visits":4096,"changed_symbol_source_read_bytes":PACKET_LIMIT,"symbol_declaration_candidates":1024,"symbol_references_per_instruction":256},
            "scope":"Current conventional guidance inventory, bounded literal Markdown links and direct inline-code symbol mentions, including names inside authored code expressions. Ordinary prose observations remain in the manifest but do not drive inverse symbol selection. Symbol declarations are parsed once from complete before/current copies of the caller-selected VB/JS/TS/SQL and appSettings configuration changed files only; no codebase inventory, graph or model. Unchanged observed method bytes alone do not select an instruction. References retain their own snapshot hashes; instruction_manifest reports captured dependency freshness. Name matches and review selection do not establish bindings, semantic impact or expanded scope authority. Listing, discovery and final packet capture are separate observations."},
        "interpretation":format!("{interpretation} Discovery selects current scoped guidance, directly changed conventional guides, bounded prior applicable guidance and literal-reference maintenance candidates. A review candidate does not gain authority over files outside its current scope. Prior guidance is explicitly revision-pinned and cannot substitute for current policy. Discovery remains bounded, does not infer semantic impact, accept policy or establish that no guidance change is warranted. Each selected current instruction and evidence file retains exact base-commit and current snapshots; linked-reference coverage is reported by instruction_manifest and prior_guidance. Explicit instruction_paths remain available for otherwise undiscovered current guidance. No index or authored file is changed.")
    });
    Ok(Selection {
        instructions,
        evidence,
        context,
        prior_source_bytes: prior.retained_bytes,
    })
}
