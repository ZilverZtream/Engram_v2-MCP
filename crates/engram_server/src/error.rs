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
        // The match picks the JSON-RPC CODE; the message always comes from
        // the variant's own Display.
        //
        // Destructuring to get the message is how `ProjectNotFound` came to
        // report a bare project_id: it carries the id, not a sentence, and
        // its `#[error(...)]` attribute is what turns that into "Unknown
        // project_id: '<id>'. Call list_projects …". Passing the inner
        // String threw the guidance away — 24 times in the daemon log —
        // and the same shape silently duplicated TooManyJobs's text here,
        // where it had already drifted from the attribute above.
        let message = e.to_string();
        match e {
            EngramError::ProjectNotFound(_) | EngramError::InvalidParams(_) => {
                McpError::invalid_params(message, None)
            }
            _ => McpError::internal_error(message, None),
        }
    }
}
