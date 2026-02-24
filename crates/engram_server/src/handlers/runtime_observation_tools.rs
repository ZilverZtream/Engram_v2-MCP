use crate::models::{
    IngestRuntimeArtifactsRequest, RuntimeArtifactInputRequest, RuntimeArtifactKindRequest,
};
use crate::services::runtime_observation_service::{
    RuntimeArtifactInput, RuntimeArtifactKind, ingest_runtime_artifacts,
};
use crate::tools::Engram;
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

impl Engram {
    pub async fn handle_ingest_runtime_artifacts(
        &self,
        req: IngestRuntimeArtifactsRequest,
    ) -> Result<CallToolResult, McpError> {
        let _ = self.ensure_project_runtime(&req.project_id).await?;
        let generation = self.get_active_generation(&req.project_id).await?;
        let graph = self.state.graph.clone();
        let project_id = req.project_id.clone();

        let artifacts: Vec<RuntimeArtifactInput> =
            req.artifacts.into_iter().map(map_artifact).collect();

        let summary = tokio::task::spawn_blocking(move || {
            ingest_runtime_artifacts(&graph, &project_id, generation, &artifacts)
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        Ok(CallToolResult::success(vec![Content::text(format!(
            "✅ Runtime artifacts ingested: {} artifacts, {} rows, runtime control edges={}, runtime sql edges={}, merged static/runtime edges={}",
            summary.artifacts_processed,
            summary.evidence_rows_parsed,
            summary.observed_runtime_control_edges,
            summary.observed_runtime_sql_edges,
            summary.merged_static_edges
        ))]))
    }
}

fn map_artifact(input: RuntimeArtifactInputRequest) -> RuntimeArtifactInput {
    RuntimeArtifactInput {
        kind: match input.kind {
            RuntimeArtifactKindRequest::IisLog => RuntimeArtifactKind::IisLog,
            RuntimeArtifactKindRequest::CustomTrace => RuntimeArtifactKind::CustomTrace,
            RuntimeArtifactKindRequest::PageLifecycleSnapshot => {
                RuntimeArtifactKind::PageLifecycleSnapshot
            }
            RuntimeArtifactKindRequest::SqlProfilerExport => RuntimeArtifactKind::SqlProfilerExport,
        },
        content: input.content,
        label: input.label,
    }
}
