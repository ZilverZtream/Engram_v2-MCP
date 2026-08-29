//! retrieval.rs (Task 6) — intent-specific arms run concurrently via a JoinSet,
//! each a self-contained boxed future that self-limits with `deadline`. Async
//! search arms await directly; sync graph arms run in spawn_blocking. Per-arm
//! local id counters; gather re-ids evidence globally at the end.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use engram_core::namespaces;
use engram_core::registry::Registry;
use engram_graph::GraphStore;
use engram_index::HybridSearchEngine;
use tokio_util::sync::CancellationToken;

use super::evidence::{Authority, EvidenceItem, EvidenceKind};
use super::plan::{Intent, QueryPlan};
use super::providers;
use super::status::{ProviderReport, ProviderStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Quick,
    Standard,
    Deep,
}
impl Depth {
    pub fn arm_top_k(self) -> usize {
        match self {
            Depth::Quick => 3,
            Depth::Standard => 6,
            Depth::Deep => 10,
        }
    }
    pub fn evidence_cap(self) -> usize {
        match self {
            Depth::Quick => 6,
            Depth::Standard => 10,
            Depth::Deep => 16,
        }
    }
}
pub fn parse_depth(s: &str) -> Depth {
    match s.to_lowercase().as_str() {
        "quick" => Depth::Quick,
        "deep" => Depth::Deep,
        _ => Depth::Standard,
    }
}

pub struct RetrievalCtx {
    /// Dream ablation switch (external audit 2026-08-29): when false the
    /// dreamer's `insights` arm is not run — and nothing else changes.
    pub insights_enabled: bool,
    pub search: Arc<HybridSearchEngine>,
    pub graph: Arc<GraphStore>,
    pub registry: Arc<Registry>,
    pub project_id: String,
    pub generation: u64,
}

type ArmOut = (String, Vec<EvidenceItem>, ProviderStatus, Option<String>);
type ArmFuture = Pin<Box<dyn Future<Output = ArmOut> + Send>>;

#[allow(clippy::too_many_arguments)]
fn search_arm(
    ctx: &RetrievalCtx,
    namespace: &'static str,
    kind: EvidenceKind,
    authority: Authority,
    provider: &'static str,
    question: &str,
    top_k: usize,
    generation: u64,
    deadline: Duration,
    cancel: CancellationToken,
) -> ArmFuture {
    let search = ctx.search.clone();
    let pid = ctx.project_id.clone();
    let gen_ = generation;
    let q = question.to_string();
    Box::pin(async move {
        let fut = async move {
            let mut id = 0usize;
            let (items, out) = providers::knowledge_evidence(
                &search, &pid, gen_, namespace, kind, authority, provider, &q, top_k, &cancel,
                &mut id,
            )
            .await;
            (provider.to_string(), items, out.status, out.note)
        };
        match tokio::time::timeout(deadline, fut).await {
            Ok(o) => o,
            Err(_) => (provider.to_string(), vec![], ProviderStatus::TimedOut, None),
        }
    })
}

fn memory_arm(ctx: &RetrievalCtx, question: &str, top_k: usize, deadline: Duration) -> ArmFuture {
    let registry = ctx.registry.clone();
    let pid = ctx.project_id.clone();
    let q = question.to_string();
    Box::pin(async move {
        let fut = async move {
            match tokio::task::spawn_blocking(move || {
                let mut id = 0usize;
                providers::memory_evidence(&registry, &pid, &q, top_k, &mut id)
            })
            .await
            {
                Ok((items, out)) => ("memory".to_string(), items, out.status, out.note),
                Err(e) => (
                    "memory".to_string(),
                    vec![],
                    ProviderStatus::Failed,
                    Some(e.to_string()),
                ),
            }
        };
        match tokio::time::timeout(deadline, fut).await {
            Ok(o) => o,
            Err(_) => ("memory".to_string(), vec![], ProviderStatus::TimedOut, None),
        }
    })
}

