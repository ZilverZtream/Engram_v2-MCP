//! Agents navigate to complete advisory rulings from a file, not a known review ID.
use anyhow::{Result, ensure};
use engram_core::{Config, ProjectRecord};
use engram_server::{AppState, Engram};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

const PROJECT: &str = "review-path-discovery";
const PATH: &str = "Site/Preview.aspx.vb";
const CONDITIONS: &str = "Only cached previews are covered. Exception: any write or export still requires per-action authorization.";
struct Fixture {
    e: Engram,
    owned: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("rd-").tempdir_in(parent)?;
        let root = owned.path().join("r");
        std::fs::create_dir(&root)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: PROJECT.into(),
            project_name: "Owned review evidence".into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })?;
        Ok(Self {
            e: Engram::new(state),
            owned,
        })
    }
    async fn record(&self, review: &str, events: Vec<Value>) -> Result<()> {
        let response = self
            .e
            .handle_record_review_decisions(serde_json::from_value(
                json!({"project_id":PROJECT,"review_id":review,"decisions":events}),
            )?)
            .await?;
        ensure!(response.is_error != Some(true));
        Ok(())
    }
    async fn discover(&self, path: &str) -> Result<Value> {
        let response = self
            .e
            .handle_get_review_decisions(serde_json::from_value(
                json!({"project_id":PROJECT,"discover_for_paths":[path]}),
            )?)
            .await?;
        parse(response)
    }
    fn close(self) -> Result<()> {
        let Self { e, owned } = self;
        drop(e);
        owned.close()?;
        Ok(())
    }
}
fn parse(response: CallToolResult) -> Result<Value> {
    ensure!(response.is_error != Some(true));
    Ok(serde_json::from_str(
        &response.content[0].as_text().unwrap().text,
    )?)
}
fn event(id: &str, finding: &str, previous: Option<&str>, path: Option<&str>) -> Value {
    json!({"event_id":id,"finding_id":finding,"supersedes":previous,
        "kind":if previous.is_some(){"accepted_exception"}else{"open"},
        "source_url":"https://example.invalid/reviews/source","author":"Imported reviewer",
        "recorded_at":"2026-01-02T12:00:00Z","rationale":format!("Review evidence for {PATH}; preserve the complete qualification."),"verification":null,
        "scope":path.map(|path|json!({"reviewed_commit":"1".repeat(40),"paths":[path],"conditions":CONDITIONS}))})
}

#[tokio::test]
async fn path_discovery_retains_latest_conditions_legacy_limits_and_callable_exact_history()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .record(
            "PR-1",
            vec![
                event("open", "preview", None, Some(PATH)),
                event("exception", "preview", Some("open"), Some(PATH)),
                event("legacy", "legacy", None, None),
            ],
        )
        .await?;
    fixture
        .record(
            "PR-2",
            vec![event(
                "unrelated",
                "unrelated",
                None,
                Some("Site/Other.aspx.vb"),
            )],
        )
        .await?;
    let report = fixture.discover(PATH).await?;
    ensure!(
        report["candidates"].as_array().unwrap().len() == 2,
        "{report}"
    );
    let selected = report["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["event"]["event_id"] == "exception")
        .unwrap();
    ensure!(selected["event"]["scope"]["conditions"] == CONDITIONS);
    ensure!(selected["relevance_basis"] == "declared_path_intersection");
    ensure!(
        report["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["relevance_basis"] == "legacy_complete_path_mention")
    );
    ensure!(
        report["coverage"]["status"] == "partial"
            && report["automatic_finding_suppression"] == false
    );
    ensure!(report["current_head_fix_verification_performed"] == false);
    let history = fixture
        .e
        .handle_get_review_decisions(serde_json::from_value(
            selected["event_history"]["arguments"].clone(),
        )?)
        .await?;
    let history = parse(history)?;
    ensure!(history["events"].as_array().unwrap().len() == 3);
    ensure!(
        history["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row == &selected["event"])
    );
    ensure!(
        fixture.discover("Site/Other.aspx.vb").await?["candidates"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    fixture.close()
}

#[tokio::test]
async fn superseded_scope_is_not_resurrected_and_external_fix_is_not_current_head_verification()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .record(
            "PR-moved",
            vec![
                event("old", "moved", None, Some(PATH)),
                event("new", "moved", Some("old"), Some("Site/Other.aspx.vb")),
            ],
        )
        .await?;
    ensure!(
        fixture.discover(PATH).await?["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut verified = event("attested", "fixed", None, Some(PATH));
    verified["kind"] = json!("verified_fix");
    verified["verification"] = json!({"commit":"2".repeat(40),"check":"Externally claimed fixture check","evidence_url":"https://example.invalid/artifact","artifact_sha256":"3".repeat(64),"verifier":"Imported verifier"});
    fixture.record("PR-fix", vec![verified]).await?;
    let report = fixture.discover(PATH).await?;
    ensure!(report["candidates"][0]["effective_status"] == "externally_attested_fix_not_checked");
    ensure!(report["current_head_fix_verification_performed"] == false);
    fixture.close()
}

#[tokio::test]
async fn discovery_rejects_invalid_cutoffs_mixed_selectors_and_non_exact_paths() -> Result<()> {
    let fixture = Fixture::new()?;
    for extra in [
        json!({"review_id":"PR-1"}),
        json!({"recorded_before":"not-a-date"}),
        json!({"discussion":{}}),
        json!({"view":"current"}),
        json!({"discover_for_paths":["Site/**"]}),
    ] {
        let mut request = json!({"project_id":PROJECT,"discover_for_paths":[PATH]});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        ensure!(
            fixture
                .e
                .handle_get_review_decisions(serde_json::from_value(request)?)
                .await
                .is_err()
        );
    }
    fixture.close()
}
