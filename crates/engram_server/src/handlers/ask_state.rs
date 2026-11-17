//! Explicit legacy and browser questions share the ask-held source selection.
use crate::{
    services::ask_engine::{
        browser_state,
        literal_state::{self, StateEvidence},
        plan::QueryPlan,
    },
    tools::Engram,
};
use engram_index::SourceSnapshotGuard;
use std::{sync::Arc, time::Duration};

fn failed(browser: bool, detail: String) -> StateEvidence {
    let mut failure = StateEvidence::empty();
    failure.fail(detail.clone());
    if browser {
        failure.provider.provider = "browser_state".into();
        failure.provider.note = Some(format!("{} {detail}", browser_state::INTERPRETATION));
    }
    failure
}

fn failures(legacy: bool, browser: bool, detail: String) -> Vec<StateEvidence> {
    let mut out = Vec::new();
    if legacy { out.push(failed(false, detail.clone())); }
    if browser { out.push(failed(true, detail)); }
    out
}

pub(super) async fn gather(
    engram: &Engram,
    project: &str,
    question: &str,
    plan: &QueryPlan,
    generation: u64,
    view: Option<Arc<SourceSnapshotGuard>>,
    deadline: Duration,
) -> Vec<StateEvidence> {
    let queries = literal_state::queries(question);
    let browser_queries = browser_state::queries(question);
    let legacy = !queries.is_empty();
    let browser = !browser_queries.is_empty();
    if !legacy && !browser {
        return Vec::new();
    }
    let Some(view) = view else {
        return failures(legacy, browser, "Selected source unavailable for state relationships".into());
    };
    let record = match engram.ensure_project_record(project).await {
        Ok(record) => record,
        Err(error) => {
            return failures(legacy, browser, error.to_string());
        }
    };
    let runtime = match engram.ensure_project_runtime(project).await {
        Ok(runtime) => runtime,
        Err(error) => {
            return failures(legacy, browser, error.to_string());
        }
    };
    if let Err(error) = runtime
        .search
        .validate_source_snapshot_guard(project, generation, &view)
    {
        return failures(legacy, browser, error.to_string());
    }
    let graph = engram.state.graph.clone();
    let budget = engram.state.memory_budget.clone();
    let project = project.to_owned();
    let scopes = plan.qualifiers.path_prefixes.clone();
    let work = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<StateEvidence>> {
        let selected = view.selected_file_revisions().ok_or_else(|| {
            anyhow::anyhow!(
                "State evidence requires managed source selection; update_project first"
            )
        })?;
        let root = std::path::Path::new(&record.directory);
        let mut out = Vec::new();
        if legacy {
            out.push(literal_state::collect(&graph, &project, root, &queries, selected, &scopes, &budget)
                .unwrap_or_else(|error| failed(false, format!("{error:#}"))));
        }
        if browser {
            let mut evidence = literal_state::collect(&graph, &project, root, &browser_queries, selected, &scopes, &budget)
                .unwrap_or_else(|error| failed(true, format!("{error:#}")));
            browser_state::configure(&mut evidence)?;
            for query in browser_queries.iter().take(3) {
                let clear = (|| -> anyhow::Result<serde_json::Value> {
                    let _reservation = engram_core::memory::AllocationGuard::try_new(
                        budget.as_ref(), 32 * 1024 * 1024,
                        engram_core::memory::Subsystem::Graph, "ask browser clear evidence")?;
                    crate::handlers::browser_state_tools::collect(&graph, &project, root,
                        query.state_type, &query.key, selected, 16, &scopes)
                })();
                match clear {
                    Ok(clear) => browser_state::append_clear(&mut evidence, &project, query, clear)?,
                    Err(error) => browser_state::clear_unavailable(&mut evidence, query, &format!("{error:#}")),
                }
            }
            out.push(evidence);
        }
        Ok(out)
    });
    match tokio::time::timeout(deadline, work).await {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(error))) => failures(legacy, browser, format!("{error:#}")),
        Ok(Err(error)) => failures(legacy, browser, error.to_string()),
        Err(_) => {
            let mut out = failures(legacy, browser, "State source deadline reached; the owned blocking reader may finish while retaining its source lease. No state facts returned.".into());
            for evidence in &mut out { evidence.provider.status = crate::services::ask_engine::status::ProviderStatus::TimedOut; }
            out
        }
    }
}
