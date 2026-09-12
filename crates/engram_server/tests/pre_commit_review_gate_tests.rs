#![allow(clippy::unwrap_used)]
//! Behavioral tests for the pre-commit review gates.
//!
//! Each test builds a real `AppState` (with a temp data dir), seeds the
//! registry/graph with the minimum data needed to drive a specific gate,
//! constructs a `GateContext` by hand, and asserts on the emitted
//! findings. This exercises the production gate code paths without
//! going through the MCP handler — we don't need an HTTP runtime or
//! index engine to test a gate's logic.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use engram_core::RelPath;
use engram_core::config::Config;
use engram_core::registry::RepoRule;
use engram_graph::{Edge, EdgeKind, Node};

use engram_server::services::pre_commit_review_service::gates::{
    AuditGate, BlastRadiusGate, GuardParityGate, ImmuneGate, NewFileGate, SecretLeakageGate,
    StateGate, TemporalGate, TestCoverageGate, SyncContractGate, UnwiredGate, CoAddedFamilyGate, all_gates,
};
use engram_server::services::pre_commit_review_service::{
    ChangeType, DiffFile, Gate, GateContext, ReviewFinding, Severity, aggregate_findings,
    parse_unified_diff,
};
use engram_server::state::AppState;

// ── helpers ──────────────────────────────────────────────────────────────────

