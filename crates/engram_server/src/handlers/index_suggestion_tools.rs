use crate::{
    models::FindMissingIndexesRequest,
    services::{
        index_suggestions::{QuerySource, Suggestions},
        schema_query::{Catalog, Citation},
    },
    tools::Engram,
};
use engram_graph::{EdgeKind, GraphStore};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

fn read_source(
    graph: &GraphStore,
    project: &str,
    directory: &Path,
    path: &str,
    revision: u64,
) -> anyhow::Result<String> {
    let file = graph
        .get_node(project, &format!("file:{path}"))?
        .ok_or_else(|| anyhow::anyhow!("Indexed file fingerprint unavailable"))?;
    anyhow::ensure!(
        file.generation == revision,
        "File revision differs from selected source"
    );
    let mut source = String::new();
    std::fs::File::open(engram_core::safe_join(directory, path)?)?
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut source)?;
    anyhow::ensure!(source.len() <= 2 * 1024 * 1024, "Source exceeds2MiB");
    let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
    anyhow::ensure!(
        file.metadata
            .as_ref()
            .and_then(|m| m.get("file_hash"))
            .and_then(Value::as_str)
            == Some(hash.as_str()),
        "Source changed since indexing; update_project before relying on index evidence"
    );
    Ok(source)
}

