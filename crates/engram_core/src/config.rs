use crate::types::{EngramError, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ENG-AUD-2026-N2-0001: reject unknown YAML keys so operator typos are caught at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Optional provider metadata for OpenAI-compatible backends (e.g. "openrouter")
    #[serde(default)]
    pub llm_provider: Option<String>,
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
    /// Optional HTTP-Referer metadata header for OpenAI-compatible requests
    #[serde(default)]
    pub llm_http_referer: Option<String>,
    /// Optional X-Title metadata header for OpenAI-compatible requests
    #[serde(default)]
    pub llm_x_title: Option<String>,
    /// Additional headers for OpenAI-compatible LLM requests
    #[serde(default)]
    pub llm_extra_headers: Option<HashMap<String, String>>,

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

    // --- Memory budget (Issue 5) ---
    /// Total memory budget in bytes (0 = default 2 GB).
    /// Controls backpressure: operations are rejected when the hard limit is exceeded.
    #[serde(default)]
    pub memory_budget_bytes: u64,

    // --- Safety rails (Issue 9) ---
    /// Enable policy mode that blocks high-risk refactors unless confidence thresholds are met.
    #[serde(default)]
    pub safety_policy_enabled: bool,

    /// Minimum impact-analysis confidence (0.0-1.0) required to proceed with automated edits.
    #[serde(default = "default_safety_min_confidence")]
    pub safety_min_confidence: f64,

    /// Minimum test coverage delta threshold. Refactors reducing coverage below this are blocked.
    #[serde(default = "default_safety_min_coverage")]
    pub safety_min_coverage: f64,

    // --- Integrity checks (Issue 6) ---
    /// Interval in seconds between automatic integrity checks (0 = disabled).
    #[serde(default = "default_integrity_check_interval_secs")]
    pub integrity_check_interval_secs: u64,

    /// Whether to auto-repair mismatches found during integrity checks.
    #[serde(default = "default_integrity_auto_repair")]
    pub integrity_auto_repair: bool,

    // --- Retrieval production gates (Issue 2) ---
    /// Minimum NDCG@10 score for vector search to be considered production-ready.
    #[serde(default = "default_retrieval_min_ndcg")]
    pub retrieval_min_ndcg: f64,

    /// Minimum Recall@10 score for vector search to be considered production-ready.
    #[serde(default = "default_retrieval_min_recall")]
    pub retrieval_min_recall: f64,

    // --- Extraction confidence (Feature 6) ---
    /// Minimum extraction confidence score (0.0–1.0) below which a warning is
    /// injected into tool responses to alert agents of potentially incomplete results.
    #[serde(default = "default_confidence_warning_threshold")]
    pub confidence_warning_threshold: f64,

    // --- Autonomous Decision Protocol (ADP) ---
    /// Enable the autonomous decision gate pipeline. When enabled, auto-apply
    /// changes require all mandatory gates to pass.
    #[serde(default)]
    pub adp_enabled: bool,

    /// Minimum extraction confidence for the ADP extraction gate (0.0–1.0).
    #[serde(default = "default_adp_min_extraction_confidence")]
    pub adp_min_extraction_confidence: f64,

    /// Maximum blast radius score (1–10) allowed for auto-apply.
    #[serde(default = "default_adp_max_blast_radius")]
    pub adp_max_blast_radius: u8,

    // --- ADP Rollout Policy ---
    /// Rollout phase: "shadow" (log only), "advisory" (warn but allow),
    /// "guarded" (require mandatory review), "autonomous" (selective auto-apply).
    #[serde(default = "default_adp_rollout_phase")]
    pub adp_rollout_phase: String,

    /// ADP kill-switch: when true, ALL autonomous decisions are forced to deny.
    /// Overrides all other ADP settings. Use for emergency halt.
    #[serde(default)]
    pub adp_kill_switch: bool,

    // --- ADP vNext ---
    /// Default evidence depth: "fast", "standard", or "deep". Default: "standard".
    #[serde(default = "default_adp_evidence_depth")]
    pub adp_default_evidence_depth: String,

    /// Whether to cache benchmark results for the retrieval gate.
    #[serde(default = "default_adp_cache_retrieval")]
    pub adp_cache_retrieval: bool,

    /// Timeout in milliseconds for evidence orchestration.
    #[serde(default = "default_adp_evidence_timeout_ms")]
    pub adp_evidence_timeout_ms: u64,

    /// Minimum reconciliation confirmed ratio to pass runtime gate (0.0–1.0).
    #[serde(default = "default_adp_min_reconciliation_confirmed")]
    pub adp_min_reconciliation_confirmed: f64,

    /// Maximum reconciliation contradicted ratio to pass runtime gate (0.0–1.0).
    #[serde(default = "default_adp_max_reconciliation_contradicted")]
    pub adp_max_reconciliation_contradicted: f64,

    /// Maximum items per wave before ADP forces wave splitting.
    #[serde(default = "default_adp_max_wave_items")]
    pub adp_max_wave_items: usize,

    // ── Phase 30: Migration Engine ──────────────────────────────────────
    /// Default target stack for scaffold generation. "blazor", "react", or "angular".
    #[serde(default = "default_scaffold_target_stack")]
    pub scaffold_default_target_stack: String,

    /// Whether scaffold generation includes test scaffolds by default.
    #[serde(default = "default_true")]
    pub scaffold_include_tests: bool,

    /// Enable classic ASP (.asp) file extraction during indexing.
    #[serde(default)]
    pub enable_classic_asp_extraction: bool,

    /// Enable report file (.rdl/.rdlc) extraction during indexing.
    #[serde(default)]
    pub enable_report_extraction: bool,

    /// Default test framework for characterization test generation.
    #[serde(default = "default_characterization_test_framework")]
    pub characterization_test_framework: String,

    // ── ENG-AUD-2026-S12-0004: configurable embedding dimensions ────────────
    /// Output dimension for the Ollama embedder.
    /// Must match the model's actual output size (e.g. 768 for nomic-embed-text,
    /// 1024 for mxbai-embed-large).  Defaults to 768 when not set.
    #[serde(default)]
    pub ollama_embed_dim: Option<usize>,

    /// Output dimension for the OpenAI-compatible embedder.
    /// Must match the model's actual output size (e.g. 1536 for
    /// text-embedding-3-small).  Defaults to 1536 when not set.
    #[serde(default)]
    pub openai_embed_dim: Option<usize>,
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

fn default_safety_min_confidence() -> f64 {
    0.7
}

fn default_safety_min_coverage() -> f64 {
    0.6
}

fn default_integrity_check_interval_secs() -> u64 {
    3600 // 1 hour
}

fn default_integrity_auto_repair() -> bool {
    true
}

fn default_retrieval_min_ndcg() -> f64 {
    0.5
}

fn default_retrieval_min_recall() -> f64 {
    0.6
}

fn default_confidence_warning_threshold() -> f64 {
    0.5
}

fn default_adp_min_extraction_confidence() -> f64 {
    0.5
}

fn default_adp_max_blast_radius() -> u8 {
    6
}

fn default_adp_rollout_phase() -> String {
    "shadow".into()
}

fn default_adp_evidence_depth() -> String {
    "standard".into()
}

fn default_adp_cache_retrieval() -> bool {
    true
}

fn default_adp_evidence_timeout_ms() -> u64 {
    30_000
}

fn default_adp_min_reconciliation_confirmed() -> f64 {
    0.6
}

fn default_adp_max_reconciliation_contradicted() -> f64 {
    0.2
}

fn default_adp_max_wave_items() -> usize {
    20
}

fn default_scaffold_target_stack() -> String {
    "blazor".into()
}

fn default_true() -> bool {
    true
}

fn default_characterization_test_framework() -> String {
    "nunit".into()
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
            llm_provider: None,
            llm_model: None,
            llm_ollama_url: None,
            llm_openai_api_key: None,
            llm_openai_api_base: None,
            llm_http_referer: None,
            llm_x_title: None,
            llm_extra_headers: None,
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
            memory_budget_bytes: 0,
            safety_policy_enabled: false,
            safety_min_confidence: default_safety_min_confidence(),
            safety_min_coverage: default_safety_min_coverage(),
            integrity_check_interval_secs: default_integrity_check_interval_secs(),
            integrity_auto_repair: default_integrity_auto_repair(),
            retrieval_min_ndcg: default_retrieval_min_ndcg(),
            retrieval_min_recall: default_retrieval_min_recall(),
            confidence_warning_threshold: default_confidence_warning_threshold(),
            adp_enabled: false,
            adp_min_extraction_confidence: default_adp_min_extraction_confidence(),
            adp_max_blast_radius: default_adp_max_blast_radius(),
            adp_rollout_phase: default_adp_rollout_phase(),
            adp_kill_switch: false,
            adp_default_evidence_depth: default_adp_evidence_depth(),
            adp_cache_retrieval: default_adp_cache_retrieval(),
            adp_evidence_timeout_ms: default_adp_evidence_timeout_ms(),
            adp_min_reconciliation_confirmed: default_adp_min_reconciliation_confirmed(),
            adp_max_reconciliation_contradicted: default_adp_max_reconciliation_contradicted(),
            adp_max_wave_items: default_adp_max_wave_items(),
            scaffold_default_target_stack: default_scaffold_target_stack(),
            scaffold_include_tests: default_true(),
            enable_classic_asp_extraction: false,
            enable_report_extraction: false,
            characterization_test_framework: default_characterization_test_framework(),
            ollama_embed_dim: None,
            openai_embed_dim: None,
        }
    }
}