fn temp_state() -> (tempfile::TempDir, AppState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    let cfg = Config {
        data_dir,
        allowed_roots: vec![project_dir],
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

fn make_file_node(path: &str) -> Node {
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

fn make_fn_node(id: &str, name: &str, file: &str) -> Node {
    Node {
        node_id: id.into(),
        node_type: "function".into(),
        name: name.into(),
        namespace: "test".into(),
        language: "vbnet".into(),
        file_path: RelPath::new(file),
        start_line: 1,
        end_line: 20,
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

struct Ctx {
    state: AppState,
    project_id: String,
    project_dir: PathBuf,
    diff_files: Vec<DiffFile>,
    changed_paths: HashSet<String>,
    total_commits: u32,
    _tmp: tempfile::TempDir,
}

fn ctx_from_diff(diff: &str) -> Ctx {
    let (tmp, state) = temp_state();
    let diff_files = parse_unified_diff(diff);
    let changed_paths = diff_files.iter().map(|f| f.path.clone()).collect();
    let project_dir = tmp.path().join("project");
    Ctx {
        state,
        project_id: "proj-t".into(),
        project_dir,
        diff_files,
        changed_paths,
        total_commits: 500,
        _tmp: tmp,
    }
}

fn as_gate_ctx<'a>(c: &'a Ctx) -> GateContext<'a> {
    // Tests build the pre-computed indices in-line so gates read from
    // them the same way production code does.
    let repo_rules = Arc::new(
        c.state
            .registry
            .list_repo_rules(&c.project_id)
            .unwrap_or_default(),
    );
    let file_nodes = c
        .state
        .graph
        .query_nodes(&c.project_id, Some("file"), None, None, 50_000)
        .unwrap_or_default();
    let mut files_by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for n in file_nodes {
        let p = n.file_path.as_str().to_string();
        let parent = match p.rfind('/') {
            Some(i) => p[..i].to_string(),
            None => String::new(),
        };
        files_by_parent.entry(parent).or_default().push(p);
    }
    // Detect audit function — same logic as the production orchestrator,
    // inlined so tests exercise the gate's fast path.
    let audit_function = [
        "handelselogg",
        "AuditLog",
        "audit_log",
        "LogActivity",
        "AuditTrail",
    ]
    .iter()
    .find_map(|pat| {
        let matches = c
            .state
            .graph
            .query_nodes(&c.project_id, Some("function"), Some(pat), None, 10)
            .unwrap_or_default();
        if matches.is_empty() {
            None
        } else {
            matches
                .iter()
                .max_by_key(|n| n.name.len())
                .map(|n| n.name.clone())
        }
    });
    GateContext {
        search_index_note: None,
        state: &c.state,
        graph: c.state.graph.clone(),
        registry: c.state.registry.clone(),
        project_id: &c.project_id,
        project_dir: c.project_dir.as_path(),
        generation: 1,
        diff_files: &c.diff_files,
        changed_paths: &c.changed_paths,
        total_commits: c.total_commits,
        repo_rules,
        files_by_parent: Arc::new(files_by_parent),
        audit_function,
        degraded: std::sync::Mutex::new(Vec::new()),
        caps: std::sync::Mutex::new(Vec::new()),
    }
}

fn identity_diff(path: &str) -> String {
    format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,6 @@\n existing\n+one\n+two\n+three\n+four\n+five\n")
}

#[test]
fn temporal_gate_preserves_exact_nested_partner_and_unique_historical_alias() {
    for (raw, files, expected) in [
        ("nested/Rules.vb", vec!["Rules.vb", "nested/Rules.vb"], "nested/Rules.vb"),
        ("Old.vb", vec!["Rules.vb", "new/Old.vb"], "new/Old.vb"),
    ] {
        let c = ctx_from_diff(&identity_diff("Rules.vb"));
        c.state.graph.upsert_nodes("proj-t", &files.iter().map(|path| make_file_node(path)).collect::<Vec<_>>()).unwrap();
        c.state.graph.upsert_edges("proj-t", &[edge("file:Rules.vb", &format!("file:{raw}"), EdgeKind::TemporalCoupling, 100)]).unwrap();
        let ctx = as_gate_ctx(&c);
        let findings = TemporalGate.run(&ctx).unwrap();
        assert!(findings.iter().any(|finding| finding.title.contains(expected)), "{findings:#?}");
    }
    let diff = identity_diff("Rules.vb") + &identity_diff("nested/Rules.vb");
    let c = ctx_from_diff(&diff);
    c.state.graph.upsert_nodes("proj-t", &[make_file_node("Rules.vb"), make_file_node("nested/Rules.vb")]).unwrap();
    c.state.graph.upsert_edges("proj-t", &[edge("file:Rules.vb", "file:nested/Rules.vb", EdgeKind::TemporalCoupling, 100)]).unwrap();
    assert!(TemporalGate.run(&as_gate_ctx(&c)).unwrap().is_empty());
}

#[test]
fn temporal_gate_degrades_ambiguous_historical_identity_instead_of_selecting_shortest() {
    let c = ctx_from_diff(&identity_diff("Changed.vb"));
    c.state.registry.set_meta("proj-t", "last_git_oid", "fixture").unwrap();
    c.state.registry.set_meta("proj-t", "git_backfill_complete", "1").unwrap();
    c.state.graph.upsert_nodes("proj-t", &[
        make_file_node("Changed.vb"), make_file_node("a/Rules.vb"), make_file_node("longer/Rules.vb")
    ]).unwrap();
    c.state.graph.upsert_edges("proj-t", &[edge("file:Changed.vb", "file:Rules.vb", EdgeKind::TemporalCoupling, 100)]).unwrap();
    let ctx = as_gate_ctx(&c);
    assert!(TemporalGate.run(&ctx).unwrap().is_empty());
    let failures = ctx.degraded.lock().unwrap();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("ambiguous historical path Rules.vb"), "{failures:?}");
}

#[test]
fn test_coverage_does_not_count_a_distinct_root_test_as_nested_test_update() {
    let diff = identity_diff("Service.ts") + &identity_diff("service.test.ts");
    let c = ctx_from_diff(&diff);
    c.state.graph.upsert_nodes("proj-t", &[
        make_file_node("Service.ts"), make_file_node("service.test.ts"), make_file_node("nested/service.test.ts")
    ]).unwrap();
    c.state.graph.upsert_edges("proj-t", &[edge("file:Service.ts", "file:nested/service.test.ts", EdgeKind::TemporalCoupling, 100)]).unwrap();
    let findings = TestCoverageGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(findings.iter().any(|finding| finding.title.contains("nested/service.test.ts")), "{findings:#?}");
}

#[test]
fn sync_contract_does_not_count_nested_current_path_or_symbol_as_edited() {
    for (sites, root, nested) in [
        ("src/Rules.vb||nested/src/Rules.vb", "src/Rules.vb", "nested/src/Rules.vb"),
        ("Inside||Outside", "src/Rules.vb", "nested/src/Rules.vb"),
        ("Rules.vb||nested/Rules.vb", "Rules.vb", "nested/Rules.vb"),
    ] {
        let c = ctx_from_diff(&identity_diff(root));
        let mut contract = make_fn_node("contract", "Keep in sync", root);
        contract.node_type = "sync_contract".into();
        contract.metadata = Some(serde_json::json!({"sites": sites}));
        c.state.graph.upsert_nodes("proj-t", &[
            contract, make_file_node(root), make_file_node(nested),
            make_fn_node("inside", "Inside", root),
            make_fn_node("outside", "Outside", nested),
        ]).unwrap();
        let findings = SyncContractGate.run(&as_gate_ctx(&c)).unwrap();
        assert!(findings.iter().any(|finding| finding.title.contains("1/2")), "{sites}: {findings:#?}");
    }
}

#[test]
fn unwired_gate_does_not_borrow_callers_from_nested_file_in_another_class() {
    let diff = "diff --git a/Rules.vb b/Rules.vb\n--- a/Rules.vb\n+++ b/Rules.vb\n@@ -1,2 +1,4 @@\n Public Class Current\n+ Public Sub Save()\n+ End Sub\n End Class\n";
    let c = ctx_from_diff(diff);
    c.state.graph.upsert_nodes("proj-t", &[
        make_fn_node("current", "Current.Save", "Rules.vb"),
        make_fn_node("other", "Other.Save", "nested/Rules.vb"),
        make_fn_node("caller", "CallOther", "Caller.vb"),
    ]).unwrap();
    c.state.graph.upsert_edges("proj-t", &[edge("caller", "other", EdgeKind::Calls, 1)]).unwrap();
    let findings = UnwiredGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(findings.iter().any(|finding| finding.gate == "unwired"), "{findings:#?}");
}

#[test]
fn unwired_gate_binds_known_class_and_existing_class_to_their_own_declaration() {
    let existing = "diff --git a/Rules.vb b/Rules.vb\n--- a/Rules.vb\n+++ b/Rules.vb\n@@ -1,2 +1,4 @@\n Public Class Current\n+ Public Sub Save()\n+ End Sub\n End Class\n";
    let added = "diff --git a/Rules.vb b/Rules.vb\nnew file mode 100644\n--- /dev/null\n+++ b/Rules.vb\n@@ -0,0 +1,4 @@\n+Public Class Current\n+ Public Sub Save()\n+ End Sub\n+End Class\n";
    for diff in [existing, added] {
        let c = ctx_from_diff(diff);
        let mut other = make_fn_node("other", "Other.Save", "Rules.vb");
        other.start_line = 30;
        other.end_line = 40;
        c.state.graph.upsert_nodes("proj-t", &[
            make_fn_node("current", "Current.Save", "Rules.vb"), other,
            make_fn_node("nested", "Current.Save", "nested/Rules.vb"),
            make_fn_node("caller", "CallOther", "Caller.vb"),
        ]).unwrap();
        c.state.graph.upsert_edges("proj-t", &[
            edge("caller", "other", EdgeKind::Calls, 1),
            edge("caller", "nested", EdgeKind::Calls, 1),
        ]).unwrap();
        let ctx = as_gate_ctx(&c);
        let findings = UnwiredGate.run(&ctx).unwrap();
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert!(ctx.take_degraded().is_empty());
        assert!(findings[0].detail.contains("does not establish"));
        c.state.graph.upsert_edges("proj-t", &[edge("caller", "current", EdgeKind::Calls, 1)]).unwrap();
        assert!(UnwiredGate.run(&ctx).unwrap().is_empty(), "the bound declaration's caller must count");
        assert!(ctx.take_degraded().is_empty());
    }
}

#[test]
fn unwired_gate_degrades_missing_or_ambiguous_declaration_identity() {
    let diff = "diff --git a/Rules.vb b/Rules.vb\n--- a/Rules.vb\n+++ b/Rules.vb\n@@ -1,2 +1,4 @@\n Public Class Current\n+ Public Sub Save()\n+ End Sub\n End Class\n";
    for ambiguous in [false, true] {
        let c = ctx_from_diff(diff);
        c.state.graph.upsert_nodes("proj-t", &[
            make_fn_node("nested", "Other.Save", "nested/Rules.vb"),
            make_fn_node("caller", "CallOther", "Caller.vb"),
        ]).unwrap();
        if ambiguous {
            c.state.graph.upsert_nodes("proj-t", &[
                make_fn_node("one", "Current.Save", "Rules.vb"),
                make_fn_node("two", "Other.Save", "Rules.vb"),
            ]).unwrap();
        }
        c.state.graph.upsert_edges("proj-t", &[edge("caller", "nested", EdgeKind::Calls, 1)]).unwrap();
        let ctx = as_gate_ctx(&c);
        assert!(UnwiredGate.run(&ctx).unwrap().is_empty());
        assert!(ctx.take_degraded().iter().any(|note| note.contains("caller identity unknown")));
    }
}

#[tokio::test]
async fn co_added_family_keeps_distinct_nested_companion_missing_from_diff() {
    let diff = "diff --git a/src/family/New.vb b/src/family/New.vb\nnew file mode 100644\n--- /dev/null\n+++ b/src/family/New.vb\n@@ -0,0 +1,1 @@\n+Public Class NewItem\n".to_string() + &identity_diff("Companion.md");
    let c = ctx_from_diff(&diff);
    c.state.registry.put_project(&engram_core::ProjectRecord {
        project_id:c.project_id.clone(), project_name:"gate identity".into(),
        directory:c.project_dir.to_string_lossy().into_owned(), project_type:"general".into(),
        created_at_ms:0, updated_at_ms:0, reindex_required_since_ms:None,
    }).unwrap();
    c.state.registry.set_meta(&c.project_id, "active_generation", "1").unwrap();
    c.state.graph.upsert_nodes("proj-t", &[
        make_file_node("Companion.md"), make_file_node("nested/Companion.md"),
        make_file_node("src/family/New.vb"),
    ]).unwrap();
    let runtime = engram_server::services::project_service::ensure_project_runtime(&c.state, &c.project_id).await.unwrap();
    let docs: Vec<_> = (1..=3).map(|index| {
        let path = format!("pr:{index}");
        let content = format!("# PR-{index}: Add src family item\n## Files shipped together\n- src/family/Item{index}.vb\n- nested/Companion.md\n");
        let hash = engram_core::ContentHash::compute(content.as_bytes());
        engram_index::IndexDoc {
            generation:0, chunk_id:index, path:RelPath::new(&path), language:"markdown".into(),
            content, namespace:"history".into(), author:None, timestamp:None, start_line:0, end_line:0,
            doc_id:engram_core::DocIdStr::compute(&path,0,0,&hash).0, content_hash:hash.0,
        }
    }).collect();
    runtime.search.index_docs(&c.project_id, &docs, &tokio_util::sync::CancellationToken::new()).await.unwrap();
    let findings = CoAddedFamilyGate.run_async(&as_gate_ctx(&c)).await.unwrap();
    assert!(findings.iter().any(|finding| finding.gate == "co_added_family" && finding.detail.contains("nested/Companion.md")), "{findings:#?}");
}

// ── Gate 1: Immune ───────────────────────────────────────────────────────────

const IMMUNE_HASH: &str = "f7766bb1a1006ffd36432be2ae4fdb89b5291012";

#[test]
fn immune_gate_emits_critical_when_destructive_on_immune_file() {
    let diff = "\
diff --git a/Site/App_Code/dal/fiberjobb.vb b/Site/App_Code/dal/fiberjobb.vb
--- a/Site/App_Code/dal/fiberjobb.vb
+++ b/Site/App_Code/dal/fiberjobb.vb
@@ -10,5 +10,6 @@
 Public Sub WipeJobs()
     Dim db As New iFaltDataContext()
+    db.Fiberjobb.DeleteAllOnSubmit(db.Fiberjobb)
     db.SubmitChanges()
 End Sub
";
    let c = ctx_from_diff(diff);
    c.state
        .registry
        .put_repo_rule(
            "proj-t",
            &RepoRule {
                rule_id: format!("immune_{IMMUNE_HASH}"),
                file_pattern: "Site/App_Code/dal/fiberjobb.vb".into(),
                rule_text: "Previous revert removed unscoped DeleteAllOnSubmit.".into(),
                priority: 1,
                updated_at_ms: 1,
            },
        )
        .unwrap();
    let findings = ImmuneGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.severity == Severity::Critical && f.gate == "immune"),
        "expected CRITICAL immune finding, got {findings:#?}"
    );
    let critical = findings
        .iter()
        .find(|f| f.severity == Severity::Critical)
        .unwrap();
    assert!(
        critical
            .evidence
            .iter()
            .any(|e| e.contains("destructive_patterns")),
        "evidence must cite destructive patterns"
    );
    assert!(
        critical
            .evidence
            .iter()
            .any(|e| e.contains(&IMMUNE_HASH[..8])),
        "evidence must include the revert hash"
    );
}

