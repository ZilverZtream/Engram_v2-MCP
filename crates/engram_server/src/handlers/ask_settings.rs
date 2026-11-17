//! Current named-setting questions reuse the shared VB literal getter extractor.
use crate::{
    services::ask_engine::{
        literal_settings::{self, SettingsEvidence},
        plan::QueryPlan,
    },
    tools::Engram,
};
use engram_index::SourceSnapshotGuard;
use std::{sync::Arc, time::Duration};

pub(super) async fn gather(
    engram: &Engram,
    project: &str,
    plan: &QueryPlan,
    generation: u64,
    view: Option<Arc<SourceSnapshotGuard>>,
    deadline: Duration,
) -> Option<SettingsEvidence> {
    let mut keys = Vec::new();
    for entity in &plan.entities {
        if !entity.text.is_empty() && entity.text.len() <= 512 && !keys.contains(&entity.text) {
            keys.push(entity.text.clone());
        }
    }
    if keys.is_empty() {
        return None;
    }
    let mut failure = SettingsEvidence::empty();
    let Some(view) = view else {
        failure.fail("Selected source unavailable for named-setting observations".into());
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
                "Named-setting evidence requires a managed source selection; update_project first"
            )
        })?;
        literal_settings::collect(
            &graph,
            &project,
            std::path::Path::new(&record.directory),
            &keys,
            selected,
            &scopes,
            &budget,
        )
    });
    match tokio::time::timeout(deadline, work).await {
        Ok(Ok(Ok(result))) => (!result.items.is_empty()
            || result.provider.status
                != crate::services::ask_engine::status::ProviderStatus::Empty)
            .then_some(result),
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
            failure.provider.note = Some("Named-setting source deadline reached; an owned blocking reader may still finish with its source lease. No settings facts returned.".into());
            Some(failure)
        }
    }
}