fn graph_arm<F>(provider: &'static str, deadline: Duration, f: F) -> ArmFuture
where
    F: FnOnce() -> (Vec<EvidenceItem>, providers::ProviderOutcome) + Send + 'static,
{
    Box::pin(async move {
        let fut = async move {
            match tokio::task::spawn_blocking(f).await {
                Ok((items, out)) => (provider.to_string(), items, out.status, out.note),
                Err(e) => (
                    provider.to_string(),
                    vec![],
                    ProviderStatus::Failed,
                    Some(e.to_string()),
                ),
            }
        };
        match tokio::time::timeout(deadline, fut).await {
            Ok(o) => o,
            Err(_) => (provider.to_string(), vec![], ProviderStatus::TimedOut, None),
        }
    })
}

/// Whether the dreamer-insight arm runs: Explain-intent questions only, and
/// only while insights are enabled (the Dream on/off ablation switch).
pub fn insight_arm_enabled(intents: &HashSet<Intent>, insights_enabled: bool) -> bool {
    insights_enabled && intents.contains(&Intent::Explain)
}

/// Run the intent-specific retrieval arms concurrently; return globally-re-id'd
/// evidence plus one ProviderReport per arm.
pub async fn gather_evidence(
    ctx: &RetrievalCtx,
    plan: &QueryPlan,
    question: &str,
    depth: Depth,
    deadline: Duration,
    cancel: CancellationToken,
) -> (Vec<EvidenceItem>, Vec<ProviderReport>) {
    let top_k = depth.arm_top_k();
    let intents: HashSet<Intent> = plan.intents.iter().map(|(i, _)| *i).collect();
    let has = |i: Intent| intents.contains(&i);

    // Source code is the workhorse evidence source for every question; the
    // ranker enforces precision, so always run it.
    let want_code = true;
    let want_doc = has(Intent::Explain)
        || has(Intent::Rationale)
        || has(Intent::Requirements)
        || has(Intent::Compare)
        || has(Intent::Unknowns);
    let want_business = has(Intent::Explain) || has(Intent::Impact) || has(Intent::BugDiagnosis);
    let want_insight = insight_arm_enabled(&intents, ctx.insights_enabled);
    let want_history = has(Intent::History) || has(Intent::Rationale) || has(Intent::Compare);
    let want_memory = has(Intent::Explain)
        || has(Intent::Rationale)
        || has(Intent::Feature)
        || has(Intent::Requirements)
        || has(Intent::Unknowns);
    let want_concept = has(Intent::Explain) || has(Intent::Usage) || has(Intent::Feature);
    let want_impact = has(Intent::Impact) || has(Intent::BugDiagnosis);
    let want_symbolrefs = has(Intent::Usage) || has(Intent::Test);
    let want_companion = has(Intent::Impact) || has(Intent::Feature);

    let impact_targets: Vec<String> = plan
        .entities
        .iter()
        .flat_map(|e| e.resolved.iter().filter_map(|r| r.node_id.clone()))
        .take(3)
        .collect();
    let symbol_names: Vec<String> = plan
        .entities
        .iter()
        .map(|e| e.text.clone())
        .take(3)
        .collect();
    let companion_files: Vec<String> = plan
        .entities
        .iter()
        .flat_map(|e| e.resolved.iter().filter_map(|r| r.node_id.clone()))
        .filter(|nid| nid.starts_with("file:"))
        .take(2)
        .collect();

    let mut arms: Vec<ArmFuture> = Vec::new();
    if want_code {
        arms.push(search_arm(
            ctx,
            namespaces::NAMESPACE_MEMORY,
            EvidenceKind::SourceCode,
            Authority::CurrentCode,
            "code",
            question,
            top_k,
            ctx.generation,
            deadline,
            cancel.clone(),
        ));
    }
    if want_doc {
        arms.push(search_arm(
            ctx,
            namespaces::NAMESPACE_MEMORY_BANK,
            EvidenceKind::DocSection,
            Authority::CurrentDocs,
            "doc",
            question,
            top_k,
            0,
            deadline,
            cancel.clone(),
        ));
    }
    if want_business {
        arms.push(search_arm(
            ctx,
            namespaces::NAMESPACE_BUSINESS_LOGIC,
            EvidenceKind::BusinessRule,
            Authority::DerivedBusinessLogic,
            "business_logic",
            question,
            top_k,
            0,
            deadline,
            cancel.clone(),
        ));
    }
    if want_insight {
        arms.push(search_arm(
            ctx,
            namespaces::NAMESPACE_INSIGHTS,
            EvidenceKind::Insight,
            Authority::DreamerInsight,
            "insight",
            question,
            top_k,
            0,
            deadline,
            cancel.clone(),
        ));
    }
    if want_history {
        arms.push(search_arm(
            ctx,
            namespaces::NAMESPACE_HISTORY,
            EvidenceKind::HistoryCommit,
            Authority::MergedHistory,
            "history",
            question,
            top_k,
            0,
            deadline,
            cancel.clone(),
        ));
    }
    if want_memory {
        arms.push(memory_arm(ctx, question, top_k, deadline));
    }
    if want_impact {
        for nid in impact_targets {
            let graph = ctx.graph.clone();
            let pid = ctx.project_id.clone();
            arms.push(graph_arm("impact", deadline, move || {
                let mut id = 0usize;
                providers::impact_evidence(&graph, &pid, &nid, 50, &mut id)
            }));
        }
    }
    if want_symbolrefs {
        for name in symbol_names {
            let graph = ctx.graph.clone();
            let pid = ctx.project_id.clone();
            arms.push(graph_arm("usage", deadline, move || {
                let mut id = 0usize;
                providers::symbol_ref_evidence(&graph, &pid, &name, None, 25, &mut id)
            }));
        }
    }
    if want_concept {
        let graph = ctx.graph.clone();
        let pid = ctx.project_id.clone();
        let concept = question.to_string();
        arms.push(graph_arm("concept", deadline, move || {
            let mut id = 0usize;
            providers::concept_evidence(&graph, &pid, &concept, top_k, &mut id)
        }));
    }
    if want_companion {
        for nid in companion_files {
            let graph = ctx.graph.clone();
            let pid = ctx.project_id.clone();
            arms.push(graph_arm("companion", deadline, move || {
                let mut id = 0usize;
                providers::companion_evidence(&graph, &pid, &nid, &mut id)
            }));
        }
    }

    let mut set: tokio::task::JoinSet<ArmOut> = tokio::task::JoinSet::new();
    for a in arms {
        set.spawn(a);
    }

    let mut evidence: Vec<EvidenceItem> = Vec::new();
    let mut reports: Vec<ProviderReport> = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((provider, mut items, status, note)) => {
                reports.push(ProviderReport {
                    provider,
                    status,
                    count: items.len(),
                    note,
                });
                evidence.append(&mut items);
            }
            Err(e) => reports.push(ProviderReport {
                provider: "unknown".into(),
                status: ProviderStatus::Failed,
                count: 0,
                note: Some(e.to_string()),
            }),
        }
    }
    // Arms complete in non-deterministic order; sort by a stable key BEFORE
    // assigning ev_N ids so ids (and everything keyed off them) are reproducible
    // run-to-run — the "deterministic evidence engine" contract.
    evidence.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.symbol_id.cmp(&b.symbol_id))
            .then_with(|| a.lines.cmp(&b.lines))
            .then_with(|| a.content.cmp(&b.content))
    });
    // Reports too, so the provider list renders stably.
    reports.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.count.cmp(&b.count)));
    for (i, ev) in evidence.iter_mut().enumerate() {
        ev.evidence_id = format!("ev_{}", i + 1);
    }
    (evidence, reports)
}
