//! Cognitive tool helpers and service delegates.
//!
//! These impl blocks add methods to `Engram` that are invoked by:
//!  - The #[tool] handlers in tools.rs (via self.*)
//!  - The cognitive_service module (via AppState)
//!
//! Ported from v1's dreaming.py cognitive pipeline.

use crate::services::{cognitive_service, job_service};
use crate::tools::Engram;

// ---------------------------------------------------------------------------
// Job management
// ---------------------------------------------------------------------------

/// Job management helper methods on Engram.
impl Engram {
    pub(crate) async fn cancel_job_internal(&self, job_id: &str) -> bool {
        job_service::cancel_job_internal(&self.state, job_id).await
    }
}

// ---------------------------------------------------------------------------
// Dreaming / cognitive service helpers
// ---------------------------------------------------------------------------

#[allow(dead_code)]
impl Engram {
    /// Trigger a dream cycle for a project and return insight count.
    /// Called by the `dream_project` MCP tool handler.
    pub(crate) async fn run_dream_cycle(
        &self,
        project_id: &str,
    ) -> anyhow::Result<usize> {
        cognitive_service::dream_project(&self.state, project_id).await
    }

    /// Analyze a file's coding style from git history.
    /// Falls back to AST/regex mimicry if no LLM is configured.
    pub(crate) async fn cognitive_analyze_file_style(
        &self,
        project_id: &str,
        file_path: &str,
        diff_limit: usize,
    ) -> cognitive_service::StyleAnalysisResult {
        cognitive_service::analyze_file_style(
            &self.state,
            project_id,
            file_path,
            diff_limit,
        )
        .await
    }

    /// Find files that frequently change together (temporal coupling).
    pub(crate) async fn cognitive_temporal_couplings(
        &self,
        project_id: &str,
        min_frequency: u32,
        limit: usize,
    ) -> anyhow::Result<Vec<cognitive_service::TemporalCoupling>> {
        cognitive_service::find_temporal_couplings(
            &self.state,
            project_id,
            min_frequency,
            limit,
        )
        .await
    }
}
