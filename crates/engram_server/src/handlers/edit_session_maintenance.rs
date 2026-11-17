//! Navigation from a completed edit to the existing maintenance-only review.
use serde_json::{Value, json};

pub(super) fn handoff(project: &str, base: Option<&str>, edited: &[String], full: bool) -> Value {
    let valid_base =
        base.filter(|base| base.len() == 40 && base.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let status = if edited.is_empty() {
        "not_requested_no_edits"
    } else if !full {
        "full_profile_required"
    } else if valid_base.is_none() {
        "unavailable_start_commit"
    } else {
        "not_reviewed_actions_available"
    };
    let calls = if status == "not_reviewed_actions_available" {
        edited
            .chunks(16)
            .map(|paths| {
                json!({"tool":"pre_push_audit","arguments":{
            "project_id":project,"instruction_maintenance":{
                "base_commit":valid_base,"changed_paths":paths}}})
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    json!({"status":status,"base_commit":valid_base,"declared_edited_file_count":edited.len(),
        "call_count":calls.len(),"calls":calls,"maintenance_review_performed":false,
        "automatic_instruction_writes":false,"instruction_compliance_verified":false,
        "interpretation":"Review guidance after this declared edit scope using the existing maintenance-only mode. The recorded start commit supplies Git-before evidence; it does not capture an initially dirty worktree or prove session-exclusive changes. Actions read then-current disk state and are not immutable evidence recovery. Each action selects at most16 declared files; splitting does not establish complete cross-packet dependencies. Review the returned evidence and preserve accepted exceptions; no_change, conflict or needs_evidence may be appropriate. No policy intent, freshness or compliance is inferred by this handoff. Core must switch to a profile advertising pre_push_audit (Full or a named workflow preset) for maintenance review; missing start commits are not guessed."})
}

/// Action details remain in the immutable full JSON/Markdown report. Optional
/// navigation must not make a large edit's required lifecycle manifest unfit.
pub(super) fn compact_summary(summary: &Value) -> Value {
    let mut summary = summary.clone();
    if let Some(handoff) = summary
        .get_mut("instruction_maintenance")
        .and_then(Value::as_object_mut)
    {
        handoff.remove("calls");
        handoff.remove("interpretation");
        handoff.insert("action_details".into(), json!("Recover the complete original report for every maintenance call; none are executed by completion."));
    }
    summary
}
