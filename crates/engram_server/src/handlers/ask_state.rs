//! Explicit state questions reuse selected-source legacy graph relationships.
use crate::{
    services::ask_engine::{
        literal_state::{self, StateEvidence},
        plan::QueryPlan,
    },
    tools::Engram,
};
use engram_index::SourceSnapshotGuard;
use std::{sync::Arc, time::Duration};

pub(super) async fn gather(
    engram: &Engram,
    project: &str,
    question: &str,
    plan: &QueryPlan,
    generation: u64,
    view: Option<Arc<SourceSnapshotGuard>>,
    deadline: Duration,
) -> Option<StateEvidence> {
    let queries = literal_state::queries(question);
    if queries.is_empty() {
        return None;
    }
    let mut failure = StateEvidence::empty();
    let Some(view) = view else {
        failure.fail("Selected source unavailable for state relationships".into());
        return Some(failure);
    };
    let record = match engram.ensure_project_record(project).await {
        Ok(record) => record,
        Err(error) => {
            failure.fail(error.to_string());
            return Some(failure);
        }
    };
    let runtime = match engram.ensure_project_runtime(project).await {
        Ok(runtime) => runtime,
        Err(error) => {
            failure.fail(error.to_string());
            return Some(failure);
        }
    };
    if let Err(error) = runtime
        .search
        .validate_source_snapshot_guard(project, generation, &view)
    {
        failure.fail(error.to_string());
        return Some(failure);
    }
    let graph = engram.state.graph.clone();
    let budget = engram.state.memory_budget.clone();
    let project = project.to_owned();
    let scopes = plan.qualifiers.path_prefixes.clone();
    let work = tokio::task::spawn_blocking(move || {
        let selected = view.selected_file_revisions().ok_or_else(|| {
            anyhow::anyhow!(
                "State evidence requires managed source selection; update_project first"
            )
        })?;
        literal_state::collect(
            &graph,
            &project,
            std::path::Path::new(&record.directory),
            &queries,
            selected,
            &scopes,
            &budget,
        )
    });
    match tokio::time::timeout(deadline, work).await {
        Ok(Ok(Ok(result))) => Some(result),
        Ok(Ok(Err(error))) => {
            failure.fail(format!("{error:#}"));
            Some(failure)
        }
        Ok(Err(error)) => {
            failure.fail(error.to_string());
            Some(failure)
        }
        Err(_) => {
            failure.provider.status = crate::services::ask_engine::status::ProviderStatus::TimedOut;
            failure.provider.note = Some("State source deadline reached; the owned blocking reader may finish while retaining its source lease. No state facts returned.".into());
            Some(failure)
        }
    }
}
