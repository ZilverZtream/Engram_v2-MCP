//! Prior review evidence informs pre-push callers without suppressing rule outcomes.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{AppState, Engram};
use futures::FutureExt;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
};

const FILE: &str = "Site/Preview.vb";
const OTHER: &str = "Site/Other.vb";
const HINT: &str = "Site/Hint.vb";
const BEFORE: &str = "Public Class PreviewService\nPublic Function RenderInvoicePreview() As String\nReturn \"old preview\"\nEnd Function\nEnd Class\n";
const AFTER: &str = "Public Class PreviewService\nPublic Function RenderInvoicePreview() As String\nReturn \"new preview\"\nEnd Function\nEnd Class\n";
const DIFF: &str = "diff --git a/Site/Preview.vb b/Site/Preview.vb\n--- a/Site/Preview.vb\n+++ b/Site/Preview.vb\n@@ -1,5 +1,5 @@\n Public Class PreviewService\n Public Function RenderInvoicePreview() As String\n-Return \"old preview\"\n+Return \"new preview\"\n End Function\n End Class\n";
const MARKER: &str = "\n\n## Prior review evidence (advisory)\n";
const GUIDE: &str = "Preserve read-only preview and publishing safeguards.\n";

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}
fn context(result: &CallToolResult) -> Result<(&str, Value)> {
    let (prefix, suffix) = text(result)
        .split_once(MARKER)
        .ok_or_else(|| anyhow::anyhow!("missing prior context"))?;
    Ok((prefix, serde_json::from_str(suffix)?))
}

