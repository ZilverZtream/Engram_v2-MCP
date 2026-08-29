use crate::services::migration_progress_service::MigrationProgressStore;
use dashmap::{DashMap, DashSet};
use engram_core::{CheckpointStore, Config, MemoryBudget, PathContext, Registry};
use engram_graph::GraphStore;
use engram_index::HybridSearchEngine;
use engram_ml::{DreamingEngine, ImmuneEngine, StyleMimicryEngine};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

// MAX_PARSE_CONCURRENCY now comes from Config.max_parse_concurrency
// (defaults to num_cpus, capped at 16). See engram_core::config.

/// Maximum number of project search engines cached in memory simultaneously.
/// Beyond this limit, the least recently inserted project is evicted.
const MAX_CACHED_PROJECTS: usize = 5;

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// A completed search returned chunk_ids; record co-occurrence edges.
    SearchSession {
        project_id: String,
        hits: Vec<SearchHitLite>,
    },
    /// Force a dream pass (manual or periodic).
    TriggerDream { project_id: String },
    /// Enable or disable watching for a project.
    WatchUpdate {
        project_id: String,
        directory: String,
        enabled: bool,
    },
    /// VEC1/X1: vector table was recreated due to schema mismatch; all historical
    /// vector data was lost. Consumers must schedule a full reindex. Emitted by
    /// the index job when `open_or_create_table` returns `Recreated`.
    FullReindexRequired { project_id: String },
}

#[derive(Debug, Clone)]
pub struct SearchHitLite {
    pub pk: String,
    pub doc_id: String,
    pub path: engram_core::RelPath,
    pub chunk_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub project_id: String,
    pub project_name: String,
    pub project_type: String,
    pub directory: String,
    pub tantivy_dir: PathBuf,
    pub lancedb_dir: PathBuf,
}

#[derive(Clone)]
pub struct ProjectState {
    pub info: ProjectInfo,
    pub search: Arc<HybridSearchEngine>,
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub paths: Arc<PathContext>,

    pub registry: Arc<Registry>,
    pub graph: Arc<GraphStore>,

    pub dreaming: Arc<DreamingEngine>,
    pub mimicry: Arc<StyleMimicryEngine>,
    pub immune: Arc<ImmuneEngine>,

    /// Runtime cache: project_id -> open HybridSearchEngine handle.
    /// Uses DashMap for lock-striped concurrent access — reads and writes to
    /// different project keys never contend with each other.
    pub projects: Arc<DashMap<String, ProjectState>>,
    /// Last-access timestamps used to implement true LRU eviction.
    pub project_lru: Arc<DashMap<String, std::time::Instant>>,

    /// Active job cancellation handles.
    pub active_jobs: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,

    /// TODO-40: on-demand GC wakeup. Crash-resume cycles accumulate stale
    /// generations; completing a RESUMED job nudges the GC instead of
    /// waiting up to an hour for the next tick.
    pub gc_nudge: Arc<tokio::sync::Notify>,

    /// Cooperative cancellation tokens for active jobs.
    pub cancellation_tokens: Arc<RwLock<HashMap<String, CancellationToken>>>,

    /// Counter for active indexing jobs (to throttle dreamer).
    pub active_indexing_count: Arc<std::sync::atomic::AtomicUsize>,

    /// Completed GC sweeps (external audit 2026-08-29 P0-1: lets a test prove
    /// the scheduler's first sweep is delayed instead of firing at startup).
    pub gc_sweeps_completed: Arc<std::sync::atomic::AtomicU64>,

    /// Semaphore bounding concurrent parse/chunking blocking tasks.
    pub parse_semaphore: Arc<Semaphore>,

    /// Per-project update mutex. Prevents concurrent calls to update_project_impl
    /// for the same project (e.g. watcher + agent MCP call racing each other),
    /// which would corrupt Tantivy/LanceDB by writing the same generation twice.
    pub project_update_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,

    /// Send events to background actors.
    pub events_tx: broadcast::Sender<AppEvent>,

    /// Memory budget tracker for OOM prevention.
    pub memory_budget: Arc<MemoryBudget>,

