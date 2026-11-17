//! Review agents can follow each finding's exact retained decision evidence.
use anyhow::{Result, ensure};
use engram_core::Config;
use engram_server::{Engram, state::AppState};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const FILE: &str = "Site/Export.ts";
const OTHER: &str = "Site/billing/Export.ts";
const BEFORE: &str = "export function exportData() { return true; }\n";
// Deliberately invented, never a real credential.
const AFTER: &str = "export function exportData() { const password = \"fixture_dummy_value_only\"; return password; }\n";
const CONDITIONS: &str = "Demonstration only. Production must use the secret store; changed source requires reassessment.";

fn body(reply: &rmcp::model::CallToolResult) -> &str {
    &reply.content[0].as_text().unwrap().text
}

// All runtime/database handles precede the owned directory in drop order.
struct Fixture {
    app: Engram,
    state: AppState,
    root: PathBuf,
    pid: String,
    base: String,
    temporary: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("Site/billing"))?;
        std::fs::write(root.join(FILE), BEFORE)?;
        std::fs::write(root.join(OTHER), BEFORE)?;
        let base = {
            let repo = git2::Repository::init(&root)?;
            let mut index = repo.index()?;
            index.add_path(Path::new(FILE))?;
            index.add_path(Path::new(OTHER))?;
            let tree = repo.find_tree(index.write_tree()?)?;
            let signature = git2::Signature::now("Owned fixture", "fixture@example.invalid")?;
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Before evidence",
                &tree,
                &[],
            )?
            .to_string()
        };
        std::fs::write(root.join(FILE), AFTER)?;
        std::fs::write(root.join(OTHER), AFTER)?;
        let (state, _) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            llm_backend: "none".into(),
            advertise_all_tools: true,
            ..Default::default()
        })?;
        let app = Engram::new(state.clone());
        let indexed = app.handle_index_project(serde_json::from_value(json!({
            "directory":root,"project_name":"Owned finding links","project_type":"general","wait":true
        }))?).await?;
        let pid = body(&indexed)
            .split("project_id: ")
            .nth(1)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        Ok(Self {
            app,
            state,
            root,
            pid,
            base,
            temporary,
        })
    }

    fn diff(&self) -> String {
        [FILE, OTHER].iter().map(|file| format!(
            "diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1 +1 @@\n-{BEFORE}+{AFTER}"
        )).collect()
    }

    async fn review(&self, json_output: bool, detail: &str, tokens: usize) -> Result<String> {
        let reply = self
            .app
            .handle_pre_commit_review(serde_json::from_value(json!({
                "project_id":self.pid,"diff":self.diff(),"output_json":json_output,
                "detail_level":detail,"max_tokens":tokens,"max_findings":100
            }))?)
            .await?;
        Ok(body(&reply).to_owned())
    }

    async fn record(&self, review: &str, path: Option<&str>, rationale: &str) -> Result<Value> {
        let mut event = json!({"event_id":"exception","finding_id":"prior-thread",
            "supersedes":null,"kind":"accepted_exception","source_url":"https://example.invalid/fixture/review",
            "author":"Fixture owner","recorded_at":"2026-01-01T12:00:00Z",
            "rationale":rationale,"verification":null});
        if let Some(path) = path {
            event["scope"] =
                json!({"reviewed_commit":self.base,"paths":[path],"conditions":CONDITIONS});
        }
        self.app
            .handle_record_review_decisions(serde_json::from_value(json!({
                "project_id":self.pid,"review_id":review,"decisions":[event]
            }))?)
            .await?;
        Ok(event)
    }

    fn close(self) -> Result<()> {
        let Self {
            app,
            state,
            temporary,
            ..
        } = self;
        drop(app);
        drop(state);
        temporary.close()?;
        Ok(())
    }
}

fn secret<'a>(review: &'a Value, path: &str) -> &'a Value {
    review["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["gate"] == "secret_leakage" && finding["file_path"] == path)
        .expect("Authored fixture must produce an actual secret finding")
}

