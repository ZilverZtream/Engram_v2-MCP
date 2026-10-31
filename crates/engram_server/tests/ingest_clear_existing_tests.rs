#![allow(clippy::unwrap_used)]
//! Row-3 audit A7 (live finding 2026-08-29): `ingest_quality_gates` documents
//! `clear_existing` as "reserved … currently a no-op", so a corpus ingested
//! with a broken parser could never be replaced — the quiet-failure class
//! "schema param no handler reads". `clear_existing=true` must purge the
//! project's `quality_gate` namespace before ingesting, and say so.

use engram_core::config::Config;
use engram_server::models::{IngestQualityGatesRequest, PrePushAuditRequest};
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

async fn build() -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site")).unwrap();
    std::fs::write(root.join("Site/a.vb"), "Public Class a\nEnd Class\n").unwrap();
    std::fs::write(
        root.join("rules_v1.txt"),
        "- Every endpoint that reads pr_id must call check_pr_id before touching project data\n- Never build SQL by string concatenation; always parameterize\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rules_v2.txt"),
        "- Always return IQueryable from data-access helpers and call ToList only at the boundary\n",
    )
    .unwrap();
    let cfg = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        max_project_files: Some(20),
        max_project_bytes: Some(512 * 1024),
        embedding_backend: "fts_only".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let (state, _rx) = AppState::new(cfg).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(engram_server::IndexProjectRequest {
            directory: root.to_string_lossy().to_string(),
            project_name: "IngestFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    (tmp, state, engram, pid)
}

async fn ingest(engram: &Engram, pid: &str, path: &str, clear: bool) -> String {
    let req: IngestQualityGatesRequest = serde_json::from_value(json!({
        "project_id": pid, "source_path": path, "source_type": "text", "clear_existing": clear
    }))
    .unwrap();
    let res = engram.handle_ingest_quality_gates(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

fn qg_count(state: &AppState, pid: &str) -> usize {
    let engine = state.get_project_cached(pid).unwrap().search;
    engine
        .count_docs_by_namespace(pid)
        .unwrap()
        .get("quality_gate")
        .copied()
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_existing_replaces_the_namespace_instead_of_adding_to_it() {
    let (_tmp, state, engram, pid) = build().await;
    let first = ingest(&engram, &pid, "rules_v1.txt", false).await;
    assert!(first.contains("Ingested 2"), "{first}");
    assert_eq!(qg_count(&state, &pid), 2);

    let second = ingest(&engram, &pid, "rules_v2.txt", true).await;

    assert_eq!(
        qg_count(&state, &pid),
        1,
        "clear_existing=true must purge the old rules before ingesting: {second}"
    );
    assert!(
        second.contains("purged") || second.contains("cleared"),
        "the purge must be stated in the output: {second}"
    );
    let req: PrePushAuditRequest = serde_json::from_value(json!({
        "project_id": pid,
        "code": "Dim sql = \"SELECT * FROM projekt WHERE id = \" & pr_id",
        "file_path": "Site/a.vb",
        "top_k": 10
    }))
    .unwrap();
    let audit = engram.handle_pre_push_audit(req).await.unwrap();
    let out = audit.content[0].as_text().unwrap().text.clone();
    assert!(
        !out.contains("string concatenation"),
        "a purged rule must not be retrievable any more:\n{out}"
    );
    // The tally reflects the post-purge count in either header form:
    // "retrieved of 1 in the namespace" (a hit) or "1 rule(s) exist in the
    // … namespace and were searched" (no lexical hit for this probe).
    assert!(
        out.contains("of 1 in the namespace") || out.contains("1 rule(s) exist in the"),
        "the tally must reflect the purged namespace (1 rule):\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_clear_existing_ingest_still_accumulates() {
    let (_tmp, state, engram, pid) = build().await;
    ingest(&engram, &pid, "rules_v1.txt", false).await;
    ingest(&engram, &pid, "rules_v2.txt", false).await;
    assert_eq!(qg_count(&state, &pid), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checked_markdown_rejection_never_clears_existing_rules() {
    let (tmp, state, engram, pid) = build().await;
    ingest(&engram, &pid, "rules_v1.txt", false).await;
    assert_eq!(qg_count(&state, &pid), 2);
    for rejected in ["x".repeat(16385), "- Always inspect.\n".repeat(129)] {
        std::fs::write(tmp.path().join("proj/rejected.md"), rejected).unwrap();
        let req = serde_json::from_value(json!({
            "project_id": pid, "source_path": "rejected.md", "source_type": "rules", "clear_existing": true
        })).unwrap();
        let error = engram.handle_ingest_quality_gates(req).await.unwrap_err();
        assert!(error.message.contains("No rules imported"), "{error:?}");
        assert_eq!(qg_count(&state, &pid), 2);
        let distill = serde_json::from_value(json!({
            "project_id": pid, "source_path": "rejected.md", "source_type": "rules"
        })).unwrap();
        let error = engram.handle_distill_quality_gates(distill).await.unwrap_err();
        assert!(error.message.contains("No rules imported"), "{error:?}");
        assert_eq!(qg_count(&state, &pid), 2);
        let req = serde_json::from_value(json!({
            "project_id": pid, "code": "Never build SQL by string concatenation", "file_path": "Site/a.vb", "top_k": 10
        })).unwrap();
        let audit = engram.handle_pre_push_audit(req).await.unwrap();
        let text = &audit.content[0].as_text().unwrap().text;
        assert!(text.contains("string concatenation") && text.contains("of 2 in the namespace"), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contextual_markdown_remains_retrievable_without_automatic_enforcement() {
    let (tmp, state, engram, pid) = build().await;
    let policy = "Always document gateway APIs.\n\nException: internal prototypes can omit documentation.\n";
    std::fs::write(tmp.path().join("proj/context.md"), policy).unwrap();
    let req = serde_json::from_value(json!({
        "project_id": pid, "source_path": "context.md", "source_type": "rules"
    })).unwrap();
    let result = engram.handle_ingest_quality_gates(req).await.unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("1 context-required Markdown candidate") && text.contains("not auto-promoted"), "{text}");
    assert_eq!(qg_count(&state, &pid), 1);
    assert!(state.registry.list_repo_rules(&pid).unwrap().is_empty());
    let req = serde_json::from_value(json!({
        "project_id": pid, "code": "Gateway APIs", "top_k": 10
    })).unwrap();
    let result = engram.handle_pre_push_audit(req).await.unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(text.contains("full_rule: get_chunk") && text.contains("of 1 in the namespace"), "{text}");
}

const SCOPED_MANDATE: &str =
    "Never add ScopeProbe tokens to production code. [check: forbid=ScopeProbe]";

async fn ingest_json(engram: &Engram, pid: &str, path: &str, clear: bool) {
    engram.handle_ingest_quality_gates(serde_json::from_value(json!({
        "project_id":pid,"source_path":path,"source_type":"rules","clear_existing":clear
    })).unwrap()).await.unwrap();
}

async fn scope_probe_is_enforced(state: &AppState, pid: &str, root: &std::path::Path, path: &str) -> bool {
    use engram_server::services::pre_commit_review_service::{all_gates, run_pre_commit_review_with, ReviewConfig};
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "Public Class Widget\nPublic Sub Save()\nEnd Sub\nEnd Class\n").unwrap();
    let diff = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,4 +1,5 @@\n Public Class Widget\n Public Sub Save()\n+Dim ScopeProbe As Integer = 1\n End Sub\n End Class\n");
    let (findings, _, _, _) = run_pre_commit_review_with(
        state, pid, root, 1, &diff, &ReviewConfig::default(), all_gates(),
    ).await.unwrap();
    findings.iter().any(|finding| finding.gate == "repo_rules")
}

#[tokio::test]
async fn authored_json_scope_limits_enforcement_to_the_requested_area() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    std::fs::write(root.join("area-rules.json"), json!([
        {"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**"}
    ]).to_string()).unwrap();
    ingest_json(&engram, &pid, "area-rules.json", false).await;
    assert!(scope_probe_is_enforced(&state, &pid, &root, "Site/maps/Widget.vb").await);
    assert!(!scope_probe_is_enforced(&state, &pid, &root, "Site/billing/Widget.vb").await,
        "an authored map rule must not become a repository-wide mandate");
}

#[tokio::test]
async fn identical_mandates_in_distinct_scopes_both_remain_effective() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    for (source, field, scope) in [
        ("maps.json", "path_scope", "Site/maps/**"),
        ("billing.json", "file_pattern", "Site/billing/**"),
    ] {
        let mut rule = json!({"rule":SCOPED_MANDATE});
        rule[field] = json!(scope);
        std::fs::write(root.join(source), json!([rule]).to_string()).unwrap();
        ingest_json(&engram, &pid, source, false).await;
    }
    for path in ["Site/maps/Widget.vb", "Site/billing/Widget.vb"] {
        assert!(scope_probe_is_enforced(&state, &pid, &root, path).await, "{path}");
    }
    assert!(!scope_probe_is_enforced(&state, &pid, &root, "Site/reports/Widget.vb").await);
}

#[tokio::test]
async fn narrowing_a_reimported_rule_retires_its_previous_global_enforcement() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    let source = root.join("reviewed.json");
    std::fs::write(&source, json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
    ingest_json(&engram, &pid, "reviewed.json", false).await;
    assert!(scope_probe_is_enforced(&state, &pid, &root, "Site/billing/Widget.vb").await);
    std::fs::write(&source, json!([
        {"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**"}
    ]).to_string()).unwrap();
    ingest_json(&engram, &pid, "reviewed.json", true).await;
    assert!(scope_probe_is_enforced(&state, &pid, &root, "Site/maps/Widget.vb").await);
    assert!(!scope_probe_is_enforced(&state, &pid, &root, "Site/billing/Widget.vb").await);
    assert_eq!(qg_count(&state, &pid), 1);
}

#[tokio::test]
async fn invalid_json_scope_never_clears_or_broadens_existing_rules() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    std::fs::write(root.join("reviewed.json"), json!([
        {"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**"}
    ]).to_string()).unwrap();
    ingest_json(&engram, &pid, "reviewed.json", false).await;
    let previous = serde_json::to_value(state.registry.list_repo_rules(&pid).unwrap()).unwrap();
    let mut invalid = Vec::new();
    for scope in [json!(null), json!(""), json!([]), json!({"include":"Site/maps/**"}),
        json!("../**"), json!("Site/{maps,billing}/**"), json!("!Site/maps/**")] {
        invalid.push(json!([{"rule":SCOPED_MANDATE,"path_scope":scope}]).to_string());
    }
    invalid.push(json!([{"rule":SCOPED_MANDATE,"applyTo":"Site/maps/**"}]).to_string());
    invalid.push(json!([{"rule":SCOPED_MANDATE,"pathScope":"Site/maps/**"}]).to_string());
    invalid.push(json!([{"rule":SCOPED_MANDATE,"applies_to":"Site/maps/**"}]).to_string());
    invalid.push(json!([{"rule":SCOPED_MANDATE,"directory":"Site/maps"}]).to_string());
    invalid.push(json!([{"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**","exclude":["Site/maps/generated/**"]}]).to_string());
    invalid.push(json!([{"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**","file_pattern":"**/*"}]).to_string());
    invalid.push(format!("\u{feff}{}", json!([{"rule":SCOPED_MANDATE,"path_scope":null}])));
    invalid.push(json!({"path_scope":"Site/maps/**","rules":[{"rule":SCOPED_MANDATE}]}).to_string());
    invalid.push("[{\"rule\":\"Never silently broaden a rule\",".into());
    // Validate the complete document before importing even its first valid row.
    invalid.push(json!([{"rule":SCOPED_MANDATE}, {"rule":SCOPED_MANDATE,"path_scope":false}]).to_string());
    for content in invalid {
        std::fs::write(root.join("invalid.json"), content).unwrap();
        let request = serde_json::from_value(json!({
            "project_id":pid,"source_path":"invalid.json","source_type":"rules","clear_existing":true
        })).unwrap();
        assert!(engram.handle_ingest_quality_gates(request).await.is_err());
        assert_eq!(qg_count(&state, &pid), 1);
        assert_eq!(serde_json::to_value(state.registry.list_repo_rules(&pid).unwrap()).unwrap(), previous);
    }
}

#[tokio::test]
async fn authored_policy_cannot_be_distilled_into_universal_mandates() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    for (source, kind, content) in [
        ("area.md", "copilot", "---\napplyTo: 'Site/maps/**'\n---\nAlways preserve map state.\nException: read-only previews.\n".into()),
        ("area.json", "rules", json!([{"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**"}]).to_string()),
    ] {
        std::fs::write(root.join(source), content).unwrap();
        let request = serde_json::from_value(json!({
            "project_id":pid,"source_path":source,"source_type":kind
        })).unwrap();
        let error = engram.handle_distill_quality_gates(request).await.unwrap_err();
        assert!(error.message.contains("ingest_quality_gates"), "{error}");
        assert_eq!(qg_count(&state, &pid), 0);
        assert!(state.registry.list_repo_rules(&pid).unwrap().is_empty());
    }
}

#[tokio::test]
async fn same_named_sources_in_two_directories_keep_independent_scope_and_recovery() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    for area in ["maps", "billing"] {
        let source = format!("policies/{area}/rules.json");
        std::fs::create_dir_all(root.join(format!("policies/{area}"))).unwrap();
        std::fs::write(root.join(&source), json!([
            {"rule":SCOPED_MANDATE,"path_scope":format!("Site/{area}/**")}
        ]).to_string()).unwrap();
        ingest_json(&engram, &pid, &source, false).await;
    }
    for path in ["Site/maps/Widget.vb", "Site/billing/Widget.vb"] {
        assert!(scope_probe_is_enforced(&state, &pid, &root, path).await, "{path}");
    }
    assert!(!scope_probe_is_enforced(&state, &pid, &root, "Site/reports/Widget.vb").await);
    let docs = state.get_project_cached(&pid).unwrap().search
        .list_docs_in_namespace(&pid, "quality_gate").unwrap();
    assert_eq!(docs.len(), 2);
    let mut recovered = String::new();
    for doc in docs {
        let response = engram.handle_get_chunk(serde_json::from_value(json!({
            "project_id":pid,"namespace":"quality_gate","doc_id":doc.doc_id
        })).unwrap()).await.unwrap();
        recovered.push_str(&response.content[0].as_text().unwrap().text);
    }
    for source in ["policies/maps/rules.json", "policies/billing/rules.json"] {
        assert!(recovered.contains(source), "full-rule recovery must identify {source}");
    }
}

#[tokio::test]
async fn ambiguous_legacy_source_ownership_is_rejected_before_replacing_the_corpus() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    std::fs::write(root.join("rules.json"), json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
    ingest_json(&engram, &pid, "rules.json", false).await;
    let mut legacy = state.registry.list_repo_rules(&pid).unwrap().remove(0);
    legacy.provenance = Some("quality-gate finding".into());
    state.registry.put_repo_rule(&pid, &legacy).unwrap();
    let before = serde_json::to_value(state.registry.list_repo_rules(&pid).unwrap()).unwrap();
    let engine = state.get_project_cached(&pid).unwrap().search;
    let before_docs = engine.list_docs_in_namespace(&pid, "quality_gate").unwrap()
        .into_iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    std::fs::create_dir_all(root.join("policies")).unwrap();
    std::fs::write(root.join("policies/rules.json"), json!([
        {"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**"}
    ]).to_string()).unwrap();
    let request = serde_json::from_value(json!({
        "project_id":pid,"source_path":"policies/rules.json","source_type":"rules","clear_existing":true
    })).unwrap();
    let error = engram.handle_ingest_quality_gates(request).await.unwrap_err();
    assert!(error.message.contains(&legacy.rule_id) && error.message.contains("No rules imported"), "{error}");
    assert_eq!(serde_json::to_value(state.registry.list_repo_rules(&pid).unwrap()).unwrap(), before);
    assert_eq!(engine.list_docs_in_namespace(&pid, "quality_gate").unwrap()
        .into_iter().map(|doc| doc.doc_id).collect::<Vec<_>>(), before_docs);
}

#[tokio::test]
async fn refreshing_authored_source_retires_removed_rows_and_empty_policy() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    let other = "Always retain OtherProbe protections in production. [check: forbid=OtherProbe]";
    let source = root.join("policy.json");
    std::fs::write(&source, json!([{"rule":other}, {"rule":SCOPED_MANDATE}]).to_string()).unwrap();
    ingest_json(&engram, &pid, "policy.json", false).await;
    assert!(scope_probe_is_enforced(&state, &pid, &root, "Site/maps/Widget.vb").await);
    // Removing row 2 and shifting row 1 must retire the old positional mandate.
    std::fs::write(&source, json!([{"rule":other}]).to_string()).unwrap();
    ingest_json(&engram, &pid, "policy.json", false).await;
    assert!(!scope_probe_is_enforced(&state, &pid, &root, "Site/maps/Widget.vb").await);
    assert_eq!(qg_count(&state, &pid), 1);
    assert_eq!(state.registry.list_repo_rules(&pid).unwrap().len(), 1);
    std::fs::write(&source, "[]").unwrap();
    ingest_json(&engram, &pid, "policy.json", false).await;
    assert_eq!(qg_count(&state, &pid), 0);
    assert!(state.registry.list_repo_rules(&pid).unwrap().is_empty());
}

#[tokio::test]
async fn stale_policy_is_withheld_and_coverage_recovers_after_refresh() {
    use engram_server::services::project_service::inject_repo_rules;
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    let source = root.join("policy.json");
    let original = json!([{"rule":SCOPED_MANDATE}]).to_string();
    std::fs::write(&source, &original).unwrap();
    ingest_json(&engram, &pid, "policy.json", false).await;
    let path = engram_core::RelPath::new("Site/maps/Widget.vb");
    let current = inject_repo_rules(&state, &pid, &path, "source body").await;
    assert!(current.contains("[Repo Constraint]") && !current.contains("INCOMPLETE"), "{current}");
    let doc = state.get_project_cached(&pid).unwrap().search.list_docs_in_namespace(&pid, "quality_gate").unwrap().remove(0);
    std::fs::write(&source, "[]").unwrap();
    assert!(!scope_probe_is_enforced(&state, &pid, &root, path.as_str()).await);
    let stale = inject_repo_rules(&state, &pid, &path, "source body").await;
    assert!(stale.contains("INCOMPLETE policy coverage") && stale.contains("stale:"), "{stale}");
    assert!(!stale.contains("[Repo Constraint]"), "{stale}");
    let recovered = engram.handle_get_chunk(serde_json::from_value(json!({
        "project_id":pid, "namespace":"quality_gate", "doc_id":doc.doc_id
    })).unwrap()).await.unwrap();
    let recovered = &recovered.content[0].as_text().unwrap().text;
    assert!(recovered.contains("stale:") && recovered.contains(SCOPED_MANDATE), "stored evidence stays recoverable: {recovered}");
    std::fs::write(&source, original).unwrap();
    ingest_json(&engram, &pid, "policy.json", false).await;
    assert!(scope_probe_is_enforced(&state, &pid, &root, path.as_str()).await);
    let refreshed = inject_repo_rules(&state, &pid, &path, "source body").await;
    assert!(!refreshed.contains("INCOMPLETE"), "{refreshed}");
}

#[tokio::test]
async fn refreshing_one_source_preserves_another_sources_identical_mandate() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    for source in ["first.json", "second.json"] {
        std::fs::write(root.join(source), json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
        ingest_json(&engram, &pid, source, false).await;
    }
    assert_eq!(state.registry.list_repo_rules(&pid).unwrap().len(), 2);
    std::fs::write(root.join("first.json"), "[]").unwrap();
    ingest_json(&engram, &pid, "first.json", false).await;
    assert_eq!(state.registry.list_repo_rules(&pid).unwrap().len(), 1);
    assert!(scope_probe_is_enforced(&state, &pid, &root, "Site/maps/Widget.vb").await);
    assert_eq!(qg_count(&state, &pid), 1);
}

#[tokio::test]
async fn a_manual_edit_is_never_deleted_by_a_source_refresh() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    std::fs::write(root.join("policy.json"), json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
    ingest_json(&engram, &pid, "policy.json", false).await;
    let mut manual = state.registry.list_repo_rules(&pid).unwrap().remove(0);
    manual.rule_text = "Manual change that must be preserved".into();
    state.registry.put_repo_rule(&pid, &manual).unwrap();
    std::fs::write(root.join("policy.json"), "[]").unwrap();
    let req = serde_json::from_value(json!({"project_id":pid,"source_path":"policy.json","source_type":"rules","clear_existing":true})).unwrap();
    let error = engram.handle_ingest_quality_gates(req).await.unwrap_err();
    assert!(error.message.contains("manually changed") && error.message.contains("No rules imported"), "{error}");
    assert_eq!(qg_count(&state, &pid), 1);
    assert_eq!(state.registry.list_repo_rules(&pid).unwrap()[0].rule_text, manual.rule_text);
}

#[tokio::test]
async fn deleting_a_historical_export_does_not_invalidate_its_evidence() {
    let (tmp, state, engram, pid) = build().await;
    ingest(&engram, &pid, "rules_v1.txt", false).await;
    let doc = state.get_project_cached(&pid).unwrap().search.list_docs_in_namespace(&pid, "quality_gate").unwrap().remove(0);
    std::fs::remove_file(tmp.path().join("proj/rules_v1.txt")).unwrap();
    let response = engram.handle_get_chunk(serde_json::from_value(json!({
        "project_id":pid,"namespace":"quality_gate","doc_id":doc.doc_id
    })).unwrap()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(text.contains("historical: published finding evidence"), "{text}");
}

#[tokio::test]
async fn reordering_rules_preserves_only_the_latest_source_revision() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    let other = "Always retain OtherProbe protections in production. [check: forbid=OtherProbe]";
    for rules in [json!([{"rule":other}, {"rule":SCOPED_MANDATE}]),
        json!([{"rule":SCOPED_MANDATE}, {"rule":other}])] {
        std::fs::write(root.join("policy.json"), rules.to_string()).unwrap();
        ingest_json(&engram, &pid, "policy.json", false).await;
        assert_eq!(qg_count(&state, &pid), 2);
        assert_eq!(state.registry.list_repo_rules(&pid).unwrap().len(), 2);
        assert!(scope_probe_is_enforced(&state, &pid, &root, "Site/maps/Widget.vb").await);
    }
}

#[tokio::test]
async fn clearing_search_corpus_keeps_valid_registry_rules_and_reports_missing_search_coverage() {
    use engram_server::services::project_service::inject_repo_rules;
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    for source in ["first.json", "second.json"] {
        std::fs::write(root.join(source), json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
        ingest_json(&engram, &pid, source, false).await;
    }
    ingest_json(&engram, &pid, "second.json", true).await;
    assert_eq!(qg_count(&state, &pid), 1);
    let path = engram_core::RelPath::new("Site/maps/Widget.vb");
    let output = inject_repo_rules(&state, &pid, &path, "body").await;
    assert!(output.contains("corpus was cleared") && output.contains("[Repo Constraint]"), "{output}");
    ingest_json(&engram, &pid, "first.json", false).await;
    assert_eq!(qg_count(&state, &pid), 2);
    let output = inject_repo_rules(&state, &pid, &path, "body").await;
    assert!(!output.contains("INCOMPLETE"), "{output}");
}

#[tokio::test]
async fn concurrent_refreshes_leave_one_consistent_published_source() {
    let (tmp, state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    std::fs::write(root.join("policy.json"), json!([{"rule":SCOPED_MANDATE}]).to_string()).unwrap();
    let ((), ()) = tokio::join!(ingest_json(&engram, &pid, "policy.json", false),
        ingest_json(&engram, &pid, "policy.json", false));
    assert_eq!(qg_count(&state, &pid), 1);
    assert_eq!(state.registry.list_repo_rules(&pid).unwrap().len(), 1);
    assert!(scope_probe_is_enforced(&state, &pid, &root, "Site/maps/Widget.vb").await);
}

#[tokio::test]
async fn scoped_managed_policy_never_becomes_a_generated_root_mandate() {
    let (tmp, _state, engram, pid) = build().await;
    let root = tmp.path().join("proj");
    std::fs::write(root.join("policy.json"), json!([
        {"rule":SCOPED_MANDATE,"path_scope":"Site/maps/**"}
    ]).to_string()).unwrap();
    ingest_json(&engram, &pid, "policy.json", false).await;
    // Check both a current scoped source and its stale imported revision.
    for stale in [false, true] {
        if stale { std::fs::write(root.join("policy.json"), "[]").unwrap(); }
        let response = engram.handle_produce_claude_md(serde_json::from_value(json!({
            "project_id":pid,"write_to_disk":false,"use_llm":false,"generate_agents_md":true
        })).unwrap()).await.unwrap();
        let text = response.content[0].as_text().unwrap().text.clone();
        assert!(!text.contains("ScopeProbe"), "must not create root policy from scoped/stale managed source: {text}");
        assert!(text.contains("Source-managed policies excluded"), "{text}");
    }
}
