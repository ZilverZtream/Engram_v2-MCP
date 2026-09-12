#![allow(clippy::unwrap_used)]
use engram_core::{Config, ContentHash, DocIdStr, ProjectRecord, RelPath};
use engram_index::IndexDoc;
use engram_server::{AppState, Engram, services::{project_service,business_rule_diagnostics}};
use serde_json::{Value,json};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn actual_handlers_preserve_legacy_unknown_counts_and_block_warned_claims() {
    // Separate databases prevent ranking among fixture documents from hiding a rendering failure.
    for kind in ["legacy_quiet", "legacy_warned", "current_exact"] {
        let temp=tempfile::tempdir().unwrap();let pid="legacy-claim-fixture";
        let (state,_)=AppState::new(Config{data_dir:temp.path().join("data"),allowed_roots:vec![temp.path().into()],embedding_backend:"fts_only".into(),llm_backend:"none".into(),..Default::default()}).unwrap();
        state.registry.put_project(&ProjectRecord{project_id:pid.into(),project_name:pid.into(),project_type:"general".into(),directory:temp.path().to_string_lossy().into_owned(),created_at_ms:0,updated_at_ms:0,reindex_required_since_ms:None}).unwrap();
        state.registry.set_meta(pid,"active_generation","1").unwrap();
        project_service::ensure_project_runtime(&state,pid).await.unwrap();let e=Engram::new(state.clone());
        let rules:Vec<_>=(1..=11).map(|n| (n,format!("IF generic policy {n} THEN retain fixture behavior\n  original continuation {n}"))).collect();
        let warnings=if kind=="legacy_quiet"{vec![]}else{vec!["Rule 2: generic source reference needs review".into(),"Rule 10: generic source anchor needs review".into()]};
        let mut mapping=business_rule_diagnostics::build(rules.clone(),&warnings);
        if kind!="current_exact" { for entry in &mut mapping.rules {entry.displayed_rule=None;} }
        let block=rules.iter().map(|(_,r)|format!("- {r}\n")).collect::<String>();
        let body=format!("# FixturePolicy.Run\n**Rule source diagnostics v1**: `{}`\n\n## Business Rules\n{block}\n## Data Flow\nGeneric policy fixture\n\n_Source: FixturePolicy.vb_\n",serde_json::to_string(&mapping).unwrap());
        let hash=ContentHash::compute(body.as_bytes());let path="__business_logic/FixturePolicy.vb/Run.md";let id=DocIdStr::compute(path,0,0,&hash).0;
        state.get_project_cached(pid).unwrap().search.index_docs(pid,&[IndexDoc{generation:0,chunk_id:99,path:RelPath::new(path),language:"markdown".into(),content:body.clone(),namespace:"business_logic".into(),author:None,timestamp:None,start_line:0,end_line:0,doc_id:id.clone(),content_hash:hash.0}],&CancellationToken::new()).await.unwrap();
        let query=e.handle_query_business_logic(serde_json::from_value(json!({"project_id":pid,"query":"FixturePolicy Run generic policy","top_k":1})).unwrap()).await.unwrap();
        let query=&query.content[0].as_text().unwrap().text;assert!(query.contains(&id),"fixture not retrieved: {query}");
        let ask=e.handle_ask_codebase(serde_json::from_value(json!({"project_id":pid,"question":"What business rules govern FixturePolicy.Run generic policy?","depth":"standard","output_format":"json","include_insights":false})).unwrap()).await.unwrap();
        let report:Value=serde_json::from_str(&ask.content[0].as_text().unwrap().text).unwrap();
        let evidence=report["evidence"].as_array().unwrap().iter().find(|v|v["document_id"]==id).expect("exact fixture must be retrieved");
        for view in [query.as_str(), evidence["content"].as_str().unwrap()] {
            if kind=="current_exact" {
                assert!(view.contains("2 diagnosed withheld, 9 inferred/unverified"),"{view}");
            } else {
                assert!(view.contains("Rule claims: counts unknown; ASSOCIATION_UNKNOWN"),"{view}");
                assert!(!view.contains("0 diagnosed withheld, 0 inferred"));
                if kind=="legacy_quiet" {assert!(view.contains("INFERRED_UNVERIFIED") && view.contains(&rules[0].1),"{view}");}
                else {assert!(view.contains("rule prose withheld") && !view.contains(&rules[0].1),"{view}");}
            }
            if kind!="legacy_quiet" {assert!(!view.contains(&rules[1].1) && !view.contains(&rules[9].1));}
        }
        let raw=e.handle_get_chunk(serde_json::from_value(json!({"project_id":pid,"doc_id":id,"namespace":"business_logic","citation":{"unit":"utf8_bytes"}})).unwrap()).await.unwrap();
        let recovered:Value=serde_json::from_str(&raw.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(recovered["content"].as_str().unwrap().as_bytes(),body.as_bytes());assert_eq!(recovered["document_complete"],true);
    }
}
