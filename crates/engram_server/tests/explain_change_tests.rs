#![allow(clippy::unwrap_used)]
//! End-to-end tests for `explain_change`. Exercises the full pipeline
//! (diff parse → classify → scope → per-file facts → rule alignment →
//! coupling notes → render) against a real `AppState`.

use std::path::PathBuf;

use engram_core::RelPath;
use engram_core::config::Config;
use engram_core::registry::RepoRule;
use engram_graph::{Edge, EdgeKind, Node};

use engram_server::services::explain_change_service::{
    explain_change, ChangeKind, ExplainChangeConfig, SubjectStyle,
};
use engram_server::state::AppState;

// ─── helpers ────────────────────────────────────────────────────────────────

fn build_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
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
    (tmp, state)
}

fn register_project(state: &AppState, tmp: &tempfile::TempDir) -> (String, PathBuf) {
    let project_dir = tmp.path().join("project");
    let rec = engram_core::ProjectRecord {
        project_id: "explain-test".into(),
        project_name: "explain-test".into(),
        directory: project_dir.to_string_lossy().into_owned(),
        project_type: "general".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        reindex_required_since_ms: None,
    };
    state.registry.put_project(&rec).unwrap();
    state
        .registry
        .set_meta("explain-test", "active_generation", "1")
        .unwrap();
    ("explain-test".into(), project_dir)
}

fn file_node(path: &str) -> Node {
    Node {
        node_id: format!("file:{path}"),
        node_type: "file".into(),
        name: path.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(path),
        start_line: 1,
        end_line: 100,
        generation: 1,
        metadata: None,
    }
}

fn edge(src: &str, tgt: &str, kind: EdgeKind, weight: u32) -> Edge {
    Edge {
        source_id: src.into(),
        target_id: tgt.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        edge_kind: kind,
        weight,
        generation: 1,
        metadata: None,
        updated_at_ms: 1,
    }
}

// ─── tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_diff_with_immune_file_gets_red_risk_and_addresses_rule() {
    let (tmp, state) = build_state();
    let (project_id, project_dir) = register_project(&state, &tmp);

    // Seed: one immune repo rule pointing at the file we'll modify.
    state
        .registry
        .put_repo_rule(
            &project_id,
            &RepoRule {
                rule_id: "immune_8133c1338133".into(),
                file_pattern: "site/app_code/dal/orders.vb".into(),
                rule_text: "BaseDeleteByInputQuery lacked multitenant WHERE".into(),
                priority: 1,
                updated_at_ms: 1,
            },
        )
        .unwrap();

    let diff = "\
diff --git a/Site/App_Code/dal/Orders.vb b/Site/App_Code/dal/Orders.vb
--- a/Site/App_Code/dal/Orders.vb
+++ b/Site/App_Code/dal/Orders.vb
@@ -10,6 +10,8 @@
 Public Sub DeleteOrder(id As Integer)
     Dim db As New iFaltDataContext()
+    Dim row = db.Orders.FirstOrDefault(Function(o) o.Id = id)
+    If row Is Nothing Then Return
     db.SubmitChanges()
 End Sub
