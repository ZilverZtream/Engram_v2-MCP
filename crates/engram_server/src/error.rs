use rmcp::ErrorData as McpError;

/// Centralized error type for the engram server.
///
/// All services return `Result<T, EngramError>` and the handler layer converts
/// to `McpError` at the boundary via the `From` impl below.
#[derive(Debug, thiserror::Error)]
pub enum EngramError {
    #[error("{0}")]
    Internal(String),

    #[error(
        "Unknown project_id: '{0}'. Call list_projects to see indexed projects, \
         or index_project to index a new directory."
    )]
    ProjectNotFound(String),

    #[error("{0}")]
    InvalidParams(String),

    #[error("Too many concurrent jobs running (limit: {0})")]
    TooManyJobs(usize),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),

    #[error("Task join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
}

impl From<EngramError> for McpError {
    fn from(e: EngramError) -> Self {
        match e {
            EngramError::ProjectNotFound(msg) => McpError::invalid_params(msg, None),
            EngramError::InvalidParams(msg) => McpError::invalid_params(msg, None),
            EngramError::TooManyJobs(limit) => {
                McpError::internal_error(format!("Too many concurrent jobs (limit: {limit})"), None)
            }
            other => McpError::internal_error(other.to_string(), None),
        }
    }
}
