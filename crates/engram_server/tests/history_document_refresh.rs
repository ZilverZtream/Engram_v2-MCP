#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, DocIdStr, ProjectRecord};
use engram_graph::{Edge, EdgeKind};
use engram_index::IndexDoc;
use engram_server::{AppState, Engram};
use git2::{Repository, Signature};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const PID: &str = "history-refresh-test";

#[tokio::test]
async fn multi_commit_batch_retains_old_documents_on_preparation_failure_and_retries() {
    let (_temp, state, repo) = setup();
    let a = commit(&repo, "before\n");
    let b = commit(&repo, "after\n");
    let ps = engram_server::services::project_service::ensure_project_runtime(&state, PID).await.unwrap();
    let bad = doc(format!("diff:{}:sample.txt", "f".repeat(40)), "missing", "history");
    let old = vec![doc(format!("diff:{a}:sample.txt"), "old a", "history"),
        doc(format!("diff:{b}:sample.txt"), "old b", "history"), bad.clone()];
    ps.search.index_docs(PID, &old, &CancellationToken::new()).await.unwrap();
    let e = Engram::new(state.clone());
    let request = || serde_json::from_value(json!({"project_id":PID,"mode":"refresh", "max_commits":10, "wait":true})).unwrap();
    assert!(e.handle_index_git_history(request()).await.is_err());
    let retained = ps.search.list_docs_in_namespace(PID, "history").unwrap();
    assert_eq!(retained.len(), 3);
    assert!(old.iter().all(|old| retained.iter().any(|r| r.doc_id == old.doc_id)));
    assert!(state.registry.get_meta(PID, "git_document_refresh_v1_after").unwrap().is_none());
    ps.search.delete_documents(PID, "history", &[bad.doc_id]).await.unwrap();
    let response = e.handle_index_git_history(request()).await.unwrap();
    let text = &response.content[0].as_text().unwrap().text;
    assert!(text.contains("documents_refreshed: 2"), "{text}");
    assert!(text.contains("remaining_commits: 0"), "{text}");
    let final_docs = ps.search.list_docs_in_namespace(PID, "history").unwrap();
    assert_eq!(final_docs.len(), 2);
    assert!(!old.iter().any(|old| final_docs.iter().any(|r| r.doc_id == old.doc_id)));
}

fn setup() -> (tempfile::TempDir, AppState, Repository) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let repo = Repository::init(&root).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    (temp, state, repo)
}

fn commit(repo: &Repository, text: &str) -> git2::Oid {
    std::fs::write(repo.workdir().unwrap().join("sample.txt"), text).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("sample.txt")).unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    let sig = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "fixture", &tree, &parents)
        .unwrap()
}

fn doc(path: String, content: &str, ns: &str) -> IndexDoc {
    let hash = ContentHash::compute(content.as_bytes());
    IndexDoc {
        generation: 1,
        chunk_id: engram_index::chunk_id_from_content_hash(&hash),
        doc_id: DocIdStr::compute(&path, 0, 0, &hash).0,
        content_hash: hash.0,
        path: path.into(),
        content: content.into(),
        namespace: ns.into(),
        language: "diff".into(),
        author: None,
        timestamp: None,
        start_line: 0,
        end_line: 0,
    }
}

async fn refresh(
    e: &Engram,
    force: bool,
    wait: bool,
) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
    e.handle_index_git_history(
        serde_json::from_value(json!({"project_id":PID,
        "mode":"refresh", "max_commits":1, "force":force, "wait":wait}))
        .unwrap(),
    )
    .await
}

