use crate::{
    models::QuerySchemaRequest,
    services::schema_query::{self, Catalog},
    tools::Engram,
};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::json;
use std::{io::Read, path::Path};

#[path = "schema_policy.rs"]
mod schema_policy;

impl Engram {
    pub async fn handle_query_schema(
        &self,
        req: QuerySchemaRequest,
    ) -> Result<CallToolResult, McpError> {
        if !(1..=65536).contains(&req.max_tokens.unwrap_or(16_384)) {
            return Err(McpError::invalid_params("max_tokens must be 1..=65536", None));
        }
        let budget = super::search_tools::response_budget::token_bytes(req.max_tokens)?
            .unwrap_or(16_384 * 4);
        let delivery = crate::read_report_delivery::Report::handler_owned(
            "query_schema", req.project_id.clone(), budget, req.response_id.clone(), req.response_offset,
        )?;
        if let Some(original) = delivery.recover(&self.state.cfg.data_dir)? {
            return Ok(original);
        }
        let mut guidance = serde_json::to_value(&req)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
            .as_object().cloned().unwrap_or_default();
        let mut original = self.query_schema_evidence(req).await?;
        if original.content.first().and_then(|part| part.as_text())
            .is_some_and(|text| text.text.len() > budget) {
            // Capture the complete report as it would be delivered with ample
            // supported headroom, including its existing advisory actions.
            guidance.insert("max_tokens".into(), json!(65536));
        }
        crate::tool_guidance::annotate(
            "query_schema", &crate::tool_guidance::context(Some(&guidance)),
            &mut original, &self.tool_router.list_all(), self.state.cfg.advertise_all_tools,
        );
        delivery.finish(&self.state.cfg.data_dir, original)
    }

    /// Preserve the planner's existing complete-context size gate. A deferred
    /// public report is not a schema fact and must not enter its evidence slot.
    pub(super) async fn query_schema_for_planning(
        &self, req: QuerySchemaRequest,
    ) -> Result<CallToolResult, McpError> {
        let budget = req.max_tokens.or(Some(16_384));
        let original = self.query_schema_evidence(req).await?;
        let text = original.content.first().and_then(|part| part.as_text())
            .ok_or_else(|| McpError::internal_error("Schema evidence returned no text", None))?;
        super::search_tools::response_budget::bounded_message(text.text.clone(), budget)?;
        Ok(original)
    }

