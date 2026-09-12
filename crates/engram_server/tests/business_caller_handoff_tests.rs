#![allow(clippy::unwrap_used)]
//! A business-rule result must offer a usable, scoped route to caller arguments.
use engram_core::{Config, ContentHash, DocIdStr, RelPath};
use engram_index::IndexDoc;
use engram_server::{AppState, Engram};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn action(text: &str) -> Option<Value> {
    text.lines().find_map(|line| line.strip_prefix("caller_context: get_method_edit_context("))
        .map(|value| serde_json::from_str(value.strip_suffix(')').unwrap()).unwrap())
}

#[tokio::test]
async fn business_card_recovers_distinct_arguments_and_withholds_stale_evidence() {
    let temp=tempfile::tempdir().unwrap();
    let root=temp.path().join("repo");std::fs::create_dir(&root).unwrap();
    let source="Public Class Catalog\n Public Shared Function Find(Optional activeOnly As Boolean = False) As Boolean\n  Return activeOnly\n End Function\nEnd Class\n";
    let caller="Public Class Dashboard\n Public Sub Render()\n  Catalog.Find()\n  Catalog.Find(activeOnly := True)\n End Sub\nEnd Class\n";
    std::fs::write(root.join("Catalog.vb"),source).unwrap();
    std::fs::write(root.join("Dashboard.vb"),caller).unwrap();
    let (state,_)=AppState::new(Config {data_dir:temp.path().join("data"),allowed_roots:vec![root.clone()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()}).unwrap();
    let e=Engram::new(state.clone());
    e.handle_index_project(serde_json::from_value(json!({"directory":root,"project_name":"caller-handoff","project_type":"general","wait":true})).unwrap()).await.unwrap();
    let pid=state.registry.list_projects().unwrap()[0].project_id.clone();
    let method_body="Public Shared Function Find(Optional activeOnly As Boolean = False) As Boolean\n  Return activeOnly\n End Function";
    let body=format!("# Catalog.Find\n\n**Analysis method hash**: `{}`\n\n## Business Rules\n- Filter according to the supplied optional activity flag.\n\n_Source: Catalog.vb_\n",ContentHash::compute(method_body.as_bytes()).0);
    let hash=ContentHash::compute(body.as_bytes());let path="__business_logic/Catalog.vb/Find.md";
    let id=DocIdStr::compute(path,0,0,&hash).0;
    state.get_project_cached(&pid).unwrap().search.index_docs(&pid,&[IndexDoc {generation:0,chunk_id:9001,path:RelPath::new(path),language:"markdown".into(),content:body,namespace:"business_logic".into(),author:None,timestamp:None,start_line:0,end_line:0,doc_id:id,content_hash:hash.0}],&CancellationToken::new()).await.unwrap();
    let request=json!({"project_id":pid,"query":"Catalog Find activity flag","top_k":1});
    let result=e.handle_query_business_logic(serde_json::from_value(request.clone()).unwrap()).await.unwrap();
    let text=&result.content[0].as_text().unwrap().text;
    let args=action(text).expect("verified business card must offer caller-context recovery");
    assert_eq!(args["file_path"],"Catalog.vb");assert_eq!(args["method_name"],"Find");
    assert_eq!(args["line"],2);assert_eq!(args["include_history"],false);assert_eq!(args["include_business_logic"],false);
    assert!(text.contains("caller-supplied arguments") && text.contains("not exhaustive"),"{text}");
    let context=e.handle_get_method_edit_context(serde_json::from_value(args.clone()).unwrap()).await.unwrap();
    let context:Value=serde_json::from_str(&context.content[0].as_text().unwrap().text).unwrap();
    let excerpts=context["caller_excerpts"].as_array().unwrap();
    assert!(excerpts.iter().any(|x| x["status"]=="verified_source" && x["numbered_source"].as_str().is_some_and(|s| s.contains("Catalog.Find()") && s.contains("activeOnly := True"))),"{context}");
    std::fs::write(root.join("Dashboard.vb"),format!("' changed caller\n{caller}")).unwrap();
    let stale=e.handle_get_method_edit_context(serde_json::from_value(args).unwrap()).await.unwrap();
    let stale:Value=serde_json::from_str(&stale.content[0].as_text().unwrap().text).unwrap();
    assert!(stale["caller_excerpts"].as_array().unwrap().iter().all(|x| x["status"]!="verified_source" && x["numbered_source"]==""),"{stale}");
    std::fs::write(root.join("Catalog.vb"),source.replace("Return activeOnly","Return False")).unwrap();
    let stale=e.handle_query_business_logic(serde_json::from_value(request).unwrap()).await.unwrap();
    let stale=&stale.content[0].as_text().unwrap().text;
    assert!(action(stale).is_none(),"{stale}");
}