    /// Durable checkpoint store for crash-safe job resume.
    pub checkpoints: Arc<CheckpointStore>,

    /// Migration progress tracker (standalone Redb database).
    pub migration_progress: Arc<MigrationProgressStore>,

    /// In-memory TTL cache for per-project PageRank centrality scores.
    ///
    /// Prevents issuing a `spawn_blocking` Redb read (or full graph recomputation)
    /// on every search request. Key: `"{project_id}:{generation}"`. Entries are
    /// considered stale after `PAGERANK_CACHE_TTL` seconds (defined in search_tools)
    /// and evicted on next access, triggering a background refresh.
    pub pagerank_cache: Arc<
        DashMap<
            String,
            (
                std::time::Instant,
                Arc<engram_graph::analysis::CentralityMetrics>,
            ),
        >,
    >,

    /// In-flight keys for PageRank background tasks.
    ///
    /// Before spawning a background PageRank task, a handler inserts the cache
    /// key into this set. If the insert returns `false` the key was already
    /// present (another task is running), so the spawn is skipped. The entry
    /// is removed by the background task when it finishes.
    pub pagerank_inflight: Arc<DashSet<String>>,

    /// Per-project cache of the git co-change walk (commit → changed files).
    ///
    /// `find_similar_changes` (also the co-change arm of `get_change_set`)
    /// used to re-diff up to 800 commits on EVERY call — 24 s measured live
    /// on the pilot corpus. History is immutable, so the walk result is cached keyed
    /// by the repo's HEAD oid: repeat calls skip git entirely until a new
    /// commit lands, and the long-lived shared daemon amortises the one
    /// slow walk across every connected session. Key: project_id.
    pub co_change_cache: Arc<DashMap<String, Arc<CoChangeSnapshot>>>,
    /// External audit 2026-08-29 row 1: the project's resx-derived EN↔SV lexicon,
    /// rebuilt when its resource files change (signature check on use).
    pub lexicon_cache: Arc<DashMap<String, Arc<crate::services::lexicon::Lexicon>>>,

    /// ADP1: Runtime kill-switch for autonomous decisions.
    ///
    /// Initialised from OR(config.adp_kill_switch, registry-persisted value)
    /// so that a kill-switch activated at runtime survives process restarts.
    /// Handlers that invoke the ADP pipeline read this flag rather than the
    /// (immutable) Config field, allowing runtime toggle without a restart.
    pub adp_kill_switch: Arc<std::sync::atomic::AtomicBool>,
}

/// One walked commit: oid + summary + the files it changed. Immutable once
/// walked, so safe to share via Arc across concurrent callers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CoChangeCommit {
    pub oid: String,
    pub summary: String,
    pub files: Vec<String>,
}

/// Cached co-change walk for one repo state.
///
/// Per-commit results are reusable forever: a commit's oid pins its diff, so
/// a later walk only has to diff the oids it has not seen. The snapshot is
/// therefore keyed by nothing — `walked_oids` IS the key, one entry per
/// already-diffed commit.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CoChangeSnapshot {
    /// HEAD oid the walk was last extended at. Informational — reuse is
    /// decided per oid, not per head.
    pub head: String,
    /// How many commits the walk covered (the sanitized max_commits used).
    pub walked: usize,
    /// Oldest→newest is NOT guaranteed; consumers score per-commit and don't
    /// depend on order beyond what the walker produced.
    pub commits: Vec<CoChangeCommit>,
    /// Every oid already diffed, INCLUDING the ones dropped as shape noise
    /// (bulk or empty commits). Without the dropped ones, each refresh would
    /// re-diff them forever.
    pub walked_oids: Vec<String>,
    /// True when the walk stopped on its time budget rather than on
    /// `max_commits`, so the caller can say the coverage is partial.
    pub partial: bool,
}

