//! Memory budget tracking and backpressure for OOM prevention.
//!
//! Provides a `MemoryBudget` that tracks estimated memory usage across all
//! subsystems and enforces hard/soft limits. When the soft limit is breached,
//! new allocations proceed but a warning is logged. When the hard limit is
//! breached, backpressure is applied (new operations are rejected until memory
//! is freed).

use crate::metrics;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Default memory budget: 2 GB.
const DEFAULT_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Soft limit at 80% of budget triggers warnings and GC hints.
const SOFT_LIMIT_RATIO: f64 = 0.80;

/// Backpressure decision returned by `try_allocate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryDecision {
    /// Allocation granted, under soft limit.
    Allowed,
    /// Allocation granted but above soft limit — caller should shed load.
    SoftPressure,
    /// Allocation denied — hard limit breached.
    Rejected,
}

/// Thread-safe memory budget tracker.
///
/// All operations are lock-free using atomics.
#[derive(Clone)]
pub struct MemoryBudget {
    inner: Arc<MemoryBudgetInner>,
}

struct MemoryBudgetInner {
    /// Total budget in bytes.
    budget: AtomicU64,
    /// Current estimated usage in bytes.
    used: AtomicU64,
    /// Whether backpressure is active (for fast-path check).
    pressure_active: AtomicBool,
    /// Per-subsystem breakdown for diagnostics.
    tantivy_bytes: AtomicU64,
    lancedb_bytes: AtomicU64,
    graph_bytes: AtomicU64,
    docstore_bytes: AtomicU64,
    parse_buffer_bytes: AtomicU64,
    misc_bytes: AtomicU64,
}

impl MemoryBudget {
    pub fn new(budget_bytes: u64) -> Self {
        let budget = if budget_bytes == 0 {
            DEFAULT_BUDGET_BYTES
        } else {
            budget_bytes
        };
        metrics::metrics().memory_budget_bytes.set(budget as i64);
        Self {
            inner: Arc::new(MemoryBudgetInner {
                budget: AtomicU64::new(budget),
                used: AtomicU64::new(0),
                pressure_active: AtomicBool::new(false),
                tantivy_bytes: AtomicU64::new(0),
                lancedb_bytes: AtomicU64::new(0),
                graph_bytes: AtomicU64::new(0),
                docstore_bytes: AtomicU64::new(0),
                parse_buffer_bytes: AtomicU64::new(0),
                misc_bytes: AtomicU64::new(0),
            }),
        }
    }

    /// Attempt to allocate `bytes` from the budget.
    pub fn try_allocate(&self, bytes: u64, subsystem: Subsystem) -> MemoryDecision {
        let budget = self.inner.budget.load(Ordering::Relaxed);
        let soft_limit = (budget as f64 * SOFT_LIMIT_RATIO) as u64;
        let prev = self.inner.used.fetch_add(bytes, Ordering::Relaxed);
        let new_used = prev + bytes;

        // Update subsystem counter
        self.subsystem_counter(subsystem)
            .fetch_add(bytes, Ordering::Relaxed);

        // Update global metrics
        metrics::metrics().memory_bytes_used.set(new_used as i64);

        if new_used > budget {
            // Hard limit: roll back and reject
            self.inner.used.fetch_sub(bytes, Ordering::Relaxed);
            self.subsystem_counter(subsystem)
                .fetch_sub(bytes, Ordering::Relaxed);
            self.inner.pressure_active.store(true, Ordering::Relaxed);
            metrics::metrics().backpressure_rejections.inc();
            metrics::metrics()
                .memory_bytes_used
                .set(self.inner.used.load(Ordering::Relaxed) as i64);
            tracing::warn!(
                used = new_used,
                budget,
                subsystem = ?subsystem,
                "Memory budget hard limit exceeded — rejecting allocation of {bytes} bytes"
            );
            MemoryDecision::Rejected
        } else if new_used > soft_limit {
            self.inner.pressure_active.store(true, Ordering::Relaxed);
            metrics::metrics().memory_pressure_events.inc();
            tracing::debug!(
                used = new_used,
                soft_limit,
                subsystem = ?subsystem,
                "Memory soft limit exceeded — consider shedding load"
            );
            MemoryDecision::SoftPressure
        } else {
            self.inner.pressure_active.store(false, Ordering::Relaxed);
            MemoryDecision::Allowed
        }
    }

    /// Release previously allocated bytes back to the budget.
    pub fn release(&self, bytes: u64, subsystem: Subsystem) {
        let prev = self.inner.used.fetch_sub(bytes, Ordering::Relaxed);
        self.subsystem_counter(subsystem)
            .fetch_sub(bytes, Ordering::Relaxed);
        let new_used = prev.saturating_sub(bytes);
        metrics::metrics().memory_bytes_used.set(new_used as i64);

        let budget = self.inner.budget.load(Ordering::Relaxed);
        let soft_limit = (budget as f64 * SOFT_LIMIT_RATIO) as u64;
        if new_used <= soft_limit {
            self.inner.pressure_active.store(false, Ordering::Relaxed);
        }
    }

    /// Fast check: is backpressure currently active?
    pub fn is_under_pressure(&self) -> bool {
        self.inner.pressure_active.load(Ordering::Relaxed)
    }

