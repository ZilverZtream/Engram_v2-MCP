#![allow(clippy::unwrap_used)]
use std::{collections::HashMap, sync::Arc};
use engram_core::{Config, RelPath};
use engram_graph::EdgeKind;
use engram_server::AppState;

#[tokio::test]
async fn bare_display_names_bind_corroborated_fqns_without_borrowing_other_owners() {
    let temp = tempfile::tempdir().unwrap();
    let (state, _) = AppState::new(Config {
        allowed_roots:vec![temp.path().to_path_buf()], data_dir:temp.path().join("data"),
        embedding_backend:"fts_only".into(), ..Default::default()
    }).unwrap();
    let path = Arc::new(RelPath::new("Methods.cs"));
    let mut stats = engram_index::IngestStats::default();
    for (name,fqn,line) in [("Run","App.Caller.Run",5),("Run","App.Sibling.Run",20),("Save","App.Right.Save",35),("Save","App.Wrong.Save",50)] {
        stats.symbols.push((path.clone(),engram_index::ExtractedSymbol {
            name:name.into(),kind:"function".into(),start_line:line,end_line:line+8,
            metadata:Some(HashMap::from([("fqn".into(),fqn.into())])),
        }));
    }
    stats.symbols.push((Arc::new(RelPath::new("web.config")),engram_index::ExtractedSymbol {
        name:"MinPhotosRequired".into(),kind:"app_setting".into(),start_line:1,end_line:1,metadata:None,
    }));
    for (kind,target,target_kind) in [("reads_setting","MinPhotosRequired","app_setting"),("calls","App.Right.Save","function"),("calls","App.Missing.Save","function")] {
        stats.edges.push((path.clone(),engram_index::ExtractedEdge {
            source_name:"App.Caller.Run".into(),source_kind:"function".into(),source_start_line:6,
            source_language:"cs".into(),target_name:target.into(),target_kind:Some(target_kind.into()),
            target_start_line:None,kind:kind.into(),metadata:None,
        }));
    }
    engram_server::services::ingest_service::process_ingest_stats(&state,"qualified-metadata",1,&stats).await.unwrap();
    let nodes = state.graph.query_nodes("qualified-metadata",Some("function"),None,None,20).unwrap();
    let caller = nodes.iter().find(|node| node.start_line==5).unwrap();
    let sibling = nodes.iter().find(|node| node.start_line==20).unwrap();
    let right = nodes.iter().find(|node| node.start_line==35).unwrap();
    let wrong = nodes.iter().find(|node| node.start_line==50).unwrap();
    let settings = state.graph.neighbors("qualified-metadata",EdgeKind::ReadsSetting,&caller.node_id,10).unwrap();
    assert_eq!(settings.len(),1,"the FQN source must be the real bare-named declaration");
    assert!(state.graph.neighbors("qualified-metadata",EdgeKind::ReadsSetting,&sibling.node_id,10).unwrap().is_empty());
    let calls = state.graph.neighbors("qualified-metadata",EdgeKind::Calls,&caller.node_id,10).unwrap();
    assert!(calls.iter().any(|(id,_)| id==&right.node_id),"{calls:?}");
    assert!(!calls.iter().any(|(id,_)| id==&wrong.node_id),"{calls:?}");
    assert!(calls.iter().any(|(id,_)| id.contains("App.Missing.Save")),"unknown owner must remain unresolved: {calls:?}");
}
