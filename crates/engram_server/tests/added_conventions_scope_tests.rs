#![allow(clippy::unwrap_used)]
//! Exercise the actual added-conventions gate through registry-backed review.
use engram_core::{config::Config, registry::RepoRule};
use engram_server::{
    services::pre_commit_review_service::{
        ReviewConfig, ReviewFinding, gates::AddedConventionsGate, run_pre_commit_review_with,
    },
    state::AppState,
};

async fn review(patterns: &[(&str, &str)], documented: bool) -> Vec<ReviewFinding> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(root.join("Services")).unwrap();
    let declarations = if documented {
        "''' <summary>Existing first.</summary>\nPublic Sub First()\nEnd Sub\n''' <summary>Existing second.</summary>\nPublic Sub Second()\nEnd Sub\nPublic Sub Third()\nEnd Sub\n"
    } else {
        ""
    };
    let added = "Public Function Fetch() As String\n    Dim row = records.FirstOrDefault()\n    Return row.Name\nEnd Function\n";
    let disk = format!("Public Class RecordService\n{declarations}{added}End Class\n");
    std::fs::write(root.join("Services/RecordService.vb"), disk).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: tmp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&engram_core::ProjectRecord {
            project_id: "scope-test".into(),
            project_name: "scope-test".into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "dotnet_webforms_vb".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta("scope-test", "active_generation", "1")
        .unwrap();
    for (i, (pattern, text)) in patterns.iter().enumerate() {
        state
            .registry
            .put_repo_rule(
                "scope-test",
                &RepoRule {
                    rule_id: format!("rule-{i}"),
                    file_pattern: (*pattern).into(),
                    rule_text: (*text).into(),
                    priority: 50,
                    updated_at_ms: 1,
                },
            )
            .unwrap();
    }
    let line = 2 + declarations.lines().count();
    let diff = format!(
        "diff --git a/Services/RecordService.vb b/Services/RecordService.vb\n--- a/Services/RecordService.vb\n+++ b/Services/RecordService.vb\n@@ -{line},0 +{line},4 @@\n+{}",
        added.replace('\n', "\n+").trim_end_matches('+')
    );
    run_pre_commit_review_with(
        &state,
        "scope-test",
        &root,
        1,
        &diff,
        &ReviewConfig::default(),
        vec![Box::new(AddedConventionsGate)],
    )
    .await
    .unwrap()
    .0
}
const DOC: &str = "Require XML doc comments for public members.";
const NULL: &str = "Check null data-access returns before dereferencing.";
fn docs(fs: &[ReviewFinding]) -> bool {
    fs.iter().any(|f| f.title.contains("missing doc comments"))
}
fn nulls(fs: &[ReviewFinding]) -> bool {
    fs.iter().any(|f| f.title.contains("without a guard"))
}

#[tokio::test]
async fn unrelated_scoped_mandates_do_not_leak_into_another_file() {
    let fs = review(&[("Controllers/*.vb", DOC), ("Models/*.vb", NULL)], false).await;
    assert!(!docs(&fs), "unrelated documentation mandate leaked: {fs:?}");
    assert!(!nulls(&fs), "unrelated null mandate leaked: {fs:?}");
}
#[tokio::test]
async fn matching_scoped_and_global_mandates_remain_effective() {
    for pattern in [
        "Services/*.vb",
        "*",
        "SERVICES\\RecordService.vb",
        "RecordService",
        "Services/RecordServic?.vb",
    ] {
        let fs = review(&[(pattern, DOC), (pattern, NULL)], false).await;
        assert!(docs(&fs), "missing doc finding for {pattern}: {fs:?}");
        assert!(nulls(&fs), "missing null finding for {pattern}: {fs:?}");
    }
}
#[tokio::test]
async fn each_mandate_has_its_own_scope_and_empty_pattern_is_not_global() {
    let fs = review(&[("Services/*.vb", DOC), ("Other/*", NULL)], false).await;
    assert!(docs(&fs));
    assert!(!nulls(&fs));
    let fs = review(&[("Other/*", DOC), ("Services/*.vb", NULL)], false).await;
    assert!(!docs(&fs));
    assert!(nulls(&fs));
    let fs = review(&[("", DOC), ("", NULL)], false).await;
    assert!(!docs(&fs));
    assert!(!nulls(&fs));
}
#[tokio::test]
async fn file_local_documentation_style_survives_unrelated_rules() {
    let fs = review(&[("Other/*", DOC), ("Other/*", NULL)], true).await;
    assert!(
        docs(&fs),
        "existing file-local documentation style must remain effective: {fs:?}"
    );
    assert!(!nulls(&fs));
}
