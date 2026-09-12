#![allow(clippy::unwrap_used)]

use engram_core::{ProjectRecord, config::Config};
use engram_server::{
    models::{BeginEditSessionRequest, CompleteEditSessionRequest},
    state::AppState,
    tools::Engram,
};
use serde_json::{Value, json};

const PID: &str = "session-identity-test";
const KEY: &str = "edit_session_v1";

fn fixture() -> (tempfile::TempDir, AppState, Engram) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (state, _rx) = AppState::new(Config {
        data_dir: data,
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    let engram = Engram::new(state.clone());
    (tmp, state, engram)
}

fn begin(id: Option<&str>, path: &str) -> BeginEditSessionRequest {
    serde_json::from_value(json!({"project_id":PID,"session_id":id,"planned_files":[path]}))
        .unwrap()
}

fn complete(id: Option<&str>, path: &str) -> CompleteEditSessionRequest {
    serde_json::from_value(json!({"project_id":PID,"session_id":id,"edited_files":[path]})).unwrap()
}

fn stored(state: &AppState) -> Value {
    serde_json::from_str(&state.registry.get_meta(PID, KEY).unwrap().unwrap()).unwrap()
}

fn current_completion(state: &AppState, id: &str, path: &str) -> CompleteEditSessionRequest {
    let mut request = complete(Some(id), path);
    request.session_revision = stored(state)["sessions"][id]["revision"]
        .as_str()
        .map(str::to_string);
    request
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_legacy_and_current_aliases_keep_file_ownership() {
    let (_tmp, state, engram) = fixture();
    for alias in ["src\\a.vb", "./src//a.vb", " src/./a.vb "] {
        for envelope in [false, true] {
            let session = json!({"planned_files":[alias],"started_ms":1});
            let payload = if envelope {
                json!({"version":2,"sessions":{"old-worker":session}})
            } else {
                session
            };
            let raw = payload.to_string();
            state.registry.set_meta(PID, KEY, &raw).unwrap();
            let error = engram
                .handle_begin_edit_session(begin(Some("new-worker"), "src/a.vb"))
                .await
                .unwrap_err();
            assert!(error.message.contains("overlap"), "{error}");
            assert_eq!(
                state.registry.get_meta(PID, KEY).unwrap().as_deref(),
                Some(raw.as_str())
            );
            let closed = engram
                .handle_complete_edit_session(complete(None, "src/a.vb"))
                .await
                .unwrap();
            assert!(
                !closed.content[0]
                    .as_text()
                    .unwrap()
                    .text
                    .contains("Planned but NOT edited")
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_stored_scope_is_not_silently_filtered_or_consumed() {
    let (_tmp, state, engram) = fixture();
    for planned in [
        json!(["src/a.vb", 123]),
        json!(["../a.vb"]),
        json!([]),
        json!(null),
    ] {
        let raw =
            json!({"version":2,"sessions":{"old-worker":{"planned_files":planned}}}).to_string();
        state.registry.set_meta(PID, KEY, &raw).unwrap();
        assert!(
            engram
                .handle_begin_edit_session(begin(Some("new-worker"), "other.vb"))
                .await
                .is_err()
        );
        assert!(
            engram
                .handle_complete_edit_session(complete(None, "src/a.vb"))
                .await
                .is_err()
        );
        assert_eq!(
            state.registry.get_meta(PID, KEY).unwrap().as_deref(),
            Some(raw.as_str())
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disjoint_workers_keep_identity_and_legacy_completion_rejects_ambiguity() {
    let (_tmp, state, engram) = fixture();
    let (a, b) = tokio::join!(
        engram.handle_begin_edit_session(begin(Some("a"), "src/a.vb")),
        engram.handle_begin_edit_session(begin(Some("b"), "src/b.vb"))
    );
    let a = a.unwrap();
    b.unwrap();
    let text = &a.content[0].as_text().unwrap().text;
    assert!(
        text.contains("session_id: a") && text.contains("index_generation"),
        "{text}"
    );
    assert_eq!(stored(&state)["sessions"].as_object().unwrap().len(), 2);
    assert!(
        engram
            .handle_complete_edit_session(complete(None, "src/a.vb"))
            .await
            .is_err()
    );
    assert!(
        engram
            .handle_begin_edit_session(begin(Some("c"), "SRC\\a.vb"))
            .await
            .is_err()
    );
    assert!(
        engram
            .handle_begin_edit_session(begin(Some("a"), "different.vb"))
            .await
            .is_err()
    );
    assert!(
        engram
            .handle_complete_edit_session(complete(Some("wrong"), "src/a.vb"))
            .await
            .is_err()
    );
    // Invalid completion must leave both sessions available for retry.
    assert!(
        engram
            .handle_complete_edit_session(current_completion(&state, "a", " "))
            .await
            .is_err()
    );
    assert_eq!(stored(&state)["sessions"].as_object().unwrap().len(), 2);
    let out = engram
        .handle_complete_edit_session(current_completion(&state, "a", "src\\a.vb"))
        .await
        .unwrap();
    assert!(
        !out.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("Planned but NOT edited")
    );
    assert!(stored(&state)["sessions"].get("b").is_some());
    assert!(stored(&state)["sessions"].get("a").is_none());
    // Once unambiguous, the legacy no-ID completion remains supported.
    engram
        .handle_complete_edit_session(complete(None, "src/b.vb"))
        .await
        .unwrap();
    assert_eq!(stored(&state)["sessions"].as_object().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_claims_and_completions_have_exactly_one_winner() {
    let (_tmp, state, engram) = fixture();
    let (a, b) = tokio::join!(
        engram.handle_begin_edit_session(begin(Some("same"), "a.vb")),
        engram.handle_begin_edit_session(begin(Some("same"), "b.vb"))
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(stored(&state)["sessions"].as_object().unwrap().len(), 1);
    let request = current_completion(&state, "same", "a.vb");
    let (a, b) = tokio::join!(
        engram.handle_complete_edit_session(request.clone()),
        engram.handle_complete_edit_session(request)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(stored(&state)["sessions"].as_object().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_stored_session_is_retained_when_new_worker_starts() {
    let (_tmp, state, engram) = fixture();
    state
        .registry
        .set_meta(
            PID,
            KEY,
            &json!({"planned_files":["old.vb"],"story":"previous server","started_ms":1})
                .to_string(),
        )
        .unwrap();
    engram
        .handle_begin_edit_session(begin(Some("new"), "new.vb"))
        .await
        .unwrap();
    assert!(stored(&state)["sessions"].get("legacy").is_some());
    assert!(
        engram
            .handle_complete_edit_session(complete(None, "old.vb"))
            .await
            .is_err()
    );
    engram
        .handle_complete_edit_session(current_completion(&state, "new", "new.vb"))
        .await
        .unwrap();
    engram
        .handle_complete_edit_session(complete(None, "old.vb"))
        .await
        .unwrap();
    let opened = engram
        .handle_begin_edit_session(begin(None, "next.vb"))
        .await
        .unwrap();
    assert!(
        opened.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("session_id:")
    );
    engram
        .handle_complete_edit_session(complete(None, "next.vb"))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reused_worker_identity_rejects_stale_or_missing_lifecycle_token() {
    let (_tmp, state, engram) = fixture();
    engram
        .handle_begin_edit_session(begin(Some("worker"), "old.vb"))
        .await
        .unwrap();
    let stale = current_completion(&state, "worker", "old.vb");
    engram
        .handle_complete_edit_session(stale.clone())
        .await
        .unwrap();
    engram
        .handle_begin_edit_session(begin(Some("worker"), "new.vb"))
        .await
        .unwrap();
    let current = current_completion(&state, "worker", "new.vb");
    assert_ne!(stale.session_revision, current.session_revision);
    assert!(engram.handle_complete_edit_session(stale).await.is_err());
    assert!(
        engram
            .handle_complete_edit_session(complete(Some("worker"), "new.vb"))
            .await
            .is_err()
    );
    assert_eq!(stored(&state)["sessions"].as_object().unwrap().len(), 1);
    engram.handle_complete_edit_session(current).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normalized_relative_paths_are_used_for_ownership_and_index_evidence() {
    let (_tmp, state, engram) = fixture();
    state
        .graph
        .upsert_nodes(
            PID,
            &[engram_graph::Node {
                node_id: "file:src/a.vb".into(),
                node_type: "file".into(),
                name: "a.vb".into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: engram_core::RelPath::new("src/a.vb"),
                start_line: 1,
                end_line: 1,
                generation: 1,
                metadata: None,
            }],
        )
        .unwrap();
    let opened = engram
        .handle_begin_edit_session(begin(Some("worker"), " ./src//a.vb "))
        .await
        .unwrap();
    let text = &opened.content[0].as_text().unwrap().text;
    assert!(!text.contains("NOT found in the index"), "{text}");
    assert_eq!(
        stored(&state)["sessions"]["worker"]["planned_files"],
        json!(["src/a.vb"])
    );
    assert!(
        engram
            .handle_begin_edit_session(begin(Some("other"), "src\\.\\a.vb"))
            .await
            .is_err()
    );
    let closed = engram
        .handle_complete_edit_session(current_completion(&state, "worker", " .\\src\\\\a.vb "))
        .await
        .unwrap();
    let text = &closed.content[0].as_text().unwrap().text;
    assert!(!text.contains("NOT found in the index"), "{text}");
    assert!(!text.contains("Planned but NOT edited"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absolute_drive_unc_and_traversal_paths_never_claim_or_consume() {
    let (_tmp, state, engram) = fixture();
    engram
        .handle_begin_edit_session(begin(Some("worker"), "src/a.vb"))
        .await
        .unwrap();
    let before = stored(&state);
    for path in [
        "C:\\project\\src\\a.vb",
        "C:src/a.vb",
        "/src/a.vb",
        "\\src\\a.vb",
        "\\\\server\\share\\a.vb",
        "../a.vb",
        "src/../a.vb",
        " ",
    ] {
        assert!(
            engram
                .handle_begin_edit_session(begin(Some("other"), path))
                .await
                .is_err(),
            "{path}"
        );
        assert!(
            engram
                .handle_complete_edit_session(current_completion(&state, "worker", path))
                .await
                .is_err(),
            "{path}"
        );
        assert_eq!(stored(&state), before, "{path}");
    }
    engram
        .handle_complete_edit_session(current_completion(&state, "worker", "src/a.vb"))
        .await
        .unwrap();
}
