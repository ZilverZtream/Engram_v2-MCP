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
    #[serde(default)]
    pub embedding_backend: String,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub ollama_url: Option<String>,
    #[serde(default)]
    pub openai_api_key: Option<String>,

    #[serde(default = "default_max_concurrent_jobs")]
    pub max_concurrent_jobs: usize,
}

fn default_max_concurrent_jobs() -> usize {
    2
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
        Ok(cfg)
    }
}