#[tokio::test]
async fn bounded_refresh_resumes_preserves_graph_watermarks_and_other_corpora() {
    let (_temp, state, repo) = setup();
    let a = commit(&repo, "before\n");
    let b = commit(&repo, "after\n");
    let _unindexed = commit(&repo, "future\n");
    let ps = engram_server::services::project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    let old = vec![
        doc(format!("diff:{a}:sample.txt"), "old broken a", "history"),
        doc(format!("diff:{b}:sample.txt"), "old broken b", "history"),
    ];
    ps.search
        .index_docs(PID, &old, &CancellationToken::new())
        .await
        .unwrap();
    // Same ID/path in a different namespace must survive cleanup.
    let mut memory = old[0].clone();
    memory.namespace = "memory".into();
    ps.search
        .index_docs(PID, &[memory], &CancellationToken::new())
        .await
        .unwrap();
    let edge = Edge {
        source_id: "file:a".into(),
        target_id: "file:b".into(),
        namespace: "history".into(),
        language: "text".into(),
        edge_kind: EdgeKind::TemporalCoupling,
        weight: 73,
        generation: 1,
        metadata: Some(json!({"sentinel":"preserve"})),
        updated_at_ms: 17,
    };
    state.graph.upsert_edges(PID, &[edge]).unwrap();
    let before = state
        .graph
        .get_edges_by_endpoints(
            PID,
            &[("file:a".into(), EdgeKind::TemporalCoupling, "file:b".into())],
        )
        .unwrap();
    for key in [
        "last_git_oid",
        "oldest_indexed_git_oid",
        "git_backfill_complete",
    ] {
        state.registry.set_meta(PID, key, "sentinel").unwrap();
    }
    let e = Engram::new(state.clone());
    let first = refresh(&e, true, true).await.unwrap();
    assert!(
        first.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("more: true")
    );
    let second = refresh(&e, false, true).await.unwrap();
    assert!(
        second.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("more: false")
    );
    let docs = ps.search.list_docs_in_namespace(PID, "history").unwrap();
    assert_eq!(docs.len(), 2);
    let updated = docs
        .iter()
        .find(|d| d.path == format!("diff:{b}:sample.txt"))
        .unwrap();
    let (_, _, text, _, _) = ps
        .search
        .get_doc_by_doc_id(PID, "history", 1, &updated.doc_id)
        .unwrap()
        .unwrap();
    assert!(text.lines().any(|line| line == "-before"), "{text}");
    assert!(text.lines().any(|line| line == "+after"), "{text}");
    for d in &old {
        assert!(!docs.iter().any(|new| new.doc_id == d.doc_id));
    }
    assert_eq!(
        ps.search
            .list_docs_in_namespace(PID, "memory")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        format!("{before:?}"),
        format!(
            "{:?}",
            state
                .graph
                .get_edges_by_endpoints(
                    PID,
                    &[("file:a".into(), EdgeKind::TemporalCoupling, "file:b".into())]
                )
                .unwrap()
        )
    );
    for key in [
        "last_git_oid",
        "oldest_indexed_git_oid",
        "git_backfill_complete",
    ] {
        assert_eq!(
            state.registry.get_meta(PID, key).unwrap().as_deref(),
            Some("sentinel")
        );
    }
    let again = refresh(&e, true, true).await.unwrap();
    assert!(
        again.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("documents_refreshed: 0")
    );
    assert_eq!(
        ps.search
            .list_docs_in_namespace(PID, "history")
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn missing_commit_preserves_old_evidence_and_background_reports_failure() {
    let (_temp, state, _repo) = setup();
    let ps = engram_server::services::project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    let old = doc(
        format!("diff:{}:sample.txt", "a".repeat(40)),
        "irreplaceable",
        "history",
    );
    ps.search
        .index_docs(PID, &[old.clone()], &CancellationToken::new())
        .await
        .unwrap();
    let e = Engram::new(state.clone());
    assert!(refresh(&e, true, true).await.is_err());
    assert_eq!(
        ps.search.list_docs_in_namespace(PID, "history").unwrap()[0].doc_id,
        old.doc_id
    );
    let launched = refresh(&e, true, false).await.unwrap();
    let text = &launched.content[0].as_text().unwrap().text;
    let id = text.split("job_id: ").nth(1).unwrap().trim();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let job = state.registry.get_job(id).unwrap().unwrap();
            if job.status != "running" {
                assert_eq!(job.status, "failed");
                assert!(job.message.contains("history document refresh failed"));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        ps.search.list_docs_in_namespace(PID, "history").unwrap()[0].doc_id,
        old.doc_id
    );
}

#[tokio::test]
async fn background_refresh_waits_for_project_lock_and_keeps_result_summary() {
    let (_temp, state, _repo) = setup();
    let e = Engram::new(state.clone());
    let guard = state.acquire_project_update_lock(PID).await;
    let launched = refresh(&e, false, false).await.unwrap();
    let text = &launched.content[0].as_text().unwrap().text;
    let id = text.split("job_id: ").nth(1).unwrap().trim();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        state.registry.get_job(id).unwrap().unwrap().status,
        "running"
    );
    drop(guard);
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let job = state.registry.get_job(id).unwrap().unwrap();
            if job.status != "running" {
                assert_eq!(job.status, "done");
                assert!(job.message.contains("history_document_refresh:"));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}