#[test]
fn immune_gate_emits_warning_on_modification_without_destructive_pattern() {
    let diff = "\
diff --git a/Site/App_Code/dal/fiberjobb.vb b/Site/App_Code/dal/fiberjobb.vb
--- a/Site/App_Code/dal/fiberjobb.vb
+++ b/Site/App_Code/dal/fiberjobb.vb
@@ -1,1 +1,2 @@
 Module Foo
+    Public Sub Inspect() End Sub
";
    let c = ctx_from_diff(diff);
    c.state
        .registry
        .put_repo_rule(
            "proj-t",
            &RepoRule {
                rule_id: format!("immune_{IMMUNE_HASH}"),
                file_pattern: "Site/App_Code/dal/fiberjobb.vb".into(),
                rule_text: "Previous revert context.".into(),
                priority: 1,
                updated_at_ms: 1,
            },
        )
        .unwrap();
    let findings = ImmuneGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.severity == Severity::Warning && f.gate == "immune"),
        "expected WARNING, got {findings:#?}"
    );
}

// ── Gate 2: Blast Radius ─────────────────────────────────────────────────────

#[test]
fn blast_radius_gate_emits_finding_for_high_incoming_file() {
    let diff = "\
diff --git a/src/hub.rs b/src/hub.rs
--- a/src/hub.rs
+++ b/src/hub.rs
@@ -1,1 +1,2 @@
 fn hub() {}
+fn new_fn() {}
";
    let c = ctx_from_diff(diff);
    c.state
        .graph
        .upsert_nodes("proj-t", &[make_file_node("src/hub.rs")])
        .unwrap();
    let mut edges = Vec::new();
    for i in 0..50 {
        let caller_path = format!("src/caller{i}.rs");
        c.state
            .graph
            .upsert_nodes("proj-t", &[make_file_node(&caller_path)])
            .unwrap();
        // Confirmed-confidence calls: the score discounts each causal source
        // by its best edge confidence (unknown → 0.5), so a fixture without
        // confidence metadata models 50 UNCERTAIN callers (~25 expected),
        // which legitimately scores low. This test's claim is about 50
        // CONFIRMED callers.
        let mut e = edge(
            &format!("file:{caller_path}"),
            "file:src/hub.rs",
            EdgeKind::Calls,
            1,
        );
        e.metadata = Some(serde_json::json!({"confidence": "1.0"}));
        edges.push(e);
    }
    c.state.graph.upsert_edges("proj-t", &edges).unwrap();

    let findings = BlastRadiusGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings.iter().any(|f| f.gate == "blast_radius"),
        "expected blast-radius finding on 50-incoming-edge file, got {findings:#?}"
    );
}