impl AppState {
    pub fn new(cfg: Config) -> anyhow::Result<(Self, broadcast::Receiver<AppEvent>)> {
        let paths = PathContext::new(cfg.allowed_roots.clone())?;

        let registry_path = cfg.data_dir.join("registry").join("registry.redb");
        let registry = Registry::open(&registry_path)?;

        let graph_path = cfg.data_dir.join("graph").join("graph.redb");
        let graph = GraphStore::open(&graph_path)?;

        // Capacity 16384 gives the dreamer and co-occurrence actor ample headroom
        // to drain events even during heavy concurrent searching. If a receiver
        // stalls (e.g., long PageRank), excess events are silently discarded via
        // `Lagged`, which only affects analytics quality, not correctness. The
        // increased capacity (from 4096) prevents drops during burst indexing jobs
        // that simultaneously trigger many search sessions.
        let dreaming = DreamingEngine::with_config(&cfg);
        let (events_tx, events_rx) = broadcast::channel(16_384);
        let parse_concurrency = cfg.max_parse_concurrency;
        let immune_warn = cfg.immune_warn_threshold;
        let immune_block = cfg.immune_block_threshold;

        // Memory budget
        let memory_budget = MemoryBudget::new(cfg.memory_budget_bytes);

        // Checkpoint store for crash-safe job resume
        let checkpoint_path = cfg.data_dir.join("checkpoints").join("checkpoints.redb");
        let checkpoints = CheckpointStore::open(&checkpoint_path)?;

        // Migration progress tracker
        let progress_path = cfg
            .data_dir
            .join("migration_progress")
            .join("migration_progress.redb");
        let migration_progress = MigrationProgressStore::open(&progress_path)?;

        // ADP1: load persisted kill-switch; OR with config value so that either
        // source can activate it. A kill-switch that was set at runtime persists
        // across restarts even if the config file no longer has the flag set.
        let persisted_kill_switch = registry.get_adp_kill_switch().unwrap_or(false);
        let effective_kill_switch = cfg.adp_kill_switch || persisted_kill_switch;
        // If the config has it set, make sure it's also reflected in the registry
        // so subsequent restarts without the config flag still see it as active.
        if cfg.adp_kill_switch && !persisted_kill_switch {
            let _ = registry.set_adp_kill_switch(true);
        }

        Ok((
            Self {
                cfg: Arc::new(cfg),
                paths: Arc::new(paths),
                registry: Arc::new(registry),
                graph: Arc::new(graph),
                dreaming: Arc::new(dreaming),
                mimicry: Arc::new(StyleMimicryEngine::new()),
                immune: Arc::new(ImmuneEngine::new(immune_warn, immune_block)),
                projects: Arc::new(DashMap::new()),
                project_lru: Arc::new(DashMap::new()),
                active_jobs: Arc::new(RwLock::new(HashMap::new())),
                gc_nudge: Arc::new(tokio::sync::Notify::new()),
                cancellation_tokens: Arc::new(RwLock::new(HashMap::new())),
                active_indexing_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                gc_sweeps_completed: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                parse_semaphore: Arc::new(Semaphore::new(parse_concurrency)),
                project_update_locks: Arc::new(RwLock::new(HashMap::new())),
                events_tx,
                memory_budget: Arc::new(memory_budget),
                checkpoints: Arc::new(checkpoints),
                migration_progress: Arc::new(migration_progress),
                pagerank_cache: Arc::new(DashMap::new()),
                pagerank_inflight: Arc::new(DashSet::new()),
                co_change_cache: Arc::new(DashMap::new()),
                lexicon_cache: Arc::new(DashMap::new()),
                adp_kill_switch: Arc::new(std::sync::atomic::AtomicBool::new(
                    effective_kill_switch,
                )),
            },
            events_rx,
        ))
    }

    /// Return (or lazily create) the per-project update mutex, then acquire it.
    ///
    /// Uses a fast read-lock path when the mutex already exists (common case),
    /// falling back to a write lock only for first-time creation. The `entry`
    /// API inside the write lock prevents TOCTOU races where two callers both
    /// see `None` from the read path and create duplicate mutexes.
    pub async fn acquire_project_update_lock(
        &self,
        project_id: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        // Fast path: read-only check (no write contention for existing projects).
        if let Some(lock) = self
            .project_update_locks
            .read()
            .await
            .get(project_id)
            .cloned()
        {
            return lock.lock_owned().await;
        }
        // Slow path: create the mutex. `entry` ensures only one is created even
        // if multiple callers race past the read check above.
        let lock = self
            .project_update_locks
            .write()
            .await
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }

