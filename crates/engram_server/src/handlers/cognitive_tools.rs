use crate::services::job_service;
use crate::tools::Engram;

/// Job management helper methods on Engram.
impl Engram {
    pub(crate) async fn cancel_job_internal(&self, job_id: &str) -> bool {
        job_service::cancel_job_internal(&self.state, job_id).await
    }
}
