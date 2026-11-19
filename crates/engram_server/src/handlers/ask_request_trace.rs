//! Explicit requests and page/control questions reuse the held source selection.
use crate::services::ask_engine::status::ProviderStatus;
use crate::{
    models::{AskCodebaseRequest, TraceUserRequestRequest},
    services::ask_engine::{
        plan::QueryPlan,
        request_trace::{self, RequestEvidence},
    },
    tools::Engram,
};
use engram_index::SourceSnapshotGuard;
use std::{sync::Arc, time::Duration};

pub(super) async fn gather(
    engram: &Engram,
    ask: &AskCodebaseRequest,
    plan: &QueryPlan,
    generation: u64,
    view: Option<Arc<SourceSnapshotGuard>>,
    deadline: Duration,
) -> Option<RequestEvidence> {
    let project = ask.project_id.as_str();
    let queries = request_trace::requests(&ask.question);
    if queries.is_empty() {
        return None;
    }
    let mut out = RequestEvidence::new(queries.len());
    if !plan.qualifiers.path_prefixes.is_empty() {
        out.note("Request source reads and ordinary recovery honor the requested path scopes. Excluded registrations, dependencies and callers remain unknown; applicable authored guidance is resolved separately.");
    }
    let Some(view) = view else {
        out.provider.status = ProviderStatus::Failed;
        out.note("Selected source unavailable; no request trace returned.");
        return Some(out);
    };
    let setup = async {
        let record = engram.ensure_project_record(project).await?;
        let runtime = engram.ensure_project_runtime(project).await?;
        runtime
            .search
            .validate_source_snapshot_guard(project, generation, &view)
            .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
        Ok::<_, rmcp::ErrorData>(record)
    }
    .await;
    let record = match setup {
        Ok(record) => record,
        Err(error) => {
            out.provider.status = ProviderStatus::Failed;
            out.note(&error.to_string());
            return Some(out);
        }
    };
    let start = tokio::time::Instant::now();
    for query in queries.iter().take(2) {
        let entry = &query.entry_point;
        out.provider.examined += 1;
        if let Err(error) = crate::services::request_trace::routing::resolve(entry, &[]) {
            out.provider.status = ProviderStatus::Failed;
            out.provider.truncated = true;
            out.note(&format!("{entry}: {error}"));
            continue;
        }
        let request = TraceUserRequestRequest {
            project_id: project.into(),
            path_prefixes: plan.qualifiers.path_prefixes.clone(),
            entry_point: entry.clone(),
            action: query.action.clone(),
            include_db: ask.request_trace_include_db.unwrap_or(true),
            include_triggers: true,
            max_tokens: Some(request_trace::TRACE_TOKENS),
            response_id: None,
            response_offset: 0,
        };
        let read = engram.trace_user_request_selected(
            request,
            record.clone(),
            generation,
            view.clone(),
            request_trace::TRACE_TOKENS * 4,
        );
        match tokio::time::timeout(deadline.saturating_sub(start.elapsed()), read).await {
            Ok(Ok(result)) => {
                let parsed = result
                    .content
                    .first()
                    .and_then(|content| content.as_text())
                    .ok_or_else(|| anyhow::anyhow!("Request trace returned no text"))
                    .and_then(|text| serde_json::from_str(&text.text).map_err(Into::into))
                    .and_then(|trace| out.retain(project, entry, generation, trace));
                if let Err(error) = parsed {
                    out.provider.status = ProviderStatus::Failed;
                    out.provider.truncated = true;
                    out.note(&format!("{entry}: {error:#}"));
                }
            }
            Ok(Err(error)) => {
                out.provider.status = ProviderStatus::Failed;
                out.provider.truncated = true;
                out.note(&format!("{entry}: {error}"));
            }
            Err(_) => {
                out.provider.status = ProviderStatus::TimedOut;
                out.provider.truncated = true;
                out.note("Request tracing reached its retrieval deadline. Any already returned traces remain; an owned blocking reader may finish while retaining its source lease. No unfinished trace is returned.");
                break;
            }
        }
    }
    out.note("At most two explicit HTTP or page/control requests are traced; mention discovery stops at the third distinct entry/action pair. The ordinary tracer retains its existing source, memory, routing and graph expansion limits. Control identifiers are caller-supplied; captions and runtime event dispatch are not inferred.");
    Some(out)
}
