//! Optional semantic work shares one budget; lexical evidence stays available.
use engram_index::{HybridHit, SemanticQuality};
use serde_json::{Value, json};
use std::{future::Future, time::Instant};

pub(super) struct Delivery {
    started: Instant,
    budget_ms: u64,
    unavailable: bool,
    exhausted: bool,
    used_semantics: bool,
    outcomes: Vec<Value>,
}

impl Delivery {
    pub fn new(budget_ms: u64) -> Self {
        Self {
            started: Instant::now(),
            budget_ms,
            unavailable: false,
            exhausted: false,
            used_semantics: false,
            outcomes: Vec::new(),
        }
    }

    /// None asks the caller to run its existing pinned, filtered lexical path.
    /// Errors from a lexical-only backend remain errors, not empty successes.
    pub async fn query<F>(
        &mut self,
        namespace: String,
        quality: SemanticQuality,
        work: F,
    ) -> anyhow::Result<Option<Vec<HybridHit>>>
    where
        F: Future<Output = anyhow::Result<Vec<HybridHit>>>,
    {
        if quality != SemanticQuality::Semantic {
            let hits = work.await?;
            self.outcomes
                .push(json!({"namespace":namespace,"status":"lexical_only_backend"}));
            return Ok(Some(hits));
        }
        let remaining = self
            .budget_ms
            .saturating_sub(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        let status = if self.unavailable {
            "lexical_fallback_provider_unavailable"
        } else if remaining == 0 || self.exhausted {
            "lexical_fallback_budget_exhausted"
        } else {
            match tokio::time::timeout(std::time::Duration::from_millis(remaining), work).await {
                Ok(Ok(hits)) => {
                    self.used_semantics = true;
                    self.outcomes
                        .push(json!({"namespace":namespace,"status":"hybrid_completed"}));
                    return Ok(Some(hits));
                }
                Ok(Err(_)) => {
                    self.unavailable = true;
                    "lexical_fallback_provider_unavailable"
                }
                Err(_) => {
                    self.exhausted = true;
                    "lexical_fallback_budget_exhausted"
                }
            }
        };
        self.outcomes
            .push(json!({"namespace":namespace,"status":status}));
        Ok(None)
    }

    pub fn quality(&self, requested: bool, configured: SemanticQuality) -> &'static str {
        if !requested || (configured == SemanticQuality::Semantic && !self.used_semantics) {
            "not_used"
        } else {
            super::structured_response::quality(configured)
        }
    }

    pub fn metadata(&self, requested: bool, configured: SemanticQuality) -> Value {
        json!({"semantic_requested":requested,
            "configured_semantic_quality":super::structured_response::quality(configured),
            "semantic_budget_ms":self.budget_ms,"namespaces":self.outcomes,
            "budget_scope":"One optional hybrid-attempt budget shared across project and user namespaces. Cooperative deadline: synchronous work is not interrupted. Excludes runtime startup, lexical/source work and rendering. A dropped provider request does not prove remote computation stopped."})
    }

    pub fn warning(&self) -> Option<String> {
        let fallback: Vec<_> = self
            .outcomes
            .iter()
            .filter(|row| {
                row["status"]
                    .as_str()
                    .is_some_and(|status| status.starts_with("lexical_fallback_"))
            })
            .collect();
        (!fallback.is_empty()).then(|| format!(
            "WARNING: optional semantic retrieval did not complete for these namespaces: {}. Their results use lexical fallback; semantic match coverage is unavailable. No semantic ranking is claimed for those results.\n",
            serde_json::to_string(&fallback).expect("plain JSON outcomes serialize")
        ))
    }
}
