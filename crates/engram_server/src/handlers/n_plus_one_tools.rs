use crate::{models::DetectNPlusOneRequest, services::n_plus_one::Analyzer, tools::Engram};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::json;
use std::path::Path;

impl Engram {
    pub async fn handle_detect_n_plus_one(
        &self,
        req: DetectNPlusOneRequest,
    ) -> Result<CallToolResult, McpError> {
        let count = req.max_files.unwrap_or(8);
        let offset = req.file_offset.unwrap_or(0);
        if !(1..=16).contains(&count)
            || offset > 50_000
            || !(1..=65_536).contains(&req.max_tokens.unwrap_or(16_384))
        {
            return Err(McpError::invalid_params(
                "max_files must be 1..=16, file_offset<=50000 and max_tokens 1..=65536",
                None,
            ));
        }
        if req.file_path.as_ref().is_some_and(|p| {
            p.is_empty()
                || p.len() > 1024
                || p.chars().any(char::is_control)
                || !p.to_ascii_lowercase().ends_with(".vb")
                || offset != 0
        }) {
            return Err(McpError::invalid_params(
                "file_path must be a selected relative VB file of at most 1024 bytes; file_offset must be 0 for a file query",
                None,
            ));
        }
        let record = self.ensure_project_record(&req.project_id).await?;
        let runtime = self.ensure_project_runtime(&req.project_id).await?;
        let (generation, view) = self
            .pin_active_source_generation(&req.project_id, &runtime.search)
            .await?;
        let graph = self.state.graph.clone();
        let reservation = engram_core::memory::AllocationGuard::try_new(
            self.state.memory_budget.as_ref(),
            256 * 1024 * 1024,
            engram_core::memory::Subsystem::Graph,
            "N+1 source and invocation evidence",
        )
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let max_tokens = req.max_tokens.or(Some(16_384));
        let output=tokio::task::spawn_blocking(move||->anyhow::Result<String>{
            let _reservation=reservation;
            let selected=view.selected_file_revisions().ok_or_else(||anyhow::anyhow!("N+1 analysis requires managed source membership; update_project first"))?;
            anyhow::ensure!(selected.len()<=50_000,"Selected inventory exceeds 50000 files");
            let all:Vec<_>=selected.keys().filter(|p|p.to_ascii_lowercase().ends_with(".vb")).cloned().collect();
            let page:Vec<_>=if let Some(path)=&req.file_path {
                let path=path.replace('\\',"/");anyhow::ensure!(selected.contains_key(&path),"Requested VB file is not in the selected source inventory");vec![path]
            }else{all.iter().skip(offset).take(count).cloned().collect()};
            let mut analyzer=Analyzer::new(&graph,&req.project_id,Path::new(&record.directory),selected);
            for path in &page {analyzer.analyze_file(path);}
            let next=if req.file_path.is_none() && offset+page.len()<all.len() {
                let mut next=req.clone();next.file_offset=Some(offset+page.len());Some(json!({"tool":"detect_n_plus_one","arguments":next}))
            }else{None};
            Ok(serde_json::to_string(&json!({"status":"static_candidates","project_id":req.project_id,"active_generation":generation,
                "summary":format!("{} possible repeated database-call path(s) in {} selected VB file(s)",analyzer.findings.len(),page.len()),
                "findings":analyzer.findings,"files_attempted":page,"selected_vb_files":all.len(),"next_page":next,
                "coverage":analyzer.coverage(),"pagination_consistency":"Repeat while source generation and fingerprints remain unchanged; pages are not an atomic source/graph snapshot."}))?)
        }).await.map_err(|e|McpError::internal_error(e.to_string(),None))?
            .map_err(|e|McpError::internal_error(e.to_string(),None))?;
        let output = super::search_tools::response_budget::bounded_message(output, max_tokens)?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}
