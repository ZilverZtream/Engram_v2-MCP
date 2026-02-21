#![deny(clippy::print_stdout)]

pub mod benchmark;
pub mod checkpoint;
pub mod config;
// Note: runtime_evidence is pub but not re-exported at top level yet.
// Use engram_core::runtime_evidence::* directly.
pub mod ids;
pub mod memory;
pub mod metrics;
pub mod namespaces;
pub mod paths;
pub mod registry;
pub mod runtime_evidence;
pub mod security;
pub mod types;

pub use benchmark::{
    AdpCorpus, AdpScenario, AdpScenarioInput, BenchmarkPack, BenchmarkQueryEntry, BenchmarkReport,
    BenchmarkThresholds, DriftReport, TraceScenario, TraceScenarioLibrary,
};
pub use checkpoint::{Checkpoint, CheckpointStore, JobPhase};
pub use config::Config;
pub use ids::{ChunkId, ContentHash, DocId, DocIdStr, NodeId, ProjectId, build_pk};
pub use memory::{BoundedQueue, MemoryBudget, MemoryDecision, Subsystem};
pub use metrics::{MetricsRegistry, metrics, start_timer};
pub use namespaces::{
    KNOWN_NAMESPACES, NamespacePolicy, NamespaceRetention, NamespaceScope, NamespaceVersioning,
    get_namespace_scope, get_policy,
};
pub use paths::RelPath;
pub use security::PathContext;
pub use types::{EngramError, Result, guess_language};

pub use registry::{JobRecord, MemorySection, ProjectRecord, Registry, RepoRule, WatchRecord};

/// Initialize logging for tests, defaulting to INFO unless ENGRAM_TEST_LOG is set.
/// P3.4: Reduces Tantivy noise by defaulting to INFO.
pub fn setup_test_logging() {
    let filter = std::env::var("ENGRAM_TEST_LOG").unwrap_or_else(|_| "info".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
