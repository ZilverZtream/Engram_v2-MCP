#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, DocIdStr, MemorySection, ProjectRecord, RelPath};
use engram_index::IndexDoc;
use engram_server::{AppState, Engram, services::project_service};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
const PID: &str = "stored-citations";

async fn fixture() -> (tempfile::TempDir, AppState, Engram) {
    let t = tempfile::tempdir().unwrap();
    let (state, _) = AppState::new(Config {
        data_dir: t.path().join("data"),
        allowed_roots: vec![t.path().to_path_buf()],
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    })
    .unwrap();
    state
        .registry
        .put_project(&ProjectRecord {
            project_id: PID.into(),
            project_name: PID.into(),
            project_type: "general".into(),
            directory: t.path().to_string_lossy().into_owned(),
            created_at_ms: 0,
            updated_at_ms: 0,
            reindex_required_since_ms: None,
        })
        .unwrap();
    state
        .registry
        .set_meta(PID, "active_generation", "1")
        .unwrap();
    project_service::ensure_project_runtime(&state, PID)
        .await
        .unwrap();
    let engram = Engram::new(state.clone());
    (t, state, engram)
}
fn save(state: &AppState, body: &str) {
    let section: MemorySection = serde_json::from_value(
        json!({"section_id":"quote","title":"Exact quote","content":body,"updated_at_ms":10}),
    )
    .unwrap();
    state.registry.put_memory_section(PID, &section).unwrap();
}
fn decoded(r: rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(&r.content[0].as_text().unwrap().text).unwrap()
}
async fn seed(state: &AppState, body: &str, namespace: &str) -> String {
    let hash = ContentHash::compute(body.as_bytes());
    let id = DocIdStr::compute("quote.txt", 1, 1, &hash).0;
    let d = IndexDoc {
        generation: if namespace == "memory" { 1 } else { 0 },
        chunk_id: 100,
        path: RelPath::new("quote.txt"),
        language: "text".into(),
        content: body.into(),
        namespace: namespace.into(),
        author: None,
        timestamp: None,
        start_line: 1,
        end_line: 1,
        doc_id: id.clone(),
        content_hash: hash.0,
    };
    state
        .get_project_cached(PID)
        .unwrap()
        .search
        .index_docs(PID, &[d], &CancellationToken::new())
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn memory_exact_pages_and_changed_record_fail_closed_without_changing_delete_schema() {
    let (_t, state, e) = fixture().await;
    let body = "# ‘Exact’ **house rule** `quote`\r\n".repeat(3500);
    save(&state, &body);
    let legacy = e
        .handle_read_memory_bank(
            serde_json::from_value(json!({"project_id":PID,"section":"quote"})).unwrap(),
        )
        .await
        .unwrap();
    assert!(legacy.content[0].as_text().unwrap().text.ends_with(&body));
    let mut citation = json!({});
    let mut reconstructed = String::new();
    let mut pages = 0;
    loop {
        let p = decoded(
            e.handle_read_memory_bank(
                serde_json::from_value(
                    json!({"project_id":PID,"section":"quote","citation":citation}),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        );
        pages += 1;
        reconstructed.push_str(p["content"].as_str().unwrap());
        assert_eq!(
            p["raw_content_hash"],
            format!("blake3-raw-utf8:{}", blake3::hash(body.as_bytes()).to_hex())
        );
        if p["continuation"].is_null() {
            break;
        }
        citation = p["continuation"].clone();
    }
    assert!(pages > 2);
    assert_eq!(reconstructed, body);
    save(&state, &body.replace("\r\n", "\n"));
    assert!(
        e.handle_read_memory_bank(
            serde_json::from_value(json!({"project_id":PID,"section":"quote","citation":citation}))
                .unwrap()
        )
        .await
        .unwrap_err()
        .message
        .contains("citation_hash_mismatch")
    );
    assert!(
        serde_json::from_value::<engram_server::MemorySectionRequest>(
            json!({"project_id":PID,"section":"quote","citation":{}})
        )
        .is_err()
    );
}

#[tokio::test]
async fn chunk_pages_recover_legacy_truncation_and_reject_transformations() {
    let (_t, state, e) = fixture().await;
    let body = "‘literal’ **format**\n".repeat(5000);
    let id = seed(&state, &body, "business_logic").await;
    let base = json!({"project_id":PID,"namespace":"business_logic","doc_id":id});
    let legacy = e
        .handle_get_chunk(serde_json::from_value(base.clone()).unwrap())
        .await
        .unwrap();
    assert!(
        legacy.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("chunk truncated")
    );
    let notice = &legacy.content[0].as_text().unwrap().text;
    assert!(notice.contains("citation: {}") && notice.contains("continuation as citation") && notice.contains("same project_id/namespace/doc_id") && notice.contains("no transformations"), "{notice}");
    let mut citation = json!({});
    let mut recovered = String::new();
    loop {
        let mut req = base.clone();
        req["citation"] = citation;
        let p = decoded(
            e.handle_get_chunk(serde_json::from_value(req).unwrap())
                .await
                .unwrap(),
        );
        assert_eq!(p["metadata"]["active_generation"], 1);
        assert!(p["metadata"].get("document_generation").is_none());
        recovered.push_str(p["content"].as_str().unwrap());
        if p["continuation"].is_null() {
            break;
        }
        citation = p["continuation"].clone();
    }
    assert_eq!(recovered, body);
    for (key, value) in [
        ("inject_rules", json!(true)),
        ("logical_slice", json!("data_methods")),
    ] {
        let mut req = base.clone();
        req["citation"] = json!({});
        req[key] = value;
        assert!(
            e.handle_get_chunk(serde_json::from_value(req).unwrap())
                .await
                .unwrap_err()
                .message
                .contains("transformations")
        );
    }
}

#[tokio::test]
async fn citation_still_withholds_stale_source_even_when_raw_stored_hash_matches() {
    let (t, state, e) = fixture().await;
    let body = "exact source\n";
    let id = seed(&state, body, "memory").await;
    std::fs::write(t.path().join("quote.txt"), body).unwrap();
    state.graph.upsert_nodes(PID,&[engram_graph::Node {node_id:"file:quote.txt".into(),node_type:"file".into(),name:"quote.txt".into(),namespace:"code".into(),language:"text".into(),file_path:RelPath::new("quote.txt"),start_line:1,end_line:1,generation:1,metadata:Some(json!({"file_hash":blake3::hash(body.as_bytes()).to_hex().to_string(),"source_index_version":engram_index::SOURCE_INDEX_VERSION}))}]).unwrap();
    let req = json!({"project_id":PID,"namespace":"memory","doc_id":id,"citation":{"expected_raw_hash":format!("blake3-raw-utf8:{}",blake3::hash(body.as_bytes()).to_hex()),"verify_quote":"exact source"}});
    let fresh = decoded(
        e.handle_get_chunk(serde_json::from_value(req.clone()).unwrap())
            .await
            .unwrap(),
    );
    assert!(fresh.get("content").is_none());
    assert_eq!(fresh["content_status"], "not_returned_for_quote_verification");
    assert_eq!(fresh["quote_verification"]["status"], "exact_match");
    assert!(
        fresh["metadata"]["source_freshness"]
            .as_str()
            .unwrap()
            .starts_with("verified:")
    );
    std::fs::write(t.path().join("quote.txt"), "changed source\n").unwrap();
    assert!(
        e.handle_get_chunk(serde_json::from_value(req).unwrap())
            .await
            .unwrap_err()
            .message
            .contains("Stale source chunk withheld")
    );
}


#[tokio::test]
async fn quote_verification_binds_chunk_and_memory_section_identity() {
    let (_t,state,e)=fixture().await;
    let body="Primary wording\r\nSecond line";
    let hash=engram_server::services::stored_citation::raw_hash(body);
    save(&state,body);
    let id=seed(&state,body,"business_logic").await;
    let citation=json!({"expected_raw_hash":hash,"verify_quote":"Primary wording"});
    let chunk=decoded(e.handle_get_chunk(serde_json::from_value(json!({"project_id":PID,"namespace":"business_logic","doc_id":id,"citation":citation})).unwrap()).await.unwrap());
    let section=decoded(e.handle_read_memory_bank(serde_json::from_value(json!({"project_id":PID,"section":"quote","citation":citation})).unwrap()).await.unwrap());
    assert_eq!(chunk["identity"],json!({"project_id":PID,"namespace":"business_logic","doc_id":id}));
    assert_eq!(section["identity"],json!({"project_id":PID,"section":"quote"}));
    assert_eq!(chunk["quote_verification"],section["quote_verification"]);
    assert!(chunk.get("content").is_none() && section.get("content").is_none());
    assert_eq!(chunk["content_status"], "not_returned_for_quote_verification");
    assert_eq!(section["content_status"], "not_returned_for_quote_verification");
    assert_eq!(chunk["quote_verification"]["status"],"exact_match");
    assert!(e.handle_get_chunk(serde_json::from_value(json!({"project_id":PID,"namespace":"insights","doc_id":id,"citation":citation})).unwrap()).await.is_err());
    save(&state,"Changed wording");
    assert!(e.handle_read_memory_bank(serde_json::from_value(json!({"project_id":PID,"section":"quote","citation":citation})).unwrap()).await.unwrap_err().message.contains("citation_hash_mismatch"));
    assert!(e.handle_read_memory_bank(serde_json::from_value(json!({"project_id":PID,"section":"absent","citation":citation})).unwrap()).await.is_err());
}
