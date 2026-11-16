//! A failed search publication must not hide a durable registry write.
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};
use serde_json::json;

#[derive(Debug)]
pub(crate) struct MemoryIndexFailure {
    error: McpError,
    vector_recovery_available: bool,
}

impl From<McpError> for MemoryIndexFailure {
    fn from(error: McpError) -> Self {
        Self {
            error,
            vector_recovery_available: false,
        }
    }
}

impl MemoryIndexFailure {
    pub(super) fn from_indexing(error: anyhow::Error) -> Self {
        // This typed failure is emitted after index_docs commits its lexical
        // documents. Other errors do not establish a recoverable lexical copy.
        let vector_recovery_available =
            engram_index::HybridSearchEngine::embedding_provider_failed(&error);
        Self {
            error: McpError::internal_error(format!("{error:#}"), None),
            vector_recovery_available,
        }
    }

    pub(super) fn stored_result(
        self,
        project_id: &str,
        section_id: &str,
        updated_at_ms: u64,
    ) -> CallToolResult {
        let mut next_tools = vec![json!({
            "tool":"read_memory_bank",
            "arguments":{"project_id":project_id,"section":section_id},
            "reason":"Read the saved section before any further edit."
        })];
        if self.vector_recovery_available {
            next_tools.push(json!({
                "tool":"repair_project",
                "arguments":{"project_id":project_id,"scope":"vector_only"},
                "reason":"After restoring the embedding provider, rebuild vectors from the committed search documents without repeating the memory write."
            }));
        } else {
            next_tools.push(json!({
                "tool":"project_health",
                "arguments":{"project_id":project_id},
                "reason":"Diagnose search publication before choosing recovery; a complete lexical copy has not been established."
            }));
        }
        CallToolResult::success(vec![
            Content::text(format!(
                "Saved memory_bank: {section_id}, but search indexing failed. \
                 The write is already committed. Do not repeat this write to repair search; \
                 retrying an append would duplicate its content."
            )),
            Content::text(json!({
                "status":"stored_search_incomplete",
                "stored":true,
                "project_id":project_id,
                "section_id":section_id,
                "updated_at_ms":updated_at_ms,
                "search_indexing":"failed",
                "lexical_indexing":if self.vector_recovery_available { "committed" } else { "unknown" },
                "indexing_error":self.error.message,
                "retry_write":false,
                "next_tools":next_tools
            }).to_string()),
        ])
    }
}
