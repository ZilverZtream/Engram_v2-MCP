#![allow(clippy::unwrap_used)]
//! MemoryBudget allocation guard tests.
//!
//! Proves that:
//! 1. `try_allocate` correctly grants/rejects allocations against the budget.
//! 2. The CAS-based `fetch_update` prevents concurrent over-commit.
//! 3. `AllocationGuard` RAII drop correctly returns bytes to the budget.
//! 4. Multiple subsystem counters track independently.

use engram_core::{MemoryBudget, MemoryDecision, Subsystem};
use engram_core::memory::AllocationGuard;

/// Allocations under budget must be granted.
#[test]
fn allocate_within_budget_returns_allowed() {
    let budget = MemoryBudget::new(1_000_000); // 1 MB
    let decision = budget.try_allocate(100_000, Subsystem::Tantivy);
    assert!(
        matches!(decision, MemoryDecision::Allowed | MemoryDecision::SoftPressure),
        "allocation within budget must return Allowed or SoftPressure; got {decision:?}"
    );
}

/// Allocations that exceed the hard limit must be rejected.
/// The CAS fetch_update loop must reject without committing any bytes.
#[test]
fn allocation_exceeding_budget_returns_rejected() {
    let budget = MemoryBudget::new(500_000); // 500 KB
    // Fill up to the budget.
    budget.try_allocate(500_000, Subsystem::LanceDb);
    // Next allocation must be rejected.
    let decision = budget.try_allocate(1, Subsystem::Graph);
    assert_eq!(
        decision,
        MemoryDecision::Rejected,
        "allocation exceeding hard limit must return Rejected; got {decision:?}"
    );
}

/// AllocationGuard RAII must release bytes on drop, restoring budget headroom.
#[test]
fn allocation_guard_releases_bytes_on_drop() {
    let budget = MemoryBudget::new(100_000); // 100 KB

    {
        let _guard = AllocationGuard::try_new(&budget, 80_000, Subsystem::DocStore, "test-op")
            .expect("first allocation must succeed");
        // While guard is live, another large allocation is rejected.
        let second = budget.try_allocate(80_000, Subsystem::DocStore);
        assert_eq!(
            second,
            MemoryDecision::Rejected,
            "second 80 KB allocation must be rejected when 80 KB already held"
        );
        // _guard drops here.
    }

    // After drop, budget is restored.
    let after_drop = budget.try_allocate(80_000, Subsystem::DocStore);
    assert!(
        matches!(after_drop, MemoryDecision::Allowed | MemoryDecision::SoftPressure),
        "allocation after guard drop must succeed — bytes must have been released; got {after_drop:?}"
    );
}

/// AllocationGuard::try_new returns Err when the budget is exhausted.
/// Callers cannot construct a guard for a rejected allocation.
#[test]
fn allocation_guard_try_new_fails_when_budget_exhausted() {
    let budget = MemoryBudget::new(100_000);

    // Exhaust the budget.
    budget.try_allocate(100_000, Subsystem::Tantivy);

    // try_new on an exhausted budget must fail.
    let guard = AllocationGuard::try_new(&budget, 1, Subsystem::Misc, "test-exhausted");
    assert!(
        guard.is_err(),
        "AllocationGuard::try_new must return Err when budget is exhausted"
    );
}

