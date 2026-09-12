#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, DocIdStr, ProjectRecord};
use engram_index::IndexDoc;
use engram_server::{AppState, Engram};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const PID: &str = "merged-ranking-test";

#[tokio::test]
async fn merged_work_recovery_arguments_call_get_chunk_without_project_repair() {
    let (_temp, state) = fixture().await;
    let item = doc(1, "Create invoice from selected orders", "Full cohort sentinel.", "backend", false);
    state.get_project_cached(PID).unwrap().search
        .index_docs(PID, std::slice::from_ref(&item), &CancellationToken::new())
        .await.unwrap();
    let text = query(&state, json!({})).await;
    let pointer = text.lines().find_map(|line| line.strip_prefix("full_document: get_chunk("))
        .and_then(|line| line.strip_suffix(')')).expect("complete recovery pointer");
    let args: serde_json::Value = serde_json::from_str(pointer).expect("copyable JSON arguments");
    assert_eq!(args["project_id"], PID);
    assert_eq!(args["namespace"], "history");
    assert_eq!(args["doc_id"], item.doc_id);
    let recovered = Engram::new(state).handle_get_chunk(serde_json::from_value(args).unwrap())
        .await.unwrap();
    assert!(recovered.content[0].as_text().unwrap().text.contains(&item.content));
}

#[tokio::test]
async fn primary_query_recovers_task_evidence_below_context_candidate_flood() {
    let (_temp, state) = fixture().await;
    let mut docs = vec![doc(
        1,
        "Cancel purchase requests",
        "purchase request",
        "backend",
        false,
    )];
    for id in 2..142 {
        docs.push(doc(
            id,
            "Invoice rows management",
            &"invoice rows ".repeat(20),
            "backend",
            false,
        ));
    }
    // Make task terms common in the index without making those documents
    // eligible history candidates; context terms then dominate the full query.
    let memory: Vec<_> = (1000..1600)
        .map(|id| {
            let mut item = doc(id, "Purchase request", "purchase request", "backend", false);
            item.namespace = "memory".into();
            item
        })
        .collect();
    let search = state.get_project_cached(PID).unwrap().search;
    search
        .index_docs(PID, &memory, &CancellationToken::new())
        .await
        .unwrap();
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &docs, &CancellationToken::new())
        .await
        .unwrap();
    let full_query = engram_index::HybridQuery {
        project_id: PID.into(),
        namespace: "history".into(),
        generation: 0,
        text: "create purchase request from invoice rows".into(),
        top_k: 100,
        fts_mode: "loose".into(),
        include_path_prefixes: Some(vec!["pr:".into()]),
        exclude_path_prefixes: None,
        include_path_suffixes: None,
        language_filters: None,
        author_filter: None,
        date_after: None,
        date_before: None,
        use_mmr: false,
    };
    let baseline = search.lexical_search(&full_query).unwrap();
    assert_eq!(baseline.len(), 100);
    assert!(
        !baseline.iter().any(|hit| hit.doc_id == docs[0].doc_id),
        "fixture must put the target below the full-query candidate cap"
    );
    let text = query(
        &state,
        json!({"story":"create purchase request from invoice rows"}),
    )
    .await;
    assert!(text.contains("# PR-1:"), "{text}");
    assert!(text.contains("candidate list reached its limit"), "{text}");
}

#[tokio::test]
async fn primary_task_beats_context_matches_and_classifier_labels() {
    let (_temp, state) = fixture().await;
    let docs = [
        doc(
            1,
            "Cancel purchase requests",
            "Existing request behavior",
            "backend",
            false,
        ),
        doc(
            2,
            "[Change] Invoice rows",
            "create purchase request from invoice rows",
            "backend",
            false,
        ),
        doc(
            3,
            "[Request] Invoice rows",
            "create purchase request from invoice rows",
            "backend",
            false,
        ),
    ];
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &docs, &CancellationToken::new())
        .await
        .unwrap();
    let text = query(
        &state,
        json!({"story":"create purchase request from invoice rows"}),
    )
    .await;
    assert!(text.contains("# PR-1:"), "{text}");
}

async fn fixture() -> (tempfile::TempDir, AppState) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
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
    engram_server::services::project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    (temp, state)
}

