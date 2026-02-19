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

    /// Active job cancellation handles.
    pub active_jobs: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,

    /// Cooperative cancellation tokens for active jobs.
    pub cancellation_tokens: Arc<RwLock<HashMap<String, CancellationToken>>>,

    /// Counter for active indexing jobs (to throttle dreamer).
    pub active_indexing_count: Arc<std::sync::atomic::AtomicUsize>,

    /// Semaphore bounding concurrent parse/chunking blocking tasks.
    pub parse_semaphore: Arc<Semaphore>,

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
                active_jobs: Arc::new(RwLock::new(HashMap::new())),
                cancellation_tokens: Arc::new(RwLock::new(HashMap::new())),
                active_indexing_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                parse_semaphore: Arc::new(Semaphore::new(MAX_PARSE_CONCURRENCY)),
                events_tx,
            },
            events_rx,
        ))
    }

    pub async fn get_project_cached(&self, project_id: &str) -> Option<ProjectState> {
        let map = self.projects.read().await;
        map.get(project_id).cloned()
    }

    pub async fn put_project_cached(&self, ps: ProjectState) {
        let mut map = self.projects.write().await;
        // Evict oldest entry if at capacity to bound RAM usage.
        if map.len() >= MAX_CACHED_PROJECTS && !map.contains_key(&ps.info.project_id) {
            // Remove an arbitrary entry (HashMap doesn't track insertion order,
            // but this still bounds the cache).
            if let Some(evict_key) = map.keys().next().cloned() {
                tracing::debug!(evicted = %evict_key, "Evicting project from cache (max={})", MAX_CACHED_PROJECTS);
                map.remove(&evict_key);
            }
        }
        map.insert(ps.info.project_id.clone(), ps);
    }
}
