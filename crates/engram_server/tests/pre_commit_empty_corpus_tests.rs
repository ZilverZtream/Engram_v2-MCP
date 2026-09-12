#![allow(clippy::unwrap_used)]
//! Actual registry/search-backed coverage, distinct from a violation or an empty query result.
use engram_core::{Config, ContentHash, DocIdStr, RelPath, registry::RepoRule};
use engram_server::services::pre_commit_review_service::{
    GateStatus, ReviewConfig,
    gates::{AntiPatternGate, ImmuneGate},
    run_pre_commit_review_with,
};
use engram_server::{state::AppState, tools::Engram};
use rmcp::handler::server::tool::Parameters;
use tokio_util::sync::CancellationToken;

async fn fixture() -> (tempfile::TempDir, AppState, String, std::path::PathBuf, u64) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Sample.vb"), "Public Class Sample\n Public Function Label() As String\n Return \"ordinary display label\"\n End Function\nEnd Class\n").unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    Engram::new(state.clone())
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().into_owned(),
            project_name: "coverage".into(),
            project_type: engram_server::models::ProjectType::General,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let generation = engram_server::services::project_service::get_active_generation(&state, &pid)
        .await
        .unwrap();
    (tmp, state, pid, root, generation)
}

async fn seed_doc(state: &AppState, pid: &str, ns: &str) {
    let text = "zebras migrate across remote savannah ecosystems during seasonal rainfall";
    let hash = ContentHash::compute(text.as_bytes());
    let path = "reviewed-example.md";
    let doc = engram_index::IndexDoc {
        generation: 0,
        chunk_id: 1,
        path: RelPath::new(path),
        language: "markdown".into(),
        content: text.into(),
        namespace: ns.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: DocIdStr::compute(path, 1, 1, &hash).0,
        content_hash: hash.0,
    };
    state
        .get_project_cached(pid)
        .unwrap()
        .search
        .index_docs(pid, &[doc], &CancellationToken::new())
        .await
        .unwrap();
}

fn seed_rule(state: &AppState, pid: &str, immune: bool, matching: bool) {
    state
        .registry
        .put_repo_rule(
            pid,
            &RepoRule {
                rule_id: if immune {
                    "immune_reviewed_revert"
                } else {
                    "documentation"
                }
                .into(),
                file_pattern: if matching { "Sample.vb" } else { "Other.vb" }.into(),
                rule_text: "Review destructive operations after an earlier reverted change.".into(),
                priority: 50,
                updated_at_ms: 1,
            },
        )
        .unwrap();
}

async fn review(
    state: &AppState,
    pid: &str,
    root: &std::path::Path,
    generation: u64,
    destructive: bool,
) -> (
    Vec<engram_server::services::pre_commit_review_service::ReviewFinding>,
    Vec<engram_server::services::pre_commit_review_service::GateOutcome>,
) {
    let code = if destructive {
        " db.Items.DeleteAllOnSubmit(db.Items)"
    } else {
        " Return \"ordinary display label used by the current safe presentation method\""
    };
    let diff = format!(
        "diff --git a/Sample.vb b/Sample.vb\n--- a/Sample.vb\n+++ b/Sample.vb\n@@ -3,0 +3,1 @@\n+{code}\n"
    );
    let (findings, _, _, outcomes) = run_pre_commit_review_with(
        state,
        pid,
        root,
        generation,
        &diff,
        &ReviewConfig::default(),
        vec![Box::new(ImmuneGate), Box::new(AntiPatternGate)],
    )
    .await
    .unwrap();
    (findings, outcomes)
}

#[tokio::test]
async fn empty_corpora_are_degraded_without_fabricated_violations() {
    let (_tmp, state, pid, root, generation) = fixture().await;
    let (findings, outcomes) = review(&state, &pid, &root, generation, false).await;
    assert!(findings.is_empty(), "{findings:?}");
    for (name, reason) in [
        ("immune", "No immune_ repo rules"),
        ("antipattern", "No antipattern documents"),
    ] {
        let outcome = outcomes.iter().find(|o| o.name == name).unwrap();
        assert!(
            matches!(&outcome.status, GateStatus::Degraded { findings: 0, notes } if notes.iter().any(|n| n.contains(reason))),
            "{outcomes:?}"
        );
    }
}

#[tokio::test]
async fn unrelated_rules_and_namespace_do_not_supply_missing_coverage() {
    let (_tmp, state, pid, root, generation) = fixture().await;
    seed_rule(&state, &pid, false, true);
    seed_doc(&state, &pid, "history").await;
    let (findings, outcomes) = review(&state, &pid, &root, generation, false).await;
    assert!(findings.is_empty());
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o.status, GateStatus::Degraded { .. })),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn populated_but_nonmatching_corpora_pass_without_empty_corpus_warning() {
    let (_tmp, state, pid, root, generation) = fixture().await;
    seed_rule(&state, &pid, true, false);
    seed_doc(&state, &pid, "antipattern").await;
    let (findings, outcomes) = review(&state, &pid, &root, generation, false).await;
    assert!(findings.is_empty(), "{findings:?}");
    assert!(
        outcomes.iter().all(|o| o.status == GateStatus::Passed),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn empty_antipattern_corpus_retains_destructive_fallback_and_immune_findings() {
    let (_tmp, state, pid, root, generation) = fixture().await;
    seed_rule(&state, &pid, true, true);
    let (findings, outcomes) = review(&state, &pid, &root, generation, true).await;
    assert!(findings.iter().any(|f| f.gate == "immune"), "{findings:?}");
    assert!(
        findings
            .iter()
            .any(|f| f.gate == "antipattern" && f.title.contains("Destructive")),
        "{findings:?}"
    );
    assert!(outcomes.iter().any(|o| o.name == "antipattern" && matches!(&o.status, GateStatus::Degraded { findings, notes } if *findings > 0 && notes.iter().any(|n| n.contains("regex-only")))), "{outcomes:?}");
}