";
    let config = ExplainChangeConfig {
        style: SubjectStyle::Conventional,
        include_changelog: true,
        use_llm: false,
    };
    let (narrative, rendered) = explain_change(&state, &project_id, &project_dir, 1, diff, &config)
        .await
        .unwrap()
        .expect("narrative must be produced");

    // Kind: modified-only with no "fix" keyword → refactor.
    assert!(
        matches!(
            narrative.kind,
            ChangeKind::Refactor | ChangeKind::Fix | ChangeKind::Feat
        ),
        "unexpected kind: {:?}",
        narrative.kind
    );

    // Risk: immune flag on this file → red.
    assert_eq!(narrative.risk_badge, "red", "got: {narrative:#?}");

    // Rule alignment includes the immune rule by path match.
    let immune_hit = narrative
        .rule_alignments
        .iter()
        .find(|r| r.rule_id == "immune_8133c1338133");
    assert!(
        immune_hit.is_some(),
        "expected immune rule alignment, got: {:#?}",
        narrative.rule_alignments
    );

    // Commit message includes the rule footer.
    assert!(
        rendered.commit_message.contains("Addresses:")
            && rendered.commit_message.contains("immune_8133c1338133"),
        "commit footer missing immune cite; got:\n{}",
        rendered.commit_message
    );

    // PR description mentions the immune rule in Addresses section.
    assert!(
        rendered.pr_description.contains("🛡")
            || rendered.pr_description.contains("immune"),
        "PR description missing immune tag; got:\n{}",
        rendered.pr_description
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feat_change_emits_changelog_added_entry_with_scope() {
    let (tmp, state) = build_state();
    let (project_id, project_dir) = register_project(&state, &tmp);

    let diff = "\
diff --git a/site/orders/AuditHook.vb b/site/orders/AuditHook.vb
--- /dev/null
+++ b/site/orders/AuditHook.vb
@@ -0,0 +1,3 @@
+Public Class AuditHook
+    Public Sub Record(action As String) : End Sub
+End Class
";
    let config = ExplainChangeConfig::default();
    let (narrative, rendered) = explain_change(&state, &project_id, &project_dir, 1, diff, &config)
        .await
        .unwrap()
        .expect("narrative");

    assert_eq!(narrative.kind, ChangeKind::Feat);
    assert_eq!(narrative.scope.as_deref(), Some("orders"));

    // Commit subject uses conventional `feat(orders): …` form.
    assert!(
        rendered.commit_message.starts_with("feat(orders):"),
        "got: {}",
        rendered.commit_message
    );

    // Changelog entry is emitted under "### Added".
    let cl = rendered
        .changelog_entry
        .expect("changelog entry expected for feat");
    assert!(cl.contains("### Added"), "got: {cl}");
    assert!(cl.contains("**orders**"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_coupling_partner_missing_appears_in_pr() {
    let (tmp, state) = build_state();
    let (project_id, project_dir) = register_project(&state, &tmp);

    // Seed the graph: a.ts ↔ b.ts with high coupling weight.
    state
        .graph
        .upsert_nodes(&project_id, &[file_node("a.ts"), file_node("b.ts")])
        .unwrap();
    state
        .graph
        .upsert_edges(
            &project_id,
            &[edge(
                "file:a.ts",
                "file:b.ts",
                EdgeKind::TemporalCoupling,
                300,
            )],
        )
        .unwrap();

    let diff = "\
diff --git a/a.ts b/a.ts
--- a/a.ts
+++ b/a.ts
@@ -1,1 +1,2 @@
 const x = 1;
+const y = 2;
";
    let (narrative, rendered) = explain_change(
        &state,
        &project_id,
        &project_dir,
        1,
        diff,
        &ExplainChangeConfig::default(),
    )
    .await
    .unwrap()
    .expect("narrative");

    // At least one coupling note for b.ts.
    assert!(
        narrative.coupling_notes.iter().any(|c| c.partner_file == "b.ts"),
        "coupling note missing: {:#?}",
        narrative.coupling_notes
    );

    // PR description surfaces it in the temporal-coupling section.
    assert!(
        rendered.pr_description.contains("Temporal coupling note")
            && rendered.pr_description.contains("b.ts"),
        "PR description missing coupling section; got:\n{}",
        rendered.pr_description
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_diff_returns_none() {
    let (tmp, state) = build_state();
    let (project_id, project_dir) = register_project(&state, &tmp);

    let result = explain_change(
        &state,
        &project_id,
        &project_dir,
        1,
        "",
        &ExplainChangeConfig::default(),
    )
    .await
    .unwrap();
    assert!(result.is_none(), "empty diff must yield None");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_style_omits_conventional_prefix() {
    let (tmp, state) = build_state();
    let (project_id, project_dir) = register_project(&state, &tmp);

    let diff = "\
diff --git a/site/orders/AuditHook.vb b/site/orders/AuditHook.vb
--- /dev/null
+++ b/site/orders/AuditHook.vb
@@ -0,0 +1,1 @@
+Public Class AuditHook : End Class
";
    let config = ExplainChangeConfig {
        style: SubjectStyle::Plain,
        include_changelog: false,
        use_llm: false,
    };
    let (_narrative, rendered) = explain_change(&state, &project_id, &project_dir, 1, diff, &config)
        .await
        .unwrap()
        .expect("narrative");

    // Plain style uses "Added in orders: …" not "feat(orders): …".
    assert!(
        !rendered.commit_message.starts_with("feat"),
        "plain style must not use conventional prefix; got: {}",
        rendered.commit_message
    );
    assert!(
        rendered.commit_message.contains("Added in orders"),
        "plain style label missing; got: {}",
        rendered.commit_message
    );
    assert!(
        rendered.changelog_entry.is_none(),
        "include_changelog=false must suppress entry"
    );
}