// ── Gate 4: Temporal ─────────────────────────────────────────────────────────

#[test]
fn temporal_gate_flags_coupled_partner_not_in_diff() {
    let diff = "\
diff --git a/a.ts b/a.ts
--- a/a.ts
+++ b/a.ts
@@ -1,1 +1,2 @@
 const x = 1;
+const y = 2;
";
    let mut c = ctx_from_diff(diff);
    c.total_commits = 6_729;
    c.state
        .graph
        .upsert_nodes("proj-t", &[make_file_node("a.ts"), make_file_node("b.ts")])
        .unwrap();
    c.state
        .graph
        .upsert_edges(
            "proj-t",
            &[edge(
                "file:a.ts",
                "file:b.ts",
                EdgeKind::TemporalCoupling,
                877,
            )],
        )
        .unwrap();
    let findings = TemporalGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings.iter().any(|f| f.gate == "temporal"
            && f.title.contains("b.ts")
            && f.severity == Severity::Info),
        "expected historical co-change INFO citing b.ts, got {findings:#?}"
    );
}

#[test]
fn temporal_weights_are_leads_even_with_unknown_or_smaller_project_counts() {
    let diff = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-old\n+new\n";
    for total in [0, 100, 6729] {
        let mut c = ctx_from_diff(diff);
        c.total_commits = total;
        c.state.graph.upsert_nodes("proj-t", &[make_file_node("a.ts"), make_file_node("b.ts")]).unwrap();
        c.state.graph.upsert_edges("proj-t", &[edge("file:a.ts", "file:b.ts", EdgeKind::TemporalCoupling, 877)]).unwrap();
        let findings = TemporalGate.run(&as_gate_ctx(&c)).unwrap();
        let f = findings.iter().find(|f| f.gate == "temporal" && f.title.contains("b.ts")).unwrap();
        assert_eq!(f.severity, Severity::Info);
        assert!(f.evidence.contains(&"coupling_weight = 877".to_string()));
        assert!(f.evidence.contains(&format!("total_commits = {total}")));
        assert!(f.evidence.contains(&"conditional_frequency = unknown".to_string()));
        assert!(!f.detail.contains('%') && !f.evidence.iter().any(|e| e.starts_with("co_change_pct")));
        assert!(f.suggestion.contains("only if") && f.suggestion.contains("does not require"));
        assert_eq!(f.next_tool.as_deref(), Some("analyze_temporal_couplings(project_id=\"proj-t\", file_path=\"a.ts\")"));
    }
}