fn doc(id: usize, title: &str, body: &str, kind: &str, future: bool) -> IndexDoc {
    let path = format!("pr:PR-{id}");
    let date = if future { "2026-08-01" } else { "2026-01-01" };
    let content = format!(
        "# PR-{id}: {title}\nmerged: {date} | author: fixture | kinds: {kind}\n\n{body}\n\n## Files shipped together in this change\n- feature/handler.cs\n"
    );
    let hash = ContentHash::compute(content.as_bytes());
    IndexDoc {
        generation: 0,
        chunk_id: engram_index::chunk_id_from_content_hash(&hash),
        doc_id: DocIdStr::compute(&path, 0, 0, &hash).0,
        content_hash: hash.0,
        path: path.into(),
        language: "markdown".into(),
        content,
        namespace: "history".into(),
        author: None,
        timestamp: Some(if future { 1785542400 } else { 1767225600 }),
        start_line: 0,
        end_line: 0,
    }
}

async fn query(state: &AppState, extra: serde_json::Value) -> String {
    let mut args = json!({"project_id":PID,"story":"create invoice from selected orders","top":1});
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let result = Engram::new(state.clone())
        .handle_find_merged_work(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    result.content[0].as_text().unwrap().text.clone()
}

#[tokio::test]
async fn task_title_beats_query_repetition_in_unrelated_body() {
    let (_temp, state) = fixture().await;
    let mut docs = vec![doc(
        1,
        "Create invoice from selected orders",
        "Implementation notes",
        "backend",
        false,
    )];
    for id in 2..12 {
        docs.push(doc(
            id,
            "Update contribution template",
            &"create invoice from selected orders ".repeat(20),
            "backend",
            false,
        ));
    }
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &docs, &CancellationToken::new())
        .await
        .unwrap();
    let text = query(&state, json!({})).await;
    assert!(text.contains("# PR-1:"), "{text}");
    assert!(!text.contains("# PR-2:"), "{text}");
}

#[tokio::test]
async fn eligibility_filters_precede_candidate_limit_and_title_ranking() {
    let (_temp, state) = fixture().await;
    let mut docs = vec![doc(
        1,
        "Create invoice from selected orders",
        "Eligible",
        "backend",
        false,
    )];
    for id in 2..122 {
        docs.push(doc(
            id,
            "Create invoice from selected orders",
            "Wrong kind",
            "js",
            false,
        ));
    }
    for id in 122..242 {
        docs.push(doc(
            id,
            "Create invoice from selected orders",
            "Future",
            "backend",
            true,
        ));
    }
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &docs, &CancellationToken::new())
        .await
        .unwrap();
    let text = query(
        &state,
        json!({"kind":"backend","merged_before":"2026-02-01"}),
    )
    .await;
    assert!(text.contains("# PR-1:"), "{text}");
    assert!(
        !text.contains("Wrong kind") && !text.contains("Future"),
        "{text}"
    );
}

#[tokio::test]
async fn body_only_domain_terms_still_retrieve_useful_exemplars() {
    let (_temp, state) = fixture().await;
    let docs = [doc(
        1,
        "Maintenance update",
        "Correct orphaned widget pricing",
        "backend",
        false,
    )];
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &docs, &CancellationToken::new())
        .await
        .unwrap();
    let text = query(&state, json!({"story":"orphaned widget pricing"})).await;
    assert!(text.contains("# PR-1:"), "{text}");
}

#[tokio::test]
async fn shipped_file_filter_precedes_candidate_cap_and_rejects_body_mentions() {
    let (_temp, state) = fixture().await;
    let mut docs = vec![doc(1, "Fix invoice image collision", "Correct report rendering", "backend", false)];
    for id in 2..125 {
        let mut wrong = doc(id, "Fix invoice image collision", "feature/handler.cs invoice image collision", "backend", false);
        wrong.content = wrong.content.replace("- feature/handler.cs", "- unrelated/handler.cs");
        docs.push(wrong);
    }
    state.get_project_cached(PID).unwrap().search.index_docs(PID, &docs, &CancellationToken::new()).await.unwrap();
    let text = query(&state, json!({"story":"invoice image collision", "file_paths":["web/feature/handler.cs"]})).await;
    assert!(text.contains("# PR-1:"), "{text}");
    assert!(!text.contains("# PR-2:"), "{text}");
    assert!(text.contains("Suffix matches are leads only"), "{text}");
    let none = query(&state, json!({"story":"invoice image collision", "file_paths":["other/handler.cs"]})).await;
    assert!(none.contains("no merged work matched"), "{none}");
}
