#![allow(clippy::unwrap_used)]
//! Row-3 audit (docs/audits/05-pre-commit-gates.md) A6: `pre_push_audit`
//! never says how many rules it checked, and on a project with NO
//! ingested quality-gate rules it prints the same "no rules matched" as
//! on a project where rules exist but none matched — the mandated
//! pre-push step is a silent no-op on OciusX. The output must carry a
//! tally, and an empty namespace must be called INACTIVE.

use engram_core::RelPath;
use engram_core::config::Config;
use engram_server::models::PrePushAuditRequest;
use engram_server::state::AppState;
use engram_server::tools::Engram;
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

async fn build(with_rules: bool) -> (tempfile::TempDir, AppState, Engram, String) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("Site")).unwrap();
    std::fs::write(root.join("Site/a.vb"), "Public Class a\nEnd Class\n").unwrap();
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
            project_name: "AuditFixture".into(),
            project_type: engram_server::models::ProjectType::DotnetWebformsVb,
            wait: true,
            dedupe_by_directory: false,
        }))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    if with_rules {
        let engine = state.get_project_cached(&pid).unwrap().search;
        let docs: Vec<engram_index::IndexDoc> = (0..3)
            .map(|i| engram_index::IndexDoc {
                generation: 1,
                chunk_id: 700 + i,
                doc_id: format!("qg:{i}"),
                content_hash: format!("qgh{i}"),
                path: RelPath::new(&format!("quality_gate/rule_{i}.md")),
                content: format!(
                    "RULE {i}: every endpoint that reads pr_id from the request must call check_pr_id before touching project data"
                ),
                language: "markdown".into(),
                namespace: "quality_gate".into(),
                author: None,
                timestamp: None,
                start_line: 1,
                end_line: 1,
            })
            .collect();
        engine
            .index_docs(&pid, &docs, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
    }
    (tmp, state, engram, pid)
}

async fn audit(engram: &Engram, pid: &str, code: &str) -> String {
    let req: PrePushAuditRequest =
        serde_json::from_value(json!({"project_id": pid, "code": code, "top_k": 12})).unwrap();
    let res = engram.handle_pre_push_audit(req).await.unwrap();
    res.content[0].as_text().unwrap().text.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_audit_states_how_many_rules_it_checked() {
    let (_tmp, _state, engram, pid) = build(true).await;
    let out = audit(
        &engram,
        &pid,
        "Dim pr_id = GetDictionaryIntegerValue(qry.data, \"pr_id\")\nReturn _io.x.GetAll(pr_id, db)",
    )
    .await;
    assert!(
        out.contains("rule(s) checked")
            || out.contains("rules checked")
            || out.contains("Checked:"),
        "the tally must be printed:\n{out}"
    );
    assert!(
        out.contains("of 3") || out.contains("3 rule"),
        "the namespace size (3 rules) must be part of the tally:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_quality_gate_namespace_is_reported_as_inactive() {
    let (_tmp, _state, engram, pid) = build(false).await;
    let out = audit(&engram, &pid, "Dim pr_id = qry.params(\"pr_id\")").await;
    assert!(
        out.contains("INACTIVE"),
        "no rules ingested ⇒ the mandated step checked NOTHING and must say so:\n{out}"
    );
    assert!(
        out.contains("ingest_quality_gates"),
        "the remedy must be named:\n{out}"
    );
}
