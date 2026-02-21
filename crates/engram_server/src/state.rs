use dashmap::DashMap;
use engram_core::{Config, PathContext, Registry};
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

    /// Cooperative cancellation tokens for active jobs.
    pub cancellation_tokens: Arc<RwLock<HashMap<String, CancellationToken>>>,

    /// Counter for active indexing jobs (to throttle dreamer).
    pub active_indexing_count: Arc<std::sync::atomic::AtomicUsize>,

    /// Semaphore bounding concurrent parse/chunking blocking tasks.
    pub parse_semaphore: Arc<Semaphore>,

    /// Per-project update mutex. Prevents concurrent calls to update_project_impl
    /// for the same project (e.g. watcher + agent MCP call racing each other),
    /// which would corrupt Tantivy/LanceDB by writing the same generation twice.
    pub project_update_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,

    /// Send events to background actors.
    pub events_tx: broadcast::Sender<AppEvent>,
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

        Ok((
            Self {
                cfg: Arc::new(cfg),
                paths: Arc::new(paths),
                registry: Arc::new(registry),
                graph: Arc::new(graph),
                dreaming: Arc::new(dreaming),
                mimicry: Arc::new(StyleMimicryEngine::new()),
                immune: Arc::new(ImmuneEngine::default()),
                projects: Arc::new(DashMap::new()),
                project_lru: Arc::new(DashMap::new()),
                active_jobs: Arc::new(RwLock::new(HashMap::new())),
                cancellation_tokens: Arc::new(RwLock::new(HashMap::new())),
                active_indexing_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                parse_semaphore: Arc::new(Semaphore::new(parse_concurrency)),
                project_update_locks: Arc::new(RwLock::new(HashMap::new())),
                events_tx,
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
