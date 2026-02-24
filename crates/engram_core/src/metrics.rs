//! Production observability metrics for Engram-MCP.
//!
//! Provides lock-free, thread-safe metric primitives (counters, gauges, histograms)
//! that can be recorded from any crate and queried via the `get_metrics` MCP tool.
//! All metrics use `AtomicU64` for zero-contention recording.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Histogram (HDR-lite: fixed-bucket latency histogram)
// ---------------------------------------------------------------------------

/// Latency histogram with fixed log-scale buckets (ms).
/// Buckets: 1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000, 60000, +Inf
pub struct Histogram {
    buckets: Vec<AtomicU64>,
    bucket_bounds: Vec<u64>,
    sum: AtomicU64,
    count: AtomicU64,
    min: AtomicU64,
    max: AtomicU64,
}

const BUCKET_BOUNDS: &[u64] = &[
    1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
];

impl Histogram {
    pub fn new() -> Self {
        Self {
            buckets: (0..=BUCKET_BOUNDS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            bucket_bounds: BUCKET_BOUNDS.to_vec(),
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
        }
    }

    /// Record a latency value in milliseconds.
    pub fn record_ms(&self, ms: u64) {
        let idx = self
            .bucket_bounds
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(self.bucket_bounds.len());
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(ms, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // Update min (CAS loop)
        let mut current = self.min.load(Ordering::Relaxed);
        while ms < current {
            match self
                .min
                .compare_exchange_weak(current, ms, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
        // Update max (CAS loop)
        let mut current = self.max.load(Ordering::Relaxed);
        while ms > current {
            match self
                .max
                .compare_exchange_weak(current, ms, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }

    /// Record a duration (convenience wrapper).
    pub fn record_duration(&self, d: std::time::Duration) {
        self.record_ms(d.as_millis() as u64);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let sum = self.sum.load(Ordering::Relaxed);
        let min_val = self.min.load(Ordering::Relaxed);
        let max_val = self.max.load(Ordering::Relaxed);

        let mut bucket_counts = Vec::with_capacity(self.buckets.len());
        for b in &self.buckets {
            bucket_counts.push(b.load(Ordering::Relaxed));
        }

        HistogramSnapshot {
            count,
            sum_ms: sum,
            min_ms: if count == 0 { 0 } else { min_val },
            max_ms: max_val,
            avg_ms: if count == 0 {
                0.0
            } else {
                sum as f64 / count as f64
            },
            p50_ms: self.percentile(&bucket_counts, count, 50),
            p95_ms: self.percentile(&bucket_counts, count, 95),
            p99_ms: self.percentile(&bucket_counts, count, 99),
            bucket_bounds: self.bucket_bounds.clone(),
            bucket_counts,
        }
    }

    fn percentile(&self, bucket_counts: &[u64], total: u64, pct: u64) -> u64 {
        if total == 0 {
            return 0;
        }
        let target = (total * pct).div_ceil(100);
        let mut cumulative = 0u64;
        for (i, &c) in bucket_counts.iter().enumerate() {
            cumulative += c;
            if cumulative >= target {
                if i < self.bucket_bounds.len() {
                    return self.bucket_bounds[i];
                }
                return self.bucket_bounds.last().copied().unwrap_or(0) * 2;
            }
        }
        self.bucket_bounds.last().copied().unwrap_or(0) * 2
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub sum_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
    pub avg_ms: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bucket_bounds: Vec<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bucket_counts: Vec<u64>,
}

// ---------------------------------------------------------------------------
// Counter & Gauge
// ---------------------------------------------------------------------------

pub struct Counter(AtomicU64);

impl Counter {
    pub fn new() -> Self {
        Self(AtomicU64::new(0))
    }
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Gauge(AtomicI64);

impl Gauge {
    pub fn new() -> Self {
        Self(AtomicI64::new(0))
    }
    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Global MetricsRegistry (singleton, lock-free reads)
// ---------------------------------------------------------------------------

pub struct MetricsRegistry {
    boot_time: Instant,

    // Job metrics
    pub jobs_started: Counter,
    pub jobs_completed: Counter,
    pub jobs_failed: Counter,
    pub jobs_cancelled: Counter,
    pub jobs_active: Gauge,

    // Job latency histograms
    pub index_project_latency: Histogram,
    pub update_project_latency: Histogram,
    pub search_latency: Histogram,
    pub vector_search_latency: Histogram,
    pub graph_query_latency: Histogram,
    pub dream_latency: Histogram,
    pub immune_check_latency: Histogram,
    pub git_history_latency: Histogram,

    // Queue depths
    pub event_queue_depth: Gauge,
    pub parse_queue_depth: Gauge,

    // Index drift counters
    pub tantivy_docs_indexed: Counter,
    pub tantivy_docs_deleted: Counter,
    pub vector_docs_indexed: Counter,
    pub vector_docs_deleted: Counter,
    pub graph_nodes_upserted: Counter,
    pub graph_edges_upserted: Counter,
    pub graph_nodes_deleted: Counter,
    pub graph_edges_deleted: Counter,

    // Cardinality deltas (gauges tracking current counts)
    pub tantivy_doc_count: Gauge,
    pub vector_doc_count: Gauge,
    pub graph_node_count: Gauge,
    pub graph_edge_count: Gauge,

    // Repair outcomes
    pub repairs_triggered: Counter,
    pub repairs_succeeded: Counter,
    pub repairs_failed: Counter,
    pub integrity_checks_run: Counter,
    pub integrity_mismatches_found: Counter,

    // Memory tracking
    pub memory_bytes_used: Gauge,
    pub memory_budget_bytes: Gauge,
    pub memory_pressure_events: Counter,
    pub backpressure_rejections: Counter,

    // Checkpoint / crash recovery
    pub checkpoints_written: Counter,
    pub checkpoints_resumed: Counter,

    // WebForms confidence
    pub extractions_high_confidence: Counter,
    pub extractions_medium_confidence: Counter,
    pub extractions_low_confidence: Counter,

    // Safety rails
    pub refactors_blocked: Counter,
    pub refactors_approved: Counter,
}

impl MetricsRegistry {
    fn new() -> Self {
        Self {
            boot_time: Instant::now(),
            jobs_started: Counter::new(),
            jobs_completed: Counter::new(),
            jobs_failed: Counter::new(),
            jobs_cancelled: Counter::new(),
            jobs_active: Gauge::new(),
            index_project_latency: Histogram::new(),
            update_project_latency: Histogram::new(),
            search_latency: Histogram::new(),
            vector_search_latency: Histogram::new(),
            graph_query_latency: Histogram::new(),
            dream_latency: Histogram::new(),
            immune_check_latency: Histogram::new(),
            git_history_latency: Histogram::new(),
            event_queue_depth: Gauge::new(),
            parse_queue_depth: Gauge::new(),
            tantivy_docs_indexed: Counter::new(),
            tantivy_docs_deleted: Counter::new(),
            vector_docs_indexed: Counter::new(),
            vector_docs_deleted: Counter::new(),
            graph_nodes_upserted: Counter::new(),
            graph_edges_upserted: Counter::new(),
            graph_nodes_deleted: Counter::new(),
            graph_edges_deleted: Counter::new(),
            tantivy_doc_count: Gauge::new(),
            vector_doc_count: Gauge::new(),
            graph_node_count: Gauge::new(),
            graph_edge_count: Gauge::new(),
            repairs_triggered: Counter::new(),
            repairs_succeeded: Counter::new(),
            repairs_failed: Counter::new(),
            integrity_checks_run: Counter::new(),
            integrity_mismatches_found: Counter::new(),
            memory_bytes_used: Gauge::new(),
            memory_budget_bytes: Gauge::new(),
            memory_pressure_events: Counter::new(),
            backpressure_rejections: Counter::new(),
            checkpoints_written: Counter::new(),
            checkpoints_resumed: Counter::new(),
            extractions_high_confidence: Counter::new(),
            extractions_medium_confidence: Counter::new(),
            extractions_low_confidence: Counter::new(),
            refactors_blocked: Counter::new(),
            refactors_approved: Counter::new(),
        }
    }

    /// Return structured snapshot of all metrics.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            uptime_secs: self.boot_time.elapsed().as_secs(),
            jobs: JobMetrics {
                started: self.jobs_started.get(),
                completed: self.jobs_completed.get(),
                failed: self.jobs_failed.get(),
                cancelled: self.jobs_cancelled.get(),
                active: self.jobs_active.get(),
            },
            latencies: LatencyMetrics {
                index_project: self.index_project_latency.snapshot(),
                update_project: self.update_project_latency.snapshot(),
                search: self.search_latency.snapshot(),
                vector_search: self.vector_search_latency.snapshot(),
                graph_query: self.graph_query_latency.snapshot(),
                dream: self.dream_latency.snapshot(),
                immune_check: self.immune_check_latency.snapshot(),
                git_history: self.git_history_latency.snapshot(),
            },
            queues: QueueMetrics {
                event_queue_depth: self.event_queue_depth.get(),
                parse_queue_depth: self.parse_queue_depth.get(),
            },
            index_drift: IndexDriftMetrics {
                tantivy_docs_indexed: self.tantivy_docs_indexed.get(),
                tantivy_docs_deleted: self.tantivy_docs_deleted.get(),
                vector_docs_indexed: self.vector_docs_indexed.get(),
                vector_docs_deleted: self.vector_docs_deleted.get(),
                graph_nodes_upserted: self.graph_nodes_upserted.get(),
                graph_edges_upserted: self.graph_edges_upserted.get(),
                graph_nodes_deleted: self.graph_nodes_deleted.get(),
                graph_edges_deleted: self.graph_edges_deleted.get(),
            },
            cardinality: CardinalityMetrics {
                tantivy_doc_count: self.tantivy_doc_count.get(),
                vector_doc_count: self.vector_doc_count.get(),
                graph_node_count: self.graph_node_count.get(),
                graph_edge_count: self.graph_edge_count.get(),
            },
            repairs: RepairMetrics {
                triggered: self.repairs_triggered.get(),
                succeeded: self.repairs_succeeded.get(),
                failed: self.repairs_failed.get(),
                integrity_checks_run: self.integrity_checks_run.get(),
                integrity_mismatches_found: self.integrity_mismatches_found.get(),
            },
            memory: MemoryMetrics {
                bytes_used: self.memory_bytes_used.get(),
                budget_bytes: self.memory_budget_bytes.get(),
                pressure_events: self.memory_pressure_events.get(),
                backpressure_rejections: self.backpressure_rejections.get(),
            },
            recovery: RecoveryMetrics {
                checkpoints_written: self.checkpoints_written.get(),
                checkpoints_resumed: self.checkpoints_resumed.get(),
            },
            extraction_confidence: ExtractionConfidenceMetrics {
                high: self.extractions_high_confidence.get(),
                medium: self.extractions_medium_confidence.get(),
                low: self.extractions_low_confidence.get(),
            },
            safety: SafetyMetrics {
                refactors_blocked: self.refactors_blocked.get(),
                refactors_approved: self.refactors_approved.get(),
            },
        }
    }
}

/// Global singleton, accessible from any crate.
pub static METRICS: LazyLock<MetricsRegistry> = LazyLock::new(MetricsRegistry::new);

/// Convenience: get a reference to the global metrics.
pub fn metrics() -> &'static MetricsRegistry {
    &METRICS
}

// ---------------------------------------------------------------------------
// Snapshot types (serializable for JSON output)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub uptime_secs: u64,
    pub jobs: JobMetrics,
    pub latencies: LatencyMetrics,
    pub queues: QueueMetrics,
    pub index_drift: IndexDriftMetrics,
    pub cardinality: CardinalityMetrics,
    pub repairs: RepairMetrics,
    pub memory: MemoryMetrics,
    pub recovery: RecoveryMetrics,
    pub extraction_confidence: ExtractionConfidenceMetrics,
    pub safety: SafetyMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMetrics {
    pub started: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub active: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyMetrics {
    pub index_project: HistogramSnapshot,
    pub update_project: HistogramSnapshot,
    pub search: HistogramSnapshot,
    pub vector_search: HistogramSnapshot,
    pub graph_query: HistogramSnapshot,
    pub dream: HistogramSnapshot,
    pub immune_check: HistogramSnapshot,
    pub git_history: HistogramSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueMetrics {
    pub event_queue_depth: i64,
    pub parse_queue_depth: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDriftMetrics {
    pub tantivy_docs_indexed: u64,
    pub tantivy_docs_deleted: u64,
    pub vector_docs_indexed: u64,
    pub vector_docs_deleted: u64,
    pub graph_nodes_upserted: u64,
    pub graph_edges_upserted: u64,
    pub graph_nodes_deleted: u64,
    pub graph_edges_deleted: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardinalityMetrics {
    pub tantivy_doc_count: i64,
    pub vector_doc_count: i64,
    pub graph_node_count: i64,
    pub graph_edge_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairMetrics {
    pub triggered: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub integrity_checks_run: u64,
    pub integrity_mismatches_found: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMetrics {
    pub bytes_used: i64,
    pub budget_bytes: i64,
    pub pressure_events: u64,
    pub backpressure_rejections: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryMetrics {
    pub checkpoints_written: u64,
    pub checkpoints_resumed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionConfidenceMetrics {
    pub high: u64,
    pub medium: u64,
    pub low: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyMetrics {
    pub refactors_blocked: u64,
    pub refactors_approved: u64,
}

// ---------------------------------------------------------------------------
// Timer guard (RAII latency recording)
// ---------------------------------------------------------------------------

/// RAII guard that records elapsed time to a histogram on drop.
pub struct LatencyTimer<'a> {
    histogram: &'a Histogram,
    start: Instant,
}

impl<'a> LatencyTimer<'a> {
    pub fn new(histogram: &'a Histogram) -> Self {
        Self {
            histogram,
            start: Instant::now(),
        }
    }
}

impl Drop for LatencyTimer<'_> {
    fn drop(&mut self) {
        self.histogram.record_duration(self.start.elapsed());
    }
}

/// Start a latency timer that records to the given histogram on drop.
pub fn start_timer(histogram: &Histogram) -> LatencyTimer<'_> {
    LatencyTimer::new(histogram)
}

// ---------------------------------------------------------------------------
// Per-project metrics (stored in MetricsRegistry per-project map)
// ---------------------------------------------------------------------------

/// Per-project cardinality snapshot for drift detection.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectCardinality {
    pub project_id: String,
    pub tantivy_docs: u64,
    pub vector_docs: u64,
    pub graph_nodes: u64,
    pub graph_edges: u64,
    pub generation: u64,
    pub timestamp_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_basic() {
        let h = Histogram::new();
        h.record_ms(5);
        h.record_ms(50);
        h.record_ms(500);
        let snap = h.snapshot();
        assert_eq!(snap.count, 3);
        assert_eq!(snap.min_ms, 5);
        assert_eq!(snap.max_ms, 500);
    }

    #[test]
    fn counter_basic() {
        let c = Counter::new();
        c.inc();
        c.inc();
        c.add(3);
        assert_eq!(c.get(), 5);
    }

    #[test]
    fn gauge_basic() {
        let g = Gauge::new();
        g.set(10);
        assert_eq!(g.get(), 10);
        g.inc();
        assert_eq!(g.get(), 11);
        g.dec();
        assert_eq!(g.get(), 10);
    }

    #[test]
    fn global_metrics_accessible() {
        metrics().jobs_started.inc();
        assert!(metrics().jobs_started.get() >= 1);
    }
}