impl Engram {
    pub async fn handle_find_missing_indexes(
        &self,
        req: FindMissingIndexesRequest,
    ) -> Result<CallToolResult, McpError> {
        let limit = req.limit.unwrap_or(10);
        let offset = req.offset.unwrap_or(0);
        if !(1..=50).contains(&limit)
            || offset > 100_000
            || !(1..=65536).contains(&req.max_tokens.unwrap_or(16384))
        {
            return Err(McpError::invalid_params(
                "limit must be1..=50, offset<=100000 and max_tokens1..=65536",
                None,
            ));
        }
        if req.file_path.as_ref().is_some_and(|p| {
            p.is_empty()
                || p.len() > 1024
                || p.chars().any(char::is_control)
                || ![".sql", ".vb"]
                    .iter()
                    .any(|e| p.to_ascii_lowercase().ends_with(e))
        }) {
            return Err(McpError::invalid_params(
                "file_path must identify a selected SQL or VB source file",
                None,
            ));
        }
        let record = self.ensure_project_record(&req.project_id).await?;
        let runtime = self.ensure_project_runtime(&req.project_id).await?;
        let (generation, view) = self
            .pin_active_source_generation(&req.project_id, &runtime.search)
            .await?;
        let graph = self.state.graph.clone();
        let max_tokens = req.max_tokens.or(Some(16384));
        let reservation = engram_core::memory::AllocationGuard::try_new(
            self.state.memory_budget.as_ref(),
            256 * 1024 * 1024,
            engram_core::memory::Subsystem::Graph,
            "Index suggestion source evidence",
        )
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let output=tokio::task::spawn_blocking(move||->anyhow::Result<String>{
            let _reservation=reservation;
            let selected=view.selected_file_revisions().ok_or_else(||anyhow::anyhow!("Index suggestions require managed source membership; update_project first"))?;
            anyhow::ensure!(selected.len()<=50_000,"Selected source inventory exceeds50000 files");
            let requested=req.file_path.as_ref().map(|s|s.replace('\\',"/"));
            if let Some(path)=&requested {anyhow::ensure!(selected.contains_key(path),"Query source is not selected");}
            let directory=Path::new(&record.directory);let mut catalog=Catalog::default();let mut queries=vec![];
            let mut bytes=0usize;let mut admitted_sql=0usize;let mut query_gaps=vec![];let mut truncated=false;let mut schema_complete=true;
            for (path,revision) in selected {
                if !path.to_ascii_lowercase().ends_with(".sql"){continue;}
                match read_source(&graph,&req.project_id,directory,path,*revision) {
                    Ok(source)=>{
                        bytes+=source.len();anyhow::ensure!(bytes<=64*1024*1024,"Source scan exceeds64MiB");
                        catalog.add_source(path,&source)?;admitted_sql+=1;
                        if requested.as_ref().is_none_or(|p|p==path){
                            if queries.len()<2048 {queries.push(QuerySource{citation:Citation{path:path.clone(),start_line:1,end_line:source.lines().count() as u64,source_hash:blake3::hash(source.as_bytes()).to_hex().to_string()},sql:source,exact_lines:true});}
                            else{truncated=true;}
                        }
                    }
                    Err(e)=>{schema_complete=false;if query_gaps.len()<64 {query_gaps.push(format!("{path}: {e}"));}else{truncated=true;}}
                }
            }
            let edges=graph.list_edges_by_kind(&req.project_id,EdgeKind::SqlCalls,5001)?;truncated |= edges.len()>5000;
            let mut verified=BTreeMap::<String,Option<String>>::new();let mut seen=BTreeSet::new();let mut inline_admitted=0usize;let mut inline_withheld=0usize;
            for edge in edges.iter().take(5000) {
                let Some(node)=graph.get_node(&req.project_id,&edge.source_id)? else {inline_withheld+=1;continue;};
                let path=node.file_path.as_str();
                if !path.to_ascii_lowercase().ends_with(".vb") || requested.as_ref().is_some_and(|p|p!=path){continue;}
                let Some(revision)=selected.get(path) else {inline_withheld+=1;continue;};
                if node.generation!=*revision || edge.generation!=*revision {inline_withheld+=1;continue;}
                let Some(meta)=edge.metadata.as_ref() else {inline_withheld+=1;continue;};
                // Existing concatenation extraction may retain only fragments. Do not
                // turn a partial assembled query into apparently executable index DDL.
                if meta.get("source").and_then(Value::as_str)==Some("local_concat"){inline_withheld+=1;continue;}
                let Some(sql)=meta.get("sql_text").and_then(Value::as_str) else {inline_withheld+=1;continue;};
                if sql.len()>128*1024 || queries.len()>=2048 {truncated=true;continue;}
                if !seen.insert((node.node_id.clone(),sql.to_string())){continue;}
                if !verified.contains_key(path){
                    if verified.len()>=256 {truncated=true;continue;}
                    let value=match read_source(&graph,&req.project_id,directory,path,*revision){Ok(s)=>{bytes+=s.len();anyhow::ensure!(bytes<=64*1024*1024,"Source scan exceeds64MiB");Some(s)},Err(e)=>{if query_gaps.len()<64{query_gaps.push(format!("{path}: {e}"));}else{truncated=true;}None}};
                    verified.insert(path.to_string(),value);
                }
                let Some(source)=verified[path].as_ref() else {inline_withheld+=1;continue;};
                if node.start_line==0 || node.end_line<node.start_line || node.end_line as usize>source.lines().count(){inline_withheld+=1;continue;}
                queries.push(QuerySource{sql:sql.into(),citation:Citation{path:path.into(),start_line:node.start_line as u64,end_line:node.end_line as u64,source_hash:blake3::hash(source.as_bytes()).to_hex().to_string()},exact_lines:false});inline_admitted+=1;
            }
            let mut suggestions=Suggestions::new(&catalog);for query in &queries {suggestions.add_source(query);}
            let mut result=suggestions.finish();
            if !schema_complete || catalog.gaps.iter().any(|g|g.contains("SQL tokenization failed")) {
                if let Some(rows)=result["suggestions"].as_array_mut(){for row in rows {row["status"]=json!("ddl_withheld_schema_inventory");row["create_index"]=Value::Null;row["filtered_alternative"]=Value::Null;row["warnings"].as_array_mut().expect("warnings array").push(json!("Selected SQL inventory contains unavailable or unparsed sources; existing index coverage cannot be established."));}}
            }
            let rows=result["suggestions"].as_array().cloned().unwrap_or_default();let total=rows.len();
            result["suggestions"]=json!(rows.into_iter().skip(offset).take(limit).collect::<Vec<_>>());result["total_suggestions"]=json!(total);result["offset"]=json!(offset);
            result["summary"]=json!(format!("{} source-backed index candidate(s); {} access pattern(s) already have an eligible source-declared leading-key index",total,result["covered_pattern_count"]));
            result["project_id"]=json!(req.project_id);result["active_generation"]=json!(generation);
            result["coverage"]["source_inventory"]=json!({"admitted_sql_files":admitted_sql,"inline_query_sites_admitted":inline_admitted,"inline_evidence_withheld":inline_withheld,"query_sources":queries.len(),"source_bytes":bytes,"source_gaps":query_gaps,"truncated":truncated,"admission":"Selected membership, physical graph revision and current source BLAKE3; no atomic graph/disk snapshot. Inline query text comes from indexed SqlCalls metadata with containing-method citations. Assembled/fragment SQL is withheld. Schema comparison uses all admitted SQL declarations even when file_path narrows query sources."});
            if offset+limit<total {let mut next=req.clone();next.offset=Some(offset+limit);result["next_page"]=json!({"tool":"find_missing_indexes","arguments":next,"consistency":"Repeat only while selected generation and source fingerprints remain unchanged"});}
            Ok(serde_json::to_string(&result)?)
        }).await.map_err(|e|McpError::internal_error(e.to_string(),None))?.map_err(|e|McpError::internal_error(e.to_string(),None))?;
        let output = super::search_tools::response_budget::bounded_message(output, max_tokens)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}