#[test]
fn temporal_gate_threshold_scales_with_small_project() {
    let diff = "\
diff --git a/a.ts b/a.ts
--- a/a.ts
+++ b/a.ts
@@ -1,1 +1,2 @@
 const x = 1;
+const y = 2;
";
    let mut c = ctx_from_diff(diff);
    c.total_commits = 200;
    c.state
        .graph
        .upsert_nodes("proj-t", &[make_file_node("a.ts"), make_file_node("b.ts")])
        .unwrap();
    c.state
        .graph
        .upsert_edges(
            "proj-t",
            &[edge(
                "file:a.ts",
                "file:b.ts",
                EdgeKind::TemporalCoupling,
                10,
            )],
        )
        .unwrap();
    let findings = TemporalGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings.iter().any(|f| f.gate == "temporal"),
        "auto-tuned threshold must fire on small-project coupling, got {findings:#?}"
    );
}

// ── Gate 5: State ────────────────────────────────────────────────────────────

#[test]
fn state_gate_reports_other_readers_and_writers() {
    let diff = "\
diff --git a/Site/Page.aspx.vb b/Site/Page.aspx.vb
--- a/Site/Page.aspx.vb
+++ b/Site/Page.aspx.vb
@@ -1,1 +1,2 @@
 Module P
+    Session(\"userId\") = 42
";
    let c = ctx_from_diff(diff);
    let state_node = "state:Session:userId";
    c.state
        .graph
        .upsert_nodes(
            "proj-t",
            &[
                make_fn_node(state_node, "Session_userId", "state"),
                make_fn_node("fn:Reader1", "Reader1", "Site/OtherReader.aspx.vb"),
                make_fn_node("fn:Reader2", "Reader2", "Site/AnotherReader.aspx.vb"),
            ],
        )
        .unwrap();
    c.state
        .graph
        .upsert_edges(
            "proj-t",
            &[
                edge("fn:Reader1", state_node, EdgeKind::ReadsState, 1),
                edge("fn:Reader2", state_node, EdgeKind::ReadsState, 1),
            ],
        )
        .unwrap();
    let findings = StateGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.gate == "state" && f.title.contains("userId")),
        "expected state-key finding for userId, got {findings:#?}"
    );
}

