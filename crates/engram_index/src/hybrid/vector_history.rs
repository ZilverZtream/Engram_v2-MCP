//! Explicit, reader-exclusive pruning of verified native history.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeSessionRotationOutcome {
    Rotated { generation: u64, elapsed_ms: u128 },
    Deferred { reason: String, elapsed_ms: u128 },
    Skipped { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeHistoryPruneOutcome {
    Pruned {
        old_versions: u64,
        bytes_removed: u64,
        cache_rotation: NativeSessionRotationOutcome,
    },
    Deferred {
        reason: String,
    },
    Unavailable,
}

#[cfg(all(test, feature = "vector"))]
pub(super) struct HistoryPrunePause {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub completed: tokio::sync::Notify,
}

#[cfg(feature = "vector")]
mod native;

impl HybridSearchEngine {
    /// Call after explicit compaction has released its mutation authority.
    /// This never prunes automatically or changes the active source authority.
    /// Active native readers defer pruning. Logical source guards may remain:
    /// their selected physical rows stay referenced by the latest native view.
    /// After actual version retirement, optional cache-owner rotation keeps
    /// configured weighted limits unchanged. Its result/cost is separate from
    /// successful file pruning; subsequent native reads refill cold caches.
    /// Session-wrapper counts do not count independently cloned SDK cache handles.
    pub async fn prune_vector_history(
        &self,
        project: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<NativeHistoryPruneOutcome> {
        #[cfg(feature = "vector")]
        {
            if cancel.is_cancelled() {
                return Ok(NativeHistoryPruneOutcome::Deferred {
                    reason: "History pruning cancelled before admission".into(),
                });
            }
            let worker = self.clone();
            let project = project.to_owned();
            let child = cancel.child_token();
            let _cancel_on_drop = child.clone().drop_guard();
            let task = tokio::spawn(async move {
                let result = worker.prune_vector_history_inner(&project, &child).await;
                #[cfg(test)]
                if let Some(pause) = &worker.history_prune_pause {
                    pause.completed.notify_one();
                }
                result
            });
            tokio::select! {
                _ = cancel.cancelled() => Ok(NativeHistoryPruneOutcome::Deferred {
                    reason: "History maintenance wait cancelled; its worker retains exclusion until backend completion".into(),
                }),
                result = task => result.map_err(|error| anyhow::anyhow!("History maintenance worker failed: {error}"))?,
            }
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = (project, cancel);
            Ok(NativeHistoryPruneOutcome::Unavailable)
        }
    }
}

#[cfg(all(test, feature = "vector"))]
mod tests;
