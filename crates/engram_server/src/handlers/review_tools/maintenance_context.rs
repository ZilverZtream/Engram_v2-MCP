//! Reuse evidence-bound maintenance for the exact native-Git review file scope.
use crate::services::instruction_maintenance::{self, MaintenanceRequest};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

const INTERPRETATION: &str = "This is instruction-maintenance evidence, not a semantic policy review or a requirement to rewrite guidance. Before is the exact comparison commit; after is current disk, including staged/unstaged differences. The review diff and this capture are separate observations, not an atomic snapshot or proof the patch matches current files. Preserve accepted exceptions, scope and shared-rule references. An implementation conflict may be a code defect; it is not permission to weaken policy. Draft and validate grounded decisions using this same checkout and complete packet; no proposal, policy adoption, authored write or compliance clearance is performed here.";

pub(super) fn collect(
    root: &Path,
    project: &str,
    mode: &str,
    head: Option<&str>,
    paths: Vec<String>,
) -> Value {
    let paths: Vec<_> = paths
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut context = json!({"status":"unavailable_comparison_base","changed_paths":paths,
        "review_working_directory":root,"comparison_commit":Value::Null,
        "evidence_collection_performed":false,"maintenance_semantic_review_performed":false,
        "authored_files_written":0,"interpretation":INTERPRETATION});
    if !matches!(mode.trim(), "staged" | "unstaged" | "head") {
        context["reason"]="Raw/patch diffs do not identify an exact comparison commit. Use the existing maintenance workflow with an explicit base_commit; no current HEAD is guessed as the patch base.".into();
        return context;
    }
    let base = (|| -> anyhow::Result<String> {
        let head = git2::Oid::from_str(
            head.ok_or_else(|| anyhow::anyhow!("Review start commit unavailable"))?,
        )?;
        if mode.trim() == "head" {
            Ok(git2::Repository::open(root)?
                .find_commit(head)?
                .parent_id(0)?
                .to_string())
        } else {
            Ok(head.to_string())
        }
    })();
    let base = match base {
        Ok(base) => base,
        Err(error) => {
            context["reason"] = format!("Exact comparison commit unavailable: {error:#}").into();
            return context;
        }
    };
    context["comparison_commit"] = base.clone().into();
    if paths.is_empty() || paths.len() > 16 {
        context["status"] = "unavailable_changed_file_scope".into();
        context["reason"]="The existing explicit maintenance packet accepts1..16 distinct changed files. No subset was silently selected; use bounded explicit maintenance scopes and reconcile their omissions.".into();
        return context;
    }
    let request = MaintenanceRequest {
        use_git_changes: false,
        git_path_prefixes: Vec::new(),
        changed_paths: paths,
        area_context: None,
        instruction_paths: Vec::new(),
        new_instruction_targets: Vec::new(),
        evidence_paths: Vec::new(),
        base_commit: base,
        draft: None,
    };
    match instruction_maintenance::review(root, project, &request)
        .and_then(|report| Ok(serde_json::to_value(report)?))
    {
        Ok(report) => {
            context["status"] = "maintenance_evidence_collected_draft_required".into();
            context["evidence_collection_performed"] = true.into();
            context["packet_blake3"] = report["packet_blake3"].clone();
            context["coverage"] = report["coverage"].clone();
            context["report"] = report;
        }
        Err(error) => {
            context["status"] = "maintenance_evidence_unavailable".into();
            context["reason"]=format!("Complete bounded maintenance capture unavailable: {error:#}. Existing review findings remain; no no-change or policy-compliance conclusion follows.").into();
        }
    }
    context
}

pub(super) fn summary(context: &Value) -> Value {
    if context.is_null() {
        return json!({"status":"not_requested_no_reviewed_diff",
            "evidence_collection_performed":false,"maintenance_semantic_review_performed":false,
            "authored_files_written":0,"maintenance_evidence_deferred":false});
    }
    json!({"status":context["status"],"comparison_commit":context["comparison_commit"],
        "changed_file_count":context["changed_paths"].as_array().map(Vec::len),
        "packet_blake3":context["packet_blake3"],"coverage":context["coverage"],
        "evidence_collection_performed":context["evidence_collection_performed"],
        "maintenance_semantic_review_performed":false,"authored_files_written":0,
        "maintenance_evidence_deferred":true,
        "instruction":"Recover the complete original review for the maintenance state/reason, exact file scope, before/current instruction and code snapshots, dependency manifest and accepted qualifications before drafting. No semantic decision or compliance clearance is supplied."})
}

pub(super) fn markdown(context: &Value) -> String {
    format!(
        "\n## Instruction maintenance evidence ? semantic review remains open\n\n```json\n{}\n```\n",
        serde_json::to_string_pretty(context)
            .unwrap_or_else(|_| "{\"status\":\"rendering_unavailable\"}".into())
    )
}
