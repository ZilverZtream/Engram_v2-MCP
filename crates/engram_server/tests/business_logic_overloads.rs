#![allow(clippy::unwrap_used)]
use engram_core::Config;
use engram_server::{state::AppState, tools::Engram};
use rmcp::handler::server::tool::Parameters;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overloads_persist_separately_require_selection_and_retire_old_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Public Class Sample\n Sub New()\n\n End Sub\n Sub New(value As Integer)\n End Sub\nEnd Class\n".replace('\n', "\r\n");
    std::fs::write(root.join("Sample.vb"), &source).unwrap();
    let config = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _rx) = AppState::new(config).unwrap();
    let engram = Engram::new(state.clone());
    engram.index_project(Parameters(serde_json::from_value(json!({
        "directory": root, "project_name": "Overloads", "project_type": "dotnet_webforms_vb",
        "wait": true, "dedupe_by_directory": false
    })).unwrap())).await.unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let request = json!({"project_id": pid, "file_path": "Sample.vb", "output_json": true});
    let result = engram
        .handle_analyze_business_logic(serde_json::from_value(request.clone()).unwrap())
        .await
        .unwrap();
    let result: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(result["methods"].as_array().unwrap().len(), 2);
    assert_eq!(result["methods"][0]["overload_line"], 2);
    assert_eq!(result["methods"][1]["overload_line"], 5);
    let search = state.get_project_cached(&pid).unwrap().search;
    let docs = search
        .list_docs_in_namespace(&pid, "business_logic")
        .unwrap();
    assert_eq!(docs.len(), 2, "{docs:?}");
    assert!(docs.iter().any(|d| d.path.ends_with("New__L2.md")));
    assert!(docs.iter().any(|d| d.path.ends_with("New__L5.md")));

    // Method context must retrieve only the selected overload, in full.
    let context = engram
        .handle_get_method_edit_context(
            serde_json::from_value(json!({
                "project_id": pid, "file_path": "Sample.vb", "method_name": "New",
                "line": 5, "output_json": true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let context: serde_json::Value =
        serde_json::from_str(&context.content[0].as_text().unwrap().text).unwrap();
    let hits = context["business_logic"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{context}");
    assert!(hits[0]["path"].as_str().unwrap().ends_with("New__L5.md"));
    assert!(
        hits[0]["content"]
            .as_str()
            .unwrap()
            .contains("_Source: Sample.vb_")
    );

    let mut single = request.clone();
    single["method_name"] = json!("New");
    assert!(
        engram
            .handle_analyze_business_logic(serde_json::from_value(single.clone()).unwrap())
            .await
            .is_err()
    );
    single["line"] = json!(5);
    engram
        .handle_analyze_business_logic(serde_json::from_value(single).unwrap())
        .await
        .unwrap();
    assert_eq!(
        search
            .list_docs_in_namespace(&pid, "business_logic")
            .unwrap()
            .len(),
        2
    );

    std::fs::write(root.join("Sample.vb"), format!("\n{source}")).unwrap();
    engram
        .handle_analyze_business_logic(serde_json::from_value(request.clone()).unwrap())
        .await
        .unwrap();
    let docs = search
        .list_docs_in_namespace(&pid, "business_logic")
        .unwrap();
    assert_eq!(
        docs.len(),
        2,
        "obsolete overload identities must be retired: {docs:?}"
    );
    assert!(docs.iter().any(|d| d.path.ends_with("New__L3.md")));
    assert!(docs.iter().any(|d| d.path.ends_with("New__L6.md")));
    std::fs::write(root.join("Sample.vb"), "Public Class Sample\nEnd Class\n").unwrap();
    engram
        .handle_analyze_business_logic(serde_json::from_value(request).unwrap())
        .await
        .unwrap();
    assert!(
        search
            .list_docs_in_namespace(&pid, "business_logic")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_type_vb_analysis_persists_the_actual_owner_for_each_declaration() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = "Namespace _ata\nPublic Module ChangeRequestHelper\nPublic Function GetAccessRightsByCr() As Boolean\nEnd Function\nEnd Module\nPublic Class CrAccessRights\nPublic Function GetAccessRightsByCr() As Boolean\nEnd Function\nEnd Class\nEnd Namespace\n";
    std::fs::write(root.join("Helper.vb"), source).unwrap();
    let config = Config {
        allowed_roots: vec![root.clone()],
        data_dir: tmp.path().join("data"),
        embedding_backend: "fts_only".into(),
        llm_backend: "none".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let (state, _rx) = AppState::new(config).unwrap();
    let engram = Engram::new(state.clone());
    engram
        .index_project(Parameters(
            serde_json::from_value(json!({
                "directory": root, "project_name": "Owners", "project_type": "dotnet_webforms_vb",
                "wait": true, "dedupe_by_directory": false
            }))
            .unwrap(),
        ))
        .await
        .unwrap();
    let pid = state.registry.list_projects().unwrap()[0]
        .project_id
        .clone();
    let request = json!({"project_id":pid,"file_path":"Helper.vb","output_json":true});
    let response = engram
        .handle_analyze_business_logic(serde_json::from_value(request.clone()).unwrap())
        .await
        .unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    let methods = report["methods"].as_array().unwrap();
    assert_eq!(methods.len(), 2);
    assert_eq!(
        methods[0]["fqn"],
        "_ata.ChangeRequestHelper.GetAccessRightsByCr"
    );
    assert_eq!(methods[1]["fqn"], "_ata.CrAccessRights.GetAccessRightsByCr");
    let mut selected = request.clone();
    selected["method_name"] = json!("getaccessrightsbycr");
    selected["line"] = json!(7);
    let response = engram
        .handle_analyze_business_logic(serde_json::from_value(selected).unwrap())
        .await
        .unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(report["fqn"], "_ata.CrAccessRights.GetAccessRightsByCr");
    let search = state.get_project_cached(&pid).unwrap().search;
    let docs = search
        .list_docs_in_namespace(&pid, "business_logic")
        .unwrap();
    assert_eq!(docs.len(), 2);
    let legacy = docs
        .iter()
        .find(|d| d.path.ends_with("__L3.md"))
        .unwrap()
        .clone();
    for doc in docs {
        let (_, _, content, _, _) = search
            .get_doc_by_doc_id(&pid, "business_logic", 0, &doc.doc_id)
            .unwrap()
            .unwrap();
        let expected = if doc.path.ends_with("__L3.md") {
            "_ata.ChangeRequestHelper"
        } else {
            "_ata.CrAccessRights"
        };
        assert!(
            content.starts_with(&format!("# {expected}.GetAccessRightsByCr")),
            "{content}"
        );
        assert!(content.contains("**Analysis method hash**:"));
    }
    let old_text = "# WrongOwner.GetAccessRightsByCr\n\n**Purpose**: Historical analysis with the wrong owner.";
    search
        .index_docs(
            &pid,
            &[engram_index::IndexDoc {
                generation: 0,
                chunk_id: 998877,
                path: engram_core::RelPath::new(&legacy.path),
                language: "markdown".into(),
                content: old_text.into(),
                namespace: "business_logic".into(),
                author: None,
                timestamp: None,
                start_line: 0,
                end_line: 0,
                doc_id: legacy.doc_id,
                content_hash: engram_core::ContentHash::compute(old_text.as_bytes()).0,
            }],
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let response = engram
        .handle_get_method_edit_context(
            serde_json::from_value(json!({
                "project_id":pid,"file_path":"Helper.vb","method_name":"GetAccessRightsByCr",
                "class_name":"ChangeRequestHelper","line":3,"output_json":true
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let context: serde_json::Value =
        serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
    assert!(
        context["business_logic"]["note"]
            .as_str()
            .unwrap()
            .contains("STALE ANALYSIS OWNERSHIP")
    );
    assert_eq!(
        context["business_logic"]["hits"][0]["content"], old_text,
        "legacy evidence must not be silently rewritten"
    );
}
