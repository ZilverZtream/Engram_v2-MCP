//! Delivery only: composed checks keep their original full evidence.
use super::change_set_delivery as snapshots;
use rmcp::ErrorData as McpError;
use serde_json::{Value, json};
pub(super) use snapshots::EditReport;
use std::path::Path;

pub(super) fn recover(
    data: &Path,
    project: &str,
    kind: EditReport,
    id: Option<&str>,
    offset: usize,
    budget: usize,
) -> Result<Option<String>, McpError> {
    if let Some(id) = id {
        return snapshots::recover_edit_report(data,project,id,offset,budget,kind).map(Some).map_err(|mut error| {
            error.message=format!("{} Edit-workflow recovery is read-only. Do not repeat begin/complete merely to retrieve a report. Use tracked lifecycle tokens only for intended state changes; use detect_incomplete_changes for a fresh advisory check.",error.message).into();
            error
        });
    }
    if offset != 0 {
        return Err(McpError::invalid_params(
            "response_offset requires response_id from edit-workflow recovery",
            None,
        ));
    }
    Ok(None)
}

/// Prepare before a lifecycle mutation: errors must leave that lifecycle intact.
pub(super) fn finish(
    data: &Path,
    project: &str,
    kind: EditReport,
    body: String,
    summary: Value,
    output_json: bool,
    budget: usize,
) -> Result<String, McpError> {
    let original = if output_json {
        serde_json::to_string(
            &json!({"schema_version":1,"advisory":true,"summary":summary,"report_markdown":body}),
        )
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
    } else {
        body
    };
    if original.len() <= budget {
        return Ok(original);
    }
    let id = blake3::hash(original.as_bytes()).to_hex().to_string();
    let manifest = json!({"summary":summary,"advisory":true,"report_content":"DEFERRED_NOT_SUPPLIED",
        "response_delivery":{"status":"DEFERRED_EDIT_WORKFLOW","complete":false,"response_id":id,
            "original_bytes":original.len(),"original_format":if output_json {"json"} else {"markdown"},
            "response_budget_bytes":budget,"estimate":"ceil(UTF-8 reply-text bytes/4); not provider tokens or transport envelope bytes",
            "instruction":"Recover the complete immutable report before acting on its findings, coverage, policy or freshness. Recovery does not rerun analysis, open a session or complete it again. Lifecycle metadata is not approval or current-source certification.",
            "retention":"At most24hours or32newer snapshots for this project/tool",
            "next_call":snapshots::edit_report_continuation(kind,project,&id,0,budget)}});
    let encoded = serde_json::to_string(&manifest)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    let reply = if output_json {
        encoded
    } else {
        format!("# Edit workflow: deferred report\n\n```json\n{encoded}\n```\n")
    };
    if reply.len() > budget {
        return Err(McpError::invalid_params(
            format!(
                "max_tokens is too small for edit-workflow lifecycle/recovery metadata; at least {} estimated tokens ({} UTF-8 bytes) required. Increase the budget; no lifecycle was changed by report delivery.",
                reply.len().div_ceil(4),
                reply.len()
            ),
            None,
        ));
    }
    snapshots::save_edit_report(data, project, &original, kind)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "edit-delivery-test";
    const SESSIONS: &str = "edit_session_v1";

    fn fixture() -> (
        tempfile::TempDir,
        crate::state::AppState,
        crate::tools::Engram,
    ) {
        let owned = tempfile::tempdir().unwrap();
        let root = owned.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("AGENTS.md"),
            "Preserve the manual approval exception for this source scope.\n".repeat(100),
        )
        .unwrap();
        let (state, _) = crate::state::AppState::new(engram_core::config::Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })
        .unwrap();
        state
            .registry
            .put_project(&engram_core::ProjectRecord {
                project_id: PROJECT.into(),
                project_name: PROJECT.into(),
                directory: root.to_string_lossy().into_owned(),
                project_type: "general".into(),
                created_at_ms: 0,
                updated_at_ms: 0,
                reindex_required_since_ms: None,
            })
            .unwrap();
        state
            .registry
            .set_meta(PROJECT, "active_generation", "1")
            .unwrap();
        let app = crate::tools::Engram::new(state.clone());
        (owned, state, app)
    }

    fn text(result: rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn opening(tokens: usize) -> crate::models::BeginEditSessionRequest {
        serde_json::from_value(
            json!({"project_id":PROJECT,"session_id":"worker","planned_files":["Site/Feature.vb"],
            "max_tokens":tokens,"output_json":true}),
        )
        .unwrap()
    }

    fn manifest(raw: &str, output_json: bool) -> Value {
        serde_json::from_str(if output_json {
            raw
        } else {
            raw.split("```json\n")
                .nth(1)
                .unwrap()
                .split("\n```")
                .next()
                .unwrap()
        })
        .unwrap()
    }

    #[test]
    fn completeness_pages_preserve_original_report_and_tool_project_identity() {
        let owned = tempfile::tempdir().unwrap();
        let body = format!(
            "# Advisory completeness\nINCOMPLETE coverage; candidates are not mandatory edits.\n{}\nPolicy hash: original-policy\n",
            "Preserve Å and \"quoted\" scope.\r\n".repeat(1200)
        );
        for output_json in [false, true] {
            let reply = finish(
                owned.path(),
                "project",
                EditReport::Completeness,
                body.clone(),
                json!({"requested_file_count":3}),
                output_json,
                2048,
            )
            .unwrap();
            assert!(reply.len() <= 2048);
            let packet = manifest(&reply, output_json);
            assert_eq!(packet["report_content"], "DEFERRED_NOT_SUPPLIED");
            let id = packet["response_delivery"]["response_id"].as_str().unwrap();
            assert!(
                recover(
                    owned.path(),
                    "other-project",
                    EditReport::Completeness,
                    Some(id),
                    0,
                    2048
                )
                .is_err()
            );
            assert!(
                recover(
                    owned.path(),
                    "project",
                    EditReport::Begin,
                    Some(id),
                    0,
                    2048
                )
                .is_err()
            );
            let mut next = packet["response_delivery"]["next_call"].clone();
            let mut original = String::new();
            loop {
                assert_eq!(next["tool"], "detect_incomplete_changes");
                let req: crate::models::DetectIncompleteChangesRequest =
                    serde_json::from_value(next["arguments"].clone()).unwrap();
                assert!(req.edited_files.is_empty());
                let page = recover(
                    owned.path(),
                    &req.project_id,
                    EditReport::Completeness,
                    req.response_id.as_deref(),
                    req.response_offset,
                    snapshots::budget(req.max_tokens).unwrap(),
                )
                .unwrap()
                .unwrap();
                assert!(page.len() <= 2048);
                let page: Value = serde_json::from_str(&page).unwrap();
                assert_eq!(page["start_byte"], original.len());
                original.push_str(page["content"].as_str().unwrap());
                if page["complete"] == true {
                    break;
                }
                next = page["next_call"].clone();
            }
            assert_eq!(blake3::hash(original.as_bytes()).to_hex().as_str(), id);
            if output_json {
                let value: Value = serde_json::from_str(&original).unwrap();
                assert_eq!(value["report_markdown"], body);
                assert_eq!(value["summary"]["requested_file_count"], 3);
            } else {
                assert_eq!(original, body);
            }
        }
    }

    #[test]
    fn small_legacy_report_is_exact_and_undeliverable_budget_does_not_write_snapshot() {
        let owned = tempfile::tempdir().unwrap();
        let body = "# Advisory report\nNo completeness certification.\n";
        assert_eq!(
            finish(
                owned.path(),
                "project",
                EditReport::Completeness,
                body.into(),
                json!({}),
                false,
                40960
            )
            .unwrap(),
            body
        );
        assert!(
            finish(
                owned.path(),
                "project",
                EditReport::Completeness,
                body.into(),
                json!({}),
                false,
                4
            )
            .is_err()
        );
        assert!(!owned.path().join("responses").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn opening_delivery_failures_leave_no_session_and_recovery_does_not_open_again() {
        let (_owned, state, app) = fixture();
        assert!(app.handle_begin_edit_session(opening(1)).await.is_err());
        assert!(
            state
                .registry
                .get_meta(PROJECT, SESSIONS)
                .unwrap()
                .is_none()
        );
        let blocked = state.cfg.data_dir.join("responses/edit-begin");
        std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        std::fs::write(&blocked, b"owned cache blocker").unwrap();
        assert!(app.handle_begin_edit_session(opening(1024)).await.is_err());
        assert!(
            state
                .registry
                .get_meta(PROJECT, SESSIONS)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&blocked).unwrap(), b"owned cache blocker");
        std::fs::remove_file(blocked).unwrap();
        let reply = text(app.handle_begin_edit_session(opening(1024)).await.unwrap());
        assert!(reply.len() <= 4096);
        let packet: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(packet["summary"]["session_id"], "worker");
        assert_eq!(packet["report_content"], "DEFERRED_NOT_SUPPLIED");
        let before = state.registry.get_meta(PROJECT, SESSIONS).unwrap();
        let mut next = packet["response_delivery"]["next_call"].clone();
        let mut original = String::new();
        loop {
            assert_eq!(next["tool"], "begin_edit_session");
            let req: crate::models::BeginEditSessionRequest =
                serde_json::from_value(next["arguments"].clone()).unwrap();
            assert!(req.planned_files.is_empty());
            let page = text(app.handle_begin_edit_session(req).await.unwrap());
            assert!(page.len() <= 4096);
            let page: Value = serde_json::from_str(&page).unwrap();
            original.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            next = page["next_call"].clone();
        }
        assert_eq!(state.registry.get_meta(PROJECT, SESSIONS).unwrap(), before);
        let full: Value = serde_json::from_str(&original).unwrap();
        assert_eq!(
            full["summary"]["session_revision"],
            packet["summary"]["session_revision"]
        );
        assert!(
            full["report_markdown"]
                .as_str()
                .unwrap()
                .contains("manual approval exception")
        );
        let request=serde_json::from_value(json!({"project_id":PROJECT,"session_id":"worker", "session_revision":packet["summary"]["session_revision"],"edited_files":[]})).unwrap();
        assert!(
            text(app.handle_complete_edit_session(request).await.unwrap())
                .contains("Edit session COMPLETE")
        );
    }
}
