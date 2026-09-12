#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, ProjectRecord, RelPath};
use engram_graph::Node;
use engram_index::IndexDoc;
use engram_server::{AppState, Engram};
use serde_json::json;

#[tokio::test]
async fn matrix_preserves_document_checks_without_certifying_rules_or_losing_empty_and_stale_warnings()
 {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Class Rules\nPublic Function Save(value As Integer) As String\nIf value < 0 Then Return \"invalid\"\nReturn \"ok\"\nEnd Function\nEnd Class\n";
    std::fs::write(root.join("Rules.vb"), source).unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: temp.path().join("data"),
        allowed_roots: vec![root.clone()],
        embedding_backend: "fts_only".into(),
        ..Default::default()
    })
    .unwrap();
    let pid = "matrix-warning-review";
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: pid.into(),
            project_name: pid.into(),
            directory: root.to_string_lossy().into_owned(),
            project_type: "general".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(pid, "active_generation", "1")
        .unwrap();
    state
        .graph
        .upsert_nodes(
            pid,
            &[Node {
                node_id: "file:Rules.vb".into(),
                name: "Rules.vb".into(),
                node_type: "file".into(),
                namespace: "memory".into(),
                language: "vbnet".into(),
                file_path: RelPath::new("Rules.vb"),
                start_line: 1,
                end_line: 6,
                generation: 1,
                metadata: Some(
                    json!({"file_hash":blake3::hash(source.as_bytes()).to_hex().to_string()}),
                ),
            }],
        )
        .unwrap();
    let runtime = engram_server::services::project_service::ensure_project_runtime(&state, pid)
        .await
        .unwrap();
    let method_body = source
        .lines()
        .skip(1)
        .take(4)
        .collect::<Vec<_>>()
        .join("\n");
    let hash = ContentHash::compute(method_body.as_bytes()).0;
    let inputs = [
        (
            "warned",
            "## Source checks requiring review\n- Rule 2: cited source line is blank or a comment; verify its executable condition.\n- Helper behavior remains unverified.\n  Inspect its implementation separately.\n## Business Rules\n- IF value is negative THEN reject.\n- IF value is nonnegative THEN accept.\n",
        ),
        (
            "clean",
            "## Business Rules\n- IF normal completion THEN return the result.\n",
        ),
        (
            "empty",
            "## Source checks requiring review\n- No usable cases; inspect source before planning.\n",
        ),
    ];
    let docs: Vec<_> = inputs
        .iter()
        .enumerate()
        .map(|(index, (id, body))| {
            let content = format!(
                "# Rules.Save\n**Analysis method hash**: `{hash}`\n{body}\n_Source: Rules.vb_\n"
            );
            IndexDoc {
                generation: 0,
                chunk_id: index as u64,
                path: RelPath::new(&format!("__business_logic/Rules.vb/{id}.md")),
                language: "markdown".into(),
                content_hash: ContentHash::compute(content.as_bytes()).0,
                content,
                namespace: "business_logic".into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id: id.to_string(),
            }
        })
        .collect();
    runtime
        .search
        .index_docs(pid, &docs, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let engram = Engram::new(state);
    let request =
        || serde_json::from_value(json!({"project_id":pid,"files":["Rules.vb"]})).unwrap();
    let result = engram.handle_derive_test_matrix(request()).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert_eq!(
        text.matches("Expected outcome (inferred):").count(),
        3,
        "{text}"
    );
    assert_eq!(
        text.matches("cited source line is blank or a comment")
            .count(),
        1,
        "warnings should be shown once per document: {text}"
    );
    assert!(
        text.contains("Analysis source checks requiring review — document `warned`"),
        "{text}"
    );
    assert!(
        !text.contains("Analysis source checks requiring review — document `clean`"),
        "{text}"
    );
    assert!(text.contains("Warning rule numbers refer to this document, not matrix case numbers"));
    assert!(
        text.contains(
            "Helper behavior remains unverified.\nInspect its implementation separately."
        )
    );
    assert!(text.contains("No usable cases; inspect source before planning."));
    assert!(text.contains("get_chunk(doc_id=\"warned\", namespace=\"business_logic\")"));
    assert!(text.contains("Test execution: not_run"));
    assert!(text.contains("A matching method hash does not clear these warnings"));
    std::fs::write(
        root.join("Rules.vb"),
        source.replace("value < 0", "value > 0"),
    )
    .unwrap();
    let result = engram.handle_derive_test_matrix(request()).await.unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();
    assert!(
        !text.contains("Expected outcome (inferred):"),
        "stale cases must remain withheld: {text}"
    );
    assert!(text.contains("STALE:"));
    assert!(text.contains("cited source line is blank or a comment"));
    assert!(text.contains("get_chunk(doc_id=\"warned\", namespace=\"business_logic\")"));
}
