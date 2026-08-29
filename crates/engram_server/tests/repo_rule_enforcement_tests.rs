#![allow(clippy::unwrap_used)]
//! External audit 2026-08-29 row 8 (security, settings, durable project
//! laws — "valuable knowledge, incomplete enforcement"): repo rules and the
//! quality-gate mandates promoted into them reached the review only as
//! advisory text (ImmuneGate context, two hard-coded demands). A rule that
//! carries a checkable clause must be ENFORCED: `[check: forbid=<regex>]`
//! flags an added line matching it; `[check: require=<regex> when=<regex>]`
//! flags a file whose added lines match `when` but never `require`.

use engram_core::config::Config;
use engram_core::registry::RepoRule;
use engram_server::services::pre_commit_review_service::{
    ReviewConfig, Severity, all_gates, run_pre_commit_review_with,
};
use engram_server::state::AppState;

const PID: &str = "repo-rule-enforcement-test";

fn build_state() -> (tempfile::TempDir, AppState, std::path::PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(project_dir.join("Site/App_Code")).unwrap();
    std::fs::write(
        project_dir.join("Site/App_Code/api.vb"),
        "Public Class api\n    Public Function Get(qry As Object) As String\n        Return \"x\"\n    End Function\nEnd Class\n",
    )
    .unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir.clone()],
        max_project_files: None,
        max_project_bytes: None,
        embedding_backend: "fts_only".into(),
        embedding_model: None,
        ollama_url: None,
        openai_api_key: None,
        max_concurrent_jobs: 1,
        ..Default::default()
    };
    let (state, _rx) = AppState::new(cfg).unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: project_dir.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (tmp, state, project_dir)
}

fn rule(state: &AppState, id: &str, text: &str, priority: i32) {
    state
        .registry
        .put_repo_rule(
            PID,
            &RepoRule {
                rule_id: id.into(),
                file_pattern: "**/*.vb".into(),
                rule_text: text.into(),
                priority,
                updated_at_ms: 1,
            },
        )
        .unwrap();
}

const SQL_CONCAT_DIFF: &str = "diff --git a/Site/App_Code/api.vb b/Site/App_Code/api.vb\n\
--- a/Site/App_Code/api.vb\n\
+++ b/Site/App_Code/api.vb\n\
@@ -1,5 +1,7 @@\n \
Public Class api\n \
    Public Function Get(qry As Object) As String\n\
+        Dim pr_id = GetDictionaryIntegerValue(qry.data, \"pr_id\")\n\
+        Dim sql = \"SELECT * FROM projekt WHERE id = \" & pr_id\n \
        Return \"x\"\n \
    End Function\n \
End Class\n";

const CLEAN_DIFF: &str = "diff --git a/Site/App_Code/api.vb b/Site/App_Code/api.vb\n\
--- a/Site/App_Code/api.vb\n\
+++ b/Site/App_Code/api.vb\n\
@@ -1,5 +1,7 @@\n \
Public Class api\n \
    Public Function Get(qry As Object) As String\n\
+        Dim pr_id = GetDictionaryIntegerValue(qry.data, \"pr_id\")\n\
+        If Not _us.UserAccess.check_pr_id(pr_id) Then Return \"denied\"\n \
        Return \"x\"\n \
    End Function\n \
End Class\n";

async fn review(
    state: &AppState,
    dir: &std::path::Path,
    diff: &str,
) -> Vec<engram_server::services::pre_commit_review_service::ReviewFinding> {
    let (findings, _, _, _) = run_pre_commit_review_with(
        state,
        PID,
        dir,
        1,
        diff,
        &ReviewConfig::default(),
        all_gates(),
    )
    .await
    .unwrap();
    findings
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forbid_clause_turns_a_repo_rule_into_a_finding() {
    let (_tmp, state, dir) = build_state();
    rule(
        &state,
        "qg_sql_concat",
        "Never build SQL by string concatenation; always parameterize [check: forbid=\"\\s*&\\s*[A-Za-z_]|&\\s*\"]",
        90,
    );
    let findings = review(&state, &dir, SQL_CONCAT_DIFF).await;
    let hit = findings
        .iter()
        .find(|f| f.gate == "repo_rules" && f.title.contains("qg_sql_concat"))
        .unwrap_or_else(|| panic!("the forbid clause must produce a repo_rules finding naming the rule; findings: {:?}", findings.iter().map(|f| (f.gate, f.title.clone())).collect::<Vec<_>>()));
    assert!(
        matches!(hit.severity, Severity::Critical),
        "priority 90 ⇒ Critical, got {:?}",
        hit.severity
    );
    assert!(
        hit.lines.contains(&4),
        "the offending added line (4) is cited: {:?}",
        hit.lines
    );
    assert!(
        hit.detail.contains("string concatenation"),
        "the rule text is the detail: {}",
        hit.detail
    );

    let clean = review(&state, &dir, CLEAN_DIFF).await;
    assert!(
        !clean.iter().any(|f| f.gate == "repo_rules"),
        "a clean diff produces no repo_rules finding: {:?}",
        clean
            .iter()
            .map(|f| (f.gate, f.title.clone()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_require_when_clause_flags_the_missing_guard() {
    let (_tmp, state, dir) = build_state();
    rule(
        &state,
        "qg_pr_id_guard",
        "Every endpoint that reads a client pr_id must object-check it [check: require=check_pr_id when=GetDictionaryIntegerValue\\(qry\\.data,\\s*\"pr_id\"]",
        60,
    );
    let findings = review(&state, &dir, SQL_CONCAT_DIFF).await;
    let hit = findings
        .iter()
        .find(|f| f.gate == "repo_rules" && f.title.contains("qg_pr_id_guard"))
        .unwrap_or_else(|| {
            panic!(
                "pr_id read without check_pr_id must be flagged; findings: {:?}",
                findings
                    .iter()
                    .map(|f| (f.gate, f.title.clone()))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        matches!(hit.severity, Severity::Warning),
        "priority 60 ⇒ Warning, got {:?}",
        hit.severity
    );

    let ok = review(&state, &dir, CLEAN_DIFF).await;
    assert!(
        !ok.iter()
            .any(|f| f.gate == "repo_rules" && f.title.contains("qg_pr_id_guard")),
        "the guarded diff satisfies the require clause"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_without_a_clause_stays_advisory() {
    let (_tmp, state, dir) = build_state();
    rule(
        &state,
        "qg_prose_only",
        "Prefer IQueryable for composable queries",
        90,
    );
    let findings = review(&state, &dir, SQL_CONCAT_DIFF).await;
    assert!(
        !findings.iter().any(|f| f.gate == "repo_rules"),
        "prose rules are not enforced (no clause) — no repo_rules finding"
    );
}
