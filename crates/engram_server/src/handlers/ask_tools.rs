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

/// Round-2 audit P1-4: the dreamer-insight arm is OFF unless the caller asks
/// for it (`include_insights: true`) — the ablation showed no measurable
/// effect, so the default must not spend retrieval budget on it.
pub fn insights_enabled(req: &crate::models::AskCodebaseRequest) -> bool {
    req.include_insights.unwrap_or(false)
}

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
        let question_for_resolve = req.question.clone();
        plan = tokio::task::spawn_blocking(move || {
            resolver::resolve_entities_in_context(
                &graph,
                &pid_for_resolve,
                &mut plan,
                &question_for_resolve,
            );
            plan
        })
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // Retrieve across the intent-specific arms.
        let ctx = RetrievalCtx {
            insights_enabled: insights_enabled(&req),
            search: ps.search.clone(),
            graph: self.state.graph.clone(),
            registry: self.state.registry.clone(),
            project_id: req.project_id.clone(),
            generation: gen_,
        };
        let (raw, providers) =
            retrieval::gather_evidence(&ctx, &plan, &req.question, depth, deadline, cancel).await;
        // Round-2 audit P0-4c (owner 2026-08-30): one bounded call-graph hop
        // from the files the first pass cited — the answer to "how does X get
        // authorized" is usually one call away from the entry point.
        let (raw, providers) = {
            let mut raw = raw;
            let mut providers = providers;
            let mut seeds: Vec<String> = Vec::new();
            for e in &raw {
                if let Some(p) = &e.path
                    && !p.starts_with("pr:")
                    && !p.starts_with("commit:")
                    && !seeds.contains(p)
                {
                    seeds.push(p.clone());
                }
                if seeds.len() >= 4 {
                    break;
                }
            }
            // P0-4d: only multi-hop questions (how/why/what breaks) get the hop;
            // a lookup ("which table", "which resource keys") does not.
            let wants_hop = plan.intents.iter().any(|(i, _)| {
                matches!(
                    i,
                    crate::services::ask_engine::plan::Intent::Explain
                        | crate::services::ask_engine::plan::Intent::Impact
                        | crate::services::ask_engine::plan::Intent::BugDiagnosis
                        | crate::services::ask_engine::plan::Intent::Rationale
                        | crate::services::ask_engine::plan::Intent::Compare
                )
            });
            if wants_hop && !seeds.is_empty() {
                let graph = self.state.graph.clone();
                let pid = req.project_id.clone();
                let question = req.question.clone();
                let project_dir = self
                    .state
                    .registry
                    .get_project(&pid)
                    .ok()
                    .flatten()
                    .map(|rec| std::path::PathBuf::from(rec.directory));
                let hop = tokio::task::spawn_blocking(move || {
                    let mut id = 10_000usize;
                    crate::services::ask_engine::providers::callee_evidence(
                        &graph,
                        project_dir.as_deref(),
                        &pid,
                        &seeds,
                        &question,
                        3,
                        &mut id,
                    )
                })
                .await
                .unwrap_or_default();
                let status = if hop.is_empty() {
                    status::ProviderStatus::Empty
                } else {
                    status::ProviderStatus::Hit
                };
                providers.push(status::ProviderReport {
                    provider: "callee".into(),
                    status,
                    count: hop.len(),
                    note: None,
                });
                raw.extend(hop);
            }
            (raw, providers)
        };

        // Rank (anti-anchoring), detect conflicts, snapshot, calibrate status.
        // Round-2 audit P0-4: the requested modality survives the cap.
        let raw_pool = raw.clone();
        let mut evidence = ranking::rank_and_select(raw, depth.evidence_cap());
        // P0-4e: one reserve pass — modality, needed kind, named file — with
        // protected eviction (no reserve evicts another reserve's item).
        ranking::reserve_required(&mut evidence, &raw_pool, &plan, &req.question);
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
        // Row 6 slice 2: the planner's resolved terms count as covered/anchoring.
        let known: Vec<String> = plan
            .entities
            .iter()
            .flat_map(|e| {
                std::iter::once(e.text.clone())
                    .filter(|_| !e.resolved.is_empty())
                    .chain(e.resolved.iter().map(|r| r.canonical.clone()))
            })
            .collect();
        let adequate = status::has_adequate_support_with(&req.question, &evidence, &known);
        let st = status::assess_status(&plan, &evidence, &providers, &snapshot, adequate);
        let mut unknowns = report::coverage_gaps(&plan, &evidence, &providers);
        // Name the premise terms nothing supports, first — the reader must not
        // fill them in from the answer's other evidence.
        for t in status::uncovered_named_terms_with(&req.question, &evidence, &known)
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