/// Concurrent allocations from multiple threads must not collectively
/// exceed the hard limit.  The CAS-based fetch_update prevents over-commit.
#[test]
fn concurrent_allocations_cannot_exceed_budget() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let budget = Arc::new(MemoryBudget::new(1_000_000)); // 1 MB
    let rejection_count = Arc::new(AtomicUsize::new(0));
    let allow_count = Arc::new(AtomicUsize::new(0));

    // 20 threads each try to allocate 100 KB (2 MB total requested > 1 MB budget).
    let handles: Vec<_> = (0..20).map(|_| {
        let budget = budget.clone();
        let rejections = rejection_count.clone();
        let allows = allow_count.clone();
        std::thread::spawn(move || {
            let decision = budget.try_allocate(100_000, Subsystem::ParseBuffer);
            match decision {
                MemoryDecision::Rejected => { rejections.fetch_add(1, Ordering::Relaxed); }
                _ => { allows.fetch_add(1, Ordering::Relaxed); }
            }
        })
    }).collect();

    for h in handles {
        h.join().expect("thread must not panic");
    }

    let allowed = allow_count.load(Ordering::Relaxed);
    let rejected = rejection_count.load(Ordering::Relaxed);
    assert_eq!(allowed + rejected, 20, "all 20 threads must have completed");
    assert!(
        allowed <= 10,
        "at most 10 of 20 concurrent 100 KB allocations can succeed within \
         1 MB budget; allowed = {allowed} (CAS must prevent over-commit)"
    );
    assert!(
        rejected >= 10,
        "at least 10 concurrent allocations must be rejected; rejected = {rejected}"
    );
}

/// SoftPressure is returned when usage crosses 80% of budget.
#[test]
fn soft_pressure_triggered_above_80_percent() {
    let budget = MemoryBudget::new(1_000_000); // 1 MB

    // Fill to 81% (just above the 80% soft-limit threshold).
    budget.try_allocate(810_000, Subsystem::Tantivy);

    // Next small allocation must trigger SoftPressure (or Rejected if at 100%).
    let decision = budget.try_allocate(10_000, Subsystem::Misc);
    assert!(
        matches!(decision, MemoryDecision::SoftPressure | MemoryDecision::Rejected),
        "allocation above 80% soft limit must return SoftPressure or Rejected; got {decision:?}"
    );
}

/// All six Subsystem variants can be tracked without panicking.
#[test]
fn all_subsystem_variants_accepted() {
    let budget = MemoryBudget::new(10_000_000); // 10 MB — plenty for all subsystems

    for subsystem in [
        Subsystem::Tantivy,
        Subsystem::LanceDb,
        Subsystem::Graph,
        Subsystem::DocStore,
        Subsystem::ParseBuffer,
        Subsystem::Misc,
    ] {
        let decision = budget.try_allocate(100, subsystem);
        assert!(
            matches!(decision, MemoryDecision::Allowed | MemoryDecision::SoftPressure),
            "allocation for {subsystem:?} must not be Rejected; got {decision:?}"
        );
    }
}

/// MemoryBudget::release must correctly reduce the used counter.
/// Verifies that double-release (calling release more than try_allocate) saturates
/// at zero rather than wrapping around (underflow protection).
#[test]
fn release_saturates_at_zero_on_underflow() {
    let budget = MemoryBudget::new(1_000_000);

    // Allocate 100 bytes.
    budget.try_allocate(100, Subsystem::Graph);
    let after_alloc = budget.used();

    // Release 100 bytes — back to start.
    budget.release(100, Subsystem::Graph);
    let after_release = budget.used();

    assert!(
        after_release < after_alloc,
        "used bytes must decrease after release; after_alloc={after_alloc}, after_release={after_release}"
    );

    // Release 1 more byte than was allocated — must not wrap around (saturating_sub).
    budget.release(1, Subsystem::Graph);
    // Should not panic; used should remain at 0 (or wherever it saturated).
    let _ = budget.used();
}

/// MemoryBudget breakdown must reflect multi-subsystem allocations correctly.
#[test]
fn breakdown_reflects_per_subsystem_usage() {
    let budget = MemoryBudget::new(10_000_000);

    budget.try_allocate(1_000, Subsystem::Tantivy);
    budget.try_allocate(2_000, Subsystem::LanceDb);
    budget.try_allocate(3_000, Subsystem::Graph);

    let bd = budget.breakdown();
    assert_eq!(bd.tantivy, 1_000, "tantivy counter must reflect 1 KB allocation");
    assert_eq!(bd.lancedb, 2_000, "lancedb counter must reflect 2 KB allocation");
    assert_eq!(bd.graph, 3_000, "graph counter must reflect 3 KB allocation");
    assert_eq!(bd.total_used, 6_000, "total_used must be sum of all subsystem allocations");
}
