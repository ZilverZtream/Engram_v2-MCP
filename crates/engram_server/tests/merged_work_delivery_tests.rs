//! Actual merged-history reports retain their complete selected PR cards,
//! source actions and qualifications within caller/default delivery budgets.
use anyhow::{Result, ensure};
use engram_core::{Config, ContentHash, DocIdStr, ProjectRecord};
use engram_index::IndexDoc;
use engram_server::{AppState, Engram, read_report_delivery as delivery, tool_surface};
use rmcp::model::{CallToolRequestParam, CallToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const PID: &str = "merged-delivery-owned";
fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn prepared(args: Value) -> Result<delivery::Report> {
    let mut req: CallToolRequestParam =
        serde_json::from_value(json!({"name":"find_merged_work","arguments":args}))?;
    Ok(delivery::prepare(&mut req, &Engram::tool_registry().list_all())?.unwrap())
}

struct Fixture {
    state: AppState,
    // All registry, graph and index owners above close before this directory.
    _owned: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let owned = tempfile::tempdir()?;
        let root = owned.path().join("project");
        std::fs::create_dir(&root)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            ..Default::default()
        })?;
        state.registry.put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: "History delivery".into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })?;
        state.registry.set_meta(PID, "active_generation", "1")?;
        engram_server::services::project_service::ensure_project_runtime(&state, PID).await?;
        Ok(Self {
            state,
            _owned: owned,
        })
    }
    async fn seed(&self, count: usize) -> Result<()> {
        let docs:Vec<_> = (1..=count).map(|id| {
            let path = format!("pr:PR-{id}");
            let mut content = format!("# PR-{id}: Reconcile imported invoice rows\nmerged: 2026-01-01 | author: fixture | kinds: backend\n\nRetain the original exclusions and review conditions.\n\n## Files shipped together in this change\n");
            for file in 0..40 {
                content.push_str(&format!("- Site/modules/dashboard/pages/operations/reconciliation-of-archived-imported-invoice-rows/shared-accounting-validation-and-source-ownership/implementation-{id:02}-{file:02}.vb\n"));
            }
            let hash = ContentHash::compute(content.as_bytes());
            IndexDoc {generation:0,chunk_id:engram_index::chunk_id_from_content_hash(&hash),
                doc_id:DocIdStr::compute(&path,0,0,&hash).0,content_hash:hash.0,path:path.into(),
                language:"markdown".into(),content,namespace:"history".into(),author:None,timestamp:Some(1767225600),start_line:0,end_line:0}
        }).collect();
        self.state
            .get_project_cached(PID)
            .unwrap()
            .search
            .index_docs(PID, &docs, &CancellationToken::new())
            .await?;
        Ok(())
    }
    async fn original(&self, top: usize) -> Result<CallToolResult> {
        Ok(Engram::new(self.state.clone())
            .handle_find_merged_work(serde_json::from_value(json!({
                "project_id":PID,"story":"reconcile imported invoice rows","top":top
            }))?)
            .await?)
    }
}

#[test]
fn actual_tool_schema_exposes_budget_and_recovery_without_requiring_story_on_recovery() -> Result<()>
{
    let registry = Engram::tool_registry().list_all();
    let original = registry
        .iter()
        .find(|tool| tool.name == "find_merged_work")
        .unwrap();
    ensure!(delivery::eligible(original) == Some("find_merged_work"));
    let full = tool_surface::advertised(registry.clone(), true);
    let schema = &full
        .iter()
        .find(|tool| tool.name == "find_merged_work")
        .unwrap()
        .input_schema;
    ensure!(schema["required"] == json!(["project_id"]));
    for key in ["max_tokens", "response_id", "response_offset"] {
        ensure!(schema["properties"].get(key).is_some());
    }
    for key in ["story", "file_paths", "merged_before", "as_of_rev"] {
        ensure!(schema["properties"].get(key).is_some());
    }
    ensure!(prepared(json!({"project_id":PID,"story":"task","max_tokens":1})).is_err());
    Ok(())
}

#[tokio::test]
async fn complete_default_and_caller_budget_reports_recover_after_history_disappears() -> Result<()>
{
    let fixture = Fixture::new().await?;
    fixture.seed(10).await?;
    let original = fixture.original(10).await?;
    ensure!(
        text(&original).len() > 40960,
        "Fixture must exercise the default whole-response limit"
    );
    for id in 1..=10 {
        ensure!(text(&original).contains(&format!("# PR-{id}:")));
    }
    ensure!(
        text(&original).contains("review approvals were not fetched")
            && text(&original).contains("full_document: get_chunk")
    );
    let expected = serde_json::to_string(&original)?;
    let mut pending = Vec::new();
    for tokens in [None, Some(2048usize)] {
        let limit = tokens.map_or(40960, |tokens| tokens * 4);
        let report = prepared(
            json!({"project_id":PID,"story":"reconcile imported invoice rows","top":10,"max_tokens":tokens}),
        )?;
        let first = report.finish(&fixture.state.cfg.data_dir, original.clone())?;
        ensure!(text(&first).len() <= limit);
        let manifest: Value = serde_json::from_str(text(&first))?;
        ensure!(manifest["report_content"] == "DEFERRED_NOT_SUPPLIED");
        pending.push((limit, manifest));
    }
    // Recovering an old report must not perform the original search again.
    fixture
        .state
        .get_project_cached(PID)
        .unwrap()
        .search
        .delete_namespace(PID, "history")
        .await?;
    ensure!(text(&fixture.original(10).await?).contains("no merged work matched"));
    for (limit, manifest) in pending {
        let mut next = manifest["response_delivery"]["next_call"].clone();
        let mut restored = String::new();
        loop {
            ensure!(next["tool"] == "find_merged_work");
            ensure!(next["arguments"].get("story").is_none());
            let page = prepared(next["arguments"].clone())?
                .recover(&fixture.state.cfg.data_dir)?
                .unwrap();
            ensure!(text(&page).len() <= limit);
            let page: Value = serde_json::from_str(text(&page))?;
            ensure!(page["start_byte"].as_u64() == Some(restored.len() as u64));
            restored.push_str(page["content"].as_str().unwrap());
            if page["complete"] == true {
                break;
            }
            next = page["next_call"].clone();
        }
        ensure!(restored == expected);
        ensure!(
            manifest["response_delivery"]["response_id"]
                == blake3::hash(restored.as_bytes()).to_hex().to_string()
        );
    }
    Ok(())
}

#[tokio::test]
async fn small_selected_history_report_preserves_its_original_text_and_status() -> Result<()> {
    let fixture = Fixture::new().await?;
    let original = fixture.original(1).await?;
    let report = prepared(json!({"project_id":PID,"story":"missing example"}))?;
    let bounded = report.finish(&fixture.state.cfg.data_dir, original.clone())?;
    ensure!(serde_json::to_value(bounded)? == serde_json::to_value(original)?);
    Ok(())
}
