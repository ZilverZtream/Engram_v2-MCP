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
        if cfg.allowed_roots.is_empty() {
            return Err(EngramError::Config(
                "allowed_roots must contain at least one directory".into(),
            ));
        }
        if cfg.data_dir.as_os_str().is_empty() {
            return Err(EngramError::Config("data_dir must be set".into()));
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
