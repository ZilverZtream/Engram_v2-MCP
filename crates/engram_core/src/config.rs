use crate::types::{EngramError, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Allowed roots for indexing (security boundary).
    pub allowed_roots: Vec<PathBuf>,

    /// Directory for all persistent data.
    pub data_dir: PathBuf,

    /// Optional max files/bytes safety limits.
    #[serde(default)]
    pub max_project_files: Option<u64>,
    #[serde(default)]
    pub max_project_bytes: Option<u64>,

    // Embeddings (keep this flexible - local model, Ollama, OpenAI, etc.)
    /// "local" (default, trigram projection), "ollama", or "openai"
    #[serde(default)]
    pub embedding_backend: String,
    /// Model name for local or remote embedding (e.g. "nomic-embed-text")
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Base URL for Ollama server (default: http://localhost:11434)
    #[serde(default)]
    pub ollama_url: Option<String>,
    /// API key for OpenAI-compatible embedding endpoint
    #[serde(default)]
    pub openai_api_key: Option<String>,
    /// Base URL for OpenAI-compatible API (default: https://api.openai.com/v1)
    #[serde(default)]
    pub openai_api_base: Option<String>,

    // LLM / Text generation (for dreaming insights and style analysis)
    /// "none" (deterministic fallback), "ollama", or "openai"
    #[serde(default)]
    pub llm_backend: String,
    /// Model name for LLM generation (e.g. "llama3.2", "gpt-4o-mini")
    #[serde(default)]
    pub llm_model: Option<String>,
    /// Ollama URL for LLM (falls back to ollama_url if not set)
    #[serde(default)]
    pub llm_ollama_url: Option<String>,
    /// API key for OpenAI-compatible LLM endpoint (falls back to openai_api_key if not set)
    #[serde(default)]
    pub llm_openai_api_key: Option<String>,
    /// Base URL for OpenAI-compatible LLM API (falls back to openai_api_base if not set)
    #[serde(default)]
    pub llm_openai_api_base: Option<String>,

    #[serde(default = "default_max_concurrent_jobs")]
    pub max_concurrent_jobs: usize,

    /// Maximum number of chunks produced per file during indexing.
    /// Previously hardcoded to 2000. Set in engram_mcp.yaml to override.
    #[serde(default = "default_max_chunks_per_file")]
    pub max_chunks_per_file: usize,

    /// Maximum number of git commits inspected per watcher-triggered update.
    /// Previously hardcoded to 50. Set in engram_mcp.yaml to override.
    #[serde(default = "default_max_commits_per_watch")]
    pub max_commits_per_watch: usize,

    /// Tantivy IndexWriter heap budget in bytes.
    /// Controls segment merge frequency — larger values reduce merges for big repos.
    /// Default 50 MB. Set higher (150_000_000) for repos with 100k+ files.
    #[serde(default = "default_tantivy_writer_memory")]
    pub tantivy_writer_memory: usize,

    /// MMR oversampling multiplier for hybrid search.
    /// fetch_k = top_k * this value. Higher values give MMR more diversity
    /// candidates but cost more compute. Default 5.
    #[serde(default = "default_mmr_oversampling")]
    pub mmr_oversampling: usize,

    /// Maximum concurrent parse/chunk blocking tasks.
    /// Defaults to the number of physical CPU cores (capped at 16).
    /// Set lower (2-4) on memory-constrained systems, higher (8-16) on beefy machines.
    #[serde(default = "default_max_parse_concurrency")]
    pub max_parse_concurrency: usize,

    // --- Dream (REM cycle) configuration ---
    /// Seconds of idle time before the dreamer actor triggers an auto-dream cycle.
    #[serde(default = "default_dream_idle_after_secs")]
    pub dream_idle_after_secs: u64,

    /// Tick interval in seconds for the dreamer actor's idle check.
    #[serde(default = "default_dream_tick_secs")]
    pub dream_tick_secs: u64,

    /// Default max co-occurrence clusters to process per dream cycle.
    #[serde(default = "default_dream_max_clusters")]
    pub dream_default_max_clusters: usize,

    /// Default minimum edge weight to include in co-occurrence clustering.
    #[serde(default = "default_dream_min_edge_weight")]
    pub dream_default_min_edge_weight: u32,

    /// Default minimum cluster size for insight generation.
    #[serde(default = "default_dream_min_cluster_size")]
    pub dream_default_min_cluster_size: usize,

    // --- Vector search configuration ---
    /// Timeout in milliseconds for a single vector search query.
    #[serde(default = "default_vector_search_timeout_ms")]
    pub vector_search_timeout_ms: u64,

    // --- Immune system configuration ---
    /// Default similarity threshold for WARN decisions.
    #[serde(default = "default_immune_warn_threshold")]
    pub immune_warn_threshold: f32,

    /// Default similarity threshold for BLOCK decisions.
    #[serde(default = "default_immune_block_threshold")]
    pub immune_block_threshold: f32,

    // --- Migration boundary configuration ---
    /// Timeout in seconds for LLM-based boundary suggestion.
    #[serde(default = "default_boundary_suggestion_timeout_secs")]
    pub boundary_suggestion_timeout_secs: u64,
}

fn default_max_concurrent_jobs() -> usize {
    2
}

fn default_max_chunks_per_file() -> usize {
    2000
}

fn default_max_commits_per_watch() -> usize {
    50
}

fn default_tantivy_writer_memory() -> usize {
    50_000_000 // 50 MB
}

fn default_mmr_oversampling() -> usize {
    5
}

fn default_max_parse_concurrency() -> usize {
    // Scale to hardware: use physical CPU count, capped to avoid memory pressure.
    std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4)
}