// ── Gate 6: Audit ────────────────────────────────────────────────────────────

#[test]
fn audit_gate_fires_when_mutation_has_no_audit_call() {
    let diff = "\
diff --git a/Site/App_Code/x.vb b/Site/App_Code/x.vb
--- a/Site/App_Code/x.vb
+++ b/Site/App_Code/x.vb
@@ -1,1 +1,2 @@
 Module X
+    db.SubmitChanges()
";
    let c = ctx_from_diff(diff);
    c.state
        .graph
        .upsert_nodes(
            "proj-t",
            &[make_fn_node(
                "fn:handelselogg.Create",
                "handelselogg.Create",
                "Site/Audit.vb",
            )],
        )
        .unwrap();
    let findings = AuditGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings.iter().any(|f| f.gate == "audit"),
        "expected audit finding, got {findings:#?}"
    );
}

#[test]
fn audit_gate_skips_silently_when_no_convention() {
    let diff = "\
diff --git a/Site/App_Code/x.vb b/Site/App_Code/x.vb
--- a/Site/App_Code/x.vb
+++ b/Site/App_Code/x.vb
@@ -1,1 +1,2 @@
 Module X
+    db.SubmitChanges()
";
    let c = ctx_from_diff(diff);
    let findings = AuditGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings.is_empty(),
        "audit gate must stay silent when the project has no audit convention, got {findings:#?}"
    );
}

// ── Gate 8: New file ─────────────────────────────────────────────────────────

#[test]
fn new_file_gate_flags_extension_mismatch() {
    let diff = "\
diff --git a/Site/App_Code/permits/helpers.cs b/Site/App_Code/permits/helpers.cs
--- /dev/null
+++ b/Site/App_Code/permits/helpers.cs
@@ -0,0 +1,2 @@
+public class Helpers {}
+public class Other {}
";
    let c = ctx_from_diff(diff);
    let nodes: Vec<Node> = [
        "perm_create.vb",
        "perm_delete.vb",
        "perm_update.vb",
        "perm_search.vb",
    ]
    .iter()
    .map(|n| make_file_node(&format!("Site/App_Code/permits/{n}")))
    .collect();
    c.state.graph.upsert_nodes("proj-t", &nodes).unwrap();

    let findings = NewFileGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.gate == "new_file" && f.title.contains("extension")),
        "expected extension-mismatch finding, got {findings:#?}"
    );
}

// ── Gate 9: Test coverage ────────────────────────────────────────────────────

