//! Wanted workflow: follow omitted guidance without inventing arguments or mixing revisions.
use super::*;
use crate::services::project_policy;
use serde_json::{Value, json};

fn guide() -> String {
    (1..=310)
        .map(|line| format!("Rule {line}: preserve the authored approval exception.\r\n"))
        .collect()
}

fn write_guide(root: &Path, text: &str) {
    std::fs::create_dir_all(root.join(".github")).unwrap();
    std::fs::write(root.join(".github/copilot-instructions.md"), text).unwrap();
}

fn policy(root: &Path) -> project_policy::PolicyContext {
    project_policy::resolve(root, &["Site/Order.aspx.vb".into()], &[])
        .with_source_recovery("project")
}

fn action(policy: &project_policy::PolicyContext) -> Value {
    serde_json::to_value(
        policy
            .omissions
            .iter()
            .find(|row| row.path == ".github/copilot-instructions.md")
            .unwrap()
            .read_action
            .as_ref()
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn policy_source_handoff_reads_complete_omitted_guidance_through_executable_pinned_pages() {
    let owned = tempfile::tempdir().unwrap();
    let original = guide();
    write_guide(owned.path(), &original);
    let context = policy(owned.path());
    assert_ne!(context.coverage, "complete");
    let mut next = action(&context);
    let hash = blake3::hash(original.as_bytes()).to_hex().to_string();
    assert_eq!(next["arguments"]["expected_file_blake3"], hash);
    let markdown = context.render_markdown();
    let encoded = markdown
        .split("```json\n")
        .nth(1)
        .unwrap()
        .split("\n```")
        .next()
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(encoded).unwrap(), next);
    let graph = GraphStore::open(&owned.path().join("graph.redb")).unwrap();
    let mut lines = Vec::new();
    loop {
        assert_eq!(next["tool"], "get_full_method_body");
        let req = serde_json::from_value(next["arguments"].clone()).unwrap();
        let page = current_file_excerpt(&graph, "project", owned.path(), &req).unwrap();
        assert_eq!(page.source_file_hash, hash);
        lines.extend(page.source_code.lines().map(str::to_owned));
        let preview = page.file_excerpt.unwrap();
        if preview.reaches_eof {
            assert!(preview.next_page.is_none());
            break;
        }
        next = preview.next_page.unwrap();
        assert_eq!(next["arguments"]["expected_file_blake3"], hash);
    }
    assert_eq!(
        lines.join("\n"),
        original.lines().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn policy_source_handoff_refuses_changed_page_and_refreshed_guidance_can_restart() {
    let owned = tempfile::tempdir().unwrap();
    write_guide(owned.path(), &guide());
    let graph = GraphStore::open(&owned.path().join("graph.redb")).unwrap();
    let first = action(&policy(owned.path()));
    let req = serde_json::from_value(first["arguments"].clone()).unwrap();
    let page = current_file_excerpt(&graph, "project", owned.path(), &req).unwrap();
    let next = page.file_excerpt.unwrap().next_page.unwrap();
    write_guide(
        owned.path(),
        &(guide() + "Preserve this new human exception.\r\n"),
    );
    let req = serde_json::from_value(next["arguments"].clone()).unwrap();
    let error = current_file_excerpt(&graph, "project", owned.path(), &req).unwrap_err();
    assert!(
        error.contains("No source returned") && error.contains("Refresh the originating guidance")
    );
    let refreshed = action(&policy(owned.path()));
    assert_ne!(
        refreshed["arguments"]["expected_file_blake3"],
        first["arguments"]["expected_file_blake3"]
    );
    let req = serde_json::from_value(refreshed["arguments"].clone()).unwrap();
    assert!(current_file_excerpt(&graph, "project", owned.path(), &req).is_ok());
}

#[test]
fn policy_source_handoff_ranges_check_the_same_raw_snapshot_and_reject_invalid_guards() {
    let owned = tempfile::tempdir().unwrap();
    std::fs::write(
        owned.path().join("a.vb"),
        "' first\r\n' second\r\n' third\r\n",
    )
    .unwrap();
    let bytes = std::fs::read(owned.path().join("a.vb")).unwrap();
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let mut args = json!({"project_id":"project","file_path":"a.vb","context_lines":0,"source_ranges":[{"line_start":1,"line_end":1},{"line_start":3,"line_end":3}],"expected_file_blake3":hash.to_uppercase()});
    let req = serde_json::from_value(args.clone()).unwrap();
    let result = current_source_ranges(owned.path(), &req).unwrap();
    assert_eq!(result.source_file_hash, hash);
    assert_eq!(result.windows[1].source_code, "' third");
    args["expected_file_blake3"] = json!("0".repeat(64));
    assert!(current_source_ranges(owned.path(), &serde_json::from_value(args).unwrap()).is_err());
    for args in [
        json!({"project_id":"project","fqn":"Save","expected_file_blake3":hash}),
        json!({"project_id":"project","file_path":"a.vb","expected_file_blake3":"typo"}),
    ] {
        assert!(validate_source_hash_request(&serde_json::from_value(args).unwrap()).is_err());
    }
}

#[test]
fn policy_source_handoff_does_not_turn_historical_or_unreadable_omissions_into_live_reads() {
    let owned = tempfile::tempdir().unwrap();
    write_guide(owned.path(), &guide());
    let mut historical = project_policy::resolve(owned.path(), &["a.ts".into()], &[]);
    historical.source_revision = Some("historical-commit".into());
    assert!(
        historical
            .with_source_recovery("project")
            .omissions
            .iter()
            .all(|row| row.read_action.is_none())
    );
    let mut unknown = project_policy::resolve(owned.path(), &["a.ts".into()], &[]);
    for row in &mut unknown.omissions {
        row.blake3 = None;
    }
    assert!(
        unknown
            .with_source_recovery("project")
            .omissions
            .iter()
            .all(|row| row.read_action.is_none())
    );
    assert!(
        project_policy::PolicyContext::historical_unavailable()
            .with_source_recovery("project")
            .omissions
            .is_empty()
    );
}

#[test]
fn policy_source_handoff_direct_source_preserves_evidence_and_emits_project_bound_recovery() {
    let owned = tempfile::tempdir().unwrap();
    write_guide(owned.path(), &guide());
    let original = json!({"file_path":"Site/Order.aspx.vb","source_code":"Save()","freshness":{"status":"INDEX_BEHIND"}});
    let (body, context) = method_body_policy::attach(
        "project",
        owned.path(),
        &["Site/Order.aspx.vb".into()],
        original.to_string(),
        true,
        None,
    )
    .unwrap();
    let mut body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        body["project_policy_sources"]["omissions"][0]["read_action"],
        action(&context)
    );
    body.as_object_mut()
        .unwrap()
        .remove("project_policy_sources");
    assert_eq!(body, original);
}
