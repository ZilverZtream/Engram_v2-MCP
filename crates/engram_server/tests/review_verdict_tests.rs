//! A review finding's verdict comes from the people who decide it — their
//! replies and thumbs-up — not from the thread status, and a precedent
//! search returns that ruling with its reason. Fixture names are invented.

use engram_core::Config;
use engram_server::Engram;
use engram_server::state::AppState;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;
use tempfile::tempdir;

fn text(res: &rmcp::model::CallToolResult) -> String {
    match &res.content[0].raw {
        rmcp::model::RawContent::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    }
}

const ARGUMENT: &str = "Acknowledged, but I would rather leave this as-is. The limit is a \
    compile-time constant, so changing it already means a code change, a build and a test \
    pass, and the resource strings are part of that same change set.";

fn threads() -> Vec<serde_json::Value> {
    let t =
        |pr: u64, id: u64, author: &str, status: &str, file: &str, comments: serde_json::Value| {
            json!({"pr_id": pr, "pr_title": "Bulk download", "pr_author": author,
               "pr_date": "2026-09-01T10:00:00Z", "thread_id": id, "status": status,
               "file_path": file, "line": 10, "comments": comments})
        };
    vec![
        // Closed as fixed, but the lead ruled it intended.
        t(
            100,
            1,
            "Riley Dev",
            "fixed",
            "/src/sheetsearch.vb",
            json!([
            {"author": "Morgan Reviewer", "text": "Forms with a custom report get the attachments option in bulk but not per row."},
            {"author": "Casey Lead", "text": "This is the intended behaviour."}]),
        ),
        // The lead's thumbs-up endorses; the author still declined, with an argument.
        t(
            100,
            2,
            "Riley Dev",
            "wontFix",
            "/src/customFormSheet.vb",
            json!([
            {"author": "Morgan Reviewer", "text": "The download limit is hardcoded in the message text of every resx file.", "likes": ["Casey Lead"]},
            {"author": "Riley Dev", "text": ARGUMENT}]),
        ),
        // Author declined with an argument; no ruling.
        t(
            100,
            3,
            "Riley Dev",
            "wontFix",
            "/src/sheetexport.vb",
            json!([
            {"author": "ReviewBot", "text": "Use CompressionLevel.Optimal for the shared zip helper."},
            {"author": "Riley Dev", "text": ARGUMENT}]),
        ),
        // A later PR: excluded by before_pr_id.
        t(
            200,
            4,
            "Riley Dev",
            "fixed",
            "/src/sheetsearch.vb",
            json!([
            {"author": "ReviewBot", "text": "The bulk download attachments option is missing a null check."},
            {"author": "Casey Lead", "text": "Fixed in commit abc123."}]),
        ),
    ]
}