#[test]
fn test_coverage_gate_warns_when_coupled_test_missing() {
    let diff = "\
diff --git a/src/service.ts b/src/service.ts
--- a/src/service.ts
+++ b/src/service.ts
@@ -1,1 +1,6 @@
 export class Svc {
+    public foo() {}
+    public bar() {}
+    public baz() {}
+    public qux() {}
+    public zim() {}
 }
";
    let c = ctx_from_diff(diff);
    c.state
        .graph
        .upsert_nodes(
            "proj-t",
            &[
                make_file_node("src/service.ts"),
                make_file_node("src/service.test.ts"),
            ],
        )
        .unwrap();
    c.state
        .graph
        .upsert_edges(
            "proj-t",
            &[edge(
                "file:src/service.ts",
                "file:src/service.test.ts",
                EdgeKind::TemporalCoupling,
                150,
            )],
        )
        .unwrap();
    let findings = TestCoverageGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.gate == "test_coverage" && f.title.contains("service.test.ts")),
        "expected test-coverage WARNING citing service.test.ts, got {findings:#?}"
    );
}

// ── Gate 10: Secret leakage ──────────────────────────────────────────────────

#[test]
fn secret_gate_emits_critical_for_hardcoded_aws_key() {
    let diff = "\
diff --git a/src/config.ts b/src/config.ts
--- a/src/config.ts
+++ b/src/config.ts
@@ -1,1 +1,2 @@
 const cfg = {};
+const key = \"AKIAIOSFODNN7EXAMPLE\";
";
    let c = ctx_from_diff(diff);
    let findings = SecretLeakageGate.run(&as_gate_ctx(&c)).unwrap();
    let crit = findings
        .iter()
        .find(|f| f.severity == Severity::Critical)
        .expect("expected CRITICAL secret finding");
    // Verify the raw secret does NOT leak into the finding text.
    let full = format!(
        "{}{}{}{}",
        crit.title,
        crit.detail,
        crit.suggestion,
        crit.evidence.join("|")
    );
    assert!(
        !full.contains("AKIAIOSFODNN7EXAMPLE"),
        "secret value leaked into finding output: {full}"
    );
}

#[test]
fn secret_gate_skips_fixtures_dir() {
    let diff = "\
diff --git a/tests/fixtures/fake.env b/tests/fixtures/fake.env
--- /dev/null
+++ b/tests/fixtures/fake.env
@@ -0,0 +1,1 @@
+AKIAIOSFODNN7EXAMPLE
";
    let c = ctx_from_diff(diff);
    let findings = SecretLeakageGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings.is_empty(),
        "secret gate must skip test fixtures, got {findings:#?}"
    );
}

// ── Aggregation + corroboration ──────────────────────────────────────────────

#[test]
fn aggregate_dedups_and_sorts_and_corroborates() {
    let warn_a = ReviewFinding::new(Severity::Warning, "immune", "a.rs", "x", "", "fix");
    let info_b = ReviewFinding::new(Severity::Info, "blast_radius", "a.rs", "y", "", "fix");
    let style_c = ReviewFinding::new(Severity::Style, "style", "a.rs", "z", "", "fix");
    let out = aggregate_findings(
        vec![warn_a.clone(), info_b.clone(), style_c.clone(), warn_a],
        &[],
        Severity::Style,
        100,
    );
    let corr = out.iter().filter(|f| f.gate == "corroboration").count();
    assert_eq!(
        corr, 1,
        "expected one corroboration meta-finding, got {out:#?}"
    );
    // Critical / Warning come first in severity order.
    assert!(
        out.first().map(|f| f.severity).unwrap_or(Severity::Style) <= Severity::Warning,
        "sort order: strongest findings first: {out:#?}"
    );
}

// ── all_gates() registry ─────────────────────────────────────────────────────

#[test]
fn all_gates_returns_expected_roster_in_order() {
    let names: Vec<&str> = all_gates().iter().map(|g| g.name()).collect();
    assert_eq!(
        names,
        vec![
            "immune",
            // external audit 2026-08-29 row 8: enforced repo rules run right after the
            // immune check, before the advisory gates
            "repo_rules",
            "blast_radius",
            "style",
            "temporal",
            "state",
            "audit",
            "antipattern",
            "new_file",
            "test_coverage",
            "secret_leakage",
            "guard_parity",
            "unwired",
            "product_intent",
            "sync_contract",
            "co_added_family",
            "complexity_budget",
            "added_conventions",
            "ui_house_style",
        ]
    );
}

// ── parse_unified_diff (multi-file) ─────────────────────────────────────────

