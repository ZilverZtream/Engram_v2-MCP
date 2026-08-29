//! `ask_codebase` — the deterministic evidence engine's MCP entry point (M1).
//! Plans the question (multi-intent), resolves entities, runs parallel typed
//! retrieval, ranks by authority/directness, detects conflicts, calibrates an
//! honest status, and renders a `retrieval_only` report (Markdown and/or JSON).
//! No LLM in M1 — the calling agent synthesizes, or opts into depth=deep later.

use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};

use crate::services::ask_engine::report::{self, AskReport};
use crate::services::ask_engine::retrieval::{self, RetrievalCtx, parse_depth};
use crate::services::ask_engine::{planner, ranking, resolver, status};
use crate::tools::Engram;

impl Engram {
    pub async fn handle_ask_codebase(
        &self,
        req: crate::models::AskCodebaseRequest,
    ) -> Result<CallToolResult, McpError> {
        crate::handlers::validate_project_id(&req.project_id)?;
        if req.question.trim().is_empty() {
            return Err(McpError::invalid_params(
                "question must not be empty".to_string(),
                None,
            ));
        }
        let rec = self.ensure_project_record(&req.project_id).await?;
        let ps = self.ensure_project_runtime(&req.project_id).await?;
        let gen_ = self.get_active_generation(&req.project_id).await?;

        let depth = parse_depth(&req.depth);
        let deadline =
            Duration::from_millis(req.deadline_ms.unwrap_or(15_000).clamp(1_000, 60_000));
        let cancel = tokio_util::sync::CancellationToken::new();

        // Plan, then resolve entities against the graph (sync redb → spawn_blocking).
        let mut plan = planner::plan_query(&req.question);
        let graph = self.state.graph.clone();
        let pid_for_resolve = req.project_id.clone();
        plan = tokio::task::spawn_blocking(move || {
            resolver::resolve_entities(&graph, &pid_for_resolve, &mut plan);
            plan
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Retrieve across the intent-specific arms.
        let ctx = RetrievalCtx {
            insights_enabled: req.include_insights.unwrap_or(true),
            search: ps.search.clone(),
            graph: self.state.graph.clone(),
            registry: self.state.registry.clone(),
            project_id: req.project_id.clone(),
            generation: gen_,
        };
        let (raw, providers) =
            retrieval::gather_evidence(&ctx, &plan, &req.question, depth, deadline, cancel).await;

        // Rank (anti-anchoring), detect conflicts, snapshot, calibrate status.
        let evidence = ranking::rank_and_select(raw, depth.evidence_cap());
        let conflicts = ranking::detect_conflicts(&evidence, gen_);
        let snapshot = status::build_snapshot(
            &ctx,
            &rec,
            req.as_of.as_ref().and_then(|a| a.branch.as_deref()),
        )
        .await;
        // `incompatible` stays false: per-item snapshot mismatch can't be
        // reliably detected (graph nodes keep per-node generations); staleness
        // rides on reindex_required inside build_snapshot instead.
        let adequate = status::has_adequate_support(&req.question, &evidence);
        let st = status::assess_status(&plan, &evidence, &providers, &snapshot, adequate);
        let mut unknowns = report::coverage_gaps(&plan, &evidence, &providers);
        // Name the premise terms nothing supports, first — the reader must not
        // fill them in from the answer's other evidence.
        for t in status::uncovered_named_terms(&req.question, &evidence)
            .into_iter()
            .rev()
        {
            unknowns.insert(
                0,
                format!("no evidence mentions `{t}` — the question's premise may be false; do not assume it exists"),
            );
        }
        let next_best = report::next_best(&plan, &evidence, st);

        let out = AskReport {
            question: req.question.clone(),
            plan,
            status: st,
            mode: "retrieval_only".into(),
            evidence,
            conflicts,
            unknowns,
            next_best,
            snapshot,
            providers,
        };

        let body = match req.output_format.to_lowercase().as_str() {
            "json" => {
                serde_json::to_string_pretty(&report::to_json(&out)).unwrap_or_else(|_| "{}".into())
            }
            "both" => format!(
                "{}\n\n```json\n{}\n```",
                report::render_markdown(&out),
                serde_json::to_string_pretty(&report::to_json(&out)).unwrap_or_default()
            ),
            _ => report::render_markdown(&out),
        };
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}