#[tokio::test]
async fn precedents_carry_the_decision_makers_ruling_and_reason() {
    let tmp = tempdir().unwrap();
    let project_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join("a.vb"), "Module A\nEnd Module\n").unwrap();
    let jsonl = project_dir.join("threads.jsonl");
    std::fs::write(
        &jsonl,
        threads()
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![project_dir.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state);
    let indexed = text(
        &engram
            .index_project(Parameters(engram_server::IndexProjectRequest {
                directory: project_dir.to_string_lossy().to_string(),
                project_name: "verdicts".into(),
                project_type: engram_server::models::ProjectType::General,
                wait: true,
                dedupe_by_directory: true,
            }))
            .await
            .unwrap(),
    );
    let pid = indexed
        .split("project_id: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();

    let ingest: engram_server::models::IngestReviewVerdictsRequest =
        serde_json::from_value(json!({
            "project_id": pid, "decision_makers": ["Casey Lead"], "source": "json_file",
            "file_path": "threads.jsonl", "author_trust": {"Riley Dev": "high"}
        }))
        .unwrap();
    let summary = text(&engram.handle_ingest_review_verdicts(ingest).await.unwrap());
    assert!(summary.contains("rejected: 1"), "{summary}");
    assert!(summary.contains("endorsed: 2"), "{summary}");
    assert!(summary.contains("contested_by_author: 1"), "{summary}");

    let precedents = |query: &str, extra: serde_json::Value| {
        let mut req = json!({"project_id": pid, "query": query});
        req.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<engram_server::models::GetReviewPrecedentsRequest>(req).unwrap()
    };

    // A draft finding like the rejected one gets the ruling and its reason.
    let out = text(
        &engram
            .handle_get_review_precedents(precedents(
                "custom report forms missing per row attachments option",
                json!({"before_pr_id": 200}),
            ))
            .await
            .unwrap(),
    );
    let first = out.split("--- #2").next().unwrap();
    assert!(
        first.contains("Review verdict: rejected by Casey Lead"),
        "{out}"
    );
    assert!(first.contains("This is the intended behaviour."), "{out}");
    assert!(
        !out.contains("PR-200"),
        "before_pr_id must hide later reviews: {out}"
    );

    // The contested decline shows the argument, the author's record and owner trust.
    let out = text(
        &engram
            .handle_get_review_precedents(precedents(
                "zip helper compression level",
                json!({"verdicts": ["contested_by_author"]}),
            ))
            .await
            .unwrap(),
    );
    assert!(
        out.contains("Review verdict: contested_by_author by Riley Dev"),
        "{out}"
    );
    assert!(out.contains("compile-time constant"), "{out}");
    assert!(out.contains("owner trust: high"), "{out}");
    assert!(
        !out.contains("Review verdict: rejected"),
        "verdict filter: {out}"
    );
}

/// An eligible old quotation must not borrow later author outcomes or the
/// current unversioned authority declarations as historical supporting evidence.
#[test]
fn precedent_cutoff_preserves_old_quote_without_later_author_authority() -> anyhow::Result<()> {
    // The runtime owns any blocking indexing workers and is dropped before the
    // fixture on successful return, early error, and unwinding.
    let temporary = tempfile::tempdir()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let root = temporary.path().join("repo");
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("src/export.rs"), "pub fn export() {}\n")?;
        let (state, _events) = AppState::new(Config {
            data_dir: temporary.path().join("data"),
            allowed_roots: vec![root.clone()],
            embedding_backend: "fts_only".into(),
            ..Default::default()
        })?;
        let engram = Engram::new(state);
        let indexed = text(&engram.handle_index_project(serde_json::from_value(json!({
            "directory": root, "project_name": "Cutoff authority behavior",
            "project_type": "rust", "wait": true
        }))?).await?);
        let pid = indexed
            .split("project_id: ")
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("missing project ID: {indexed}"))?
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        let records = [(100, "older wire export budget finding"),
            (200, "FutureFindingMarker wire export budget finding")]
            .into_iter()
            .map(|(pr, finding)| json!({
                "pr_id": pr, "pr_title": "Wire export budget", "pr_author": "Riley Dev",
                "pr_date": if pr == 100 { "2026-01-01T10:00:00Z" } else { "2026-02-01T10:00:00Z" },
                "thread_id": 1, "status": "wontFix", "file_path": "/src/export.rs", "line": 1,
                "comments": [{"author": "ReviewBot", "text": finding},
                    {"author": "Riley Dev", "text": ARGUMENT}]
            }))
            .map(|record| record.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("threads.jsonl"), records)?;
        engram.handle_ingest_review_verdicts(serde_json::from_value(json!({
            "project_id": pid, "decision_makers": ["CURRENT_CONFIGURED_LEAD"],
            "source": "json_file", "file_path": "threads.jsonl",
            "author_trust": {"Riley Dev": "CURRENT_ONLY_TRUST_MARKER"}
        }))?).await?;
        let request = |cutoff: Option<u64>| serde_json::from_value(json!({
            "project_id": pid, "query": "wire export budget", "file_path": "src/export.rs",
            "before_pr_id": cutoff, "limit": 10
        }));
        let historical = text(&engram.handle_get_review_precedents(request(Some(200))?).await?);
        anyhow::ensure!(historical.contains("PR-100:") && !historical.contains("PR-200:"), "{historical}");
        anyhow::ensure!(!historical.contains("FutureFindingMarker"), "{historical}");
        // Preserve the actual old argument and its actor, without converting an
        // author's argued workflow decline into an accepted policy exception.
        anyhow::ensure!(historical.contains(ARGUMENT), "{historical}");
        anyhow::ensure!(historical.contains("Review verdict: contested_by_author by Riley Dev"), "{historical}");
        anyhow::ensure!(historical.contains("Reasoning (Riley Dev):"), "{historical}");
        anyhow::ensure!(!historical.contains("Author record:"), "{historical}");
        anyhow::ensure!(!historical.contains("CURRENT_ONLY_TRUST_MARKER"), "{historical}");
        anyhow::ensure!(!historical.contains("CURRENT_CONFIGURED_LEAD"), "{historical}");
        anyhow::ensure!(historical.contains("PR-ID cutoff:") && historical.contains("below 200"), "{historical}");
        anyhow::ensure!(historical.contains("not an as-of decision or source-date replay"), "{historical}");
        anyhow::ensure!(historical.contains("accepted scope and revision applicability are not established"), "{historical}");
        anyhow::ensure!(historical.contains("retain findings; no automatic suppression"), "{historical}");
        // This control proves the later PR really contributes to the stored
        // aggregate. Current queries retain that evidence with a current label.
        let current = text(&engram.handle_get_review_precedents(request(None)?).await?);
        anyhow::ensure!(current.contains("PR-100:") && current.contains("PR-200:"), "{current}");
        anyhow::ensure!(current.contains("FutureFindingMarker"), "{current}");
        anyhow::ensure!(current.contains("Author record: 2 argued declines (2 stood, 0 overturned)"), "{current}");
        anyhow::ensure!(current.contains("CURRENT_ONLY_TRUST_MARKER") && current.contains("CURRENT_CONFIGURED_LEAD"), "{current}");
        anyhow::ensure!(current.contains("current imported configuration") && current.contains("current aggregates"), "{current}");
        Ok::<(), anyhow::Error>(())
    });
    drop(runtime);
    temporary.close()?;
    result
}
