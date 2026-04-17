//! Handler for `ingest_code_review_history` — thin wrapper that
//! validates the request, builds an `IngestConfig`, calls the service,
//! and formats the IngestStats as either markdown or JSON.

use std::path::PathBuf;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::handlers::validate_project_id;
use crate::models::requests::IngestCodeReviewHistoryRequest;
use crate::services::code_review_ingest_service::{
    ingest_code_review_history as svc_ingest, IngestConfig, IngestSource,
};
use crate::services::project_service::ensure_project_record;
use crate::tools::Engram;

impl Engram {
    pub async fn handle_ingest_code_review_history(
        &self,
        req: IngestCodeReviewHistoryRequest,
    ) -> Result<CallToolResult, McpError> {
        validate_project_id(&req.project_id)?;

        let source = match req.source.as_str() {
            "json_file" => {
                let Some(fp) = req.file_path.clone() else {
                    return Err(McpError::invalid_params(
                        "source=\"json_file\" requires `file_path`",
                        None,
                    ));
                };
                IngestSource::JsonlFile {
                    path: PathBuf::from(fp),
                }
            }
            "azure_devops" => {
                let (Some(pat), Some(org), Some(project), Some(repo)) = (
                    req.pat_token.clone(),
                    req.org.clone(),
                    req.project.clone(),
                    req.repo.clone(),
                ) else {
                    return Err(McpError::invalid_params(
                        "source=\"azure_devops\" requires `pat_token`, `org`, `project`, `repo`",
                        None,
                    ));
                };
                IngestSource::AzureDevops {
                    org,
                    project,
                    repo,
                    pat_token: pat,
                    max_prs: req.max_prs,
                }
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown source `{other}` — use `json_file` or `azure_devops`"),
                    None,
                ));
            }
        };

        let _rec = ensure_project_record(&self.state, &req.project_id)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        let config = IngestConfig {
            source,
            min_fix_rate: req.min_fix_rate.clamp(0.0, 1.0),
            token_overlap_threshold: req.token_overlap_threshold.clamp(0.1, 0.95),
            force_full_rescan: req.force_full_rescan,
            use_llm_for_ambiguous: req.use_llm_for_ambiguous,
            ..Default::default()
        };

        let stats = svc_ingest(&self.state, &req.project_id, config)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Human-readable report — emphasise what the agent / CI cares
        // about: how many positive rules, how many suppression rules,
        // and how many got promoted to repo rules.
        let mut out = String::new();
        out.push_str("# Code-Review History Ingested\n\n");
        out.push_str(&format!(
            "**Raw comments read**: {} · **Parsed**: {} · **Skipped (meta/dup)**: {}\n",
            stats.total_raw, stats.parsed_success, stats.parsed_skipped
        ));
        out.push_str(&format!(
            "**Positive clusters**: {} indexed as anti-patterns\n",
            stats.clusters_produced
        ));
        out.push_str(&format!(
            "**Suppression clusters (wontFix)**: {} indexed as file-scoped suppressions\n",
            stats.suppression_clusters
        ));
        out.push_str(&format!(
            "**Anti-pattern docs indexed**: {} · **Suppression docs indexed**: {}\n",
            stats.antipattern_docs_indexed, stats.suppression_docs_indexed
        ));
        out.push_str(&format!(
            "**Graph nodes created**: {} · **Edges**: {}\n",
            stats.graph_nodes_created, stats.graph_edges_created
        ));
        out.push_str(&format!(
            "**Repo rules auto-promoted**: {} (fix_rate ≥ 0.7, PRs ≥ 3)\n",
            stats.repo_rules_promoted
        ));
        if stats.incremental_skipped_prs > 0 {
            out.push_str(&format!(
                "**Skipped via incremental state**: {} already-seen PRs\n",
                stats.incremental_skipped_prs
            ));
        }
        if let Some(pr) = stats.newest_pr_id {
            out.push_str(&format!("**Newest PR seen**: #{pr}\n"));
        }
        out.push_str(&format!("\n_Completed in {}ms._\n", stats.elapsed_ms));

        tracing::info!(
            project_id = %req.project_id,
            raw = stats.total_raw,
            parsed = stats.parsed_success,
            clusters = stats.clusters_produced,
            suppression = stats.suppression_clusters,
            elapsed_ms = stats.elapsed_ms,
            "ingest_code_review_history complete"
        );

        Ok(CallToolResult::success(vec![Content::text(out)]))
    }
}