#[test]
fn parse_multi_file_diff_returns_one_entry_per_file() {
    let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,1 +1,2 @@
 fn a() {}
+fn aa() {}
diff --git a/b.rs b/b.rs
--- /dev/null
+++ b/b.rs
@@ -0,0 +1,1 @@
+fn b() {}
diff --git a/c.rs b/c.rs
--- a/c.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-fn c() {}
";
    let files = parse_unified_diff(diff);
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].path, "a.rs");
    assert!(matches!(files[0].change_type, ChangeType::Modified));
    assert_eq!(files[1].path, "b.rs");
    assert!(matches!(files[1].change_type, ChangeType::Added));
    assert_eq!(files[2].path, "c.rs");
    assert!(matches!(files[2].change_type, ChangeType::Deleted));
}

// ── End-to-end: no-op diff ──────────────────────────────────────────────────

#[test]
fn empty_diff_produces_no_findings() {
    let c = ctx_from_diff("");
    let ctx = as_gate_ctx(&c);
    for gate in all_gates() {
        let res = gate.run(&ctx).unwrap();
        assert!(
            res.is_empty(),
            "gate {} emitted findings on empty diff: {res:#?}",
            gate.name()
        );
    }
}

// ─── Gate 11: guard_parity ──────────────────────────────────────────────────

#[test]
fn guard_parity_flags_unguarded_new_endpoint_when_siblings_are_guarded() {
    let diff = "diff --git a/UserApi.asmx.cs b/UserApi.asmx.cs\n\
--- a/UserApi.asmx.cs\n\
+++ b/UserApi.asmx.cs\n\
@@ -20,0 +21,4 @@\n\
+    [WebMethod]\n\
+    public void AddUser(string name) {\n\
+        InsertUser(name);\n\
+    }\n";
    let c = ctx_from_diff(diff);
    std::fs::create_dir_all(&c.project_dir).unwrap();
    std::fs::write(
        c.project_dir.join("UserApi.asmx.cs"),
        "public class UserApi {\n\
         [WebMethod]\n\
         public void DeleteUser(int id) {\n\
             if (!User.IsInRole(\"Admin\")) { return; }\n\
             RemoveUser(id);\n\
         }\n\
         }\n",
    )
    .unwrap();

    let findings = GuardParityGate.run(&as_gate_ctx(&c)).unwrap();
    assert_eq!(
        findings.len(),
        1,
        "expected exactly one guard-parity finding"
    );
    let f = &findings[0];
    assert_eq!(f.gate, "guard_parity");
    assert!(
        f.evidence.iter().any(|e| e.contains("isinrole")),
        "evidence must name the sibling guard; got {:?}",
        f.evidence
    );
    assert!(
        f.next_tool
            .as_deref()
            .unwrap_or("")
            .contains("map_guards_and_settings"),
        "next_tool must point at map_guards_and_settings"
    );
}

#[test]
fn guard_parity_silent_when_new_endpoint_is_guarded() {
    let diff = "diff --git a/UserApi.asmx.cs b/UserApi.asmx.cs\n\
--- a/UserApi.asmx.cs\n\
+++ b/UserApi.asmx.cs\n\
@@ -20,0 +21,5 @@\n\
+    [WebMethod]\n\
+    public void AddUser(string name) {\n\
+        if (!User.IsInRole(\"Admin\")) { return; }\n\
+        InsertUser(name);\n\
+    }\n";
    let c = ctx_from_diff(diff);
    std::fs::create_dir_all(&c.project_dir).unwrap();
    std::fs::write(
        c.project_dir.join("UserApi.asmx.cs"),
        "if (!User.IsInRole(\"Admin\")) { }",
    )
    .unwrap();
    let findings = GuardParityGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(findings.is_empty(), "guarded addition must not be flagged");
}

#[test]
fn guard_parity_silent_when_file_has_no_guard_convention() {
    let diff = "diff --git a/Helpers.cs b/Helpers.cs\n\
--- a/Helpers.cs\n\
+++ b/Helpers.cs\n\
@@ -5,0 +6,3 @@\n\
+    protected void btnExport_Click(object s, EventArgs e) {\n\
+        Export();\n\
+    }\n";
    let c = ctx_from_diff(diff);
    std::fs::create_dir_all(&c.project_dir).unwrap();
    std::fs::write(c.project_dir.join("Helpers.cs"), "public class Helpers { }").unwrap();
    let findings = GuardParityGate.run(&as_gate_ctx(&c)).unwrap();
    assert!(
        findings.is_empty(),
        "no sibling guards = no parity judgment from this gate"
    );
}
