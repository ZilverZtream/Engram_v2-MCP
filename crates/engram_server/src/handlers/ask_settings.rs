//! Named settings compose independent VB getter and appSettings declaration evidence.
use crate::{
    services::ask_engine::{
        config_settings,
        literal_settings::{self, SettingsEvidence},
        plan::QueryPlan,
    },
    tools::Engram,
};
use engram_index::SourceSnapshotGuard;
use std::{sync::Arc, time::Duration};

fn failed(config: bool, detail: String) -> SettingsEvidence {
    let mut out = if config { config_settings::empty() } else { SettingsEvidence::empty() };
    out.fail(detail);
    out
}

fn failures(detail: String) -> Vec<SettingsEvidence> {
    vec![failed(false, detail.clone()), failed(true, detail)]
}

fn observed(result: anyhow::Result<SettingsEvidence>, config: bool) -> Option<SettingsEvidence> {
    let result = result.unwrap_or_else(|error| failed(config, format!("{error:#}")));
    (!result.items.is_empty()
        || result.provider.status != crate::services::ask_engine::status::ProviderStatus::Empty
        || (config && result.provider.truncated && result.provider.available.unwrap_or(0) > 0))
        .then_some(result)
}

pub(super) async fn gather(
    engram: &Engram,
    project: &str,
    plan: &QueryPlan,
    generation: u64,
    view: Option<Arc<SourceSnapshotGuard>>,
    deadline: Duration,
) -> Vec<SettingsEvidence> {
    let mut keys = Vec::new();
    for entity in &plan.entities {
        if !entity.text.is_empty() && entity.text.len() <= 512 && !keys.contains(&entity.text) {
            keys.push(entity.text.clone());
        }
    }
    if keys.is_empty() {
        return Vec::new();
    }
    let Some(view) = view else {
        return failures("Selected source unavailable for named-setting observations".into());
    };
    let record = match engram.ensure_project_record(project).await {
        Ok(record) => record,
        Err(error) => {
            return failures(error.to_string());
        }
    };
    let runtime = match engram.ensure_project_runtime(project).await {
        Ok(runtime) => runtime,
        Err(error) => {
            return failures(error.to_string());
        }
    };
    if let Err(error) = runtime
        .search
        .validate_source_snapshot_guard(project, generation, &view)
    {
        return failures(error.to_string());
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
        let root = std::path::Path::new(&record.directory);
        let getters = literal_settings::collect(
            &graph, &project, root, &keys, selected, &scopes, &budget,
        );
        let declarations = config_settings::collect(
            &graph, &project, root, &keys, selected, &scopes, &budget,
        );
        Ok::<_, anyhow::Error>([
            observed(getters, false), observed(declarations, true),
        ].into_iter().flatten().collect::<Vec<_>>())
    });
    match tokio::time::timeout(deadline, work).await {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(error))) => failures(format!("{error:#}")),
        Ok(Err(error)) => failures(error.to_string()),
        Err(_) => {
            let mut out = failures("Named-setting source deadline reached; an owned blocking reader may still finish with its source lease. No settings facts returned.".into());
            for evidence in &mut out {
                evidence.provider.status = crate::services::ask_engine::status::ProviderStatus::TimedOut;
            }
            out
        }
    }
}
