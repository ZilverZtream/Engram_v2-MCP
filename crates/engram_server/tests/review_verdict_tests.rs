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
