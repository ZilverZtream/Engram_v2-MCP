//! Decision views retain dated rulings and complete evidence without repeated history.
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
const PID: &str = "historical-decision-recovery";
const CUTOFF: &str = "2026-02-01";
struct Fixture {
    e: Engram,
    _temp: tempfile::TempDir,
}
fn text(response: rmcp::model::CallToolResult) -> String {
    assert_ne!(response.is_error, Some(true));
    response.content[0].as_text().unwrap().text.clone()
}
async fn fixture() -> Fixture {
    let fixture_root = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let temp = tempfile::Builder::new()
        .prefix("")
        .rand_bytes(2)
        .tempdir_in(fixture_root)
        .unwrap();
    let root = temp.path().join("r");
    std::fs::create_dir(&root).unwrap();
    drop(git2::Repository::init(&root).unwrap());
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: "Historical decision recovery".into(),
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
    let e = Engram::new(state);
    let mut events = vec![];
    for (identity, kind, date, parent) in [
        ("open", "open", "2026-01-01T12:00:00Z", None),
        (
            "exception",
            "accepted_exception",
            "2026-01-10T12:00:00Z",
            Some("open"),
        ),
        (
            "future",
            "claimed_fix",
            "2026-02-10T12:00:00Z",
            Some("exception"),
        ),
        ("undated", "open", "unknown", None),
    ] {
        events.push(json!({"event_id":identity,"finding_id":if identity=="undated"{"undated-finding"}else{"preview-finding"},"supersedes":parent,"kind":kind,"source_url":"https://example.invalid/reviews/17","author":"Recorded reviewer","recorded_at":date,"rationale":"Imported declaration for controlled historical fixture.","verification":null,"scope":{"reviewed_commit":"1".repeat(40),"paths":["Site/preview.ts"],"conditions":"Only cached previews; preserve per-action authorization."}}));
    }
    text(
        e.handle_record_review_decisions(
            serde_json::from_value(
                json!({"project_id":PID,"review_id":"PR-17","decisions":events}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    Fixture { e, _temp: temp }
}
async fn read(f: &Fixture, cutoff: Option<&str>) -> Value {
    let mut args = json!({"project_id":PID,"review_id":"PR-17"});
    if let Some(cutoff) = cutoff {
        args["recorded_before"] = json!(cutoff);
    }
    serde_json::from_str(&text(
        f.e.handle_get_review_decisions(serde_json::from_value(args).unwrap())
            .await
            .unwrap(),
    ))
    .unwrap()
}

async fn current(f: &Fixture, cutoff: Option<&str>) -> Value {
    let mut args = json!({"project_id":PID,"review_id":"PR-17","view":"current"});
    if let Some(cutoff) = cutoff {
        args["recorded_before"] = json!(cutoff);
    }
    serde_json::from_str(&text(
        f.e.handle_get_review_decisions(serde_json::from_value(args).unwrap())
            .await
            .unwrap(),
    ))
    .unwrap()
}

#[tokio::test]
async fn current_view_keeps_all_latest_scope_and_provenance_with_dated_full_history_action() {
    let f = fixture().await;
    for cutoff in [None, Some(CUTOFF)] {
        let full = read(&f, cutoff).await;
        let projected = current(&f, cutoff).await;
        assert!(projected.get("events").is_none());
        assert_eq!(projected["view"], "current");
        for (key, value) in full.as_object().unwrap() {
            if key != "events" {
                assert_eq!(&projected[key], value, "{key}");
            }
        }
        assert_eq!(
            projected["event_history"]["selected_events"],
            full["events"].as_array().unwrap().len()
        );
        let hash = blake3::hash(full["events"].to_string().as_bytes())
            .to_hex()
            .to_string();
        assert_eq!(projected["event_history"]["selected_events_blake3"], hash);
        let action = &projected["event_history"]["next_call"];
        assert_eq!(action["tool"], "get_review_decisions");
        assert_eq!(action["arguments"]["view"], "history");
        assert_eq!(action["arguments"]["recorded_before"].as_str(), cutoff);
        let recovered: Value = serde_json::from_str(&text(
            f.e.handle_get_review_decisions(
                serde_json::from_value(action["arguments"].clone()).unwrap(),
            )
            .await
            .unwrap(),
        ))
        .unwrap();
        assert_eq!(recovered, full);
        assert!(
            projected["event_history"]["freshness"]
                .as_str()
                .unwrap()
                .contains("fresh query")
        );
    }
    let error =
        f.e.handle_get_review_decisions(
            serde_json::from_value(
                json!({"project_id":PID,"review_id":"PR-17","view":"current","discussion":{}}),
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("view=current") && error.message.contains("discussion"));
    assert!(
        f.e.state.projects.is_empty(),
        "Decision views must not open source indexes"
    );
}

#[tokio::test]
async fn current_view_avoids_large_superseded_history_without_changing_complete_latest_evidence() {
    let f = fixture().await;
    let rationale =
        "Imported source declaration; preserve the explicit authorization exception. ".repeat(40);
    let events = (0..30).map(|i| json!({"event_id":format!("long-{i}"),"finding_id":"long-finding","supersedes":if i==0{None}else{Some(format!("long-{}",i-1))},"kind":if i==29{"accepted_exception"}else{"open"},"source_url":"https://example.invalid/reviews/17","author":"Recorded reviewer","recorded_at":"2026-01-15T12:00:00Z","rationale":rationale,"verification":null,"scope":{"reviewed_commit":"1".repeat(40),"paths":["Site/preview.ts"],"conditions":"Only cached previews; per-action authorization still applies."}})).collect::<Vec<_>>();
    text(
        f.e.handle_record_review_decisions(
            serde_json::from_value(
                json!({"project_id":PID,"review_id":"PR-17","decisions":events}),
            )
            .unwrap(),
        )
        .await
        .unwrap(),
    );
    let full = read(&f, None).await;
    let projected = current(&f, None).await;
    assert_eq!(projected["current"], full["current"]);
    assert_eq!(projected["automatic_finding_suppression"], false);
    assert!(projected.to_string().len() * 4 < full.to_string().len());
    assert_eq!(projected["event_history"]["selected_events"], 34);
    let latest = projected["current"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["finding_id"] == "long-finding")
        .unwrap();
    assert_eq!(latest["evidence"]["rationale"], rationale);
    assert_eq!(
        latest["evidence"]["scope"]["conditions"],
        "Only cached previews; per-action authorization still applies."
    );
    assert_eq!(full, read(&f, None).await);
}
#[tokio::test]
async fn public_dated_history_filters_before_reconstructing_current_without_mutating_raw_history() {
    let f = fixture().await;
    let current = read(&f, None).await;
    assert_eq!(current["events"].as_array().unwrap().len(), 4);
    let dated = read(&f, Some(CUTOFF)).await;
    assert_eq!(
        dated["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["event_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["open", "exception"]
    );
    assert_eq!(dated["current"].as_array().unwrap().len(), 1);
    assert_eq!(dated["current"][0]["event_id"], "exception");
    assert_eq!(
        dated["current"][0]["effective_status"],
        "accepted_exception"
    );
    assert_eq!(dated["excluded_future_or_unparseable_events"], 2);
    assert_eq!(dated["automatic_finding_suppression"], false);
    assert!(
        dated["verification_origin"]
            .as_str()
            .unwrap()
            .contains("did not authenticate")
    );
    assert_eq!(
        dated["current"][0]["evidence"]["scope"]["conditions"],
        "Only cached previews; preserve per-action authorization."
    );
    let boundary = read(&f, Some("2026-01-10")).await;
    assert_eq!(boundary["current"][0]["event_id"], "open");
    let empty = read(&f, Some("2025-12-01")).await;
    assert!(empty["events"].as_array().unwrap().is_empty());
    assert_eq!(current, read(&f, None).await);
    assert!(
        f.e.state.projects.is_empty(),
        "Exact-ID event recovery needs no source index"
    );
}
#[tokio::test]
async fn malformed_dates_and_discussion_combinations_refuse_without_altering_events() {
    let f = fixture().await;
    let original = read(&f, None).await;
    for extra in [
        json!({"recorded_before":"2026-02-30"}),
        json!({"recorded_before":CUTOFF,"discussion":{}}),
    ] {
        let mut args = json!({"project_id":PID,"review_id":"PR-17"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            f.e.handle_get_review_decisions(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
    }
    assert_eq!(original, read(&f, None).await);
}
#[tokio::test]
async fn merged_work_history_action_recovers_the_same_exclusive_decision_date_scope() {
    let f = fixture().await;
    let runtime = engram_server::services::project_service::ensure_project_runtime(&f.e.state, PID)
        .await
        .unwrap();
    let content="# PR-17: Preview authorization behavior\nmerged: 2026-01-05T12:00:00Z | by: Fixture | kinds: backend\n\nPreview authorization history.\n\n## Files shipped together in this change\n- Site/preview.ts\n".to_string();
    let doc = engram_index::IndexDoc {
        generation: 0,
        chunk_id: 0,
        path: RelPath::new("pr:17"),
        language: "markdown".into(),
        content_hash: ContentHash::compute(content.as_bytes()).0,
        content,
        namespace: "history".into(),
        author: None,
        timestamp: Some(1_767_614_400),
        start_line: 0,
        end_line: 0,
        doc_id: "pr:17".into(),
    };
    runtime
        .search
        .index_docs(PID, &[doc], &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let result=text(f.e.handle_find_merged_work(serde_json::from_value(json!({"project_id":PID,"story":"preview authorization","top":1,"merged_before":CUTOFF})).unwrap()).await.unwrap());
    let action: Value = serde_json::from_str(
        result
            .lines()
            .find_map(|line| line.strip_prefix("decision_history: "))
            .expect("Executable decision history action"),
    )
    .unwrap();
    assert_eq!(action["tool"], "get_review_decisions");
    assert_eq!(action["arguments"]["recorded_before"], CUTOFF);
    let recovered: Value = serde_json::from_str(&text(
        f.e.handle_get_review_decisions(
            serde_json::from_value(action["arguments"].clone()).unwrap(),
        )
        .await
        .unwrap(),
    ))
    .unwrap();
    assert_eq!(recovered, read(&f, Some(CUTOFF)).await);
    assert!(!recovered["events"].to_string().contains("future"));
}