    async fn query_schema_evidence(
        &self, req: QuerySchemaRequest,
    ) -> Result<CallToolResult, McpError> {
        schema_query::validate(&req).map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        super::search_tools::response_budget::token_bytes(req.max_tokens)?;
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
            "SQL schema source evidence",
        )
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let output=tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let _reservation=reservation;
            let selected=view.selected_file_revisions().ok_or_else(||anyhow::anyhow!("Schema queries require a managed source inventory; update_project first"))?;
            anyhow::ensure!(selected.len()<=50_000,"Selected inventory exceeds 50000 paths");
            let mut catalog=Catalog::default();
            let mut admitted_paths=std::collections::BTreeSet::new();
            let (mut bytes,mut admitted,mut sql_files)=(0usize,0usize,0usize);
            for (path,revision) in selected {
                if !path.to_ascii_lowercase().ends_with(".sql") {continue;}
                sql_files+=1;
                let source=(|| -> anyhow::Result<String> {
                    let target=engram_core::safe_join(Path::new(&record.directory),path)?;
                    let mut content=String::new();
                    std::fs::File::open(target)?.take(2*1024*1024+1).read_to_string(&mut content)?;
                    anyhow::ensure!(content.len()<=2*1024*1024,"SQL file exceeds 2 MiB source limit");
                    let file=graph.get_node(&req.project_id,&format!("file:{path}"))?.ok_or_else(||anyhow::anyhow!("Indexed file fingerprint unavailable"))?;
                    anyhow::ensure!(file.generation==*revision,"File graph revision differs from selected source revision");
                    let hash=blake3::hash(content.as_bytes()).to_hex().to_string();
                    anyhow::ensure!(file.metadata.as_ref().and_then(|m|m.get("file_hash")).and_then(|v|v.as_str())==Some(hash.as_str()),"SQL source differs from indexed fingerprint; update_project before relying on schema");
                    Ok(content)
                })();
                match source {
                    Ok(source)=>{
                        bytes+=source.len();anyhow::ensure!(bytes<=64*1024*1024,"Schema source scan exceeds 64 MiB");
                        catalog.add_source(path,&source)?;admitted+=1;
                        admitted_paths.insert(path.clone());
                    },
                    Err(error)=>catalog.gaps.push(format!("{path}: {error}")),
                }
            }
            let mut result=catalog.query(&req)?;
            if let Some(name)=catalog.code_reference_name(&req) {
                let id=engram_core::ids::NodeId::table(&name).0;
                let edges=graph.find_incoming_edges(&req.project_id,Some(engram_graph::EdgeKind::QueriesTable),&id,51)?;
                let mut leads=Vec::new();let mut withheld=0;
                for (node_id,_) in edges.iter().take(50) {
                    let Some(node)=graph.get_node(&req.project_id,node_id)? else {withheld+=1;continue;};
                    let path=node.file_path.as_str();
                    let verified=(|| -> anyhow::Result<String> {
                        let revision=selected.get(path).ok_or_else(||anyhow::anyhow!("Source no longer selected"))?;
                        anyhow::ensure!(node.generation==*revision,"Reference revision changed");
                        let file=graph.get_node(&req.project_id,&format!("file:{path}"))?.ok_or_else(||anyhow::anyhow!("File fingerprint unavailable"))?;
                        anyhow::ensure!(file.generation==*revision,"File revision changed");
                        let target=engram_core::safe_join(Path::new(&record.directory),path)?;
                        let mut source=String::new();std::fs::File::open(target)?.take(2*1024*1024+1).read_to_string(&mut source)?;
                        anyhow::ensure!(source.len()<=2*1024*1024,"Code source exceeds 2 MiB");
                        let hash=blake3::hash(source.as_bytes()).to_hex().to_string();
                        anyhow::ensure!(file.metadata.as_ref().and_then(|m|m.get("file_hash")).and_then(|v|v.as_str())==Some(hash.as_str()),"Code source changed");
                        anyhow::ensure!(node.start_line>0 && node.end_line>=node.start_line && node.end_line as usize<=source.lines().count(),"Reference source span unavailable");
                        Ok(hash)
                    })();
                    match verified {
                        Ok(hash)=>{
                            admitted_paths.insert(path.to_owned());
                            leads.push(json!({"node_id":node.node_id,"file":path,"start_line":node.start_line,"end_line":node.end_line,"source_hash":hash}));
                        },
                        Err(_)=>withheld+=1,
                    }
                }
                result["code_reference_leads"]=json!({"results":leads,"withheld_stale_or_unavailable":withheld,"truncated":edges.len()>50,
                    "binding":"Indexed QueriesTable basename candidates only; qualified SQL target binding is not proved. Each returned code source fingerprint and physical revision was verified.",
                    "recovery":{"tool":"get_table_schema","arguments":{"project_id":req.project_id,"table_name":name}}});
            }
            result["project_id"]=json!(req.project_id);
            result["active_generation"]=json!(generation);
            result["coverage"]=json!({"selected_sql_files":sql_files,"admitted_sql_files":admitted,"source_bytes_read":bytes,"gaps":catalog.gaps,
                "evidence":"Source declarations with selected physical revision and BLAKE3 checks; no atomic graph/disk snapshot.",
                "schema_semantics":"SQL Server source inventory, not a live/deployed database schema. Supported ALTER TABLE ADD foreign-key declarations are reported separately with application_status=not_established; they do not modify the CREATE TABLE baseline. ALTER/DROP and execution order are not applied. Omitted nullability is unknown; nullable_columns returns explicit NULL only. Computed-column result types/nullability are unknown.",
                "name_matching":"Case-insensitive source-name lookup; database collation/default schema unknown. Unqualified relationship candidates are labelled, not bound.",
                "reference_semantics":"Procedure/view references are lexical source candidates; aliases, CTEs, dynamic SQL, trigger execution and code-to-database binding are not proved.",
                "code_references":"Table queries include at most 50 verified indexed code-reference leads; basename graph identities do not establish qualified database binding.",
                "database_execution":false});
            if let Some(next)=result["next_offset"].as_u64() {
                let mut recovery=req.clone();recovery.offset=Some(next as usize);
                result["next_page"]=json!({"tool":"query_schema","arguments":recovery,"consistency":"Repeat only while source generation and fingerprints remain unchanged"});
            }
            schema_policy::attach(Path::new(&record.directory), &mut result, &admitted_paths)?;
            Ok(serde_json::to_string(&result)?)
        }).await.map_err(|e|McpError::internal_error(e.to_string(),None))?
            .map_err(|e|McpError::internal_error(e.to_string(),None))?;
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}
