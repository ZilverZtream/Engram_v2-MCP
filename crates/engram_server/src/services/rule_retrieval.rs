//! Qualified rule retrieval for the mutable business/quality corpora.
use engram_index::{HybridHit, HybridQuery, HybridSearchEngine};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub const FALLBACK_NOTE: &str = "Retrieval: lexical fallback (hybrid retrieval unavailable); semantic matches may be missing. Existing source and applicability checks still apply.\n";

pub struct RuleMatches {
    pub hits: Vec<HybridHit>,
    pub note: &'static str,
}

/// Keep the caller's complete namespace, project, filters and candidate limit.
/// This is for mutable rule corpora, not snapshot-backed source retrieval.
pub async fn search(
    engine: &Arc<HybridSearchEngine>,
    query: &HybridQuery,
    cancel: &CancellationToken,
) -> anyhow::Result<RuleMatches> {
    anyhow::ensure!(!cancel.is_cancelled(), "rule retrieval cancelled");
    match engine.search(query, None, cancel).await {
        Ok(hits) => Ok(RuleMatches { hits, note: "" }),
        Err(error) => {
            anyhow::ensure!(!cancel.is_cancelled(), "rule retrieval cancelled");
            tracing::warn!(namespace = %query.namespace, %error, "Hybrid rule retrieval failed; trying lexical retrieval");
            let engine = engine.clone();
            let query = query.clone();
            let hits = tokio::task::spawn_blocking(move || engine.lexical_search(&query)).await??;
            anyhow::ensure!(!cancel.is_cancelled(), "rule retrieval cancelled");
            Ok(RuleMatches {
                hits,
                note: FALLBACK_NOTE,
            })
        }
    }
}