    pub fn get_project_cached(&self, project_id: &str) -> Option<ProjectState> {
        let cached = self.projects.get(project_id).map(|r| r.value().clone());
        if cached.is_some() {
            // DashMap shard lock is released when the guard from `get()` is dropped
            // (above, via `map`). Updating LRU is a separate, non-blocking operation
            // on a different DashMap — no risk of cross-lock deadlocks.
            self.project_lru
                .insert(project_id.to_string(), std::time::Instant::now());
        }
        cached
    }

    /// Fix #10: Evict any projects that overshot MAX_CACHED_PROJECTS while all
    /// slots were busy indexing.  Call this immediately after decrementing
    /// `active_indexing_count` so the overshoot is cleaned up promptly.
    pub async fn evict_cache_overshoot(&self) {
        if self.projects.len() <= MAX_CACHED_PROJECTS {
            return;
        }
        let active_jobs = self.active_jobs.read().await;

        // Collect eviction candidates: idle projects sorted by LRU timestamp.
        let mut candidates: Vec<(String, std::time::Instant)> = self
            .projects
            .iter()
            .filter(|entry| !active_jobs.contains_key(entry.key()))
            .map(|entry| {
                let ts = self
                    .project_lru
                    .get(entry.key())
                    .map(|r| *r.value())
                    .unwrap_or(std::time::Instant::now());
                (entry.key().clone(), ts)
            })
            .collect();
        drop(active_jobs);

        candidates.sort_by_key(|(_, ts)| *ts);

        for (key, _) in candidates {
            if self.projects.len() <= MAX_CACHED_PROJECTS {
                break;
            }
            tracing::debug!(evicted = %key, "LRU-evicting overshoot project from cache");
            self.projects.remove(&key);
            self.project_lru.remove(&key);
        }
    }

    pub async fn put_project_cached(&self, ps: ProjectState) {
        // Evict the least-recently-used project when at capacity, but skip
        // projects with active indexing jobs to avoid mid-index corruption.
        if self.projects.len() >= MAX_CACHED_PROJECTS
            && !self.projects.contains_key(&ps.info.project_id)
        {
            let active_jobs = self.active_jobs.read().await;

            // Collect eviction candidates: idle projects sorted by LRU timestamp.
            let evict_key = self
                .projects
                .iter()
                .filter(|entry| !active_jobs.contains_key(entry.key()))
                .min_by_key(|entry| {
                    self.project_lru
                        .get(entry.key())
                        .map(|r| *r.value())
                        .unwrap_or(std::time::Instant::now())
                })
                .map(|entry| entry.key().clone());

            drop(active_jobs);

            if let Some(key) = evict_key {
                tracing::debug!(evicted = %key, "LRU-evicting project from cache (max={})", MAX_CACHED_PROJECTS);
                self.projects.remove(&key);
                self.project_lru.remove(&key);
            } else {
                // All cached projects are actively indexing — allow temporary overshoot.
                tracing::warn!(
                    "Cache at {} but all projects have active jobs; allowing temporary overshoot",
                    MAX_CACHED_PROJECTS
                );
            }
        }
        self.project_lru
            .insert(ps.info.project_id.clone(), std::time::Instant::now());
        self.projects.insert(ps.info.project_id.clone(), ps);
    }
}

/// RAII registration in `active_indexing_count` (external audit 2026-08-29
/// P0-1): every path that writes a generation — index jobs AND incremental
/// updates from the watcher or `update_project(wait=true)` — holds one of
/// these for its whole duration, so the GC's JOB1/JOB3 guards see it.
pub struct ActiveIndexingSlot(Arc<std::sync::atomic::AtomicUsize>);

impl ActiveIndexingSlot {
    pub fn acquire(state: &AppState) -> Self {
        state
            .active_indexing_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(state.active_indexing_count.clone())
    }
}

impl Drop for ActiveIndexingSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