    /// Current usage in bytes.
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Relaxed)
    }

    /// Total budget in bytes.
    pub fn budget(&self) -> u64 {
        self.inner.budget.load(Ordering::Relaxed)
    }

    /// Update budget at runtime (e.g. from config reload).
    pub fn set_budget(&self, budget_bytes: u64) {
        self.inner.budget.store(budget_bytes, Ordering::Relaxed);
        metrics::metrics()
            .memory_budget_bytes
            .set(budget_bytes as i64);
    }

    /// Get per-subsystem breakdown.
    pub fn breakdown(&self) -> MemoryBreakdown {
        MemoryBreakdown {
            total_used: self.inner.used.load(Ordering::Relaxed),
            budget: self.inner.budget.load(Ordering::Relaxed),
            tantivy: self.inner.tantivy_bytes.load(Ordering::Relaxed),
            lancedb: self.inner.lancedb_bytes.load(Ordering::Relaxed),
            graph: self.inner.graph_bytes.load(Ordering::Relaxed),
            docstore: self.inner.docstore_bytes.load(Ordering::Relaxed),
            parse_buffer: self.inner.parse_buffer_bytes.load(Ordering::Relaxed),
            misc: self.inner.misc_bytes.load(Ordering::Relaxed),
            pressure_active: self.inner.pressure_active.load(Ordering::Relaxed),
        }
    }

    fn subsystem_counter(&self, subsystem: Subsystem) -> &AtomicU64 {
        match subsystem {
            Subsystem::Tantivy => &self.inner.tantivy_bytes,
            Subsystem::LanceDb => &self.inner.lancedb_bytes,
            Subsystem::Graph => &self.inner.graph_bytes,
            Subsystem::DocStore => &self.inner.docstore_bytes,
            Subsystem::ParseBuffer => &self.inner.parse_buffer_bytes,
            Subsystem::Misc => &self.inner.misc_bytes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    Tantivy,
    LanceDb,
    Graph,
    DocStore,
    ParseBuffer,
    Misc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBreakdown {
    pub total_used: u64,
    pub budget: u64,
    pub tantivy: u64,
    pub lancedb: u64,
    pub graph: u64,
    pub docstore: u64,
    pub parse_buffer: u64,
    pub misc: u64,
    pub pressure_active: bool,
}

// ---------------------------------------------------------------------------
// Bounded queue for backpressure-aware event processing
// ---------------------------------------------------------------------------

/// Bounded async channel with backpressure semantics.
///
/// Unlike `broadcast::channel` which drops old events on overflow,
/// this returns `Err(Full)` so the caller can shed load explicitly.
pub struct BoundedQueue<T> {
    tx: tokio::sync::mpsc::Sender<T>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<T>>,
    capacity: usize,
}

impl<T: Send + 'static> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        Self {
            tx,
            rx: tokio::sync::Mutex::new(rx),
            capacity,
        }
    }

    /// Try to enqueue an item without blocking. Returns Err(item) if full.
    pub fn try_send(&self, item: T) -> Result<(), T> {
        self.tx.try_send(item).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(item) => item,
            tokio::sync::mpsc::error::TrySendError::Closed(item) => item,
        })
    }

    /// Receive the next item, waiting if empty.
    pub async fn recv(&self) -> Option<T> {
        self.rx.lock().await.recv().await
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_allows_under_soft_limit() {
        let mb = MemoryBudget::new(1_000_000);
        assert_eq!(
            mb.try_allocate(100_000, Subsystem::Tantivy),
            MemoryDecision::Allowed
        );
        assert_eq!(mb.used(), 100_000);
    }

    #[test]
    fn budget_soft_pressure_above_80_pct() {
        let mb = MemoryBudget::new(1_000_000);
        assert_eq!(
            mb.try_allocate(850_000, Subsystem::Tantivy),
            MemoryDecision::SoftPressure
        );
        assert!(mb.is_under_pressure());
    }

    #[test]
    fn budget_rejects_over_hard_limit() {
        let mb = MemoryBudget::new(1_000_000);
        mb.try_allocate(500_000, Subsystem::Tantivy);
        assert_eq!(
            mb.try_allocate(600_000, Subsystem::LanceDb),
            MemoryDecision::Rejected
        );
        // Usage didn't change (rolled back)
        assert_eq!(mb.used(), 500_000);
    }

    #[test]
    fn budget_release_clears_pressure() {
        let mb = MemoryBudget::new(1_000_000);
        mb.try_allocate(900_000, Subsystem::Graph);
        assert!(mb.is_under_pressure());
        mb.release(500_000, Subsystem::Graph);
        assert!(!mb.is_under_pressure());
    }

    #[test]
    fn breakdown_tracks_subsystems() {
        let mb = MemoryBudget::new(10_000_000);
        mb.try_allocate(1000, Subsystem::Tantivy);
        mb.try_allocate(2000, Subsystem::Graph);
        let bd = mb.breakdown();
        assert_eq!(bd.tantivy, 1000);
        assert_eq!(bd.graph, 2000);
        assert_eq!(bd.total_used, 3000);
    }

    #[tokio::test]
    async fn bounded_queue_rejects_when_full() {
        let q = BoundedQueue::new(2);
        assert!(q.try_send(1).is_ok());
        assert!(q.try_send(2).is_ok());
        assert!(q.try_send(3).is_err()); // full
    }
}