#[tokio::test]
async fn exact_scoped_decision_links_to_its_finding_without_changing_gate_results() -> Result<()> {
    let f = Fixture::new().await?;
    let before: Value = serde_json::from_str(&f.review(true, "full", 65536).await?)?;
    let event = f
        .record(
            "PR-101",
            Some(FILE),
            "Previous demonstration exception; not production clearance.",
        )
        .await?;
    f.record(
        "PR-102",
        Some("other/Export.ts"),
        "Different file; same basename grants no exception.",
    )
    .await?;
    let review: Value = serde_json::from_str(&f.review(true, "full", 65536).await?)?;
    ensure!(review["verdict"] == before["verdict"]);
    ensure!(
        review["findings"].as_array().unwrap().len()
            == before["findings"].as_array().unwrap().len()
    );
    for original in before["findings"].as_array().unwrap() {
        let current = review["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["finding_id"] == original["finding_id"])
            .unwrap();
        for field in [
            "severity",
            "gate",
            "file_path",
            "title",
            "detail",
            "suggestion",
            "evidence",
        ] {
            ensure!(
                current[field] == original[field],
                "Decision annotation changed {field}"
            );
        }
    }
    let finding = secret(&review, FILE);
    let annotation = &finding["prior_review_annotations"][0];
    let item = review
        .pointer(annotation["context_pointer"].as_str().unwrap())
        .unwrap();
    ensure!(item["review_id"] == "PR-101");
    ensure!(annotation["same_file_summary"] == false);
    let matched = &annotation["event_matches"][0];
    ensure!(matched["relevance_basis"] == "declared_exact_path");
    let evidence = review
        .pointer(matched["event_pointer"].as_str().unwrap())
        .unwrap();
    ensure!(
        evidence["evidence"] == event && evidence["evidence"]["scope"]["conditions"] == CONDITIONS
    );
    let scope = review
        .pointer(matched["scope_assessment_pointer"].as_str().unwrap())
        .unwrap();
    ensure!(scope["affected_files"][0]["source_comparison"] == "changed_bytes");
    ensure!(
        secret(&review, OTHER)
            .get("prior_review_annotations")
            .is_none()
    );
    ensure!(review["prior_review_annotation_contract"]["automatic_finding_suppression"] == false);
    for detail in ["compact", "full"] {
        let markdown = f.review(false, detail, 65536).await?;
        ensure!(
            markdown.contains("Finding links to prior review evidence")
                && markdown.contains(finding["finding_id"].as_str().unwrap())
        );
        ensure!(markdown.contains(CONDITIONS) && markdown.contains("same issue"));
    }
    ensure!(std::fs::read_to_string(f.root.join(FILE))? == AFTER);
    f.close()
}

#[tokio::test]
async fn summary_relevance_does_not_promote_unrelated_events_and_legacy_paths_stay_qualified()
-> Result<()> {
    let f = Fixture::new().await?;
    let thread = json!({"pr_id":200,"pr_title":"Export demonstration","pr_author":"Author",
        "pr_date":"2026-01-01","thread_id":1,"status":"wontFix","file_path":FILE,"line":1,
        "comments":[{"author":"Bot","text":"Export password is a hardcoded demonstration value."},
            {"author":"Owner","text":"This is by design for an old demonstration. Production safeguards remain mandatory."}]});
    std::fs::write(f.root.join("reviews.jsonl"), thread.to_string())?;
    f.app.handle_ingest_review_verdicts(serde_json::from_value(json!({
        "project_id":f.pid,"decision_makers":["Owner"],"source":"json_file","file_path":"reviews.jsonl"
    }))?).await?;
    // This latest same-review event is relevant to the OTHER submitted file.
    // It must not ride the summary's FILE association into FILE's finding.
    f.record("PR-200", Some(OTHER), "Separate billing-only exception.")
        .await?;
    let rationale =
        format!("Read /{FILE}:1 for the prior demonstration only; re-evaluate production code.");
    f.record("manual-review", None, &rationale).await?;
    let review: Value = serde_json::from_str(&f.review(true, "full", 65536).await?)?;
    let annotations = secret(&review, FILE)["prior_review_annotations"]
        .as_array()
        .unwrap();
    let summary_link = annotations
        .iter()
        .find(|link| {
            review
                .pointer(link["context_pointer"].as_str().unwrap())
                .unwrap()["review_id"]
                == "PR-200"
        })
        .unwrap();
    ensure!(
        summary_link["same_file_summary"] == true && summary_link["event_matches"] == json!([])
    );
    let legacy_link = annotations
        .iter()
        .find(|link| {
            review
                .pointer(link["context_pointer"].as_str().unwrap())
                .unwrap()["review_id"]
                == "manual-review"
        })
        .unwrap();
    ensure!(legacy_link["event_matches"][0]["relevance_basis"] == "legacy_complete_path_mention");
    let legacy = review
        .pointer(
            legacy_link["event_matches"][0]["event_pointer"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
    ensure!(legacy["evidence"]["scope"].is_null());
    ensure!(legacy["declared_scope_assessment"]["status"] == "scope_not_declared");
    ensure!(
        secret(&review, OTHER)["prior_review_annotations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|link| {
                link["event_matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["relevance_basis"] == "declared_exact_path")
            })
    );
    f.close()
}

#[tokio::test]
async fn bounded_review_preserves_annotation_counts_and_exact_original_evidence_after_changes()
-> Result<()> {
    let f = Fixture::new().await?;
    f.record(
        "PR-300",
        Some(FILE),
        "Original exception, not approval of this change.",
    )
    .await?;
    let deferred: Value = serde_json::from_str(&f.review(true, "full", 2048).await?)?;
    ensure!(
        deferred["response_delivery"]["status"] == "DEFERRED_REVIEW",
        "{deferred}"
    );
    let ledger = deferred["finding_ledger"].as_array().unwrap();
    ensure!(ledger.iter().any(|row| row["gate"] == "secret_leakage"
        && row["file_path"] == FILE
        && row["prior_review_annotation_count"] == 1));
    f.record(
        "PR-301",
        Some(FILE),
        "New record after captured review; absent from its recovery.",
    )
    .await?;
    std::fs::write(
        f.root.join(FILE),
        "export function exportData() { return false; }\n",
    )?;
    let mut next = deferred["response_delivery"]["next_call"].clone();
    let mut original = String::new();
    for _ in 0..100 {
        let reply = f
            .app
            .handle_pre_commit_review(serde_json::from_value(next["arguments"].clone())?)
            .await?;
        ensure!(body(&reply).len() <= 8192);
        let page: Value = serde_json::from_str(body(&reply))?;
        ensure!(page["start_byte"] == original.len());
        original.push_str(page["content"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        next = page["next_call"].clone();
    }
    ensure!(
        blake3::hash(original.as_bytes()).to_hex().as_str()
            == deferred["response_delivery"]["response_id"]
                .as_str()
                .unwrap()
    );
    let captured: Value = serde_json::from_str(&original)?;
    let link = &secret(&captured, FILE)["prior_review_annotations"][0];
    let item = captured
        .pointer(link["context_pointer"].as_str().unwrap())
        .unwrap();
    ensure!(item["review_id"] == "PR-300");
    ensure!(item["provider_neutral_events"][0]["evidence"]["scope"]["conditions"] == CONDITIONS);
    ensure!(!original.contains("New record after captured review"));
    ensure!(secret(&captured, FILE)["severity"] == "critical");
    f.close()
}
