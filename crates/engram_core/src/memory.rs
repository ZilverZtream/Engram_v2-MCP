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
    ///
    /// MEM1: uses a CAS loop (`fetch_update`) instead of optimistic `fetch_add` +
    /// rollback to eliminate the transient over-commit window where concurrent
    /// callers could simultaneously exceed the hard limit before either rolls back.
    pub fn try_allocate(&self, bytes: u64, subsystem: Subsystem) -> MemoryDecision {
        let budget = self.inner.budget.load(Ordering::Relaxed);
        let soft_limit = (budget as f64 * SOFT_LIMIT_RATIO) as u64;

        // CAS loop: only commit the addition when the new value stays within budget.
        let result = self
            .inner
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                let new_used = current.saturating_add(bytes);
                if new_used > budget {
                    None // reject — don't commit
                } else {
                    Some(new_used)
                }
            });

        // MEM1 fix: fetch_update returns Ok(old_value). The actual new usage is
        // old_value + bytes (the closure guarantees new_used ≤ budget, so the
        // addition cannot overflow beyond the budget ceiling).
        let actual_new_used = match result {
            Err(_current) => {
                // Hard limit: CAS refused; no bytes were written.
                self.inner.pressure_active.store(true, Ordering::Relaxed);
                metrics::metrics().backpressure_rejections.inc();
                tracing::warn!(
                    budget,
                    subsystem = ?subsystem,
                    "Memory budget hard limit exceeded — rejecting allocation of {bytes} bytes"
                );
                return MemoryDecision::Rejected;
            }
            Ok(old_used) => old_used.saturating_add(bytes),
        };

        // Allocation committed — update subsystem counter and metrics.
        self.subsystem_counter(subsystem)
            .fetch_add(bytes, Ordering::Relaxed);
        metrics::metrics()
            .memory_bytes_used
            .set(actual_new_used as i64);

        if actual_new_used > soft_limit {
            self.inner.pressure_active.store(true, Ordering::Relaxed);
            metrics::metrics().memory_pressure_events.inc();
            tracing::debug!(
                used = actual_new_used,
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

/// RAII guard for memory allocations tracked by `MemoryBudget`.
///
/// Automatically releases the reserved bytes when dropped, ensuring cleanup on
/// success, error, or cancellation paths.
pub struct AllocationGuard {
    budget: MemoryBudget,
    bytes: u64,
    subsystem: Subsystem,
}

impl AllocationGuard {
    /// Attempt to reserve bytes and return a guard that will release on drop.
    pub fn try_new(
        budget: &MemoryBudget,
        bytes: u64,
        subsystem: Subsystem,
        operation: &str,
    ) -> anyhow::Result<Self> {
        match budget.try_allocate(bytes, subsystem) {
            MemoryDecision::Rejected => anyhow::bail!(
                "memory budget exceeded for {operation}: requested={}B, used={}B, budget={}B, subsystem={:?}",
                bytes,
                budget.used(),
                budget.budget(),
                subsystem
            ),
            MemoryDecision::SoftPressure => {
                tracing::warn!(
                    operation,
                    requested_bytes = bytes,
                    used_bytes = budget.used(),
                    budget_bytes = budget.budget(),
                    subsystem = ?subsystem,
                    "memory soft pressure while reserving bytes"
                );
            }
            MemoryDecision::Allowed => {}
        }

        Ok(Self {
            budget: budget.clone(),
            bytes,
            subsystem,
        })
    }
}

impl Drop for AllocationGuard {
    fn drop(&mut self) {
        self.budget.release(self.bytes, self.subsystem);
    }
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

    // ── MEM1: concurrent allocation stress test ───────────────────────────────
    // Verifies that under concurrent allocations near the hard limit:
    //   (a) the budget never remains permanently over limit after all operations settle
    //   (b) every rejected allocation results in a clean rollback (usage unchanged)
    // Note: a brief transient oversubscription window between fetch_add and the
    // rollback fetch_sub is an accepted trade-off of the lock-free design; this
    // test ensures the steady-state invariant holds after contention.
    #[test]
    fn budget_never_permanently_exceeds_hard_limit_under_concurrent_load() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let budget_bytes: u64 = 100_000;
        let mb = Arc::new(MemoryBudget::new(budget_bytes));
        // Deliberately exceed budget with many concurrent small allocations.
        let alloc_size: u64 = 10_000; // 10 of these = 100KB = exactly the budget
        let num_threads = 20; // 200KB total attempted — 2× over budget

        let rejected = Arc::new(AtomicU64::new(0));
        let allowed = Arc::new(AtomicU64::new(0));

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let mb_clone = mb.clone();
                let rejected_clone = rejected.clone();
                let allowed_clone = allowed.clone();
                std::thread::spawn(move || {
                    match mb_clone.try_allocate(alloc_size, Subsystem::Misc) {
                        MemoryDecision::Rejected => {
                            rejected_clone.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            allowed_clone.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread must not panic");
        }

        let final_used = mb.used();
        let accepted_count = allowed.load(Ordering::Relaxed);
        let rejected_count = rejected.load(Ordering::Relaxed);

        // After all threads complete, accepted allocations × alloc_size must equal used.
        assert_eq!(
            final_used,
            accepted_count * alloc_size,
            "MEM1: final used ({final_used}) must equal accepted_count ({accepted_count}) * alloc_size ({alloc_size})"
        );
        // The steady-state used must never exceed the hard budget.
        assert!(
            final_used <= budget_bytes,
            "MEM1: final used ({final_used}) must not exceed hard budget ({budget_bytes}) after rollback"
        );
        // Sanity: at least some threads must have been rejected (since 20 × 10KB > 100KB budget).
        assert!(
            rejected_count > 0,
            "MEM1: at least one allocation must be rejected when total demand exceeds budget; got 0 rejections"
        );
        // Sanity: total accepted + rejected must equal num_threads.
        assert_eq!(
            accepted_count + rejected_count,
            num_threads as u64,
            "MEM1: accepted + rejected must equal num_threads"
        );
    }

    // ── MEM1-h9f3: estimate-based accounting documentation ────────────────────

    /// MEM1-h9f3: `try_allocate` tracks the CALLER-REPORTED byte estimate, not
    /// actual RSS.  This is by design: the budget cannot read process RSS without
    /// blocking, so it relies on callers passing accurate estimates.
    ///
    /// This test documents the estimate-based property: the budget records exactly
    /// what the caller claims, and `used()` reflects those estimates, not real
    /// process memory.  If a caller passes 0 bytes, no budget is consumed.
    #[test]
    fn mem1_budget_tracks_caller_estimate_not_rss() {
        let mb = MemoryBudget::new(1_000_000);

        // Allocate an obviously impossible estimate (1 byte) — budget accepts it.
        // This documents that the check is against the tracked counter, not RSS.
        let decision = mb.try_allocate(1, Subsystem::Misc);
        assert_eq!(decision, MemoryDecision::Allowed);
        assert_eq!(
            mb.used(),
            1,
            "MEM1-h9f3: budget tracks the 1-byte estimate exactly"
        );

        // Allocating 0 bytes is a no-op — budget unaffected.
        let zero_decision = mb.try_allocate(0, Subsystem::Misc);
        assert_eq!(zero_decision, MemoryDecision::Allowed);
        assert_eq!(
            mb.used(),
            1,
            "MEM1-h9f3: 0-byte allocation must not change tracked usage"
        );
    }

    /// MEM1-h9f3: CAS prevents overcommit — two concurrent allocations that together
    /// exceed the budget have exactly one succeed.  The CAS loop ensures the
    /// losing thread sees the already-committed first allocation and rejects.
    #[test]
    fn mem1_cas_prevents_overcommit_at_exact_budget_boundary() {
        use std::sync::{Arc, Barrier};

        // Budget: 100 bytes.  Each thread wants 60 bytes.  Together = 120 > 100.
        // CAS guarantees exactly 1 succeeds (the second sees used=60, 60+60=120 > 100).
        let budget: u64 = 100;
        let alloc: u64 = 60;
        let mb = Arc::new(MemoryBudget::new(budget));
        let barrier = Arc::new(Barrier::new(2));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let mb_c = mb.clone();
                let bar = barrier.clone();
                std::thread::spawn(move || {
                    bar.wait(); // synchronize start to maximize contention
                    mb_c.try_allocate(alloc, Subsystem::Tantivy)
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let accepted = results
            .iter()
            .filter(|d| **d != MemoryDecision::Rejected)
            .count();
        let final_used = mb.used();

        assert_eq!(
            accepted, 1,
            "MEM1-h9f3: CAS must ensure exactly 1 of 2 concurrent over-budget allocations succeeds"
        );
        assert_eq!(
            final_used, alloc,
            "MEM1-h9f3: used must equal the one accepted allocation ({alloc} bytes), not both"
        );
    }
}