struct Fixture {
    server: Engram,
    owned: tempfile::TempDir,
    root: PathBuf,
    pid: String,
    head: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let parent = std::env::var_os("ENGRAM_TEST_FIXTURE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let owned = tempfile::Builder::new().prefix("pp-").tempdir_in(parent)?;
        let root = owned.path().join("root");
        std::fs::create_dir_all(root.join("Site"))?;
        for (path, source) in [
            (FILE, BEFORE),
            (OTHER, "Public Class OtherService\nEnd Class\n"),
            (HINT, "Public Class HintService\nEnd Class\n"),
            ("AGENTS.md", GUIDE),
        ] {
            std::fs::write(root.join(path), source)?;
        }
        let head = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            for path in [FILE, OTHER, HINT, "AGENTS.md"] {
                index.add_path(Path::new(path))?;
            }
            index.write()?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Owned preview baseline",
                &tree,
                &[],
            )?
            .to_string()
        };
        std::fs::write(root.join(FILE), AFTER)?;
        let (state, _) = AppState::new(Config {
            data_dir: owned.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            watch_debounce_secs: 3600,
            ..Default::default()
        })?;
        let server = Engram::new(state);
        let indexed = server.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"Owned pre-push decisions","project_type":"general","wait":true}))?).await?;
        let pid = text(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            server,
            owned,
            root,
            pid,
            head,
        })
    }
    async fn record(&self, review: &str, path: &str) -> Result<()> {
        self.server.handle_record_review_decisions(serde_json::from_value(json!({"project_id":self.pid,"review_id":review,"decisions":[{
            "event_id":"preview-exception","finding_id":"thread:1","supersedes":null,
            "kind":"accepted_exception","source_url":"https://example.invalid/review/4100",
            "author":"Fixture owner","recorded_at":"2026-01-01T13:00:00Z",
            "rationale":"Accepted only for the original read-only preview; reassess changed source.","verification":null,
            "scope":{"reviewed_commit":self.head,"paths":[path],"conditions":"Only read-only previews; publishing still requires safeguards and owner review."}
        }]}))?).await?;
        Ok(())
    }
    async fn audit(&self, arguments: Value) -> Result<CallToolResult> {
        let mut arguments = arguments;
        arguments["project_id"] = json!(self.pid);
        Ok(self
            .server
            .handle_pre_push_audit(serde_json::from_value(arguments)?)
            .await?)
    }
    async fn rules(&self) -> Result<()> {
        std::fs::write(
            self.root.join("preview-rules.txt"),
            "RenderInvoicePreview must never publish an invoice; preserve read-only PreviewService guards.\n",
        )?;
        self.server.handle_ingest_quality_gates(serde_json::from_value(json!({"project_id":self.pid,"source_path":"preview-rules.txt","source_type":"text"}))?).await?;
        Ok(())
    }
    fn close(self) -> Result<()> {
        drop(self.server);
        self.owned.close()?;
        Ok(())
    }
}
fn finish(fixture: Fixture, result: std::thread::Result<Result<()>>) -> Result<()> {
    fixture.close()?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inactive_rule_outcome_retains_relevant_decision_without_approval_or_suppression()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let result = AssertUnwindSafe(async {
        let before = fixture.audit(json!({"code":DIFF,"file_path":FILE})).await?;
        fixture.record("PR-4100", FILE).await?;
        fixture.record("PR-4101", OTHER).await?;
        let after = fixture.audit(json!({"code":DIFF,"file_path":FILE})).await?;
        let (prefix, supplied) = context(&after)?;
        ensure!(prefix == context(&before)?.0);
        ensure!(
            prefix.contains("INACTIVE - 0 quality-gate rules")
                && prefix.contains("NOTHING was checked")
        );
        let prior = &supplied["prior_review_context"];
        ensure!(
            prior["items"].as_array().unwrap().len() == 1
                && prior["items"][0]["review_id"] == "PR-4100"
        );
        ensure!(prior["automatic_finding_suppression"] == false);
        let event = &prior["items"][0]["provider_neutral_events"][0];
        ensure!(
            event["evidence"]["kind"] == "accepted_exception"
                && event["provider_identity_authenticated"] == false
        );
        ensure!(
            event["declared_scope_assessment"]["status"] == "submitted_scope_requires_reassessment"
        );
        ensure!(
            event["declared_scope_assessment"]["affected_files"][0]["source_comparison"]
                == "changed_bytes"
        );
        ensure!(
            event["evidence"]["scope"]["conditions"]
                .as_str()
                .unwrap()
                .contains("owner review")
        );
        ensure!(
            supplied["prior_context_input_binding"]["input_blake3"]
                == blake3::hash(DIFF.as_bytes()).to_hex().to_string()
        );
        ensure!(supplied["prior_context_input_binding"]["patch_application_verified"] == false);
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    finish(fixture, result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hash_bound_diff_paths_override_hint_and_preserve_retrieved_rule_candidates() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let result = AssertUnwindSafe(async {
        fixture.rules().await?;
        let diff = format!("{DIFF}diff --git a/Site/Other.vb b/Site/Other.vb\n--- a/Site/Other.vb\n+++ b/Site/Other.vb\n@@ -1,2 +1,2 @@\n-Public Class OtherService\n+Public Class OtherServiceRenamed\n End Class\n");
        std::fs::write(fixture.root.join("change.patch"), &diff)?;
        let args = json!({"diff_file":"change.patch","diff_file_blake3":blake3::hash(diff.as_bytes()).to_hex().to_string(),"file_path":HINT});
        let before = fixture.audit(args.clone()).await?;
        for (review, path) in [("PR-4100", FILE), ("PR-4101", OTHER), ("PR-4102", HINT)] { fixture.record(review, path).await?; }
        let after = fixture.audit(args).await?;
        let (prefix, supplied) = context(&after)?;
        ensure!(prefix == context(&before)?.0 && prefix.contains("Retrieved (NOT verified against your code)"));
        let prior = &supplied["prior_review_context"];
        let reviews = prior["items"].as_array().unwrap().iter().map(|row| row["review_id"].as_str().unwrap()).collect::<std::collections::BTreeSet<_>>();
        ensure!(reviews == std::collections::BTreeSet::from(["PR-4100", "PR-4101"]));
        ensure!(prior["automatic_finding_suppression"] == false);
        ensure!(supplied["prior_context_input_binding"]["scope_origin"] == "parsed_unified_diff_paths");
        ensure!(supplied["prior_context_input_binding"]["input_blake3"] == blake3::hash(diff.as_bytes()).to_hex().to_string());
        Ok::<(), anyhow::Error>(())
    }).catch_unwind().await;
    finish(fixture, result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_source_requires_explicit_path_and_maintenance_mode_keeps_its_original_contract()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let result = AssertUnwindSafe(async {
        fixture.record("PR-4100", FILE).await?;
        let no_scope = fixture.audit(json!({"code":"RenderInvoicePreview() ' Site/Preview.vb"})).await?;
        let (_, supplied) = context(&no_scope)?;
        ensure!(supplied["prior_review_context"]["status"] == "unavailable" && supplied["prior_review_context"]["items"].as_array().unwrap().is_empty());
        let explicit = fixture.audit(json!({"code":AFTER,"file_path":FILE})).await?;
        let (_, supplied) = context(&explicit)?;
        ensure!(supplied["prior_context_input_binding"]["scope_origin"] == "explicit_file_path");
        ensure!(supplied["prior_review_context"]["items"][0]["review_id"] == "PR-4100");
        let maintenance = fixture.audit(json!({"instruction_maintenance":{"instruction_paths":["AGENTS.md"],"evidence_paths":[FILE],"base_commit":fixture.head}})).await?;
        let report: Value = serde_json::from_str(text(&maintenance))?;
        ensure!(report.is_object() && report.get("prior_review_context").is_none());
        ensure!(std::fs::read_to_string(fixture.root.join("AGENTS.md"))? == GUIDE);
        Ok::<(), anyhow::Error>(())
    }).catch_unwind().await;
    finish(fixture, result)
}
