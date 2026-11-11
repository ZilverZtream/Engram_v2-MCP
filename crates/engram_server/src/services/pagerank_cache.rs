//! Optional ranking data belongs to one live project runtime, not every edit.
use engram_graph::analysis::CentralityMetrics;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OwnedSemaphorePermit;

const TTL: Duration = Duration::from_secs(300);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(5);

/// A point observation only: never an instruction to load or compute a map.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct TargetPageRank {
    pub score: Option<f32>,
    pub status: &'static str,
}

impl TargetPageRank {
    pub fn unavailable(status: &'static str) -> Self { Self { score: None, status } }
}

#[derive(Default)]
pub struct PageRankCache {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    generation: Option<u64>,
    value: Option<(Instant, Arc<CentralityMetrics>)>,
    inflight: Option<u64>,
    ticket: u64,
    retry_after: Option<Instant>,
}

impl State {
    fn observe(&mut self, generation: u64) {
        if self.generation.is_none_or(|previous| generation > previous) {
            self.generation = Some(generation);
            self.value = None;
            self.retry_after = None;
        }
        if self
            .value
            .as_ref()
            .is_some_and(|(created, _)| created.elapsed() >= TTL)
        {
            self.value = None;
        }
    }
}

impl PageRankCache {
    /// Called while all ranking worker permits are held, after graph-only
    /// replacement. A source generation alone cannot identify changed edges.
    pub(crate) fn invalidate_after_graph_rebuild(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.value = None;
        state.retry_after = None;
    }

    /// Read one retained same-generation target without changing cache state,
    /// expiring/dropping a map, waiting for work, or starting ranking work.
    pub fn peek_target_score(&self, generation: u64, target: &str) -> TargetPageRank {
        let Ok(state)=self.state.try_lock() else { return TargetPageRank::unavailable("cache_busy_or_unavailable"); };
        if state.generation!=Some(generation) { return TargetPageRank::unavailable("generation_not_cached"); }
        let Some((created,value))=&state.value else { return TargetPageRank::unavailable("result_not_cached"); };
        if created.elapsed()>=TTL { return TargetPageRank::unavailable("cached_result_expired"); }
        match value.pagerank.get(target).copied() {
            Some(score) if score.is_finite() && score>=0.0 && score<=1.0 => TargetPageRank {score:Some(score),status:"available_same_generation_in_memory"},
            Some(_) => TargetPageRank::unavailable("invalid_cached_target_score"),
            None => TargetPageRank::unavailable("target_not_cached"),
        }
    }

    /// Does not expire, populate or start ranking work while observing it.
    pub fn retention_observation(&self) -> serde_json::Value {
        let Ok(state) = self.state.try_lock() else {
            return serde_json::json!({"status":"busy_or_unavailable"});
        };
        serde_json::json!({"status":"observed","generation":state.generation,
            "retained_results":usize::from(state.value.is_some()),
            "external_result_references":state.value.as_ref().map_or(0,|(_,v)|Arc::strong_count(v).saturating_sub(1)),
            "expired_but_retained":state.value.as_ref().is_some_and(|(at,_)|at.elapsed()>=TTL),
            "work_in_flight":state.inflight.is_some()})
    }

    pub fn get(&self, generation: u64) -> Option<Arc<CentralityMetrics>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.observe(generation);
        (state.generation == Some(generation))
            .then(|| state.value.as_ref().map(|(_, value)| value.clone()))
            .flatten()
    }

    /// The caller moves this admission into the actual blocking worker. Dropping
    /// an RPC or its JoinHandle must not admit a replacement while it still runs.
    pub fn start(
        self: &Arc<Self>,
        generation: u64,
        permit: OwnedSemaphorePermit,
    ) -> Option<PageRankWork> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.observe(generation);
        if state.generation != Some(generation)
            || state.value.is_some()
            || state.inflight.is_some()
            || state
                .retry_after
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return None;
        }
        state.ticket = state.ticket.checked_add(1)?;
        let ticket = state.ticket;
        state.inflight = Some(ticket);
        Some(PageRankWork {
            cache: self.clone(),
            generation,
            ticket,
            _permit: permit,
        })
    }
}

pub struct PageRankWork {
    cache: Arc<PageRankCache>,
    generation: u64,
    ticket: u64,
    _permit: OwnedSemaphorePermit,
}

impl PageRankWork {
    pub fn complete(self, metrics: CentralityMetrics) {
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.inflight == Some(self.ticket) && state.generation == Some(self.generation) {
            state.value = Some((Instant::now(), Arc::new(metrics)));
            state.retry_after = None;
        }
    }
}

impl Drop for PageRankWork {
    fn drop(&mut self) {
        let mut state = self
            .cache
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.inflight == Some(self.ticket) {
            state.inflight = None;
            if state.generation == Some(self.generation) && state.value.is_none() {
                state.retry_after = Some(Instant::now() + FAILURE_COOLDOWN);
            }
        }
    }
}

#[cfg(test)]
mod tests;
