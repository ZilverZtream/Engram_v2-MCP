use engram_core::{Config, PathContext, Registry};
use engram_graph::GraphStore;
use engram_index::HybridSearchEngine;
use engram_ml::{DreamingEngine, ImmuneEngine, StyleMimicryEngine};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

/// Maximum concurrent parse/chunk blocking tasks.
const MAX_PARSE_CONCURRENCY: usize = 4;

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
    pub projects: Arc<RwLock<HashMap<String, ProjectState>>>,
    /// Last-access timestamps used to implement true LRU eviction.
    pub project_lru: Arc<RwLock<HashMap<String, std::time::Instant>>>,

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

        // Capacity 4096 gives the dreamer ample headroom to drain events before
        // the `Lagged` error starts dropping co-occurrence data.  If the dreamer
        // stalls (e.g., long PageRank), excess events are silently discarded, but
        // this only affects search analytics quality, not correctness.
        let (events_tx, events_rx) = broadcast::channel(4096);

        Ok((
            Self {
                cfg: Arc::new(cfg),
                paths: Arc::new(paths),
                registry: Arc::new(registry),
                graph: Arc::new(graph),
                dreaming: Arc::new(DreamingEngine::new()),
                mimicry: Arc::new(StyleMimicryEngine::new()),
                immune: Arc::new(ImmuneEngine::default()),
                projects: Arc::new(RwLock::new(HashMap::new())),
                project_lru: Arc::new(RwLock::new(HashMap::new())),
                active_jobs: Arc::new(RwLock::new(HashMap::new())),
                cancellation_tokens: Arc::new(RwLock::new(HashMap::new())),
                active_indexing_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                parse_semaphore: Arc::new(Semaphore::new(MAX_PARSE_CONCURRENCY)),
                project_update_locks: Arc::new(RwLock::new(HashMap::new())),
                events_tx,
            },
            events_rx,
        ))
    }

    /// Return (or lazily create) the per-project update mutex, then acquire it.
    /// The returned guard keeps the lock held until dropped. Callers should hold
    /// it for the entire duration of `update_project_impl` to prevent concurrent
    /// watcher/agent updates from corrupting the same generation.
    pub async fn acquire_project_update_lock(
        &self,
        project_id: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let map = self.project_update_locks.read().await;
            map.get(project_id).cloned()
        };
        let lock = match lock {
            Some(l) => l,
            None => {
                let mut map = self.project_update_locks.write().await;
                map.entry(project_id.to_string())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                    .clone()
            }
        };
        lock.lock_owned().await
    }

    pub async fn get_project_cached(&self, project_id: &str) -> Option<ProjectState> {
        let map = self.projects.read().await;
        if let Some(ps) = map.get(project_id) {
            // Bump LRU timestamp on every access so the least-recently-used entry
            // is always the correct eviction candidate.
            drop(map);
            self.project_lru
                .write()
                .await
                .insert(project_id.to_string(), std::time::Instant::now());
            // Re-acquire to return the value (clone is cheap — Arc<Engine> inside).
            self.projects.read().await.get(project_id).cloned()
        } else {
            None
        }
    }

    pub async fn put_project_cached(&self, ps: ProjectState) {
        let mut map = self.projects.write().await;
        // Evict the least-recently-used project when at capacity, but skip
        // projects with active indexing jobs to avoid mid-index corruption.
        if map.len() >= MAX_CACHED_PROJECTS && !map.contains_key(&ps.info.project_id) {
            let active_jobs = self.active_jobs.read().await;
            let lru = self.project_lru.read().await;

            // Find the oldest idle project using LRU timestamps. Falls back to
            // an arbitrary idle project if no timestamp exists (newly inserted).
            let evict_key = map
                .keys()
                .filter(|k| !active_jobs.contains_key(*k))
                .min_by_key(|k| lru.get(*k).copied().unwrap_or(std::time::Instant::now()))
                .cloned();

            drop(lru);
            drop(active_jobs);

            if let Some(key) = evict_key {
                tracing::debug!(evicted = %key, "LRU-evicting project from cache (max={})", MAX_CACHED_PROJECTS);
                map.remove(&key);
                self.project_lru.write().await.remove(&key);
            } else {
                // All cached projects are actively indexing — allow temporary overshoot.
                tracing::warn!(
                    "Cache at {} but all projects have active jobs; allowing temporary overshoot",
                    MAX_CACHED_PROJECTS
                );
            }
        }
        self.project_lru
            .write()
            .await
            .insert(ps.info.project_id.clone(), std::time::Instant::now());
        map.insert(ps.info.project_id.clone(), ps);
    }
}