fn default_dream_idle_after_secs() -> u64 {
    20
}

fn default_dream_tick_secs() -> u64 {
    2
}

fn default_dream_max_clusters() -> usize {
    5
}

fn default_dream_min_edge_weight() -> u32 {
    2
}

fn default_dream_min_cluster_size() -> usize {
    3
}

fn default_vector_search_timeout_ms() -> u64 {
    5000
}

fn default_immune_warn_threshold() -> f32 {
    0.15
}

fn default_immune_block_threshold() -> f32 {
    0.45
}

fn default_boundary_suggestion_timeout_secs() -> u64 {
    120
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            data_dir: PathBuf::new(),
            max_project_files: None,
            max_project_bytes: None,
            embedding_backend: String::new(),
            embedding_model: None,
            ollama_url: None,
            openai_api_key: None,
            openai_api_base: None,
            llm_backend: String::new(),
            llm_model: None,
            llm_ollama_url: None,
            llm_openai_api_key: None,
            llm_openai_api_base: None,
            max_concurrent_jobs: default_max_concurrent_jobs(),
            max_chunks_per_file: default_max_chunks_per_file(),
            max_commits_per_watch: default_max_commits_per_watch(),
            tantivy_writer_memory: default_tantivy_writer_memory(),
            mmr_oversampling: default_mmr_oversampling(),
            max_parse_concurrency: default_max_parse_concurrency(),
            dream_idle_after_secs: default_dream_idle_after_secs(),
            dream_tick_secs: default_dream_tick_secs(),
            dream_default_max_clusters: default_dream_max_clusters(),
            dream_default_min_edge_weight: default_dream_min_edge_weight(),
            dream_default_min_cluster_size: default_dream_min_cluster_size(),
            vector_search_timeout_ms: default_vector_search_timeout_ms(),
            immune_warn_threshold: default_immune_warn_threshold(),
            immune_block_threshold: default_immune_block_threshold(),
            boundary_suggestion_timeout_secs: default_boundary_suggestion_timeout_secs(),
        }
    }
}

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        let p = ProjectDirs::from("io", "engram", "engram")
            .ok_or_else(|| EngramError::Config("unable to resolve config dir".into()))?;
        Ok(p.config_dir().join("engram_mcp.yaml"))
    }

    pub fn load() -> Result<Self> {
        let path = std::env::var_os("ENGRAM_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Self::default_path().unwrap_or_else(|_| PathBuf::from("engram_mcp.yaml"))
            });
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut cfg: Config = serde_yaml::from_slice(&bytes)?;

        // Default allowed_roots to the current working directory if not specified.
        // This enables ad-hoc local use without requiring explicit config while
        // still enforcing the security boundary once the server starts.
        if cfg.allowed_roots.is_empty() {
            let cwd = std::env::current_dir().map_err(|e| {
                EngramError::Config(format!(
                    "allowed_roots is empty and cannot determine cwd: {e}"
                ))
            })?;
            tracing::warn!(
                "allowed_roots is empty — defaulting to current directory: {}",
                cwd.display()
            );
            cfg.allowed_roots.push(cwd);
        }

        // Default data_dir to the platform-standard data directory if not specified.
        if cfg.data_dir.as_os_str().is_empty() {
            if let Some(dirs) = ProjectDirs::from("io", "engram", "engram") {
                cfg.data_dir = dirs.data_dir().to_path_buf();
                tracing::warn!(
                    "data_dir is empty — defaulting to platform data dir: {}",
                    cfg.data_dir.display()
                );
            } else {
                return Err(EngramError::Config("data_dir must be set".into()));
            }
        }
        if cfg.embedding_backend.trim().is_empty() {
            cfg.embedding_backend = "local".into();
        }
        if cfg.llm_backend.trim().is_empty() {
            cfg.llm_backend = "none".into();
        }
        Ok(cfg)
    }
}
