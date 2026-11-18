//! Optional semantic work must not erase usable lexical evidence.
use engram_index::{HybridHit, HybridQuery, HybridSearchEngine, SemanticQuality};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::ProviderStatus;

pub(super) struct Fallback {
    pub status: ProviderStatus,
    pub note: String,
}

pub(super) async fn search(
    engine: &HybridSearchEngine,
    query: &HybridQuery,
    cancel: &CancellationToken,
    semantic_budget: Option<Duration>,
) -> (anyhow::Result<Vec<HybridHit>>, Option<Fallback>) {
    let Some(budget) =
        semantic_budget.filter(|_| engine.semantic_quality() == SemanticQuality::Semantic)
    else {
        return (engine.search(query, None, cancel).await, None);
    };
    let attempt = tokio::select! {
        biased;
        _ = cancel.cancelled() => return (Err(anyhow::anyhow!("Evidence search cancelled")), None),
        result = tokio::time::timeout(budget, engine.search(query, None, cancel)) => result,
    };
    let fallback = match attempt {
        Ok(Ok(hits)) => return (Ok(hits), None),
        Ok(Err(_)) => Fallback {
            status: ProviderStatus::Failed,
            note: "Optional semantic retrieval failed; lexical fallback uses the same bounded query and filters. Semantic match coverage is unavailable.".into(),
        },
        Err(_) => Fallback {
            status: ProviderStatus::TimedOut,
            note: format!("Optional semantic retrieval exhausted its {} ms share; lexical fallback uses the same bounded query and filters. Semantic match coverage is unavailable.", budget.as_millis()),
        },
    };
    if cancel.is_cancelled() {
        return (Err(anyhow::anyhow!("Evidence search cancelled")), None);
    }
    // The provider retains its selected source capability through this read
    // and subsequent physical-body verification. No live source is substituted.
    (engine.lexical_search(query), Some(fallback))
}