impl Config {
    fn validate_positive_usize(field: &str, value: usize) -> Result<()> {
        if value == 0 {
            return Err(EngramError::Config(format!("{field} must be > 0")));
        }
        Ok(())
    }

    fn validate_unit_interval(field: &str, value: f64) -> Result<()> {
        if !(0.0..=1.0).contains(&value) {
            return Err(EngramError::Config(format!(
                "{field} must be within [0.0, 1.0], got {value}"
            )));
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        // Critical guards: zero/invalid values can deadlock workers or disable
        // key production safeguards silently.
        Self::validate_positive_usize("max_concurrent_jobs", self.max_concurrent_jobs)?;
        Self::validate_positive_usize("max_chunks_per_file", self.max_chunks_per_file)?;
        Self::validate_positive_usize("max_commits_per_watch", self.max_commits_per_watch)?;
        Self::validate_positive_usize("tantivy_writer_memory", self.tantivy_writer_memory)?;
        Self::validate_positive_usize("mmr_oversampling", self.mmr_oversampling)?;
        Self::validate_positive_usize("max_parse_concurrency", self.max_parse_concurrency)?;

        if self.vector_search_timeout_ms == 0 {
            return Err(EngramError::Config(
                "vector_search_timeout_ms must be > 0".into(),
            ));
        }

        if !(0.0..=1.0).contains(&self.immune_warn_threshold)
            || !(0.0..=1.0).contains(&self.immune_block_threshold)
        {
            return Err(EngramError::Config(format!(
                "immune thresholds must be within [0.0, 1.0], got warn={} block={}",
                self.immune_warn_threshold, self.immune_block_threshold
            )));
        }
        if self.immune_warn_threshold > self.immune_block_threshold {
            return Err(EngramError::Config(format!(
                "immune_warn_threshold ({}) must be <= immune_block_threshold ({})",
                self.immune_warn_threshold, self.immune_block_threshold
            )));
        }

        Self::validate_unit_interval("safety_min_confidence", self.safety_min_confidence)?;
        Self::validate_unit_interval("safety_min_coverage", self.safety_min_coverage)?;
        Self::validate_unit_interval("retrieval_min_ndcg", self.retrieval_min_ndcg)?;
        Self::validate_unit_interval("retrieval_min_recall", self.retrieval_min_recall)?;
        Self::validate_unit_interval(
            "confidence_warning_threshold",
            self.confidence_warning_threshold,
        )?;
        Self::validate_unit_interval(
            "adp_min_extraction_confidence",
            self.adp_min_extraction_confidence,
        )?;
        if self.adp_max_blast_radius == 0 || self.adp_max_blast_radius > 10 {
            return Err(EngramError::Config(format!(
                "adp_max_blast_radius must be 1–10, got {}",
                self.adp_max_blast_radius
            )));
        }

        // ADP vNext validation
        Self::validate_unit_interval(
            "adp_min_reconciliation_confirmed",
            self.adp_min_reconciliation_confirmed,
        )?;
        Self::validate_unit_interval(
            "adp_max_reconciliation_contradicted",
            self.adp_max_reconciliation_contradicted,
        )?;
        if self.adp_evidence_timeout_ms == 0 {
            return Err(EngramError::Config(
                "adp_evidence_timeout_ms must be > 0".into(),
            ));
        }

        match self.adp_rollout_phase.as_str() {
            "shadow" | "advisory" | "guarded" | "autonomous" => {}
            other => {
                return Err(EngramError::Config(format!(
                    "adp_rollout_phase must be one of: shadow, advisory, guarded, autonomous (got {other})"
                )));
            }
        }

        match self.adp_default_evidence_depth.as_str() {
            "fast" | "standard" | "deep" => {}
            other => {
                return Err(EngramError::Config(format!(
                    "adp_default_evidence_depth must be one of: fast, standard, deep (got {other})"
                )));
            }
        }

        match self.scaffold_default_target_stack.as_str() {
            "blazor" | "react" | "angular" => {}
            other => {
                return Err(EngramError::Config(format!(
                    "scaffold_default_target_stack must be one of: blazor, react, angular (got {other})"
                )));
            }
        }

        // Reject unknown embedding backends — unknown values silently fall back to
        // ProjectionEmbedder, which degrades retrieval quality without any signal.
        if !self.embedding_backend.is_empty() {
            match self.embedding_backend.as_str() {
                "local" | "candle" | "openai" | "remote" | "ollama" | "fts_only" => {}
                other => {
                    return Err(EngramError::Config(format!(
                        "embedding_backend must be one of: local, candle, openai, remote, ollama, fts_only (got '{other}')"
                    )));
                }
            }
        }

        // Reject unknown LLM backends — unknown values silently fall back to
        // deterministic mode without any signal, masking misconfiguration.
        if !self.llm_backend.is_empty() {
            match self.llm_backend.as_str() {
                "none" | "ollama" | "openai" => {}
                other => {
                    return Err(EngramError::Config(format!(
                        "llm_backend must be one of: none, ollama, openai (got '{other}')"
                    )));
                }
            }
        }

        if let Some(provider) = &self.llm_provider {
            match provider.as_str() {
                "openai" | "openrouter" => {}
                other => {
                    return Err(EngramError::Config(format!(
                        "llm_provider must be one of: openai, openrouter (got {other})"
                    )));
                }
            }
        }

        if let Some(referer) = &self.llm_http_referer {
            if referer.trim().is_empty() {
                return Err(EngramError::Config(
                    "llm_http_referer cannot be empty when set".into(),
                ));
            }
            if !(referer.starts_with("http://") || referer.starts_with("https://")) {
                return Err(EngramError::Config(format!(
                    "llm_http_referer must start with http:// or https:// (got {referer})"
                )));
            }
        }

        if let Some(title) = &self.llm_x_title {
            if title.trim().is_empty() {
                return Err(EngramError::Config(
                    "llm_x_title cannot be empty when set".into(),
                ));
            }
            if title.contains('\n') || title.contains('\r') {
                return Err(EngramError::Config(
                    "llm_x_title cannot contain newlines".into(),
                ));
            }
        }

        if let Some(extra_headers) = &self.llm_extra_headers {
            for (key, value) in extra_headers {
                if key.trim().is_empty() {
                    return Err(EngramError::Config(
                        "llm_extra_headers contains an empty header name".into(),
                    ));
                }
                if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return Err(EngramError::Config(format!(
                        "llm_extra_headers contains invalid header name: {key}"
                    )));
                }
                if value.contains(['\n', '\r', '\0']) {
                    return Err(EngramError::Config(format!(
                        "llm_extra_headers contains invalid header value for {key}"
                    )));
                }
            }
        }

        Ok(())
    }

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
        cfg.validate()?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gate: ENG-AUD-0006 — unknown embedding_backend must be rejected by validate(),
    /// not silently coerced to ProjectionEmbedder.
    #[test]
    fn validate_rejects_unknown_embedding_backend() {
        let cfg = Config {
            embedding_backend: "bad_backend_xyz".into(),
            ..Config::default()
        };
        let result = cfg.validate();
        assert!(result.is_err(), "validate() must reject unknown embedding_backend");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("embedding_backend"),
            "error message should mention the field name; got: {msg}"
        );
    }

    /// All documented embedding backends must be accepted without error.
    #[test]
    fn validate_accepts_all_known_embedding_backends() {
        for backend in &["local", "candle", "openai", "remote", "ollama", "fts_only"] {
            let cfg = Config {
                embedding_backend: backend.to_string(),
                ..Config::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "validate() should accept known backend '{backend}'"
            );
        }
    }

    /// Empty embedding_backend (the Default value) must pass validate() so that
    /// load_from_path's fill-in-default logic continues to work.
    #[test]
    fn validate_accepts_empty_embedding_backend() {
        let cfg = Config::default();
        assert!(
            cfg.validate().is_ok(),
            "validate() must accept empty embedding_backend (default fill-in path)"
        );
    }

    /// Gate: ENG-AUD-2026-0007 — unknown llm_backend must be rejected by validate().
    #[test]
    fn validate_rejects_unknown_llm_backend() {
        let cfg = Config {
            llm_backend: "groq_v99_xyz".into(),
            ..Config::default()
        };
        let result = cfg.validate();
        assert!(result.is_err(), "validate() must reject unknown llm_backend");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("llm_backend"),
            "error message should mention the field name; got: {msg}"
        );
    }

    /// All documented LLM backends must be accepted without error.
    #[test]
    fn validate_accepts_all_known_llm_backends() {
        for backend in &["none", "ollama", "openai"] {
            let cfg = Config {
                llm_backend: backend.to_string(),
                ..Config::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "validate() should accept known llm_backend '{backend}'"
            );
        }
    }

    /// Empty llm_backend (the Default value) must pass validate() so that
    /// load_from_path's fill-in-default logic (which sets it to "none") works.
    #[test]
    fn validate_accepts_empty_llm_backend() {
        let cfg = Config::default();
        assert!(
            cfg.validate().is_ok(),
            "validate() must accept empty llm_backend (default fill-in path)"
        );
    }

    // ── load_from_path tests (ENG-AUD-2026-N2-0001) ──────────────────────────

    /// ENG-AUD-2026-N2-0001: An unknown YAML key must cause load_from_path to
    /// return Err, and the error message must contain the unknown key name.
    #[test]
    fn unknown_config_key_is_rejected() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join("engram_unknown_key_test.yaml");
        {
            let mut f = std::fs::File::create(&path).expect("create temp yaml");
            writeln!(
                f,
                "allowed_roots: [/tmp]\ndata_dir: /tmp\nunknown_typo_key: true\n"
            )
            .expect("write yaml");
        }
        let result = Config::load_from_path(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            result.is_err(),
            // ENG-AUD-2026-N2-0001: unknown keys must be rejected, not silently ignored
            "load_from_path must return Err when an unknown YAML key is present"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unknown_typo_key"),
            "error message must contain the unknown key name; got: {msg}"
        );
    }

    /// ENG-AUD-2026-N2-0001 positive path: a minimal valid config must parse without error.
    #[test]
    fn known_keys_parse_successfully() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join("engram_known_keys_test.yaml");
        {
            let mut f = std::fs::File::create(&path).expect("create temp yaml");
            writeln!(f, "allowed_roots: [/tmp]\ndata_dir: /tmp\n").expect("write yaml");
        }
        let result = Config::load_from_path(&path);
        let _ = std::fs::remove_file(&path);
        assert!(
            result.is_ok(),
            "load_from_path must succeed for a minimal valid config; err: {:?}",
            result.err()
        );
    }
}
