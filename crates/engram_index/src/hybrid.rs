use crate::tantivy_index::{Fields, open_or_create};
use crate::{chunking, ingest};
use engram_core::memory::{AllocationGuard, MemoryBudget, Subsystem};
use engram_core::{ContentHash, DocIdStr, RelPath, build_pk};
#[cfg(feature = "vector")]
use lancedb::query::{ExecutableQuery, QueryBase};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::{IndexRecordOption, Term, Value};
use tantivy::{DocAddress, Score};
use tokio_util::sync::CancellationToken;

mod vector_initialization;
pub use mutation::CoordinationError;
pub use vector_initialization::VectorInitializationRejection;
mod commit;
mod mutation;
mod parse_work;
mod vector_compaction;
mod vector_history;
pub use vector_history::{NativeHistoryPruneOutcome, NativeSessionRotationOutcome};
#[cfg(feature = "vector")]
mod native_store;
#[cfg(test)]
mod reader_experiment;
#[cfg(test)]
mod selected_source_recovery_tests;
mod selected_source_membership;
pub use selected_source_membership::{SelectedSourceNavigation, SelectedSourceRevisionLookup, SelectedSourceSummary};
#[cfg(all(test, feature = "vector"))]
mod retention_observation_tests;
#[cfg(all(test, feature = "vector"))]
mod native_delete_behavior_tests;
#[cfg(all(test, feature = "vector"))]
mod auto_cleanup_tests;
#[cfg(all(test, feature = "vector"))]
mod backend_contracts;
mod snapshots;
mod source_highwater;
#[cfg(test)]
mod source_highwater_tests;
#[cfg(feature = "vector")]
mod vector_epochs;
#[cfg(feature = "vector")]
mod vector_mutations;
#[cfg(feature = "vector")]
mod vector_reads;
#[cfg(feature = "vector")]
mod vector_generation_inventory;
#[cfg(feature = "vector")]
mod vector_recovery_scan;

#[derive(Debug, Clone)]
pub struct HybridQuery {
    pub project_id: String,
    pub namespace: String,
    pub generation: u64,
    pub text: String,
    pub top_k: usize,
    pub fts_mode: String, // "strict", "loose", "regex"
    pub include_path_prefixes: Option<Vec<String>>,
    pub exclude_path_prefixes: Option<Vec<String>>,
    /// Round-2 audit P0-4: keep only paths ending with one of these suffixes
    /// (".rdl", ".sql", ".resx" …), case-insensitively, on BOTH legs.
    pub include_path_suffixes: Option<Vec<String>>,
    pub language_filters: Option<Vec<String>>,
    pub author_filter: Option<String>,
    pub date_after: Option<u64>,
    pub date_before: Option<u64>,
    pub use_mmr: bool,
}

/// A stored document's text and commit metadata.
#[derive(Debug, Clone)]
pub struct StoredDoc {
    pub path: String,
    pub content: String,
    pub author: Option<String>,
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct HybridHit {
    pub pk: String,
    pub chunk_id: u64,
    pub path: RelPath,
    pub score: f32,
    pub centrality: f32, // PageRank score
    pub snippet: Option<String>,
    /// doc_id of the specific chunk instance.
    pub doc_id: String,
    /// 1-based first line of the chunk in its file (0 = unknown, e.g. a
    /// vector-only hit that has not been enriched from Tantivy yet).
    pub start_line: u32,
    /// 1-based last line of the chunk in its file (0 = unknown).
    pub end_line: u32,
    /// True when `snippet` was cut short of the full chunk content. Callers
    /// should surface this so agents know to fetch the rest via get_chunk.
    pub snippet_truncated: bool,
    /// The doc's indexed timestamp (unix ms), when it carries one. Memory,
    /// insight, and history docs do; ordinary code chunks usually do not
    /// (`None`). Drives the recency term when recalling knowledge.
    pub timestamp: Option<u64>,
}

/// Character budget for search-hit snippets. Generous enough to show a whole
/// function signature plus context, small enough not to flood agent context.
const SNIPPET_MAX_CHARS: usize = 500;

/// How trustworthy the vector half of hybrid search is for the configured
/// embedding backend. The default install ("local"/"candle"/empty) uses a
/// deterministic trigram-projection embedder — useful for fuzzy identifier
/// matching but NOT semantic. Agents deserve to know the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticQuality {
    /// A real embedding model (ollama / openai / remote).
    Semantic,
    /// Trigram-projection stub: deterministic, non-semantic.
    DegradedTrigram,
    /// Vector search intentionally disabled (fts_only).
    Off,
}

/// Map an `embedding_backend` config string to its semantic quality tier.
pub fn semantic_quality_for_backend(backend: &str) -> SemanticQuality {
    match backend {
        "ollama" | "openai" | "remote" => SemanticQuality::Semantic,
        "fts_only" => SemanticQuality::Off,
        // "local", "candle", and "" (Config::default) all resolve to the
        // trigram-projection embedder family.
        _ => SemanticQuality::DegradedTrigram,
    }
}

/// Cut `content` at the last line boundary at or below `max_chars`.
///
/// Falls back to a plain char-boundary cut when the first line alone exceeds
/// the budget (e.g. minified JS). Returns the snippet and whether it was
/// truncated relative to the full content.
pub(crate) fn snippet_of(content: &str, max_chars: usize) -> (String, bool) {
    if content.chars().count() <= max_chars {
        return (content.to_string(), false);
    }
    let prefix: String = content.chars().take(max_chars).collect();
    match prefix.rfind('\n') {
        // Cut at the last full line inside the budget.
        Some(nl) if nl > 0 => (prefix[..nl].to_string(), true),
        // Single oversized line: keep the char-budget prefix.
        _ => (prefix, true),
    }
}

#[derive(Debug, Clone)]
pub struct IndexDoc {
    pub generation: u64,
    pub chunk_id: u64,
    pub path: RelPath,
    pub language: String,
    pub content: String,
    pub namespace: String,
    pub author: Option<String>,
    pub timestamp: Option<u64>,
    pub start_line: u32,
    pub end_line: u32,
    /// Instance-level identity (from DocIdStr).
    pub doc_id: String,
    /// Content-level hash (from ContentHash).
    pub content_hash: String,
}

#[derive(Debug, Clone)]
pub struct SearchDocSummary {
    pub namespace: String,
    pub doc_id: String,
    pub path: String,
}

/// Keep one source snapshot and its collection protection for a whole operation.
pub struct SourceSnapshotGuard {
    _catalog: Arc<snapshots::Catalog>,
    _snapshot: snapshots::Lease,
    _project: String,
    _generation: u64,
}

impl SourceSnapshotGuard {
    /// Legacy source cannot authorize snapshot GC before migration.
    pub fn is_managed(&self) -> bool {
        matches!(self._snapshot.selection, snapshots::Selection::Files(_))
    }
}

/// Protect a complete index mutation against concurrent vector recovery.
#[must_use]
#[derive(Clone)]
pub struct IndexMutationGuard {
    _permit: Arc<mutation::MutationPermit>,
    _catalog: Arc<snapshots::Catalog>,
    project: Option<Arc<str>>,
}

impl Drop for IndexMutationGuard {
    fn drop(&mut self) {
        // A surviving worker clone must not hide a caller's unwind.
        if std::thread::panicking() {
            self._catalog.mutations.mark_faulted();
        }
    }
}

/// Keep new mutations from overtaking an in-flight write or publication.
#[must_use]
pub struct IndexOperationGuard {
    _permit: mutation::OperationPermit,
    _catalog: Arc<snapshots::Catalog>,
    _admission: Option<IndexMutationGuard>,
}

#[derive(Debug, Clone, Default)]
pub struct IngestStats {
    /// Captured with the produced source output, never inferred at publication.
    pub source_extraction_identity: Option<crate::SourceExtractionIdentity>,
    /// Actual parser used per VB file, including files with no symbols.
    pub vb_parser_provenance: std::collections::HashMap<String, crate::vb_extractor::VbParserProvenance>,
    /// Position policy of newly produced WebForms output, including empty
    /// output from vendor suppression. Never attached to inherited files.
    pub webforms_position_contracts: std::collections::HashMap<String, u64>,
    pub files: usize,
    pub chunks: usize,
    pub bytes: u64,
    pub all_files: Vec<RelPath>,
    pub fingerprints: Vec<crate::docstore::FileFingerprint>,
    /// Symbols indexed by file. Uses Arc<RelPath> so all symbols/edges from one
    /// file share a single allocation instead of cloning the path N times.
    pub symbols: Vec<(Arc<RelPath>, crate::parsing::ExtractedSymbol)>,
    pub edges: Vec<(Arc<RelPath>, crate::parsing::ExtractedEdge)>,
    pub skipped_files: Vec<(RelPath, String)>,
    pub languages: std::collections::HashMap<String, usize>,
    pub warnings: Vec<String>,
}

/// Escape SQL LIKE wildcards (`%`, `_`) and single quotes in a string
/// so it can be safely used in a `LIKE '...' ESCAPE '\'` clause.
#[cfg(feature = "vector")]
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

/// How long a writer-acquiring operation waits for a busy Tantivy lock
/// before giving up. Long enough to ride out a watcher incremental pass
/// or a small tool-triggered index; a full reindex will still exhaust it,
/// which is correct — the caller should see that error.
const WRITER_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Cancel-safe scope guard for a long-lived Tantivy IndexWriter.
///
/// On drop (including panic/cancel), commits any pending docs and waits
/// for merge threads so segments are never left orphaned.
pub struct BulkWriterGuard {
    writer: Option<tantivy::IndexWriter<tantivy::TantivyDocument>>,
    docs_since_commit: usize,
    lexical_context: commit::LexicalContext,
    _mutation: IndexMutationGuard,
    #[cfg(feature = "vector")]
    vector_preflight: Option<(Arc<vector_epochs::Routes>, Arc<dyn engram_ml::Embedder>)>,
    #[cfg(feature = "vector")]
    _bootstrap: Option<tokio::sync::OwnedMutexGuard<()>>,
    #[cfg(all(test, feature = "vector"))]
    finalize_pause: Option<Arc<commit::FinalizePause>>,
    // Last field: the writer, bootstrap and mutation must finish dropping
    // before the final payload lane lifetime can be released.
    commit: commit::Guard,
}

impl BulkWriterGuard {
    /// Commit the current batch. Cheap (no merge wait).
    pub fn commit(&mut self) -> anyhow::Result<()> {
        self.commit.ensure_active()?;
        if let Some(ref mut w) = self.writer {
            self.lexical_context.commit(w)?;
            self.docs_since_commit = 0;
        }
        Ok(())
    }

    /// Commit if `docs_since_commit >= threshold`.
    pub fn maybe_commit(&mut self, threshold: usize) -> anyhow::Result<()> {
        self.commit.ensure_active()?;
        if self.docs_since_commit >= threshold {
            self.commit()?;
        }
        Ok(())
    }

    /// Final shutdown: commit + wait for all merge threads.
    /// This is the expensive call — run it once at the end.
    pub fn finish(mut self) -> anyhow::Result<()> {
        self.commit.ensure_active()?;
        if let Some(w) = self.writer.take() {
            let mut context = self.lexical_context.clone();
            context.operation = "bulk_finish";
            context.finish(w)?;
        }
        Ok(())
    }
}

impl Drop for BulkWriterGuard {
    fn drop(&mut self) {
        #[cfg(all(test, feature = "vector"))]
        if self.writer.is_some() {
            if let Some(pause) = &self.finalize_pause {
                pause.wait();
            }
        }
        if let Some(w) = self.writer.take() {
            // Best-effort commit + merge wait on cancel/panic.
            let mut context = self.lexical_context.clone();
            context.operation = "bulk_drop";
            if let Err(error) = context.finish(w) {
                tracing::warn!("Dropped lexical batch finalization failed: {error:#}");
            }
        }
    }
}

/// A chunk's stored fields, for callers that scan text directly.
#[derive(Debug, Clone)]
pub struct StoredChunk {
    pub doc_id: String,
    pub chunk_id: u64,
    pub path: String,
    pub language: String,
    pub start_line: u32,
    pub content: String,
}

#[derive(Clone)]
pub struct HybridSearchEngine {
    snapshots: Arc<snapshots::Catalog>,
    // Only explicit, short-lived derived engines carry admission. Never cache
    // them: retaining this capability deliberately excludes recovery.
    admission: Option<IndexMutationGuard>,
    parse_limiter: Option<Arc<tokio::sync::Semaphore>>,
    // The caller's complete admitted VB inventory, including zero-symbol
    // sources. Only operation-scoped engines carry this compilation input.
    vb_source_inventory: Option<Arc<Vec<PathBuf>>>,
    #[cfg(test)]
    parse_pause: Option<Arc<parse_work::ParsePause>>,
    #[cfg(test)]
    parse_preparation_pause: Option<Arc<parse_work::ParsePause>>,
    #[cfg(test)]
    parse_admission_probe: Option<Arc<parse_work::AdmissionProbe>>,
    tantivy_index: tantivy::Index,
    fields: Fields,
    extractor: Arc<crate::parsing::SymbolExtractor>,
    #[cfg(feature = "vector")]
    embedder: Arc<dyn engram_ml::Embedder>,
    #[cfg(feature = "vector")]
    embedding_cache: Option<Arc<crate::embed_cache::CachedEmbedder>>,
    #[cfg(feature = "vector")]
    query_embedder: Arc<dyn engram_ml::Embedder>,
    #[cfg(feature = "vector")]
    native_store: Arc<native_store::Store>,
    #[cfg(feature = "vector")]
    vector_routes: Arc<vector_epochs::Routes>,
    #[cfg(all(test, feature = "vector"))]
    native_write_pause: Option<Arc<vector_mutations::NativeWritePause>>,
    #[cfg(all(test, feature = "vector"))]
    native_read_pause: Option<Arc<vector_reads::NativeReadPause>>,
    #[cfg(all(test, feature = "vector"))]
    history_prune_pause: Option<Arc<vector_history::HistoryPrunePause>>,
    #[cfg(all(test, feature = "vector"))]
    history_preflight_work_limit: Option<usize>,
    #[cfg(all(test, feature = "vector"))]
    commit_waiting: Option<Arc<tokio::sync::Notify>>,
    #[cfg(all(test, feature = "vector"))]
    native_commit_completed: Option<Arc<tokio::sync::Notify>>,
    #[cfg(all(test, feature = "vector"))]
    bulk_finalize_pause: Option<Arc<commit::FinalizePause>>,
    #[cfg(all(test, feature = "vector"))]
    bootstrap_failure: Option<Arc<std::sync::atomic::AtomicU8>>,
    embedding_backend: String,
    _tantivy_dir: PathBuf,
    _lance_dir: PathBuf,
    /// Tantivy IndexWriter heap budget in bytes (default 50 MB).
    tantivy_writer_memory: usize,
    /// MMR oversampling multiplier: fetch_k = top_k * this (default 5).
    mmr_oversampling: usize,
    memory_budget: Option<Arc<MemoryBudget>>,
}

impl HybridSearchEngine {
    /// Non-atomic in-memory observations. Does not open a table, pin a snapshot
    /// or drain native cache tasks; the latter would perturb the measurement.
    pub fn retention_observation(&self, project: &str) -> serde_json::Value {
        #[cfg(feature = "vector")]
        let native = self.native_store.observation();
        #[cfg(not(feature = "vector"))]
        let native = serde_json::json!({"status":"not_compiled"});
        #[cfg(feature = "vector")]
        let routes = self.vector_routes.retention_observation(project);
        #[cfg(not(feature = "vector"))]
        let routes = serde_json::json!({"status":"not_compiled"});
        serde_json::json!({"source_catalog":self.snapshots.retention_observation(project),
            "vector_routes":routes,"native_cache":native,
            "observation":"Independent non-atomic component observations; busy components are unavailable, not zero. No stores opened, readers pinned or cache maintenance performed."})
    }

    fn lexical_context(&self, operation: &'static str, project: Option<&str>, generation_hint: Option<u64>) -> commit::LexicalContext {
        commit::LexicalContext { directory: self._tantivy_dir.clone(), project: project.map(Arc::from), generation_hint, operation }
    }
    /// Only explicit full-text-only configuration disables vector operations.
    /// Empty retains the historical default ProjectionEmbedder contract.
    fn is_lexical_only(&self) -> bool {
        self.embedding_backend == "fts_only"
    }

    pub fn begin_operation(&self) -> anyhow::Result<IndexOperationGuard> {
        let permit = match &self.admission {
            Some(admission) => self
                .snapshots
                .mutations
                .operation_within(&admission._permit)?,
            None => self.snapshots.mutations.try_operation()?,
        };
        Ok(IndexOperationGuard {
            _permit: permit,
            _catalog: self.snapshots.clone(),
            _admission: self.admission.clone(),
        })
    }

    /// Retain until the complete mutation, including source publication, ends.
    /// The catalog keeps both process exclusion and shared coordination alive.
    pub fn begin_mutation(&self) -> anyhow::Result<IndexMutationGuard> {
        if let Some(admission) = &self.admission {
            self.snapshots
                .mutations
                .validate_mutation(&admission._permit)?;
            return Ok(admission.clone());
        }
        Ok(IndexMutationGuard {
            _permit: Arc::new(self.snapshots.mutations.try_mutation()?),
            _catalog: self.snapshots.clone(),
            project: None,
        })
    }

    pub fn begin_project_mutation(&self, project: &str) -> anyhow::Result<IndexMutationGuard> {
        let mut admission = self.begin_mutation()?;
        anyhow::ensure!(
            admission
                .project
                .as_deref()
                .is_none_or(|bound| bound == project),
            "Mutation capability belongs to another project"
        );
        admission.project = Some(Arc::from(project));
        Ok(admission)
    }

    /// Derive a temporary engine that reuses this exact admitted flow. Backend
    /// handles are shared; this does not reopen stores or confer write ordering.
    pub fn with_mutation(&self, admission: &IndexMutationGuard) -> anyhow::Result<Self> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.snapshots, &admission._catalog),
            "Mutation capability belongs to another index root"
        );
        self.snapshots
            .mutations
            .validate_mutation(&admission._permit)?;
        if let Some(existing) = &self.admission {
            anyhow::ensure!(
                Arc::ptr_eq(&existing._permit, &admission._permit)
                    && existing
                        .project
                        .as_deref()
                        .is_none_or(|project| admission.project.as_deref() == Some(project)),
                "Cannot replace an existing mutation scope"
            );
        }
        let mut scoped = self.clone();
        scoped.admission = Some(admission.clone());
        Ok(scoped)
    }

    /// Derive a project-bound admitted view of the same bulk operation.
    /// Flush payloads with `embed_and_upsert_vectors_with_writer`; this view
    /// carries admission only and does not bypass ordered write ownership.
    /// The returned engine owns admission beyond `writer.finish()`. Drop it
    /// when the flow ends; do not retain it in a cache or long-lived actor.
    pub fn with_bulk_writer(
        &self,
        writer: &BulkWriterGuard,
        project: &str,
    ) -> anyhow::Result<Self> {
        let scoped = self.with_mutation(&writer._mutation)?;
        let admission = scoped.begin_project_mutation(project)?;
        scoped.with_mutation(&admission)
    }
    pub async fn new(
        tantivy_dir: PathBuf,
        lance_dir: PathBuf,
        cfg: &engram_core::Config,
    ) -> anyhow::Result<Self> {
        Self::new_with_budget(tantivy_dir, lance_dir, cfg, None).await
    }

    pub async fn new_with_budget(
        tantivy_dir: PathBuf,
        lance_dir: PathBuf,
        cfg: &engram_core::Config,
        memory_budget: Option<Arc<MemoryBudget>>,
    ) -> anyhow::Result<Self> {
        let embedding_backend = cfg.embedding_backend.clone();
        // Refuse a foreign source owner before opening either search store or
        // constructing caches. A held source view retains this admission lock.
        std::fs::create_dir_all(&tantivy_dir)?;
        let snapshots = snapshots::Catalog::shared(
            tantivy_dir.join("source-snapshots"),
            memory_budget.clone(),
        )?;
        let (index, fields) = open_or_create(&tantivy_dir)?;

        #[cfg(feature = "vector")]
        let (lance_conn, native_session) = crate::vector::connect_observed(&lance_dir).await?;
        #[cfg(feature = "vector")]
        let native_store = Arc::new(native_store::Store::new(lance_conn, native_session));

        // EMB2: fail-fast if the configured remote embedder cannot be built.
        // Silent fallback to ProjectionEmbedder would degrade semantic search
        // quality without any operator-visible signal.
        #[cfg(feature = "vector")]
        let embedder: Arc<dyn engram_ml::Embedder> = match embedding_backend.as_str() {
            // EMB3: "ollama" was missing here and fell through to the bail arm,
            // so a documented backend could never be used. Route it through the
            // same fail-fast builder as openai/remote.
            "openai" | "remote" | "ollama" => build_embedder_for_backend(cfg)?,
            "local" | "candle" => Arc::new(engram_ml::embed::LocalEmbedder),
            // Empty is the historical default projection provider. Its vectors
            // remain enabled and honestly labelled degraded, not semantic.
            // Explicit fts_only keeps a typed placeholder but disables vectors.
            // EMB2: any OTHER string is a misconfiguration — fail fast rather than
            // silently degrading to stub behaviour.
            "fts_only" | "" => Arc::new(engram_ml::embed::ProjectionEmbedder::new(
                crate::vector::VECTOR_DIM,
            )),
            _ => anyhow::bail!(
                "EMB2: unknown embedding backend {:?} — check embedding_backend in config \
                 (valid: openai, remote, ollama, local, candle, fts_only)",
                embedding_backend
            ),
        };
        // Eagerly validate that the embedder dimension is non-zero to catch
        // misconfigured backends before any data is written.
        #[cfg(feature = "vector")]
        if embedder.dimension() == 0 {
            anyhow::bail!("Embedder reported dimension 0 — check embedding_backend config");
        }

        #[cfg(feature = "vector")]
        let query_embedder: Arc<dyn engram_ml::Embedder> =
            Arc::new(crate::embed_cache::QueryEmbedder::new(embedder.clone()));

        // 16b: wrap remote embedders in the cross-project content-hash cache.
        // Reindex cycles, copy-forward, and history re-runs become cache hits
        // instead of full re-embeds. Local/deterministic embedders are cheap
        // enough that caching only adds I/O. Cache-open failure falls back to
        // the uncached embedder (warn, never fail engine construction).
        #[cfg(feature = "vector")]
        let (embedder, embedding_cache): (Arc<dyn engram_ml::Embedder>, _) =
            if matches!(embedding_backend.as_str(), "openai" | "remote" | "ollama")
                && !cfg.data_dir.as_os_str().is_empty()
            {
                let cache_path = cfg.data_dir.join("embed_cache.redb");
                let cached = Arc::new(crate::embed_cache::CachedEmbedder::new_lazy(
                    embedder,
                    &cache_path,
                ));
                (cached.clone(), Some(cached))
            } else {
                (embedder, None)
            };

        #[cfg(feature = "vector")]
        let vector_routes = snapshots.bind_vectors(&lance_dir)?;
        Ok(Self {
            snapshots,
            admission: None,
            parse_limiter: None,
            vb_source_inventory: None,
            #[cfg(test)]
            parse_pause: None,
            #[cfg(test)]
            parse_preparation_pause: None,
            #[cfg(test)]
            parse_admission_probe: None,
            tantivy_index: index,
            fields,
            extractor: Arc::new(crate::parsing::SymbolExtractor::new()),
            #[cfg(feature = "vector")]
            embedder,
            #[cfg(feature = "vector")]
            embedding_cache,
            #[cfg(feature = "vector")]
            query_embedder,
            #[cfg(feature = "vector")]
            native_store,
            #[cfg(feature = "vector")]
            vector_routes,
            #[cfg(all(test, feature = "vector"))]
            native_write_pause: None,
            #[cfg(all(test, feature = "vector"))]
            native_read_pause: None,
            #[cfg(all(test, feature = "vector"))]
            history_prune_pause: None,
            #[cfg(all(test, feature = "vector"))]
            history_preflight_work_limit: None,
            #[cfg(all(test, feature = "vector"))]
            commit_waiting: None,
            #[cfg(all(test, feature = "vector"))]
            native_commit_completed: None,
            #[cfg(all(test, feature = "vector"))]
            bulk_finalize_pause: None,
            #[cfg(all(test, feature = "vector"))]
            bootstrap_failure: None,
            embedding_backend,
            _tantivy_dir: tantivy_dir,
            _lance_dir: lance_dir,
            tantivy_writer_memory: cfg.tantivy_writer_memory,
            mmr_oversampling: cfg.mmr_oversampling,
            memory_budget,
        })
    }

    /// Semantic quality tier of this engine's vector half. Surfaced in search
    /// responses so agents know whether "semantic" hits are real embeddings
    /// or the default trigram-projection stub.
    pub fn semantic_quality(&self) -> SemanticQuality {
        #[cfg(feature = "vector")]
        { semantic_quality_for_backend(&self.embedding_backend) }
        #[cfg(not(feature = "vector"))]
        { SemanticQuality::Off }
    }

    /// Attach source-only Registry authority without pinning any view.
    /// The catalog holds only a Weak reference; reader guards cannot keep the
    /// Registry database or its process lock alive.
    pub fn bind_source_registry(&self, registry: &Arc<engram_core::Registry>) -> anyhow::Result<()> {
        self.snapshots.bind_source_registry(registry)
    }

    pub fn pin_source_generation(
        &self,
        project_id: &str,
        generation: u64,
    ) -> anyhow::Result<SourceSnapshotGuard> {
        Ok(SourceSnapshotGuard {
            _catalog: self.snapshots.clone(),
            _snapshot: self.snapshots.pin_fresh(project_id, "memory", generation)?,
            _project: project_id.to_owned(),
            _generation: generation,
        })
    }

    /// Validate an already-selected capability without creating a fresh pin.
    /// Retired views remain usable only through the original held guard.
    pub fn validate_source_snapshot_guard(
        &self,
        project_id: &str,
        generation: u64,
        view: &SourceSnapshotGuard,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&view._catalog, &self.snapshots)
                && view._project == project_id
                && view._generation == generation,
            "Selected source view does not match this project, generation or index root"
        );
        Ok(())
    }

    /// A missing authoritative head cannot be interpreted as legacy generation
    /// one once a managed source snapshot directory exists.
    pub fn has_managed_source_snapshots(&self, project_id: &str) -> anyhow::Result<bool> {
        self.snapshots.managed(project_id, "memory")
    }

    /// Bounded metadata-only observation for cold projects. Does not open an
    /// index, acquire a process lock or create a catalog directory.
    pub fn source_snapshot_directory_exists(
        tantivy_dir: &Path,
        project_id: &str,
    ) -> anyhow::Result<bool> {
        snapshots::Catalog::new(tantivy_dir.join("source-snapshots"), None)
            .managed(project_id, "memory")
    }

    /// Stage a source view by reference, without copying unchanged chunks or
    /// vectors. Publication remains the caller's final registry operation.
    pub async fn prepare_source_snapshot(
        &self,
        project_id: &str,
        old_generation: u64,
        new_generation: u64,
        changed: &[RelPath],
        deleted: &[RelPath],
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let mut commit = scoped.acquire_commit(cancel).await?;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "source snapshot preparation cancelled"
        );
        let previous = scoped.snapshots.pin(project_id, "memory", old_generation)?;
        let legacy_paths = if matches!(previous.selection, snapshots::Selection::Legacy(_)) {
            scoped
                .paths_in_generation(project_id, Some("memory"), old_generation)?
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        scoped.snapshots.prepare(
            project_id,
            "memory",
            old_generation,
            new_generation,
            &legacy_paths,
            &changed
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect::<Vec<_>>(),
            &deleted
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect::<Vec<_>>(),
        )?;
        scoped
            .clear_staged_source_generation(project_id, new_generation, cancel, &mut commit)
            .await
    }

    pub async fn prepare_full_source_snapshot(
        &self,
        project_id: &str,
        old_generation: u64,
        new_generation: u64,
        paths: &[RelPath],
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let mut commit = scoped.acquire_commit(cancel).await?;
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "source snapshot preparation cancelled"
        );
        scoped.snapshots.prepare_full(
            project_id,
            "memory",
            old_generation,
            new_generation,
            &paths
                .iter()
                .map(|path| path.as_str().to_owned())
                .collect::<Vec<_>>(),
        )?;
        scoped
            .clear_staged_source_generation(project_id, new_generation, cancel, &mut commit)
            .await
    }

    async fn clear_staged_source_generation(
        &self,
        project_id: &str,
        new_generation: u64,
        cancel: &CancellationToken,
        _commit: &mut commit::Guard,
    ) -> anyhow::Result<()> {
        // A failed attempt may have left chunks at the same unpublished number.
        // Clear that exact physical generation, never a selected older revision.
        #[cfg(feature = "vector")]
        {
            let _vector_epoch = self.pin_vector_epoch(project_id)?;
            self.ensure_managed_vector_table_present(&_vector_epoch)?;
            let name = _vector_epoch.table.clone();
            if self
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&name)
            {
                let table = self.open_vector_epoch_table(&_vector_epoch).await?;
                self.delete_vector_rows(
                    _commit,
                    &table,
                    format!("namespace = 'memory' AND generation = {}", crate::sql_literals::u64_literal(new_generation)),
                )
                .await?;
            }
        }
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.project_id, project_id),
                    IndexRecordOption::Basic,
                )) as Box<dyn tantivy::query::Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.namespace, "memory"),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.fields.generation, new_generation),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let reader = self.tantivy_index.reader()?;
        if reader
            .searcher()
            .search(&query, &tantivy::collector::Count)?
            != 0
        {
            let _allocation = self
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        self.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "abandoned source snapshot cleanup",
                    )
                })
                .transpose()?;
            let mut writer = self
                .acquire_writer("abandoned source snapshot cleanup", cancel)
                .await?;
            writer.delete_query(Box::new(query))?;
            let lexical_context = self.lexical_context("abandoned_source_cleanup", Some(project_id), Some(new_generation));
            let lexical_opstamp = lexical_context.commit(&mut writer)?;
            lexical_context.wait_merges(writer, lexical_opstamp)?;
        }
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "source snapshot preparation cancelled"
        );
        Ok(())
    }

    pub fn seal_source_snapshot(&self, project_id: &str, generation: u64) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        scoped.snapshots.seal(project_id, "memory", generation)
    }

    /// Create a long-lived Tantivy writer for bulk indexing.
    ///
    /// The returned guard commits + waits on drop (cancel-safe).
    /// Call `write_docs_to_writer` to add documents, then `finish()`.
    pub fn create_bulk_writer(&self) -> anyhow::Result<BulkWriterGuard> {
        let mutation = self.begin_mutation()?;
        let scoped = self.with_mutation(&mutation)?;
        let commit = scoped.try_commit()?;
        #[cfg(feature = "vector")]
        let bootstrap = if self.is_lexical_only() {
            Some(self.vector_routes.try_lexical_only()?)
        } else {
            None
        };
        let writer = self.acquire_writer_blocking("bulk writer")?;
        Ok(BulkWriterGuard {
            writer: Some(writer),
            docs_since_commit: 0,
            lexical_context: self.lexical_context("bulk_periodic_commit", None, None),
            _mutation: mutation,
            #[cfg(feature = "vector")]
            vector_preflight: (!self.is_lexical_only())
                .then(|| (self.vector_routes.clone(), self.embedder.clone())),
            #[cfg(feature = "vector")]
            _bootstrap: bootstrap,
            #[cfg(all(test, feature = "vector"))]
            finalize_pause: self.bulk_finalize_pause.clone(),
            commit,
        })
    }

    pub async fn create_bulk_writer_async(
        &self,
        project: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<BulkWriterGuard> {
        let mutation = self.begin_project_mutation(project)?;
        let scoped = self.with_mutation(&mutation)?;
        let commit = scoped.acquire_commit(cancel).await?;
        #[cfg(feature = "vector")]
        let bootstrap = if scoped.is_lexical_only() {
            Some(scoped.vector_routes.lexical_only().await)
        } else {
            scoped.vector_write_table(project).await?;
            None
        };
        let writer = scoped.acquire_writer("bulk writer", cancel).await?;
        Ok(BulkWriterGuard {
            writer: Some(writer),
            docs_since_commit: 0,
            lexical_context: scoped.lexical_context("bulk_periodic_commit", Some(project), None),
            _mutation: mutation,
            #[cfg(feature = "vector")]
            vector_preflight: (!scoped.is_lexical_only())
                .then(|| (scoped.vector_routes.clone(), scoped.embedder.clone())),
            #[cfg(feature = "vector")]
            _bootstrap: bootstrap,
            #[cfg(all(test, feature = "vector"))]
            finalize_pause: self.bulk_finalize_pause.clone(),
            commit,
        })
    }

    /// Acquire the Tantivy IndexWriter, retrying on `LockBusy`.
    ///
    /// Tantivy allows exactly one IndexWriter per directory. Writers here
    /// are scoped (acquired per operation, released after commit), so a
    /// `LockBusy` almost always means another operation — the file
    /// watcher's incremental update, a reindex, a concurrent tool call —
    /// holds the writer *right now* and will release it shortly. Failing
    /// the whole call on first contention turns a routine overlap into a
    /// user-visible tool error, so wait-and-retry within a bounded window
    /// instead. Non-lock errors and `LockBusy` past the deadline still
    /// propagate.
    fn acquire_writer_blocking(
        &self,
        what: &str,
    ) -> anyhow::Result<tantivy::IndexWriter<tantivy::TantivyDocument>> {
        let deadline = std::time::Instant::now() + WRITER_LOCK_WAIT;
        let mut delay = std::time::Duration::from_millis(500);
        loop {
            match self.tantivy_index.writer(self.tantivy_writer_memory) {
                Ok(w) => return Ok(w),
                Err(tantivy::TantivyError::LockFailure(
                    tantivy::directory::error::LockError::LockBusy,
                    _,
                )) if std::time::Instant::now() + delay < deadline => {
                    tracing::warn!(
                        what,
                        retry_in_ms = delay.as_millis() as u64,
                        "tantivy writer lock busy — another indexing operation \
                         is in flight; waiting for it to release"
                    );
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(std::time::Duration::from_secs(5));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Async variant of [`Self::acquire_writer_blocking`] for callers on
    /// the tokio runtime — backs off with `tokio::time::sleep` so the
    /// worker thread is not blocked while waiting for the lock.
    ///
    /// Cancel-aware (CANCEL-POLICY-INDEX): a cancelled caller must not
    /// keep waiting up to the full lock deadline for a writer it will
    /// never use.
    async fn acquire_writer(
        &self,
        what: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<tantivy::IndexWriter<tantivy::TantivyDocument>> {
        let deadline = std::time::Instant::now() + WRITER_LOCK_WAIT;
        let mut delay = std::time::Duration::from_millis(500);
        loop {
            if cancel.is_cancelled() {
                anyhow::bail!("cancelled while waiting for the tantivy writer lock ({what})");
            }
            match self.tantivy_index.writer(self.tantivy_writer_memory) {
                Ok(w) => return Ok(w),
                Err(tantivy::TantivyError::LockFailure(
                    tantivy::directory::error::LockError::LockBusy,
                    _,
                )) if std::time::Instant::now() + delay < deadline => {
                    tracing::warn!(
                        what,
                        retry_in_ms = delay.as_millis() as u64,
                        "tantivy writer lock busy — another indexing operation \
                         is in flight; waiting for it to release"
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(std::time::Duration::from_secs(5));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Get a copy of the field schema (for use in blocking closures).
    pub fn fields(&self) -> Fields {
        self.fields
    }

    /// Add documents to a bulk writer without committing.
    ///
    /// This is the Tantivy-only portion of `index_docs`. The writer is NOT
    /// committed — call `guard.maybe_commit(1000)` periodically and
    /// `guard.finish()` at the end.
    pub fn write_docs_to_writer(
        fields: &Fields,
        guard: &mut BulkWriterGuard,
        project_id: &str,
        docs: &[IndexDoc],
    ) -> anyhow::Result<usize> {
        guard.commit.ensure_active()?;
        anyhow::ensure!(
            guard
                ._mutation
                .project
                .as_deref()
                .is_none_or(|project| project == project_id),
            "Bulk writer mutation capability belongs to another project"
        );
        #[cfg(feature = "vector")]
        if !docs.is_empty() {
            if let Some((routes, provider)) = &guard.vector_preflight {
                routes.validate_bulk_write(project_id, provider)?;
            }
        }
        let writer = guard
            .writer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("BulkWriterGuard already finished"))?;
        let mut added = 0usize;
        for d in docs {
            let effective_gen = if let Ok(policy) = engram_core::get_policy(&d.namespace) {
                if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
                    0
                } else {
                    d.generation
                }
            } else {
                tracing::warn!(
                    namespace = %d.namespace,
                    generation = d.generation,
                    "ENG-AUD-2026-S15-001: get_policy failed for namespace; \
                     using provided generation as fallback"
                );
                d.generation
            };

            let pk = build_pk(project_id, &d.namespace, effective_gen, &d.doc_id);
            writer.delete_term(Term::from_field_text(fields.pk, &pk));

            let tdoc = doc!(
                fields.pk => pk.as_str(),
                fields.doc_id => d.doc_id.as_str(),
                fields.content_hash => d.content_hash.as_str(),
                fields.project_id => project_id,
                fields.namespace => d.namespace.as_str(),
                fields.generation => effective_gen,
                fields.chunk_id => d.chunk_id,
                fields.path => d.path.as_str(),
                fields.language => d.language.as_str(),
                fields.author => d.author.as_deref().unwrap_or(""),
                fields.timestamp => d.timestamp.unwrap_or(0),
                fields.start_line => d.start_line as u64,
                fields.end_line => d.end_line as u64,
                fields.content => d.content.as_str(),
                fields.content_words => d.content.as_str(),
            );
            writer.add_document(tdoc)?;
            added += 1;
        }
        guard.docs_since_commit += added;
        Ok(added)
    }

    /// Embed documents and upsert vectors to LanceDB.
    ///
    /// This is the vector-only portion of `index_docs`, extracted so bulk
    /// callers can batch vector writes on a different cadence than Tantivy.
    /// Does nothing when the `vector` feature is disabled or backend is `fts_only`.
    /// While holding a bulk writer, use `embed_and_upsert_vectors_with_writer`
    /// instead so the batch uses its existing write ownership.
    pub async fn embed_and_upsert_vectors(
        &self,
        project_id: &str,
        docs: &[IndexDoc],
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        if docs.is_empty() || cancel.is_cancelled() {
            return Ok(());
        }
        let mut commit = scoped.acquire_commit(cancel).await?;
        scoped
            .embed_vectors_in_commit(project_id, docs, cancel, None, Some(&mut commit))
            .await
    }

    /// Flush using the existing writer's unique cross-store ownership.
    pub async fn embed_and_upsert_vectors_with_writer(
        &self,
        writer: &mut BulkWriterGuard,
        project_id: &str,
        docs: &[IndexDoc],
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let scoped = self.with_bulk_writer(writer, project_id)?;
        writer.commit.validate(&scoped)?;
        scoped
            .embed_vectors_in_commit(project_id, docs, cancel, None, Some(&mut writer.commit))
            .await
    }

    // Recovery workers already own exclusive admission for their private stage.
    // Ordinary document/bulk writes use embed_vectors_in_commit with authority.
    #[cfg(feature = "vector")]
    async fn embed_and_upsert_vectors_to_table(
        &self,
        project_id: &str,
        docs: &[IndexDoc],
        cancel: &CancellationToken,
        table: Option<&str>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            table.is_some(),
            "Exclusive vector staging requires an explicit table"
        );
        self.embed_vectors_in_commit(project_id, docs, cancel, table, None)
            .await
    }

    async fn embed_vectors_in_commit(
        &self,
        project_id: &str,
        docs: &[IndexDoc],
        cancel: &CancellationToken,
        _table_override: Option<&str>,
        commit: Option<&mut commit::Guard>,
    ) -> anyhow::Result<()> {
        #[cfg(not(feature = "vector"))]
        let _ = (project_id, commit);
        if docs.is_empty() || cancel.is_cancelled() {
            return Ok(());
        }

        #[cfg(feature = "vector")]
        if !self.is_lexical_only() {
            let mut commit = commit;
            // Namespace homogeneity check
            {
                let first_ns = &docs[0].namespace;
                let bad = docs
                    .iter()
                    .enumerate()
                    .find(|(_, d)| d.namespace != *first_ns);
                if let Some((idx, bad_doc)) = bad {
                    return Err(anyhow::anyhow!(
                        "embed_and_upsert_vectors: heterogeneous namespace batch — \
                         docs[0].namespace={first_ns:?} but docs[{idx}].namespace={:?}",
                        bad_doc.namespace
                    ));
                }
            }

            let table = if let Some(table_name) = _table_override {
                {
                    let table = self.native_store.capture()?.connection.open_table(table_name).execute().await?;
                    crate::vector::validate_table_schema(&table, self.embedder.dimension()).await?;
                    table
                }
            } else {
                self.vector_write_table(project_id).await?
            };
            let retained_allocation = self.reserve_vector_batch(project_id, docs)?;
            let mut pks = Vec::with_capacity(docs.len());
            let mut doc_ids = Vec::with_capacity(docs.len());
            let mut content_hashes = Vec::with_capacity(docs.len());
            let mut chunk_ids = Vec::with_capacity(docs.len());
            let mut paths = Vec::with_capacity(docs.len());
            let mut languages = Vec::with_capacity(docs.len());
            let mut authors = Vec::with_capacity(docs.len());
            let mut timestamps = Vec::with_capacity(docs.len());
            let mut effective_gens = Vec::with_capacity(docs.len());
            let mut vectors = Vec::with_capacity(docs.len());
            let mut contents_for_embed: Vec<&str> = Vec::with_capacity(docs.len());

            for d in docs {
                if cancel.is_cancelled() {
                    break;
                }
                let effective_gen = if let Ok(policy) = engram_core::get_policy(&d.namespace) {
                    if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
                        0
                    } else {
                        d.generation
                    }
                } else {
                    tracing::warn!(
                        namespace = %d.namespace,
                        generation = d.generation,
                        "get_policy failed for namespace; using provided generation"
                    );
                    d.generation
                };

                let pk = build_pk(project_id, &d.namespace, effective_gen, &d.doc_id);
                pks.push(pk);
                doc_ids.push(d.doc_id.clone());
                content_hashes.push(d.content_hash.clone());
                chunk_ids.push(d.chunk_id);
                paths.push(d.path.as_str().to_string());
                languages.push(d.language.clone());
                authors.push(d.author.clone());
                timestamps.push(d.timestamp);
                effective_gens.push(effective_gen);
                contents_for_embed.push(&d.content);
            }

            if !cancel.is_cancelled() && !contents_for_embed.is_empty() {
                for chunk in contents_for_embed.chunks(64) {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let batch_vecs = self.embedder.embed_batch_cancellable(chunk, cancel).await?;
                    anyhow::ensure!(
                        batch_vecs.len() == chunk.len()
                            && batch_vecs
                                .iter()
                                .all(|vector| vector.len() == self.embedder.dimension()
                                    && vector.iter().all(|value| value.is_finite())),
                        "Embedding provider returned invalid count, dimensions or non-finite values; vectors were not written"
                    );
                    vectors.extend(batch_vecs);
                }
            }

            if !cancel.is_cancelled() && !pks.is_empty() && vectors.len() == pks.len() {
                let ns = &docs[0].namespace;
                let batch = crate::vector::create_record_batch_with_gens(
                    project_id,
                    ns,
                    &effective_gens,
                    &pks,
                    &doc_ids,
                    &content_hashes,
                    &chunk_ids,
                    &paths,
                    &languages,
                    &authors,
                    &timestamps,
                    &vectors,
                    self.embedder.dimension(),
                )?;
                let written = if _table_override.is_some() {
                    // Recovery already owns an uncancelled native worker and
                    // exclusive admission. Keep its reservation through I/O.
                    let _retained_allocation = retained_allocation;
                    crate::vector::upsert_vectors(&table, vec![batch]).await
                } else {
                    let table = table.clone();
                    self.run_native_vector_mutation_with_commit(
                        commit.as_deref_mut().ok_or_else(|| {
                            anyhow::anyhow!("Vector writes require ordered write ownership")
                        })?,
                        async move {
                            let _retained_allocation = retained_allocation;
                            crate::vector::upsert_vectors(&table, vec![batch]).await
                        },
                    )
                    .await
                };
                written.map_err(|e| {
                    anyhow::anyhow!("LanceDB vector upsert failed — retry to repair: {e:#}")
                })?;
            }
        }

        Ok(())
    }

    pub async fn index_docs(
        &self,
        project_id: &str,
        docs: &[IndexDoc],
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        if docs.is_empty() || cancel.is_cancelled() {
            return Ok(());
        }
        let mut commit = scoped.acquire_commit(cancel).await?;
        #[cfg(feature = "vector")]
        let _bootstrap = if scoped.is_lexical_only() {
            Some(scoped.vector_routes.lexical_only().await)
        } else {
            None
        };
        if docs.is_empty() {
            return Ok(());
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        // ENG-AUD-2026-S05-0001: enforce homogeneous namespace at the entry point,
        // before either the Tantivy or vector write paths, so the invariant is
        // always checked regardless of which storage backend is active.
        // Mixed-namespace batches are rejected here rather than silently mis-keying
        // vector rows with docs[0].namespace.
        {
            let first_ns = &docs[0].namespace;
            let bad = docs
                .iter()
                .enumerate()
                .find(|(_, d)| d.namespace != *first_ns);
            if let Some((idx, bad_doc)) = bad {
                return Err(anyhow::anyhow!(
                    "ENG-AUD-2026-S05-0001: heterogeneous namespace batch rejected — \
                     docs[0].namespace={first_ns:?} but docs[{idx}].namespace={:?}; \
                     callers must partition by namespace before calling index_docs",
                    bad_doc.namespace
                ));
            }
        }

        #[cfg(feature = "vector")]
        if !scoped.is_lexical_only() {
            // Reject wrong/missing embedding spaces before changing lexical content.
            scoped.vector_write_table(project_id).await?;
        }

        // 1. Lexical index (Tantivy) — pk-based upsert
        {
            let _tantivy_guard = scoped
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        scoped.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "tantivy index writer",
                    )
                })
                .transpose()?;

            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
                scoped.acquire_writer("index_docs", cancel).await?;
            for d in docs {
                if cancel.is_cancelled() {
                    break;
                }

                let effective_gen = if let Ok(policy) = engram_core::get_policy(&d.namespace) {
                    if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
                        0
                    } else {
                        d.generation
                    }
                } else {
                    // ENG-AUD-2026-S15-001: policy load failed for this namespace.
                    // Falling back to the provided generation preserves data but may
                    // silently diverge from the intended retention/versioning policy.
                    tracing::warn!(
                        namespace = %d.namespace,
                        generation = d.generation,
                        "ENG-AUD-2026-S15-001: get_policy failed for namespace; \
                         using provided generation as fallback — versioning semantics may drift"
                    );
                    d.generation
                };

                let pk = build_pk(project_id, &d.namespace, effective_gen, &d.doc_id);
                // Delete any existing doc with the same pk (true upsert)
                writer.delete_term(Term::from_field_text(scoped.fields.pk, &pk));

                let tdoc = doc!(
                    scoped.fields.pk => pk.as_str(),
                    scoped.fields.doc_id => d.doc_id.as_str(),
                    scoped.fields.content_hash => d.content_hash.as_str(),
                    scoped.fields.project_id => project_id,
                    scoped.fields.namespace => d.namespace.as_str(),
                    scoped.fields.generation => effective_gen,
                    scoped.fields.chunk_id => d.chunk_id,
                    scoped.fields.path => d.path.as_str(),
                    scoped.fields.language => d.language.as_str(),
                    scoped.fields.author => d.author.as_deref().unwrap_or(""),
                    scoped.fields.timestamp => d.timestamp.unwrap_or(0),
                    scoped.fields.start_line => d.start_line as u64,
                    scoped.fields.end_line => d.end_line as u64,
                    scoped.fields.content => d.content.as_str(),
                    scoped.fields.content_words => d.content.as_str(),
                );
                writer.add_document(tdoc)?;
            }
            let lexical_context = scoped.lexical_context("index_docs", Some(project_id), docs.first().map(|d| d.generation).filter(|g| docs.iter().all(|d| d.generation == *g)));
            let lexical_opstamp = lexical_context.commit(&mut writer)?;
            // IMPORTANT: wait for Tantivy merge workers before dropping the writer.
            // Dropping an IndexWriter cancels in-flight/background merges, which can
            // leave thousands of tiny segments across batched indexing runs.
            // This call may block for seconds-to-minutes on large indexes and is
            // expected for correctness/consolidation.
            lexical_context.wait_merges(writer, lexical_opstamp)?;
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        // Both ingestion paths share the same routing and embedding contract.
        scoped
            .embed_vectors_in_commit(project_id, docs, cancel, None, Some(&mut commit))
            .await?;

        Ok(())
    }

    pub async fn purge_old_generations(
        &self,
        project_id: &str,
        active_generation: u64,
    ) -> anyhow::Result<()> {
        self.purge_old_generations_with_cutoff(project_id, active_generation, active_generation)
            .await
    }

    /// A conservative physical cutoff must not replace the published logical
    /// head when retaining inherited revisions. Caller serializes publication.
    pub async fn purge_old_generations_with_cutoff(
        &self,
        project_id: &str,
        published_generation: u64,
        active_generation: u64,
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let write_cancel = CancellationToken::new();
        let _commit = scoped.acquire_commit(&write_cancel).await?;
        #[cfg(feature = "vector")]
        let mut commit = _commit;
        anyhow::ensure!(
            active_generation <= published_generation,
            "garbage collection cutoff cannot exceed the published source generation"
        );
        // Logical snapshot retention and physical revision retention differ:
        // an unchanged file can still be selected from a much older generation.
        let protected_source = if scoped.snapshots.managed(project_id, "memory")? {
            Some(
                scoped
                    .snapshots
                    .protect_for_gc(project_id, "memory", published_generation)?,
            )
        } else {
            None
        };
        // 1. Tantivy purge
        {
            // MEM3-7d30af: acquire AllocationGuard before opening writer so the
            // memory budget is informed of the write-buffer reservation even during
            // purge/GC operations (same as index_docs does for the write path).
            let _tantivy_guard = scoped
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        scoped.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "tantivy purge writer",
                    )
                })
                .transpose()?;
            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> = scoped
                .acquire_writer("generation purge", &CancellationToken::new())
                .await?;

            for ns in engram_core::KNOWN_NAMESPACES {
                if let Ok(policy) = engram_core::get_policy(ns) {
                    let pid_term = Term::from_field_text(scoped.fields.project_id, project_id);
                    let ns_term = Term::from_field_text(scoped.fields.namespace, ns);

                    if *ns == "memory" {
                        if let Some(protected) = &protected_source {
                            if active_generation == 0 {
                                continue;
                            }
                            let selected = BooleanQuery::new(
                                protected
                                    .iter()
                                    .map(|view| (Occur::Should, view.query(&scoped.fields)))
                                    .collect(),
                            );
                            let parser = QueryParser::for_index(
                                &scoped.tantivy_index,
                                vec![scoped.fields.generation],
                            );
                            let older = parser.parse_query(&format!(
                                "generation:[* TO {}]",
                                active_generation - 1
                            ))?;
                            let query = BooleanQuery::new(vec![
                                (
                                    Occur::Must,
                                    Box::new(TermQuery::new(pid_term, IndexRecordOption::Basic))
                                        as Box<dyn tantivy::query::Query>,
                                ),
                                (
                                    Occur::Must,
                                    Box::new(TermQuery::new(ns_term, IndexRecordOption::Basic)),
                                ),
                                (Occur::Must, older),
                                (Occur::MustNot, Box::new(selected)),
                            ]);
                            writer.delete_query(Box::new(query))?;
                            continue;
                        }
                    }

                    match policy.retention {
                        engram_core::NamespaceRetention::KeepLatestOnly => {
                            // External audit 2026-08-29 P0-1: "latest only" used to mean
                            // "everything that is NOT the published generation" — which
                            // also deleted the generation an incremental update was still
                            // building by copy-forward (the pilot corpus collapsed to 56 chunks).
                            // Stale is OLDER than the published generation; newer
                            // generations belong to an update in flight and are kept.
                            if active_generation == 0 {
                                continue;
                            }
                            let parser = QueryParser::for_index(
                                &scoped.tantivy_index,
                                vec![scoped.fields.generation],
                            );
                            let Ok(older) = parser.parse_query(&format!(
                                "generation:[* TO {}]",
                                active_generation - 1
                            )) else {
                                continue;
                            };
                            let query = BooleanQuery::new(vec![
                                (
                                    Occur::Must,
                                    Box::new(TermQuery::new(pid_term, IndexRecordOption::Basic)),
                                ),
                                (
                                    Occur::Must,
                                    Box::new(TermQuery::new(ns_term, IndexRecordOption::Basic)),
                                ),
                                (Occur::Must, older),
                            ]);
                            writer.delete_query(Box::new(query))?;
                        }
                        engram_core::NamespaceRetention::KeepLastGenerations(n) => {
                            let min_keep = active_generation.saturating_sub(n as u64 - 1);
                            if min_keep > 0 {
                                let parser = QueryParser::for_index(
                                    &scoped.tantivy_index,
                                    vec![scoped.fields.generation],
                                );
                                if let Ok(gen_query) = parser.parse_query(&format!(
                                    "generation:[* TO {}]",
                                    min_keep.saturating_sub(1)
                                )) {
                                    let query = BooleanQuery::new(vec![
                                        (
                                            Occur::Must,
                                            Box::new(TermQuery::new(
                                                pid_term,
                                                IndexRecordOption::Basic,
                                            )),
                                        ),
                                        (
                                            Occur::Must,
                                            Box::new(TermQuery::new(
                                                ns_term,
                                                IndexRecordOption::Basic,
                                            )),
                                        ),
                                        (Occur::Must, gen_query),
                                    ]);
                                    writer.delete_query(Box::new(query))?;
                                }
                            }
                        }
                        engram_core::NamespaceRetention::KeepForever => {}
                    }
                }
            }
            let lexical_context = scoped.lexical_context("generation_purge", Some(project_id), Some(published_generation));
            let lexical_opstamp = lexical_context.commit(&mut writer)?;

            // Trigger segment merge to reclaim disk space from deleted documents.
            // Without this, deleted docs remain as tombstones in segments indefinitely.
            drop(writer.garbage_collect_files());
            // IMPORTANT: consume the writer only after commit/GC side effects are
            // queued so merge threads can run to completion before writer teardown.
            // This may block for seconds-to-minutes on heavily fragmented indexes.
            lexical_context.wait_merges(writer, lexical_opstamp)?;
        }

        // 2. LanceDB purge
        #[cfg(feature = "vector")]
        {
            let _vector_epoch = scoped.pin_vector_epoch(project_id)?;
            scoped.ensure_managed_vector_table_present(&_vector_epoch)?;
            let table_name = _vector_epoch.table.clone();
            if scoped
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                let table = scoped.open_vector_epoch_table(&_vector_epoch).await?;
                // The Tantivy purge above has already COMMITTED. If the
                // vector purge fails here the two stores are left skewed —
                // Tantivy shed its superseded generations and LanceDB kept
                // them — which is exactly the shape the integrity checker
                // reports as `vector_orphan`. Both failure modes are in the
                // daemon log ("Too many concurrent writers", "Access is
                // denied"), so name the consequence rather than letting an
                // hour-later mismatch look like spontaneous corruption.
                let source_selection = protected_source.as_ref().map(|views| {
                    views
                        .iter()
                        .map(|view| format!("({})", view.sql()))
                        .collect::<Vec<_>>()
                        .join(" OR ")
                });
                let native_store = scoped.native_store.clone();
                scoped
                    .run_native_vector_mutation_with_commit(&mut commit, async move {
                        let _source_protection = protected_source;
                        let native = native_store.capture()?;
                        crate::vector::purge_old_generations_serialized(
                            &native.connection,
                            table.name(),
                            active_generation,
                            source_selection.as_deref(),
                        )
                        .await
                    })
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "vector purge failed AFTER the Tantivy purge committed, so the \
                             stores are now skewed for project {project_id} (Tantivy dropped \
                             generations older than {active_generation}, LanceDB kept them). \
                             This surfaces as a `vector_orphan` integrity mismatch until a \
                             later GC tick succeeds: {e}"
                        )
                    })?;
            }
        }

        Ok(())
    }

    /// Copy all docs for `unchanged_paths` from `old_generation` → `new_generation`.
    /// This implements copy-forward for snapshot namespaces: unchanged files still
    /// appear in the new generation without re-reading from disk.
    pub async fn copy_generation_for_paths(
        &self,
        project_id: &str,
        namespace: &str,
        old_generation: u64,
        new_generation: u64,
        unchanged_paths: &[RelPath],
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        if unchanged_paths.is_empty() {
            return Ok(());
        }

        use tantivy::query::Query;
        anyhow::ensure!(!cancel.is_cancelled(), "snapshot copy cancelled");
        let reader = scoped.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let wanted = unchanged_paths
            .iter()
            .map(RelPath::as_str)
            .collect::<std::collections::HashSet<_>>();
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(scoped.fields.project_id, project_id),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(scoped.fields.namespace, namespace),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(scoped.fields.generation, old_generation),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
        ]);
        let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(
            &searcher,
        ))?;
        let _allocation = scoped
            .memory_budget
            .as_ref()
            .map(|budget| {
                AllocationGuard::try_new(
                    budget,
                    scoped.tantivy_writer_memory as u64,
                    Subsystem::Tantivy,
                    "snapshot copy writer",
                )
            })
            .transpose()?;
        let mut writer = scoped.create_bulk_writer_async(project_id, cancel).await?;
        let mut batch = Vec::with_capacity(512);
        let mut batch_bytes = 0usize;
        // Scan the old snapshot once. Per-file queries and per-batch writer
        // shutdowns made a one-byte source edit repeatedly scan and compact
        // the complete repository. Keep only one bounded batch in memory.
        for (segment_ord, segment) in searcher.segment_readers().iter().enumerate() {
            let mut scorer = weight.scorer(segment, 1.0)?;
            while scorer.doc() != tantivy::TERMINATED {
                anyhow::ensure!(!cancel.is_cancelled(), "snapshot copy cancelled");
                let doc_id = scorer.doc();
                scorer.advance();
                if segment.is_deleted(doc_id) {
                    continue;
                }
                let doc: tantivy::TantivyDocument =
                    searcher.doc(DocAddress::new(segment_ord as u32, doc_id))?;
                let get_str = |field| {
                    doc.get_first(field)
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let get_u64 = |field| {
                    doc.get_first(field)
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0)
                };
                let path = get_str(scoped.fields.path);
                if !wanted.contains(path.as_str()) {
                    continue;
                }
                let hash = get_str(scoped.fields.content_hash);
                let start_line = get_u64(scoped.fields.start_line) as u32;
                let end_line = get_u64(scoped.fields.end_line) as u32;
                let content = get_str(scoped.fields.content);
                batch_bytes = batch_bytes.saturating_add(content.len());
                let author = get_str(scoped.fields.author);
                let timestamp = get_u64(scoped.fields.timestamp);
                batch.push(IndexDoc {
                    generation: new_generation,
                    chunk_id: get_u64(scoped.fields.chunk_id),
                    doc_id: DocIdStr::compute(
                        &path,
                        start_line,
                        end_line,
                        &ContentHash(hash.clone()),
                    )
                    .0,
                    path: RelPath::new(&path),
                    language: get_str(scoped.fields.language),
                    content,
                    namespace: namespace.to_string(),
                    author: (!author.is_empty()).then_some(author),
                    timestamp: (timestamp != 0).then_some(timestamp),
                    start_line,
                    end_line,
                    content_hash: hash,
                });
                if batch.len() >= 512 || batch_bytes >= 1024 * 1024 {
                    Self::write_docs_to_writer(&scoped.fields, &mut writer, project_id, &batch)?;
                    scoped
                        .embed_and_upsert_vectors_with_writer(
                            &mut writer,
                            project_id,
                            &batch,
                            cancel,
                        )
                        .await?;
                    writer.maybe_commit(4096)?;
                    batch.clear();
                    batch_bytes = 0;
                }
            }
        }
        anyhow::ensure!(!cancel.is_cancelled(), "snapshot copy cancelled");
        if !batch.is_empty() {
            Self::write_docs_to_writer(&scoped.fields, &mut writer, project_id, &batch)?;
            scoped
                .embed_and_upsert_vectors_with_writer(&mut writer, project_id, &batch, cancel)
                .await?;
        }
        writer.finish()?;
        Ok(())
    }

    /// Delete all docs for given paths from GlobalMutable/AppendOnly namespaces.
    /// For Snapshot namespaces, use copy-forward instead; this is a no-op for those.
    /// Delete EVERY doc of one namespace for one project — the purge behind
    /// `ingest_quality_gates {clear_existing: true}` (row-3 audit A7: a corpus
    /// ingested by a broken parser could never be replaced). Returns the number
    /// of Tantivy docs that matched before the delete. Same store order as
    /// `delete_files`: LanceDB first, Tantivy commits only after it succeeds.
    pub async fn delete_namespace(
        &self,
        project_id: &str,
        namespace: &str,
    ) -> anyhow::Result<usize> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let write_cancel = CancellationToken::new();
        let _commit = scoped.acquire_commit(&write_cancel).await?;
        #[cfg(feature = "vector")]
        let mut commit = _commit;
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(scoped.fields.project_id, project_id),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(scoped.fields.namespace, namespace),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let matched = {
            let reader = scoped.tantivy_index.reader()?;
            let searcher = reader.searcher();
            searcher.search(&query, &tantivy::collector::Count)?
        };
        if matched == 0 {
            return Ok(0);
        }

        #[cfg(feature = "vector")]
        {
            let _vector_epoch = scoped.pin_vector_epoch(project_id)?;
            scoped.ensure_managed_vector_table_present(&_vector_epoch)?;
            let table_name = _vector_epoch.table.clone();
            if scoped
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                let table = scoped.open_vector_epoch_table(&_vector_epoch).await?;
                let safe_ns = namespace.replace('\'', "''");
                scoped
                    .delete_vector_rows(&mut commit, &table, format!("namespace = '{safe_ns}'"))
                    .await?;
            }
        }

        {
            let _tantivy_guard = scoped
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        scoped.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "tantivy delete writer",
                    )
                })
                .transpose()?;
            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> = scoped
                .acquire_writer("delete_namespace", &CancellationToken::new())
                .await?;
            writer.delete_query(Box::new(query))?;
            let lexical_context = scoped.lexical_context("delete_namespace", Some(project_id), None);
            let lexical_opstamp = lexical_context.commit(&mut writer)?;
            lexical_context.wait_merges(writer, lexical_opstamp)?;
        }
        Ok(matched)
    }

    /// Remove only the named document identities within a project/namespace.
    /// Used after replacement content is successfully indexed; a failure can
    /// leave duplicate old evidence, but never deletes the replacement by path.
    pub async fn delete_documents(
        &self,
        project_id: &str,
        namespace: &str,
        doc_ids: &[String],
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let write_cancel = CancellationToken::new();
        let _commit = scoped.acquire_commit(&write_cancel).await?;
        #[cfg(feature = "vector")]
        let mut commit = _commit;
        if doc_ids.is_empty() {
            return Ok(());
        }
        #[cfg(feature = "vector")]
        {
            let _vector_epoch = scoped.pin_vector_epoch(project_id)?;
            scoped.ensure_managed_vector_table_present(&_vector_epoch)?;
            let table_name = _vector_epoch.table.clone();
            if scoped
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                let table = scoped.open_vector_epoch_table(&_vector_epoch).await?;
                for batch in doc_ids.chunks(200) {
                    let ids = batch
                        .iter()
                        .map(|s| format!("'{}'", s.replace('\'', "''")))
                        .collect::<Vec<_>>()
                        .join(", ");
                    scoped
                        .delete_vector_rows(
                            &mut commit,
                            &table,
                            format!(
                                "namespace = '{}' AND doc_id IN ({ids})",
                                namespace.replace('\'', "''")
                            ),
                        )
                        .await?;
                }
            }
        }
        let _budget = scoped
            .memory_budget
            .as_ref()
            .map(|budget| {
                AllocationGuard::try_new(
                    budget,
                    scoped.tantivy_writer_memory as u64,
                    Subsystem::Tantivy,
                    "document cleanup writer",
                )
            })
            .transpose()?;
        let mut writer = scoped
            .acquire_writer("delete_documents", &CancellationToken::new())
            .await?;
        for id in doc_ids {
            let terms = [
                (scoped.fields.project_id, project_id),
                (scoped.fields.namespace, namespace),
                (scoped.fields.doc_id, id.as_str()),
            ];
            let query = BooleanQuery::new(
                terms
                    .into_iter()
                    .map(|(field, value)| {
                        (
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(field, value),
                                IndexRecordOption::Basic,
                            )) as Box<dyn tantivy::query::Query>,
                        )
                    })
                    .collect(),
            );
            writer.delete_query(Box::new(query))?;
        }
        let lexical_context = scoped.lexical_context("delete_documents", Some(project_id), None);
        let lexical_opstamp = lexical_context.commit(&mut writer)?;
        lexical_context.wait_merges(writer, lexical_opstamp)?;
        Ok(())
    }

    pub async fn delete_files(
        &self,
        project_id: &str,
        namespace: &str,
        paths: &[RelPath],
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let write_cancel = CancellationToken::new();
        let _commit = scoped.acquire_commit(&write_cancel).await?;
        #[cfg(feature = "vector")]
        let mut commit = _commit;
        if paths.is_empty() {
            return Ok(());
        }

        // 1. LanceDB first — attempt before Tantivy commits so that a LanceDB
        //    failure leaves Tantivy in its pre-delete state (no cross-store divergence).
        #[cfg(feature = "vector")]
        {
            let _vector_epoch = scoped.pin_vector_epoch(project_id)?;
            scoped.ensure_managed_vector_table_present(&_vector_epoch)?;
            let table_name = _vector_epoch.table.clone();
            if scoped
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                let table = scoped.open_vector_epoch_table(&_vector_epoch).await?;
                let safe_ns = namespace.replace('\'', "''");
                // Chunked IN-filters: every `table.delete()` rewrites Lance
                // fragments, so one delete PER PATH turned a 1,670-doc
                // cleanup into ~19 minutes. One delete per 200 paths keeps
                // the filter string modest and the fragment churn bounded.
                for chunk in paths.chunks(200) {
                    let list = chunk
                        .iter()
                        // Escape single quotes (SQL injection safety).
                        .map(|p| format!("'{}'", p.as_str().replace('\'', "''")))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let filter = format!("namespace = '{safe_ns}' AND path IN ({list})");
                    scoped
                        .delete_vector_rows(&mut commit, &table, filter)
                        .await?;
                }
            }
        }

        // 2. Tantivy — commit only after LanceDB succeeds (or the vector feature is absent).
        {
            // MEM3-7d30af: acquire AllocationGuard before opening writer so the
            // memory budget is informed of the write-buffer reservation during
            // delete operations (mirrors index_docs guard pattern).
            let _tantivy_guard = scoped
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        scoped.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "tantivy delete writer",
                    )
                })
                .transpose()?;
            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> = scoped
                .acquire_writer("delete_files", &CancellationToken::new())
                .await?;
            for p in paths {
                let pid_term = Term::from_field_text(scoped.fields.project_id, project_id);
                let ns_term = Term::from_field_text(scoped.fields.namespace, namespace);
                let path_term = Term::from_field_text(scoped.fields.path, p.as_str());
                let query = BooleanQuery::new(vec![
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(pid_term, IndexRecordOption::Basic)),
                    ),
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(ns_term, IndexRecordOption::Basic)),
                    ),
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(path_term, IndexRecordOption::Basic)),
                    ),
                ]);
                writer.delete_query(Box::new(query))?;
            }
            let lexical_context = scoped.lexical_context("delete_files", Some(project_id), None);
            let lexical_opstamp = lexical_context.commit(&mut writer)?;
            // IMPORTANT: wait for Tantivy merge workers before dropping the writer.
            // Dropping an IndexWriter cancels in-flight/background merges, which can
            // leave the index in a highly fragmented segment state over time.
            // This may block for seconds-to-minutes on large indexes and is expected.
            lexical_context.wait_merges(writer, lexical_opstamp)?;
        }

        Ok(())
    }

    /// Return lightweight metadata for all Tantivy docs in a project.
    /// One indexed chunk's stored fields, as the grep full-scan tier needs
    /// them.
    ///
    /// Tantivy already STORES chunk text, so a byte scan has no reason to
    /// consult a second store for it.
    pub fn stored_chunk_at(
        &self,
        searcher: &tantivy::Searcher,
        addr: DocAddress,
    ) -> anyhow::Result<StoredChunk> {
        let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
        let get_str = |f| {
            doc.get_first(f)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Ok(StoredChunk {
            doc_id: get_str(self.fields.doc_id),
            chunk_id: doc
                .get_first(self.fields.chunk_id)
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            path: get_str(self.fields.path),
            language: get_str(self.fields.language),
            start_line: doc
                .get_first(self.fields.start_line)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            content: get_str(self.fields.content),
        })
    }

    /// Addresses of every chunk in one project/namespace/generation, plus the
    /// searcher to read them from.
    ///
    /// Two-phase on purpose: collecting addresses touches no stored fields,
    /// so a caller doing a full-corpus scan can fetch chunk text in parallel
    /// and hold only one chunk per worker instead of the whole corpus.
    ///
    /// The generation clause follows the namespace's own versioning policy,
    /// matching what search does — a Snapshot namespace pins the active
    /// generation, AppendOnly takes everything up to it, GlobalMutable is
    /// unversioned.
    pub fn chunk_addresses(
        &self,
        project_id: &str,
        namespace: &str,
        generation: u64,
    ) -> anyhow::Result<(tantivy::Searcher, Vec<DocAddress>)> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();

        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.project_id, project_id),
                    IndexRecordOption::Basic,
                )) as Box<dyn tantivy::query::Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.namespace, namespace),
                    IndexRecordOption::Basic,
                )),
            ),
        ];
        let _snapshot =
            self.add_generation_filter(project_id, namespace, generation, &mut clauses)?;
        let query = BooleanQuery::new(clauses);

        const PAGE_SIZE: usize = 4000;
        let mut offset = 0usize;
        let mut out = Vec::new();
        loop {
            let page: Vec<(Score, DocAddress)> =
                searcher.search(&query, &TopDocs::with_limit(PAGE_SIZE).and_offset(offset))?;
            if page.is_empty() {
                break;
            }
            out.extend(page.iter().map(|(_, addr)| *addr));
            offset = offset.saturating_add(page.len());
            if page.len() < PAGE_SIZE {
                break;
            }
        }
        Ok((searcher, out))
    }

    /// Doc summaries for one project, restricted to a single namespace.
    ///
    /// `list_docs_for_project` pages through EVERY doc in the project and
    /// reads each one's stored fields. Callers that only want one namespace
    /// (the `history` namespace's `pr:*` docs, say) were paying that for the
    /// whole corpus and then discarding almost all of it. Pushing the
    /// namespace into the query makes the cost proportional to the answer.
    pub fn list_docs_in_namespace(
        &self,
        project_id: &str,
        namespace: &str,
    ) -> anyhow::Result<Vec<SearchDocSummary>> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.project_id, project_id),
                    IndexRecordOption::Basic,
                )) as Box<dyn tantivy::query::Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.namespace, namespace),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);

        const PAGE_SIZE: usize = 2000;
        let mut offset = 0usize;
        let mut out = Vec::new();
        loop {
            let page: Vec<(Score, DocAddress)> =
                searcher.search(&query, &TopDocs::with_limit(PAGE_SIZE).and_offset(offset))?;
            if page.is_empty() {
                break;
            }
            for (_, addr) in page.iter().copied() {
                let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
                let get = |f| {
                    doc.get_first(f)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                out.push(SearchDocSummary {
                    namespace: get(self.fields.namespace),
                    doc_id: get(self.fields.doc_id),
                    path: get(self.fields.path),
                });
            }
            offset = offset.saturating_add(page.len());
            if page.len() < PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }

    pub fn list_docs_for_project(&self, project_id: &str) -> anyhow::Result<Vec<SearchDocSummary>> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let pid_term = Term::from_field_text(self.fields.project_id, project_id);
        let pid_q = TermQuery::new(pid_term, IndexRecordOption::Basic);

        const PAGE_SIZE: usize = 2000;
        let mut offset = 0usize;
        let mut out = Vec::new();
        loop {
            let page: Vec<(Score, DocAddress)> =
                searcher.search(&pid_q, &TopDocs::with_limit(PAGE_SIZE).and_offset(offset))?;
            if page.is_empty() {
                break;
            }

            for (_, addr) in page.iter().copied() {
                let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
                let namespace = doc
                    .get_first(self.fields.namespace)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let doc_id = doc
                    .get_first(self.fields.doc_id)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let path = doc
                    .get_first(self.fields.path)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                out.push(SearchDocSummary {
                    namespace,
                    doc_id,
                    path,
                });
            }
            offset += page.len();
        }
        Ok(out)
    }

    pub fn count_docs(&self, project_id: &str) -> anyhow::Result<usize> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let pid_term = Term::from_field_text(self.fields.project_id, project_id);
        let pid_q = TermQuery::new(pid_term, IndexRecordOption::Basic);
        let count = searcher.search(&pid_q, &tantivy::collector::Count)?;
        Ok(count)
    }

    /// Count docs per namespace for a project (memory, history, antipattern, etc.).
    /// Docs of one namespace at one exact generation — the generation
    /// completeness signal (external audit 2026-08-29 P0-2): the published
    /// generation of the code namespace must carry the whole corpus.
    pub fn count_docs_in_generation(
        &self,
        project_id: &str,
        namespace: &str,
        generation: u64,
    ) -> anyhow::Result<usize> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.project_id, project_id),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.namespace, namespace),
                    IndexRecordOption::Basic,
                )),
            ),
        ];
        let (selection, _snapshot) =
            self.exact_generation_selection(project_id, Some(namespace), generation)?;
        clauses.push((Occur::Must, selection));
        let q = BooleanQuery::new(clauses);
        Ok(searcher.search(&q, &tantivy::collector::Count)?)
    }

    /// The distinct PATHS one generation holds (all namespaces, or one) —
    /// external audit round 2 P0-2: completeness is a path-set comparison,
    /// never a chunk/file ratio.
    pub fn paths_in_generation(
        &self,
        project_id: &str,
        namespace: Option<&str>,
        generation: u64,
    ) -> anyhow::Result<std::collections::BTreeSet<String>> {
        use tantivy::schema::Value;
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![(
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.project_id, project_id),
                IndexRecordOption::Basic,
            )),
        )];
        let (selection, _snapshot) =
            self.exact_generation_selection(project_id, namespace, generation)?;
        clauses.push((Occur::Must, selection));
        if let Some(ns) = namespace {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.namespace, ns),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        let q = BooleanQuery::new(clauses);
        let addrs = searcher.search(&q, &tantivy::collector::DocSetCollector)?;
        let mut out = std::collections::BTreeSet::new();
        for addr in addrs {
            let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
            if let Some(p) = doc.get_first(self.fields.path).and_then(|v| v.as_str()) {
                out.insert(p.to_string());
            }
        }
        Ok(out)
    }

    /// The distinct PATHS one generation holds in the VECTOR store
    /// (external audit round 2 P0-2 — the vector store was never checked).
    pub async fn vector_paths_in_generation(
        &self,
        project_id: &str,
        namespace: Option<&str>,
        generation: u64,
    ) -> anyhow::Result<std::collections::BTreeSet<String>> {
        self.vector_generation_inventory(project_id, namespace, generation)
            .await
            .map(|(_, paths)| paths)
    }

    /// One complete selected-vector observation: total rows and distinct paths.
    /// Counting rows separately would plan and scan the same filtered dataset
    /// again. Return neither observation until the bounded stream reaches EOF.
    pub async fn vector_generation_inventory(
        &self,
        project_id: &str,
        namespace: Option<&str>,
        generation: u64,
    ) -> anyhow::Result<(usize, std::collections::BTreeSet<String>)> {
        #[cfg(feature = "vector")]
        {
            use lancedb::query::Select;
            let _vector_epoch = self.pin_vector_epoch(project_id)?;
            let (selection, snapshot) = self.exact_generation_sql(project_id, namespace, generation)?;
            let filter = match namespace {
                Some(ns) => format!("namespace = '{}' AND ({selection})", ns.replace('\'', "''")),
                None => selection,
            };
            let worker = self.clone();
            return self.run_native_vector_query(_vector_epoch.clone(), &CancellationToken::new(), async move {
                let _snapshot = snapshot;
                worker.ensure_managed_vector_table_present(&_vector_epoch)?;
                let table_name = _vector_epoch.table.clone();
                if !worker
                    .native_store.capture()?.connection
                    .table_names()
                    .execute()
                    .await?
                    .contains(&table_name)
                {
                    return Ok((0, Default::default()));
                }
                let table = worker.open_vector_epoch_table(&_vector_epoch).await?;
                let results = table
                    .query()
                    .only_if(filter)
                    .select(Select::Columns(vec!["path".to_string()]))
                    .limit(vector_generation_inventory::MAX_ROWS + 1)
                    .execute()
                    .await?;
                vector_generation_inventory::observe(results).await
            }).await;
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = (project_id, namespace, generation);
            Ok((0, Default::default()))
        }
    }

    /// Delete the vector rows of `paths` and nothing else — the one-sided loss
    /// the integrity tests inject (Tantivy keeps the paths).
    pub async fn delete_vector_rows_for_paths(
        &self,
        project_id: &str,
        namespace: &str,
        paths: &[RelPath],
    ) -> anyhow::Result<()> {
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let write_cancel = CancellationToken::new();
        let _commit = scoped.acquire_commit(&write_cancel).await?;
        #[cfg(feature = "vector")]
        let mut commit = _commit;
        #[cfg(feature = "vector")]
        {
            if paths.is_empty() {
                return Ok(());
            }
            let _vector_epoch = scoped.pin_vector_epoch(project_id)?;
            scoped.ensure_managed_vector_table_present(&_vector_epoch)?;
            let table_name = _vector_epoch.table.clone();
            if !scoped
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                return Ok(());
            }
            let table = scoped.open_vector_epoch_table(&_vector_epoch).await?;
            let safe_ns = namespace.replace('\'', "''");
            for chunk in paths.chunks(200) {
                let list = chunk
                    .iter()
                    .map(|p| format!("'{}'", p.as_str().replace('\'', "''")))
                    .collect::<Vec<_>>()
                    .join(", ");
                let filter = format!("namespace = '{safe_ns}' AND path IN ({list})");
                scoped
                    .delete_vector_rows(&mut commit, &table, filter)
                    .await?;
            }
            Ok(())
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = (project_id, namespace, paths);
            Ok(())
        }
    }

    pub fn count_docs_by_namespace(
        &self,
        project_id: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, usize>> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let mut counts = std::collections::HashMap::new();
        for ns in &[
            "memory",
            "history",
            "antipattern",
            "vfs",
            // pre_push_audit's rule namespace (row-3 audit A6): an empty
            // namespace must be reportable as INACTIVE.
            "quality_gate",
        ] {
            let q = BooleanQuery::new(vec![
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.project_id, project_id),
                        IndexRecordOption::Basic,
                    )) as Box<dyn tantivy::query::Query>,
                ),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.namespace, ns),
                        IndexRecordOption::Basic,
                    )),
                ),
            ]);
            let c = searcher.search(&q, &tantivy::collector::Count)?;
            if c > 0 {
                counts.insert(ns.to_string(), c);
            }
        }
        Ok(counts)
    }

    /// Count docs per language for a project (within "memory" namespace).
    pub fn count_docs_by_language(
        &self,
        project_id: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, usize>> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let mut counts = std::collections::HashMap::new();
        // Enumerate all segments and collect unique language values
        for segment_reader in searcher.segment_readers() {
            let inverted_index = segment_reader.inverted_index(self.fields.language)?;
            let dict = inverted_index.terms();
            let mut stream = dict.stream()?;
            while let Some((term_bytes, _)) = stream.next() {
                if let Ok(lang) = std::str::from_utf8(term_bytes)
                    && !lang.is_empty()
                    && !counts.contains_key(lang)
                {
                    // Now count docs matching (project_id AND namespace=memory AND language=lang)
                    let q = BooleanQuery::new(vec![
                        (
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.project_id, project_id),
                                IndexRecordOption::Basic,
                            )) as Box<dyn tantivy::query::Query>,
                        ),
                        (
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.namespace, "memory"),
                                IndexRecordOption::Basic,
                            )),
                        ),
                        (
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.language, lang),
                                IndexRecordOption::Basic,
                            )),
                        ),
                    ]);
                    let c = searcher.search(&q, &tantivy::collector::Count)?;
                    if c > 0 {
                        counts.insert(lang.to_string(), c);
                    }
                }
            }
        }
        Ok(counts)
    }

    /// Vector rows of ONE generation — external audit round 2 P0-1: the
    /// generation race is asserted on LanceDB, not only on Tantivy.
    pub async fn count_vectors_in_generation(
        &self,
        project_id: &str,
        generation: u64,
    ) -> anyhow::Result<usize> {
        #[cfg(feature = "vector")]
        {
            let _vector_epoch = self.pin_vector_epoch(project_id)?;
            let (selection, snapshot) = self.exact_generation_sql(project_id, None, generation)?;
            let worker = self.clone();
            return self.run_native_vector_query(_vector_epoch.clone(), &CancellationToken::new(), async move {
                let _snapshot = snapshot;
                worker.ensure_managed_vector_table_present(&_vector_epoch)?;
                let table_name = _vector_epoch.table.clone();
                if !worker
                    .native_store.capture()?.connection
                    .table_names()
                    .execute()
                    .await?
                    .contains(&table_name)
                {
                    return Ok(0);
                }
                let table = worker.open_vector_epoch_table(&_vector_epoch).await?;
                Ok(table.count_rows(Some(selection)).await?)
            }).await;
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = (project_id, generation);
            Ok(0)
        }
    }

    pub async fn count_vectors(&self, project_id: &str) -> anyhow::Result<usize> {
        #[cfg(feature = "vector")]
        {
            let _vector_epoch = self.pin_vector_epoch(project_id)?;
            let worker = self.clone();
            return self.run_native_vector_query(_vector_epoch.clone(), &CancellationToken::new(), async move {
                worker.ensure_managed_vector_table_present(&_vector_epoch)?;
                let table_name = _vector_epoch.table.clone();
                if !worker
                    .native_store.capture()?.connection
                    .table_names()
                    .execute()
                    .await?
                    .contains(&table_name)
                {
                    return Ok(0);
                }
                let table = worker.open_vector_epoch_table(&_vector_epoch).await?;
                Ok(table.count_rows(None).await?)
            }).await;
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = project_id;
            Ok(0)
        }
    }

    /// Bind ingestion to the complete VB path inventory admitted by its source
    /// scan. This does not enumerate disk or change file selection. Without an
    /// inventory, trusted VB ingestion refuses; standalone analysis remains
    /// available through the extractor APIs.
    pub fn with_vb_source_inventory(&self, files: Arc<Vec<PathBuf>>) -> Self {
        let mut scoped = self.clone();
        scoped.vb_source_inventory = Some(files);
        scoped
    }

    /// Full reindex of all given files. Used for both `index_project` and
    /// the "changed files" portion of `update_project`.
    #[allow(clippy::too_many_arguments)]
    pub async fn index_files<F>(
        &self,
        project_id: &str,
        namespace: &str,
        generation: u64,
        root: &Path,
        files: Vec<PathBuf>,
        max_chars_per_chunk: usize,
        cancel: &CancellationToken,
        mut progress_cb: F,
    ) -> anyhow::Result<IngestStats>
    where
        F: FnMut(usize, usize) + Send,
    {
        let parse_admission = self.acquire_parse_permit().await?;
        // Preparation owns the singleton compilation until every actual parse
        // worker/result is disposed, including after async request cancellation.
        let vb_compilation = tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("Source indexing cancelled before compilation admission"),
            lease = crate::vb_extractor::prepare_ingestion_batch(root, &files, self.vb_source_inventory.clone(),
                self.memory_budget.clone(), parse_admission.clone(),
                #[cfg(test)] self.parse_preparation_pause.clone().map(|pause|
                    Box::new(move || pause.wait()) as Box<dyn FnOnce() + Send>)) => lease?,
        };
        let mutation = self.begin_project_mutation(project_id)?;
        let scoped = self.with_mutation(&mutation)?;
        let mut guard = scoped.create_bulk_writer_async(project_id, cancel).await?;
        use rayon::prelude::*;

        let mut stats = IngestStats {
            source_extraction_identity: Some(crate::SourceExtractionIdentity::for_chunk_target(
                max_chars_per_chunk,
            )),
            ..IngestStats::default()
        };
        let total_files = files.len();
        stats.files = total_files;

        // Single Tantivy writer for the entire index_files run.
        // BulkWriterGuard commits + waits on Drop (cancel-safe).

        let fields = scoped.fields();

        let mut batch: Vec<IndexDoc> = Vec::with_capacity(512);
        let mut processed_count = 0;

        for file_chunk in files.chunks(50) {
            if cancel.is_cancelled() {
                break;
            }

            let chunking_estimate = file_chunk
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
                .sum::<u64>();
            let chunking_guard = scoped
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        chunking_estimate.max(1),
                        Subsystem::ParseBuffer,
                        "chunking/parse batch",
                    )
                })
                .transpose()?;

            let chunk_paths = file_chunk.to_vec();
            let extractor = scoped.extractor.clone();
            let root_buf = root.to_path_buf();
            let namespace_str = namespace.to_string();
            let work = parse_work::ParseWork {
                reservation: chunking_guard,
                operation: Some(scoped.begin_operation()?),
                admission: parse_admission.clone(),
                vb_compilation: vb_compilation.clone(),
                #[cfg(test)]
                pause: scoped.parse_pause.clone(),
            };
            #[cfg(test)]
            let parse_pause = scoped.parse_pause.clone();

            let parsed = tokio::task::spawn_blocking(move || {
                // Bind ownership before creating any worker-local buffers.
                // The returned envelope retains it if the waiter disappears.
                let work = work;
                let vb_compilation = work.vb_compilation.as_ref();
                let (chunk_stats, chunk_docs) = chunk_paths
                    .par_iter()
                    .map(|p| parse_work::contain_extraction_panic(p, || -> anyhow::Result<_> {
                        let mut local_stats = IngestStats::default();
                        let mut local_docs = Vec::new();
                        let rel_path = RelPath::from_relative(&root_buf, p)
                            .unwrap_or_else(|| RelPath::new(&p.to_string_lossy()));

                        let meta = std::fs::metadata(p).map_err(|error|
                            anyhow::anyhow!("Cannot read source metadata {}: {error}", p.display()))?;
                        if meta.len() > ingest::MAX_FILE_SIZE {
                            local_stats
                                .skipped_files
                                .push((rel_path, format!("File too large ({} bytes)", meta.len())));
                            return Ok((local_stats, local_docs));
                        }

                        use std::io::Read;
                        // Classify from the same open handle used for text.
                        // The advisory is_binary helper maps read errors to
                        // true; that must never turn an I/O failure into an
                        // authoritative "binary file" exclusion.
                        let mut file = std::fs::File::open(p).map_err(|error|
                            anyhow::anyhow!("Cannot open source content {}: {error}", p.display()))?;
                        let mut bytes = Vec::with_capacity(meta.len() as usize + 1);
                        (&mut file).take(8192).read_to_end(&mut bytes).map_err(|error|
                            anyhow::anyhow!("Cannot read source content {}: {error}", p.display()))?;
                        if bytes.contains(&0) {
                            local_stats
                                .skipped_files
                                .push((rel_path, "Binary file".into()));
                            return Ok((local_stats, local_docs));
                        }

                        // An I/O failure is not a policy skip. Publishing the
                        // remaining files would turn a transient access failure
                        // into a durable deletion from the source snapshot.
                        let remaining = ingest::MAX_FILE_SIZE + 1 - bytes.len() as u64;
                        file.take(remaining).read_to_end(&mut bytes).map_err(|error|
                            anyhow::anyhow!("Cannot read source content {}: {error}", p.display()))?;
                        anyhow::ensure!(bytes.len() as u64 <= ingest::MAX_FILE_SIZE,
                            "Source file grew beyond the ingestion limit while reading {}; retry the update", p.display());
                        let size = bytes.len() as u64;
                        local_stats.bytes += size;

                        let file_hash = blake3::hash(&bytes).to_hex().to_string();
                        // Reuse the metadata we already read (avoids second stat syscall)
                        let modified = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                        let mtime_ms = modified
                            .and_then(|d| u64::try_from(d.as_millis()).ok()).unwrap_or(0);
                        let mtime_ns = modified
                            .and_then(|d| u64::try_from(d.as_nanos()).ok());

                        // Wrap rel_path in Arc so all symbols/edges/chunks from this file
                        // share one allocation. O(1) clone instead of O(N) string copies.
                        let arc_rel = Arc::new(rel_path);

                        // Record the file + its fingerprint BEFORE the decode
                        // gate: an undecodable file must still get a
                        // fingerprint-carrying file node, otherwise the
                        // incremental change scan re-reads and re-hashes it
                        // on EVERY update forever (observed as a permanent
                        // 4-file "changed" tail of minified vendor JS +
                        // cp1252 CSS).
                        local_stats.all_files.push((*arc_rel).clone());
                        local_stats
                            .fingerprints
                            .push(crate::docstore::FileFingerprint {
                                rel_path: arc_rel.as_str().to_string(),
                                size,
                                mtime_ms,
                                mtime_ns,
                                file_hash,
                            });

                        // Round-2 audit P0-2 follow-up: a Latin-1 comment must not
                        // eject a whole source file from the corpus (live: 4 pilot-corpus
                        // files were fingerprinted — a graph File node — yet absent
                        // from every search store). Decode lossily and say so.
                        let text = match String::from_utf8(bytes) {
                            Ok(t) => t,
                            Err(e) => {
                                tracing::warn!(
                                    path = %p.display(),
                                    "file is not valid UTF-8 — indexed with lossy decoding"
                                );
                                local_stats.warnings.push(format!(
                                    "{}: not valid UTF-8, indexed lossily",
                                    arc_rel.as_str()
                                ));
                                String::from_utf8_lossy(e.as_bytes()).into_owned()
                            }
                        };
                        let language = engram_core::guess_language(p);
                        *local_stats
                            .languages
                            .entry(language.to_string())
                            .or_insert(0) += 1;

                        let ext_lower = p
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.to_lowercase());

                        // Vendor assets (package-manager dirs, minified
                        // bundles) stay searchable but must not feed the
                        // graph: bare-name matches from a 53k-line vendor
                        // bundle create phantom dependency edges into app
                        // code (false blast radius, false paths).
                        let is_vendor = engram_core::is_vendor_path(arc_rel.as_str());

                        #[cfg(test)]
                        if let Some(pause) = &parse_pause { pause.wait(); }
                        let mut vb_provenance = None;
                        let (syms, edges) = if is_vendor {
                            (Vec::new(), Vec::new())
                        } else if crate::webforms::is_webforms_markup(p) {
                            crate::webforms::extract_webforms(&root_buf, &arc_rel, &text)
                        } else if crate::layout_extractor::is_winforms_designer(p) {
                            let base_lang = if ext_lower.as_deref() == Some("vb") {
                                "vb"
                            } else {
                                "cs"
                            };
                            let (mut s, mut e) = if base_lang == "vb" {
                                let lease = vb_compilation.ok_or_else(|| anyhow::anyhow!("VB compilation lease missing"))?;
                                let (s, e, provenance) = crate::vb_extractor::extract_vb_in_batch(lease, p, &text)?;
                                vb_provenance = Some(provenance);
                                (s, e)
                            } else {
                                crate::cs_extractor::extract_cs(p, &text)
                            };
                            let (layout_s, layout_e) =
                                crate::layout_extractor::extract_winforms_layout(
                                    arc_rel.as_str(),
                                    &text,
                                );
                            s.extend(layout_s);
                            e.extend(layout_e);
                            (s, e)
                        } else if is_web_config(p) {
                            crate::config_extractor::extract_web_config(&arc_rel, &text)
                        } else if ext_lower.as_deref() == Some("sql") {
                            crate::ddl_extractor::extract_ddl(&arc_rel, &text)
                        } else if ext_lower.as_deref() == Some("vb") {
                            let lease = vb_compilation.ok_or_else(|| anyhow::anyhow!("VB compilation lease missing"))?;
                            let (s, e, provenance) = crate::vb_extractor::extract_vb_in_batch(lease, p, &text)?;
                            vb_provenance = Some(provenance);
                            (s, e)
                        } else if matches!(ext_lower.as_deref(), Some("ml" | "mlinc")) {
                            // MiniLang. Takes BOTH paths: the absolute one to
                            // stat conformance-golden siblings, the
                            // project-relative one to build edge targets —
                            // absolute edge targets are rejected by the
                            // ingest safety check.
                            crate::ml_extractor::extract_ml(p, arc_rel.as_str(), &text)
                        } else if ext_lower.as_deref() == Some("cs") {
                            crate::cs_extractor::extract_cs(p, &text)
                        } else if ext_lower.as_deref() == Some("asp") {
                            crate::asp_classic_extractor::extract_classic_asp(&arc_rel, &text)
                        } else if matches!(ext_lower.as_deref(), Some("rdlc" | "rdl")) {
                            crate::report_extractor::extract_ssrs_report(&arc_rel, &text)
                        } else {
                            // PROJECT-RELATIVE path, not the absolute disk
                            // path: the extractor only needs ext/filename,
                            // and its triple-slash reference resolution
                            // builds file-node target paths from the
                            // source's directory — absolute sources made
                            // every target absolute, which the ingest
                            // safety check rejects (live: whole index job
                            // died on the first BOM-visible reference).
                            extractor.extract(std::path::Path::new(arc_rel.as_str()), &text)
                        };
                        if crate::webforms::is_webforms_markup(p) {
                            local_stats.webforms_position_contracts.insert(
                                arc_rel.as_str().to_owned(), crate::webforms::POSITION_CONTRACT);
                        }
                        if let Some(provenance) = vb_provenance {
                            if let crate::vb_extractor::VbParserProvenance::RustFallback { reason, .. } = &provenance {
                                local_stats.warnings.push(format!("{}: VB source used recorded Rust fallback contract {} ({reason:?}); Roslyn semantic binding was not used for this file", arc_rel.as_str(), crate::SOURCE_INDEX_VERSION));
                            }
                            local_stats.vb_parser_provenance.insert(arc_rel.as_str().to_owned(), provenance);
                        }
                        // Language-generic pass: "must update in N places"
                        // comments are machine-readable sync contracts —
                        // extracted here so the review's SyncContractGate can
                        // assert when a diff touches a subset of the listed
                        // sites. Vendor files excluded with the rest.
                        let syms = if is_vendor {
                            syms
                        } else {
                            let mut syms = syms;
                            syms.extend(crate::parsing::detect_sync_contracts(&text));
                            syms
                        };
                        for s in &syms {
                            local_stats.symbols.push((arc_rel.clone(), s.clone()));
                        }
                        for e in edges {
                            local_stats.edges.push((arc_rel.clone(), e));
                        }

                        // ASP.NET Optimization bundles cross code and markup
                        // artifacts. Definitions live in VB/C# while render
                        // calls live in WebForms or Razor-family templates.
                        if !is_vendor && matches!(ext_lower.as_deref(), Some("vb" | "cs")) {
                            for edge in crate::asset_bundles::extract_bundle_definitions(
                                &root_buf, &arc_rel, &text,
                            ) {
                                local_stats.edges.push((arc_rel.clone(), edge));
                            }
                        }

                        // Post-processing: extract JS→ASP.NET bridge edges.
                        // Use extension-based gating so `.jsx`/`.tsx` files are included.
                        if is_vendor {
                            // No bridge extraction for vendor files either.
                        } else if crate::js_extractor::is_js_file(p) {
                            let (js_syms, js_edges) = crate::js_extractor::extract_js(p, &text);
                            for s in &js_syms {
                                local_stats.symbols.push((arc_rel.clone(), s.clone()));
                            }
                            for e in js_edges {
                                local_stats.edges.push((arc_rel.clone(), e));
                            }
                        } else if crate::webforms::is_webforms_markup(p) {
                            let inline_js = extract_inline_scripts(&text);
                            if !inline_js.is_empty() {
                                let (js_syms, js_edges) =
                                    crate::js_extractor::extract_js(p, &inline_js);
                                for s in &js_syms {
                                    local_stats.symbols.push((arc_rel.clone(), s.clone()));
                                }
                                for e in js_edges {
                                    local_stats.edges.push((arc_rel.clone(), e));
                                }
                            }
                        }

                        // General templates can host the same static bundle as WebForms.
                        // Keep their source a file; do not run WebForms control extraction.
                        if !is_vendor
                            && matches!(
                                ext_lower.as_deref(),
                                Some("html" | "htm" | "cshtml" | "vbhtml" | "razor")
                            )
                        {
                            for edge in crate::webforms::extract_template_script_includes(
                                &root_buf, &arc_rel, &text,
                            ) {
                                local_stats.edges.push((arc_rel.clone(), edge));
                            }
                        }

                        if !is_vendor
                            && (crate::webforms::is_webforms_markup(p)
                                || matches!(
                                    ext_lower.as_deref(),
                                    Some("html" | "htm" | "cshtml" | "vbhtml" | "razor")
                                ))
                        {
                            let source_kind = if crate::webforms::is_webforms_markup(p) {
                                "page"
                            } else {
                                "file"
                            };
                            for edge in crate::asset_bundles::extract_bundle_renders(
                                &arc_rel,
                                &text,
                                source_kind,
                            ) {
                                local_stats.edges.push((arc_rel.clone(), edge));
                            }
                        }

                        // Post-processing: detect Crystal Reports usage in C#/VB/ASPX files.
                        if matches!(language, "csharp" | "vbnet") {
                            let (cr_syms, cr_edges) =
                                crate::report_extractor::extract_crystal_reports_usage(
                                    &arc_rel, &text, language,
                                );
                            for s in &cr_syms {
                                local_stats.symbols.push((arc_rel.clone(), s.clone()));
                            }
                            for e in cr_edges {
                                local_stats.edges.push((arc_rel.clone(), e));
                            }
                        }
                        if crate::webforms::is_webforms_markup(p) {
                            let (cr_syms, cr_edges) =
                                crate::report_extractor::extract_crystal_reports_in_markup(
                                    &arc_rel, &text,
                                );
                            for s in &cr_syms {
                                local_stats.symbols.push((arc_rel.clone(), s.clone()));
                            }
                            for e in cr_edges {
                                local_stats.edges.push((arc_rel.clone(), e));
                            }
                        }

                        // Post-processing: detect global state accesses in C#/VB files.
                        if matches!(language, "csharp" | "vbnet") {
                            let (state_syms, state_edges) =
                                crate::state_extractor::extract_state_accesses_with_members(
                                    &arc_rel, &text, language, &syms,
                                );
                            for s in &state_syms {
                                local_stats.symbols.push((arc_rel.clone(), s.clone()));
                            }

                            // State affinity analysis: co-accessed state keys → API endpoints
                            if !state_edges.is_empty() {
                                let (affinity_syms, affinity_edges) =
                                    crate::state_extractor::analyze_state_affinity(
                                        &state_edges,
                                        &arc_rel,
                                    );
                                for s in &affinity_syms {
                                    local_stats.symbols.push((arc_rel.clone(), s.clone()));
                                }
                                for e in affinity_edges {
                                    local_stats.edges.push((arc_rel.clone(), e));
                                }
                            }

                            for e in state_edges {
                                local_stats.edges.push((arc_rel.clone(), e));
                            }
                        }

                        let mut chunks =
                            chunking::semantic_chunk_lines(&text, max_chars_per_chunk, &syms);

                        for c in &mut chunks {
                            c.set_doc_id(arc_rel.as_str());
                        }

                        for c in chunks {
                            let chunk_id = chunk_id_from_content_hash(&c.content_hash);
                            local_docs.push(IndexDoc {
                                generation,
                                chunk_id,
                                path: (*arc_rel).clone(),
                                language: language.to_string(),
                                content: c.content,
                                namespace: namespace_str.clone(),
                                author: None,
                                timestamp: None,
                                start_line: c.start_line,
                                end_line: c.end_line,
                                doc_id: c.doc_id.0,
                                content_hash: c.content_hash.0,
                            });
                            local_stats.chunks += 1;
                        }

                        Ok((local_stats, local_docs))
                    }))
                    .try_reduce(
                        || (IngestStats::default(), Vec::new()),
                        |mut a, b| {
                            a.0.chunks += b.0.chunks;
                            a.0.bytes += b.0.bytes;
                            a.0.all_files.extend(b.0.all_files);
                            a.0.fingerprints.extend(b.0.fingerprints);
                            a.0.vb_parser_provenance.extend(b.0.vb_parser_provenance);
                            a.0.webforms_position_contracts.extend(b.0.webforms_position_contracts);
                            a.0.symbols.extend(b.0.symbols);
                            a.0.edges.extend(b.0.edges);
                            a.0.skipped_files.extend(b.0.skipped_files);
                            for (lang, count) in b.0.languages {
                                *a.0.languages.entry(lang).or_insert(0) += count;
                            }
                            a.0.warnings.extend(b.0.warnings);
                            a.1.extend(b.1);
                            Ok(a)
                        },
                    )?;
                Ok::<_, anyhow::Error>(parse_work::ParsedBatch { docs: chunk_docs, stats: chunk_stats, _work: work })
            })
            .await??;

            // Keep parsed._work in the earlier-declared envelope: destructuring
            // it into a later guard local would release it before the outputs.
            let chunk_stats = parsed.stats;
            let chunk_docs = parsed.docs;

            stats.chunks += chunk_stats.chunks;
            stats.bytes += chunk_stats.bytes;
            stats.all_files.extend(chunk_stats.all_files);
            stats.fingerprints.extend(chunk_stats.fingerprints);
            stats.vb_parser_provenance.extend(chunk_stats.vb_parser_provenance);
            stats.webforms_position_contracts.extend(chunk_stats.webforms_position_contracts);
            stats.symbols.extend(chunk_stats.symbols);
            stats.edges.extend(chunk_stats.edges);
            stats.skipped_files.extend(chunk_stats.skipped_files);
            for (lang, count) in chunk_stats.languages {
                *stats.languages.entry(lang).or_insert(0) += count;
            }
            stats.warnings.extend(chunk_stats.warnings);

            batch.extend(chunk_docs);
            if batch.len() >= 512 {
                Self::write_docs_to_writer(&fields, &mut guard, project_id, &batch)?;
                guard.maybe_commit(1000)?;
                scoped
                    .embed_and_upsert_vectors_with_writer(&mut guard, project_id, &batch, cancel)
                    .await?;
                batch.clear();
            }

            processed_count += file_chunk.len();
            progress_cb(processed_count, total_files);
        }

        if !batch.is_empty() && !cancel.is_cancelled() {
            Self::write_docs_to_writer(&fields, &mut guard, project_id, &batch)?;
            scoped
                .embed_and_upsert_vectors_with_writer(&mut guard, project_id, &batch, cancel)
                .await?;
        }

        // Single merge wait for the entire run.
        guard.finish()?;

        let mut extracted_edge_kind_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (_, edge) in &stats.edges {
            *extracted_edge_kind_counts
                .entry(edge.kind.clone())
                .or_insert(0) += 1;
        }
        if !extracted_edge_kind_counts.is_empty() {
            tracing::debug!(
                project_id = %project_id,
                edge_kind_counts = ?extracted_edge_kind_counts,
                "index_files: aggregated extracted edge kinds before ingest service"
            );
        }

        Ok(stats)
    }

    pub fn lexical_search(&self, q: &HybridQuery) -> anyhow::Result<Vec<HybridHit>> {
        self.lexical_search_matching_impl(q, None)
    }

    /// Apply a stored-document predicate before counting a result toward top_k.
    /// Pages share one searcher snapshot, so rejected high-ranked documents
    /// cannot hide qualifying results below an arbitrary candidate cap.
    pub fn lexical_search_matching(
        &self,
        q: &HybridQuery,
        accept: &mut dyn FnMut(&str, &str) -> bool,
    ) -> anyhow::Result<Vec<HybridHit>> {
        self.lexical_search_matching_impl(q, Some(accept))
    }

    fn lexical_search_matching_impl(
        &self,
        q: &HybridQuery,
        mut accept: Option<&mut dyn FnMut(&str, &str) -> bool>,
    ) -> anyhow::Result<Vec<HybridHit>> {
        if q.top_k == 0 {
            return Ok(Vec::new());
        }
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();

        // ENG-AUD-2026-EXH-0003: fail-closed on unknown fts_mode — callers must
        // validate before reaching here, but defend in depth at the index layer too.
        // FTS1: cap regex patterns to prevent catastrophic backtracking / ReDoS.
        const MAX_REGEX_PATTERN_LEN: usize = 500;

        let content_q: Box<dyn tantivy::query::Query> = match q.fts_mode.as_str() {
            "regex" => {
                if q.text.len() > MAX_REGEX_PATTERN_LEN {
                    anyhow::bail!(
                        "FTS1: regex pattern too long ({} bytes, max {})",
                        q.text.len(),
                        MAX_REGEX_PATTERN_LEN
                    );
                }
                // FTS1: cap top-level alternation count to bound DFA state explosion.
                // Each top-level `|` (at paren depth 0) creates an additional branch;
                // 50+ top-level branches → unbounded DFA state growth.
                // Alternations inside `(a|b)` groups are bounded sub-expressions and
                // are excluded from this count.
                const MAX_ALTERNATIONS: usize = 20;
                let alternation_count = count_unescaped_alternations(&q.text);
                if alternation_count > MAX_ALTERNATIONS {
                    anyhow::bail!(
                        "FTS1: regex pattern has {} top-level alternations (max {}); \
                         reduce the number of top-level '|' branches to prevent DFA state explosion",
                        alternation_count,
                        MAX_ALTERNATIONS
                    );
                }
                let mut parser =
                    QueryParser::for_index(&self.tantivy_index, vec![self.fields.content]);
                parser.set_conjunction_by_default();
                parser.parse_query(&q.text)?
            }
            "loose" => literal_word_or_substring_query(
                &self.tantivy_index,
                self.fields.content_words,
                self.fields.content,
                &q.text,
                false,
                SUBSTRING_MATCH_WEIGHT,
            )?,
            "strict" => literal_word_or_substring_query(
                &self.tantivy_index,
                self.fields.content_words,
                self.fields.content,
                &q.text,
                true,
                SUBSTRING_MATCH_WEIGHT,
            )?,
            unknown => {
                anyhow::bail!(
                    "ENG-AUD-2026-EXH-0003: unknown fts_mode '{}': must be strict, loose, or regex",
                    unknown
                );
            }
        };

        let pid_term = Term::from_field_text(self.fields.project_id, &q.project_id);
        let pid_q = TermQuery::new(pid_term, IndexRecordOption::Basic);

        let ns_term = Term::from_field_text(self.fields.namespace, &q.namespace);
        let ns_q = TermQuery::new(ns_term, IndexRecordOption::Basic);

        let mut must_clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
            (Occur::Must, content_q),
            (
                Occur::Must,
                Box::new(pid_q) as Box<dyn tantivy::query::Query>,
            ),
            (
                Occur::Must,
                Box::new(ns_q) as Box<dyn tantivy::query::Query>,
            ),
        ];

        let _snapshot = self.add_generation_filter(
            &q.project_id,
            &q.namespace,
            q.generation,
            &mut must_clauses,
        )?;

        if let Some(prefixes) = &q.include_path_prefixes {
            let mut prefix_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for p in prefixes {
                if let Ok(rq) = tantivy::query::RegexQuery::from_pattern(
                    &format!("{}.*", regex::escape(p)),
                    self.fields.path,
                ) {
                    prefix_queries.push((Occur::Should, Box::new(rq)));
                }
            }
            if !prefix_queries.is_empty() {
                must_clauses.push((Occur::Must, Box::new(BooleanQuery::new(prefix_queries))));
            }
        }

        if let Some(prefixes) = &q.exclude_path_prefixes {
            for p in prefixes {
                if let Ok(rq) = tantivy::query::RegexQuery::from_pattern(
                    &format!("{}.*", regex::escape(p)),
                    self.fields.path,
                ) {
                    must_clauses.push((Occur::MustNot, Box::new(rq)));
                }
            }
        }

        if let Some(suffixes) = &q.include_path_suffixes {
            let mut suffix_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for s in suffixes {
                for variant in [s.to_lowercase(), s.to_uppercase()] {
                    if let Ok(rq) = tantivy::query::RegexQuery::from_pattern(
                        &format!(".*{}", regex::escape(&variant)),
                        self.fields.path,
                    ) {
                        suffix_queries.push((Occur::Should, Box::new(rq)));
                    }
                }
            }
            if !suffix_queries.is_empty() {
                must_clauses.push((Occur::Must, Box::new(BooleanQuery::new(suffix_queries))));
            }
        }

        if let Some(langs) = &q.language_filters {
            let mut lang_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for l in langs {
                lang_queries.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.language, l),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            must_clauses.push((Occur::Must, Box::new(BooleanQuery::new(lang_queries))));
        }

        self.add_author_and_date_filters(q, &mut must_clauses);

        let query = BooleanQuery::new(must_clauses);

        let mut out = Vec::with_capacity(q.top_k.min(128));
        let page_size = if accept.is_some() {
            q.top_k.clamp(32, 128)
        } else {
            q.top_k
        };
        let mut offset = 0;
        'pages: loop {
            let top_docs: Vec<(Score, DocAddress)> =
                searcher.search(&query, &TopDocs::with_limit(page_size).and_offset(offset))?;
            let page_len = top_docs.len();
            for (score, addr) in top_docs {
                let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
                let pk = doc
                    .get_first(self.fields.pk)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let chunk_id = doc
                    .get_first(self.fields.chunk_id)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let path_str = doc
                    .get_first(self.fields.path)
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let path = RelPath::new(path_str);
                let doc_id_str = doc
                    .get_first(self.fields.doc_id)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = doc
                    .get_first(self.fields.content)
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if accept
                    .as_mut()
                    .is_some_and(|predicate| !predicate(&doc_id_str, content))
                {
                    continue;
                }
                let (snippet, snippet_truncated) =
                    match doc.get_first(self.fields.content).and_then(|v| v.as_str()) {
                        Some(s) => {
                            let (sn, truncated) = snippet_of(s, SNIPPET_MAX_CHARS);
                            (Some(sn), truncated)
                        }
                        None => (None, false),
                    };
                let start_line = doc
                    .get_first(self.fields.start_line)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let end_line = doc
                    .get_first(self.fields.end_line)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;

                let timestamp = doc
                    .get_first(self.fields.timestamp)
                    .and_then(|v| v.as_u64())
                    .filter(|&t| t > 0);
                out.push(HybridHit {
                    pk,
                    chunk_id,
                    path,
                    score,
                    centrality: 0.0,
                    snippet,
                    doc_id: doc_id_str,
                    start_line,
                    end_line,
                    timestamp,
                    snippet_truncated,
                });
                if out.len() >= q.top_k {
                    break 'pages;
                }
            }
            if page_len < page_size {
                break;
            }
            offset += page_len;
        }

        // Same vendor demotion as the hybrid path: minified bundles match
        // almost any identifier query on BM25 and shove app code off the page.
        for h in &mut out {
            if engram_core::is_vendor_path(h.path.as_str()) {
                h.score *= 0.2;
            }
        }

        // ENG-AUD-2026-S06-001: apply the same deterministic tie-break as the
        // hybrid search path so repeated lexical_search calls for the same query
        // always return identical ordering. Tantivy's TopDocs collector sorts by
        // score but does not guarantee stable ordering for equal-score documents.
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.as_str().cmp(b.path.as_str()))
                .then_with(|| a.doc_id.cmp(&b.doc_id))
                .then_with(|| a.chunk_id.cmp(&b.chunk_id))
        });

        Ok(out)
    }

    /// Variant of [`Self::lexical_search`] that also returns each
    /// chunk's full stored content and start-line. Used by
    /// [`crate::grep::grep`] to verify literal matches without a
    /// DocStore round-trip — the content is already in Tantivy's
    /// stored-fields block.
    ///
    /// Returns `Vec<(HybridHit, content, start_line)>` so the caller
    /// can feed each hit directly into the per-chunk scanner.
    /// Build candidates from raw tokenizer output, without query syntax.
    /// Case-insensitive searches admit each trigram's case variants; grep
    /// verifies the actual literal/regex in every candidate. With no token,
    /// no content prefilter is safe (project and other filters still apply).
    /// Author is exact; time is `[date_after, date_before)` — the same bounds
    /// the vector leg applies, so a cutoff at a commit's own timestamp
    /// excludes that commit on both legs instead of leaking it through BM25.
    fn add_author_and_date_filters(
        &self,
        q: &HybridQuery,
        clauses: &mut Vec<(Occur, Box<dyn tantivy::query::Query>)>,
    ) {
        use std::ops::Bound;
        if let Some(author) = &q.author_filter {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.author, author),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if q.date_after.is_some() || q.date_before.is_some() {
            let lower = q.date_after.map_or(Bound::Unbounded, |after| {
                Bound::Included(Term::from_field_u64(self.fields.timestamp, after))
            });
            let upper = q.date_before.map_or(Bound::Unbounded, |before| {
                Bound::Excluded(Term::from_field_u64(self.fields.timestamp, before))
            });
            clauses.push((
                Occur::Must,
                Box::new(tantivy::query::RangeQuery::new(lower, upper)),
            ));
        }
    }

    fn literal_trigram_query(
        &self,
        text: &str,
        case_sensitive: bool,
    ) -> anyhow::Result<Box<dyn tantivy::query::Query>> {
        let mut analyzer = self
            .tantivy_index
            .tokenizer_for_field(self.fields.content)?;
        let mut stream = analyzer.token_stream(text);
        let mut trigrams: Vec<String> = Vec::new();
        while stream.advance() {
            trigrams.push(stream.token().text.clone());
        }
        trigrams.sort();
        trigrams.dedup();
        if trigrams.is_empty() {
            return Ok(Box::new(tantivy::query::AllQuery));
        }
        let mut must: Vec<(Occur, Box<dyn tantivy::query::Query>)> =
            Vec::with_capacity(trigrams.len());
        for tri in &trigrams {
            if case_sensitive {
                must.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.content, tri),
                        IndexRecordOption::Basic,
                    )),
                ));
                continue;
            }
            let should: Vec<(Occur, Box<dyn tantivy::query::Query>)> = case_variants(tri)
                .into_iter()
                .map(|v| {
                    let tq: Box<dyn tantivy::query::Query> = Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.content, &v),
                        IndexRecordOption::Basic,
                    ));
                    (Occur::Should, tq)
                })
                .collect();
            must.push((Occur::Must, Box::new(BooleanQuery::new(should))));
        }
        Ok(Box::new(BooleanQuery::new(must)))
    }

    pub fn lexical_search_with_content(
        &self,
        q: &HybridQuery,
    ) -> anyhow::Result<Vec<(HybridHit, String, u32)>> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();

        // Reuse the same query-building logic as `lexical_search` so
        // the two variants agree on semantics. We inline the core
        // instead of refactoring `lexical_search` because that path
        // is exercised by many existing tests and this method is
        // additive.
        const MAX_REGEX_PATTERN_LEN: usize = 500;
        let content_q: Box<dyn tantivy::query::Query> = match q.fts_mode.as_str() {
            "regex" => {
                if q.text.len() > MAX_REGEX_PATTERN_LEN {
                    anyhow::bail!(
                        "FTS1: regex pattern too long ({} bytes, max {})",
                        q.text.len(),
                        MAX_REGEX_PATTERN_LEN
                    );
                }
                let mut parser =
                    QueryParser::for_index(&self.tantivy_index, vec![self.fields.content]);
                parser.set_conjunction_by_default();
                parser.parse_query(&q.text)?
            }
            "loose" => literal_word_or_substring_query(
                &self.tantivy_index,
                self.fields.content_words,
                self.fields.content,
                &q.text,
                false,
                SUBSTRING_MATCH_WEIGHT,
            )?,
            "strict" => literal_word_or_substring_query(
                &self.tantivy_index,
                self.fields.content_words,
                self.fields.content,
                &q.text,
                true,
                SUBSTRING_MATCH_WEIGHT,
            )?,
            // Case-insensitive literal over the case-PRESERVING trigram
            // index: the content field is tokenised by
            // `NgramTokenizer::new(3, 3, false)` with no lowercasing, so a
            // lower-case pattern's exact trigrams never occur in a chunk
            // whose only occurrence is `PERSONALLIGGARE` or
            // `BokningsObjekt…` (live miss, pilot corpus 2026-08-28). Every
            // trigram becomes a Should-set of its case variants, Must
            // across trigrams; the caller verifies each candidate chunk,
            // so a superset of candidates is correct and complete.
            "literal_ci" => self.literal_trigram_query(&q.text, false)?,
            "literal_cs" => self.literal_trigram_query(&q.text, true)?,
            unknown => {
                anyhow::bail!(
                    "unknown fts_mode '{unknown}': must be strict, loose, regex, literal_ci, or literal_cs"
                )
            }
        };

        let pid_q = TermQuery::new(
            Term::from_field_text(self.fields.project_id, &q.project_id),
            IndexRecordOption::Basic,
        );
        let ns_q = TermQuery::new(
            Term::from_field_text(self.fields.namespace, &q.namespace),
            IndexRecordOption::Basic,
        );
        let mut must: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
            (Occur::Must, content_q),
            (Occur::Must, Box::new(pid_q)),
            (Occur::Must, Box::new(ns_q)),
        ];
        let _snapshot =
            self.add_generation_filter(&q.project_id, &q.namespace, q.generation, &mut must)?;

        if let Some(langs) = &q.language_filters {
            let mut lq: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for l in langs {
                lq.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.language, l),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            must.push((Occur::Must, Box::new(BooleanQuery::new(lq))));
        }

        // The "inline the core" reimplementation above drifted from
        // `lexical_search`: it never applied include_path_prefixes, so
        // grep's path_prefix silently didn't constrain (live repro
        // 2026-07-06: 75/75 results out-of-prefix) AND, once the caller
        // post-filtered, in-prefix recall collapsed for common patterns
        // because top_k was spent on out-of-prefix chunks. Same clause
        // shape as lexical_search.
        if let Some(prefixes) = &q.include_path_prefixes {
            let mut prefix_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for p in prefixes {
                if let Ok(rq) = tantivy::query::RegexQuery::from_pattern(
                    &format!("{}.*", regex::escape(p)),
                    self.fields.path,
                ) {
                    prefix_queries.push((Occur::Should, Box::new(rq)));
                }
            }
            if !prefix_queries.is_empty() {
                must.push((Occur::Must, Box::new(BooleanQuery::new(prefix_queries))));
            }
        }

        if let Some(suffixes) = &q.include_path_suffixes {
            let mut suffix_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
            for s in suffixes {
                for variant in [s.to_lowercase(), s.to_uppercase()] {
                    if let Ok(rq) = tantivy::query::RegexQuery::from_pattern(
                        &format!(".*{}", regex::escape(&variant)),
                        self.fields.path,
                    ) {
                        suffix_queries.push((Occur::Should, Box::new(rq)));
                    }
                }
            }
            if !suffix_queries.is_empty() {
                must.push((Occur::Must, Box::new(BooleanQuery::new(suffix_queries))));
            }
        }

        self.add_author_and_date_filters(q, &mut must);

        let query = BooleanQuery::new(must);
        let top_docs: Vec<(Score, DocAddress)> =
            searcher.search(&query, &TopDocs::with_limit(q.top_k))?;

        let mut out: Vec<(HybridHit, String, u32)> = Vec::with_capacity(top_docs.len());
        for (score, addr) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
            let pk = doc
                .get_first(self.fields.pk)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let chunk_id = doc
                .get_first(self.fields.chunk_id)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let path_str = doc
                .get_first(self.fields.path)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let doc_id_str = doc
                .get_first(self.fields.doc_id)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Full content — NOT truncated. This is the point of the
            // new method: grep needs the whole chunk to locate the
            // match and any requested context lines.
            let content: String = doc
                .get_first(self.fields.content)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let start_line = doc
                .get_first(self.fields.start_line)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let end_line = doc
                .get_first(self.fields.end_line)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            let timestamp = doc
                .get_first(self.fields.timestamp)
                .and_then(|v| v.as_u64())
                .filter(|&t| t > 0);
            out.push((
                HybridHit {
                    pk,
                    chunk_id,
                    path: RelPath::new(path_str),
                    score,
                    centrality: 0.0,
                    snippet: None,
                    doc_id: doc_id_str,
                    start_line,
                    end_line,
                    snippet_truncated: false,
                    timestamp,
                },
                content,
                start_line,
            ));
        }
        // Same deterministic sort as `lexical_search`.
        out.sort_by(|a, b| {
            b.0.score
                .partial_cmp(&a.0.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.path.as_str().cmp(b.0.path.as_str()))
                .then_with(|| a.0.doc_id.cmp(&b.0.doc_id))
                .then_with(|| a.0.chunk_id.cmp(&b.0.chunk_id))
        });
        Ok(out)
    }

    /// Retrieve a doc by its doc_id string (instance identity).
    #[allow(clippy::type_complexity)]
    pub fn get_doc_by_doc_id(
        &self,
        project_id: &str,
        namespace: &str,
        generation: u64,
        doc_id_str: &str,
    ) -> anyhow::Result<Option<(RelPath, String, String, u32, u32)>> {
        let snapshot = if engram_core::get_policy(namespace)
            .is_ok_and(|policy| policy.versioning == engram_core::NamespaceVersioning::Snapshot)
        {
            Some(self.snapshots.pin(project_id, namespace, generation)?)
        } else {
            None
        };
        if let Some(view) = snapshot
            .as_ref()
            .filter(|view| matches!(view.selection, snapshots::Selection::Files(_)))
        {
            let query = BooleanQuery::new(vec![
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.project_id, project_id),
                        IndexRecordOption::Basic,
                    )) as Box<dyn tantivy::query::Query>,
                ),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.namespace, namespace),
                        IndexRecordOption::Basic,
                    )),
                ),
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.doc_id, doc_id_str),
                        IndexRecordOption::Basic,
                    )),
                ),
                (Occur::Must, view.query(&self.fields)),
            ]);
            let searcher = self.tantivy_index.reader()?.searcher();
            return searcher
                .search(&query, &TopDocs::with_limit(1))?
                .first()
                .map(|(_, address)| {
                    let doc: tantivy::TantivyDocument = searcher.doc(*address)?;
                    Ok(self.document_parts(&doc))
                })
                .transpose();
        }
        let effective_gen = if let Ok(policy) = engram_core::get_policy(namespace) {
            if policy.versioning == engram_core::NamespaceVersioning::GlobalMutable {
                0
            } else {
                generation
            }
        } else {
            generation
        };
        let pk = build_pk(project_id, namespace, effective_gen, doc_id_str);
        self.get_doc_by_pk(&pk)
    }

    /// Recover a selected source hit under the request's existing source lease.
    /// The physical identity is checked against that exact view; this never
    /// re-pins a newer head or substitutes another revision of the logical ID.
    #[allow(clippy::type_complexity)]
    pub fn recover_selected_source_hit(
        &self,
        project_id: &str,
        view: &SourceSnapshotGuard,
        hit: &HybridHit,
    ) -> anyhow::Result<Option<(RelPath, String, String, u32, u32)>> {
        anyhow::ensure!(Arc::ptr_eq(&view._catalog, &self.snapshots) && view._project == project_id,
            "Selected source recovery requires the same project and index root lease");
        let mut parts = hit.pk.splitn(4, ':');
        anyhow::ensure!(parts.next() == Some(project_id) && parts.next() == Some("memory"),
            "Selected source physical key belongs to another project or namespace");
        let physical_generation = parts.next().ok_or_else(|| anyhow::anyhow!("Missing physical generation"))?.parse::<u64>()?;
        anyhow::ensure!(parts.next() == Some(hit.doc_id.as_str())
            && build_pk(project_id, "memory", physical_generation, &hit.doc_id) == hit.pk,
            "Selected source physical key is not canonical for this document");
        anyhow::ensure!(view._snapshot.contains(hit.path.as_str(), physical_generation),
            "Selected source revision is outside the held request view");
        let searcher = self.tantivy_index.reader()?.searcher();
        let query = TermQuery::new(Term::from_field_text(self.fields.pk, &hit.pk), IndexRecordOption::Basic);
        let matches = searcher.search(&query, &TopDocs::with_limit(2))?;
        anyhow::ensure!(matches.len() <= 1, "Ambiguous selected physical source identity");
        let Some((_, address)) = matches.first() else { return Ok(None); };
        let doc: tantivy::TantivyDocument = searcher.doc(*address)?;
        let string = |field| doc.get_first(field).and_then(|v| v.as_str());
        anyhow::ensure!(string(self.fields.pk) == Some(hit.pk.as_str())
            && string(self.fields.project_id) == Some(project_id)
            && string(self.fields.namespace) == Some("memory")
            && string(self.fields.doc_id) == Some(hit.doc_id.as_str())
            && string(self.fields.path) == Some(hit.path.as_str())
            && doc.get_first(self.fields.chunk_id).and_then(|v| v.as_u64()) == Some(hit.chunk_id)
            && doc.get_first(self.fields.generation).and_then(|v| v.as_u64()) == Some(physical_generation),
            "Stored source identity differs from the selected physical hit");
        let recovered = self.document_parts(&doc);
        anyhow::ensure!((hit.start_line == 0 || recovered.3 == hit.start_line)
            && (hit.end_line == 0 || recovered.4 == hit.end_line),
            "Stored source line bounds differ from the selected hit");
        Ok(Some(recovered))
    }

    /// Retrieve a doc by its primary key.
    #[allow(clippy::type_complexity)]
    pub fn get_doc_by_pk(
        &self,
        pk: &str,
    ) -> anyhow::Result<Option<(RelPath, String, String, u32, u32)>> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();

        let pk_term = Term::from_field_text(self.fields.pk, pk);
        let pk_q = TermQuery::new(pk_term, IndexRecordOption::Basic);

        let top_docs: Vec<(Score, DocAddress)> = searcher.search(&pk_q, &TopDocs::with_limit(1))?;
        if top_docs.is_empty() {
            return Ok(None);
        }
        let (_, addr) = top_docs[0];
        let doc: tantivy::TantivyDocument = searcher.doc(addr)?;

        Ok(Some(self.document_parts(&doc)))
    }

    fn document_parts(
        &self,
        doc: &tantivy::TantivyDocument,
    ) -> (RelPath, String, String, u32, u32) {
        let path_str = doc
            .get_first(self.fields.path)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = RelPath::new(path_str);
        let language = doc
            .get_first(self.fields.language)
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .to_string();
        let content = doc
            .get_first(self.fields.content)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let start_line = doc
            .get_first(self.fields.start_line)
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let end_line = doc
            .get_first(self.fields.end_line)
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        (path, language, content, start_line, end_line)
    }

    /// Returns an opaque provider identity and actual dimension for document
    /// caches. Query preprocessing is separate; disabled vectors have no identity.
    pub fn document_embedding_identity(&self) -> Option<(String, usize)> {
        #[cfg(feature = "vector")]
        {
            if self.semantic_quality() == SemanticQuality::Off { return None; }
            let tag = self.embedder.model_tag();
            let dimension = self.embedder.dimension();
            let mut hash = blake3::Hasher::new();
            hash.update(b"engram-document-embedding-v1");
            hash.update(&(tag.len() as u64).to_le_bytes());
            hash.update(tag.as_bytes());
            hash.update(&(dimension as u64).to_le_bytes());
            Some((hash.finalize().to_hex().to_string(), dimension))
        }
        #[cfg(not(feature = "vector"))]
        { None }
    }

    /// Cumulative observations for this engine's durable document cache, if
    /// configured. Reading counters does not open the cache or call a provider.
    pub fn embedding_cache_stats(&self) -> Option<crate::embed_cache::EmbeddingCacheStats> {
        #[cfg(feature = "vector")]
        { self.embedding_cache.as_ref().map(|cache| cache.stats()) }
        #[cfg(not(feature = "vector"))]
        { None }
    }

    /// Embed texts with the project's configured document provider, in order.
    pub async fn embed_texts(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        #[cfg(feature = "vector")]
        {
            let mut out = Vec::with_capacity(texts.len());
            for batch in texts.chunks(32) {
                let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
                out.extend(self.embedder.embed_batch(&refs).await?);
            }
            Ok(out)
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = texts;
            anyhow::bail!("embeddings are unavailable: built without the vector feature")
        }
    }

    /// The stored document behind a hit's primary key.
    pub fn stored_doc_by_pk(&self, pk: &str) -> anyhow::Result<Option<StoredDoc>> {
        let query = TermQuery::new(
            Term::from_field_text(self.fields.pk, pk),
            IndexRecordOption::Basic,
        );
        self.first_stored_doc(&query)
    }

    /// The newest stored document at `path` in a namespace: how a history
    /// result names a commit's author and message when only its diffs matched.
    pub fn stored_doc_at_path(
        &self,
        project_id: &str,
        namespace: &str,
        path: &str,
    ) -> anyhow::Result<Option<StoredDoc>> {
        let term = |field, text: &str| -> Box<dyn tantivy::query::Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, text),
                IndexRecordOption::Basic,
            ))
        };
        let query = BooleanQuery::new(vec![
            (Occur::Must, term(self.fields.path, path)),
            (Occur::Must, term(self.fields.project_id, project_id)),
            (Occur::Must, term(self.fields.namespace, namespace)),
        ]);
        self.first_stored_doc(&query)
    }

    fn first_stored_doc(
        &self,
        query: &dyn tantivy::query::Query,
    ) -> anyhow::Result<Option<StoredDoc>> {
        let searcher = self.tantivy_index.reader()?.searcher();
        let Some((_, addr)) = searcher
            .search(query, &TopDocs::with_limit(1))?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
        let text = |field| {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let author = text(self.fields.author);
        Ok(Some(StoredDoc {
            path: text(self.fields.path),
            content: text(self.fields.content),
            author: (!author.is_empty()).then_some(author),
            timestamp: doc
                .get_first(self.fields.timestamp)
                .and_then(|v| v.as_u64())
                .filter(|ts| *ts > 0),
        }))
    }

    /// Fill line range + snippet for hits that came from the vector store,
    /// which carries no line columns. Looks each unenriched hit up in Tantivy
    /// by pk (bounded: only runs over a final, truncated result list).
    fn enrich_hits_from_store(&self, hits: &mut [HybridHit]) {
        for hit in hits.iter_mut() {
            if hit.start_line != 0 || hit.end_line != 0 {
                continue;
            }
            if hit.pk.is_empty() {
                continue;
            }
            if let Ok(Some((_, _, content, start_line, end_line))) = self.get_doc_by_pk(&hit.pk) {
                hit.start_line = start_line;
                hit.end_line = end_line;
                if hit.snippet.is_none() {
                    let (sn, truncated) = snippet_of(&content, SNIPPET_MAX_CHARS);
                    hit.snippet = Some(sn);
                    hit.snippet_truncated = truncated;
                }
            }
            // Vector rows carry no timestamp column; fill it from Tantivy so
            // the recency term works on the semantic path too. Bounded: only
            // the final, truncated result list reaches here.
            if hit.timestamp.is_none() {
                hit.timestamp = self.timestamp_by_pk(&hit.pk);
            }
        }
    }

    /// The indexed timestamp (unix ms) of one doc, by pk. `None` when the doc
    /// has no timestamp or is not found.
    fn timestamp_by_pk(&self, pk: &str) -> Option<u64> {
        let reader = self.tantivy_index.reader().ok()?;
        let searcher = reader.searcher();
        let pk_q = TermQuery::new(
            Term::from_field_text(self.fields.pk, pk),
            IndexRecordOption::Basic,
        );
        let top: Vec<(Score, DocAddress)> = searcher.search(&pk_q, &TopDocs::with_limit(1)).ok()?;
        let (_, addr) = *top.first()?;
        let doc: tantivy::TantivyDocument = searcher.doc(addr).ok()?;
        doc.get_first(self.fields.timestamp)
            .and_then(|v| v.as_u64())
            .filter(|&t| t > 0)
    }

    fn exact_generation_selection(
        &self,
        project_id: &str,
        namespace: Option<&str>,
        generation: u64,
    ) -> anyhow::Result<(Box<dyn tantivy::query::Query>, Option<snapshots::Lease>)> {
        let exact = || {
            Box::new(TermQuery::new(
                Term::from_field_u64(self.fields.generation, generation),
                IndexRecordOption::Basic,
            )) as Box<dyn tantivy::query::Query>
        };
        if namespace.is_some_and(|namespace| namespace != "memory")
            || (namespace.is_none() && generation == 0)
        {
            return Ok((exact(), None));
        }
        let snapshot = self.snapshots.pin(project_id, "memory", generation)?;
        let selected = snapshot.query(&self.fields);
        let query: Box<dyn tantivy::query::Query> = if namespace.is_some() {
            selected
        } else {
            let memory = || {
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.namespace, "memory"),
                    IndexRecordOption::Basic,
                )) as Box<dyn tantivy::query::Query>
            };
            Box::new(BooleanQuery::new(vec![
                (
                    Occur::Should,
                    Box::new(BooleanQuery::new(vec![
                        (Occur::Must, memory()),
                        (Occur::Must, selected),
                    ])),
                ),
                (
                    Occur::Should,
                    Box::new(BooleanQuery::new(vec![
                        (Occur::Must, exact()),
                        (Occur::MustNot, memory()),
                    ])),
                ),
            ]))
        };
        Ok((query, Some(snapshot)))
    }

    #[cfg(feature = "vector")]
    fn exact_generation_sql(
        &self,
        project_id: &str,
        namespace: Option<&str>,
        generation: u64,
    ) -> anyhow::Result<(String, Option<snapshots::Lease>)> {
        if namespace.is_some_and(|namespace| namespace != "memory")
            || (namespace.is_none() && generation == 0)
        {
            return Ok((format!("generation = {}", crate::sql_literals::u64_literal(generation)), None));
        }
        let snapshot = self.snapshots.pin(project_id, "memory", generation)?;
        let clause = if namespace.is_some() {
            snapshot.sql().to_owned()
        } else {
            format!(
                "((namespace = 'memory' AND ({})) OR (namespace <> 'memory' AND generation = {}))",
                snapshot.sql(), crate::sql_literals::u64_literal(generation)
            )
        };
        Ok((clause, Some(snapshot)))
    }

    fn add_generation_filter(
        &self,
        project_id: &str,
        namespace: &str,
        generation: u64,
        clauses: &mut Vec<(Occur, Box<dyn tantivy::query::Query>)>,
    ) -> anyhow::Result<Option<snapshots::Lease>> {
        if let Ok(policy) = engram_core::get_policy(namespace) {
            match policy.versioning {
                engram_core::NamespaceVersioning::Snapshot => {
                    let snapshot = self.snapshots.pin(project_id, namespace, generation)?;
                    clauses.push((Occur::Must, snapshot.query(&self.fields)));
                    return Ok(Some(snapshot));
                }
                engram_core::NamespaceVersioning::AppendOnly => {
                    let parser =
                        QueryParser::for_index(&self.tantivy_index, vec![self.fields.generation]);
                    if let Ok(query) =
                        parser.parse_query(&format!("generation:[* TO {}]", generation))
                    {
                        clauses.push((Occur::Must, query));
                    }
                }
                engram_core::NamespaceVersioning::GlobalMutable => {}
            }
        }
        Ok(None)
    }

    pub async fn vector_search(
        &self,
        q: &HybridQuery,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<HybridHit>> {
        if self.is_lexical_only() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "vector")]
        {
            let _source = if engram_core::get_policy(&q.namespace)
                .is_ok_and(|p| p.versioning == engram_core::NamespaceVersioning::Snapshot)
            {
                Some(
                    self.snapshots
                        .pin(&q.project_id, &q.namespace, q.generation)?,
                )
            } else {
                None
            };
            let epoch = self.pin_vector_epoch(&q.project_id)?;
            self.vector_search_with_epoch(q, cancel, epoch).await
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = (q, cancel);
            Ok(Vec::new())
        }
    }

    #[cfg(feature = "vector")]
    async fn vector_search_with_epoch(
        &self,
        q: &HybridQuery,
        cancel: &CancellationToken,
        _vector_epoch: Arc<vector_epochs::Epoch>,
    ) -> anyhow::Result<Vec<HybridHit>> {
        if self.is_lexical_only() { return Ok(Vec::new()); }
        anyhow::ensure!(q.top_k <= 10_000, "Native query output exceeds 10000 rows");
        self.ensure_managed_vector_table_present(&_vector_epoch)?;
        anyhow::ensure!(_vector_epoch.record.is_some() || self.count_docs(&q.project_id)? == 0,
            "VEC1: stored documents have no identified vector store; initialize vectors from stored text before searching");
        let snapshot = if engram_core::get_policy(&q.namespace)
            .is_ok_and(|policy| policy.versioning == engram_core::NamespaceVersioning::Snapshot)
        {
            Some(self.snapshots.pin(&q.project_id, &q.namespace, q.generation)?)
        } else { None };
        // Provider failures occur before any Lance operation is started. They
        // do not leave native reader quiescence unproven or disable pruning.
        let query_vec = _vector_epoch.query_embedder.as_ref()
            .ok_or_else(|| anyhow::anyhow!("VEC1: vector embedding identity is unavailable; explicit recovery is required"))?
            .embed_cancellable(&q.text, cancel).await?;
        let worker = self.clone();
        let query = q.clone();
        self.run_native_vector_query(_vector_epoch.clone(), cancel, async move {
            worker.vector_search_backend(&query, query_vec, _vector_epoch, snapshot).await
        }).await
    }

    #[cfg(feature = "vector")]
    async fn vector_search_backend(
        &self,
        q: &HybridQuery,
        query_vec: Vec<f32>,
        _vector_epoch: Arc<vector_epochs::Epoch>,
        _snapshot: Option<snapshots::Lease>,
    ) -> anyhow::Result<Vec<HybridHit>> {
        if self.is_lexical_only() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "vector")]
        {
            self.ensure_managed_vector_table_present(&_vector_epoch)?;
            anyhow::ensure!(
                _vector_epoch.record.is_some() || self.count_docs(&q.project_id)? == 0,
                "VEC1: stored documents have no identified vector store; initialize vectors from stored text before searching"
            );
            let table_name = _vector_epoch.table.clone();
            if !self
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                return Ok(Vec::new());
            }
            let table = self.open_vector_epoch_table(&_vector_epoch).await?;

            // Build the WHERE clause into a single pre-allocated String instead
            // of N separate format!() + Vec<String> + join(). Saves ~10 intermediate
            // String allocations per search query.
            let where_clause = {
                use std::fmt::Write;
                let mut wc = String::with_capacity(256);
                let safe_ns = q.namespace.replace('\'', "''");
                let _ = write!(wc, "namespace = '{}'", safe_ns);

                if let Ok(policy) = engram_core::get_policy(&q.namespace) {
                    match policy.versioning {
                        engram_core::NamespaceVersioning::Snapshot => {
                            if let Some(snapshot) = &_snapshot {
                                let _ = write!(wc, " AND ({})", snapshot.sql());
                            }
                        }
                        engram_core::NamespaceVersioning::AppendOnly => {
                            let _ = write!(wc, " AND generation <= {}", crate::sql_literals::u64_literal(q.generation));
                        }
                        engram_core::NamespaceVersioning::GlobalMutable => {}
                    }
                }

                if let Some(prefixes) = &q.include_path_prefixes
                    && !prefixes.is_empty()
                {
                    wc.push_str(" AND (");
                    for (i, p) in prefixes.iter().enumerate() {
                        if i > 0 {
                            wc.push_str(" OR ");
                        }
                        let safe_p = escape_like(p);
                        let _ = write!(wc, "path LIKE '{}%' ESCAPE '\\'", safe_p);
                    }
                    wc.push(')');
                }

                if let Some(prefixes) = &q.exclude_path_prefixes {
                    for p in prefixes {
                        let safe_p = escape_like(p);
                        let _ = write!(wc, " AND path NOT LIKE '{}%' ESCAPE '\\'", safe_p);
                    }
                }

                if let Some(suffixes) = &q.include_path_suffixes
                    && !suffixes.is_empty()
                {
                    wc.push_str(" AND (");
                    for (i, s) in suffixes.iter().enumerate() {
                        if i > 0 {
                            wc.push_str(" OR ");
                        }
                        let lo = escape_like(&s.to_lowercase());
                        let up = escape_like(&s.to_uppercase());
                        let _ = write!(
                            wc,
                            "path LIKE '%{lo}' ESCAPE '\\' OR path LIKE '%{up}' ESCAPE '\\'"
                        );
                    }
                    wc.push(')');
                }

                if let Some(langs) = &q.language_filters
                    && !langs.is_empty()
                {
                    wc.push_str(" AND language IN (");
                    for (i, l) in langs.iter().enumerate() {
                        if i > 0 {
                            wc.push_str(", ");
                        }
                        let safe_l = l.replace('\'', "''");
                        let _ = write!(wc, "'{}'", safe_l);
                    }
                    wc.push(')');
                }

                if let Some(author) = &q.author_filter {
                    let safe_author = author.replace('\'', "''");
                    let _ = write!(wc, " AND author = '{}'", safe_author);
                }

                if let Some(after) = q.date_after {
                    let _ = write!(wc, " AND timestamp >= {}", crate::sql_literals::u64_literal(after));
                }
                if let Some(before) = q.date_before {
                    let _ = write!(wc, " AND timestamp < {}", crate::sql_literals::u64_literal(before));
                }
                wc
            };

            // Ask for COSINE explicitly. LanceDB defaults to DistanceType::L2,
            // whose `_distance` is the SQUARED euclidean distance; for the unit
            // vectors this crate produces that is `2 - 2*cos`, so the
            // `1.0 - _distance` below reported `2*cos - 1` and went negative for
            // every hit below cosine 0.5 — i.e. most genuinely relevant ones.
            // Ranking was unaffected (the transform is monotonic) but the number
            // vector_search exists to report was wrong. Safe to set per query:
            // no ANN index is built for these tables, so there is no index metric
            // to match.
            let native_query = table
                .query()
                .nearest_to(query_vec)?
                .distance_type(lancedb::DistanceType::Cosine)
                .only_if(where_clause)
                .limit(q.top_k);

            use futures::{StreamExt, TryStreamExt};
            let request = native_query.current_request();
            let mut results: futures::stream::BoxStream<'static, anyhow::Result<arrow_array::RecordBatch>> =
                match (table.dataset(), request.base.filter.as_ref()) {
                    (Some(native), Some(lancedb::query::QueryFilter::Sql(filter)))
                        if request.query_vector.len() == 1
                            && matches!(&request.base.select, lancedb::query::Select::All)
                            && request.base.full_text_search.is_none()
                            && request.base.reranker.is_none()
                            && request.base.norm.is_none() =>
                    {
                        // Clone the same immutable Dataset version as SDK create_plan;
                        // do not retain its consistency-wrapper lock across streaming.
                        let dataset = native.get().await?.clone();
                        let mut scanner = dataset.scan();
                        scanner.nearest(request.column.as_deref().unwrap_or("vector"),
                            request.query_vector[0].as_ref(),
                            request.base.limit.unwrap_or(q.top_k) + request.base.offset.unwrap_or(0))?;
                        // Scanner defaults differ from LanceDB's indexed-query defaults.
                        scanner.minimum_nprobes(request.minimum_nprobes);
                        if let Some(maximum_nprobes) = request.maximum_nprobes {
                            scanner.maximum_nprobes(maximum_nprobes);
                        }
                        scanner.use_index(request.use_index);
                        scanner.prefilter(request.base.prefilter);
                        scanner.distance_range(request.lower_bound, request.upper_bound);
                        if let Some(ef) = request.ef { scanner.ef(ef); }
                        if let Some(refine) = request.refine_factor { scanner.refine(refine); }
                        if let Some(metric) = request.distance_type { scanner.distance_metric(metric.into()); }
                        if request.base.fast_search { scanner.fast_search(); }
                        if request.base.with_row_id { scanner.with_row_id(); }
                        if request.base.disable_scoring_autoprojection { scanner.disable_scoring_autoprojection(); }
                        scanner.limit(request.base.limit.map(|n| n as i64), request.base.offset.map(|n| n as i64))?;
                        scanner.filter(filter)?;
                        scanner.batch_size(lancedb::query::QueryExecutionOptions::default().max_batch_length as usize);
                        // Bound fragment concurrency only. Keep the SDK's I/O byte
                        // budget: lowering it below an encoded batch can deadlock.
                        scanner.fragment_readahead(1);
                        scanner.try_into_stream().await?.map_err(anyhow::Error::from).boxed()
                    }
                    _ => native_query.execute().await?.map_err(anyhow::Error::from).boxed(),
                };

            let mut hits = Vec::new();
            while let Some(batch) = TryStreamExt::try_next(&mut results).await? {
                let batch: arrow_array::RecordBatch = batch;
                let pk_arr = batch
                    .column_by_name("pk")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::StringArray>())
                    .ok_or_else(|| anyhow::anyhow!("missing pk"))?;
                let chunk_id_arr = batch
                    .column_by_name("chunk_id")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt64Array>())
                    .ok_or_else(|| anyhow::anyhow!("missing chunk_id"))?;
                let path_arr = batch
                    .column_by_name("path")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::StringArray>())
                    .ok_or_else(|| anyhow::anyhow!("missing path"))?;
                let score_arr = batch
                    .column_by_name("_distance")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::Float32Array>())
                    .ok_or_else(|| anyhow::anyhow!("missing _distance"))?;
                let doc_id_arr = batch
                    .column_by_name("doc_id")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::StringArray>());

                for i in 0..batch.num_rows() {
                    let doc_id_val = doc_id_arr
                        .map(|a| a.value(i).to_string())
                        .unwrap_or_default();
                    hits.push(HybridHit {
                        pk: pk_arr.value(i).to_string(),
                        chunk_id: chunk_id_arr.value(i),
                        path: RelPath::new(path_arr.value(i)),
                        // Cosine distance is `1 - cos`, so this is the cosine
                        // similarity itself.
                        score: 1.0 - score_arr.value(i),
                        centrality: 0.0,
                        snippet: None,
                        doc_id: doc_id_val,
                        // LanceDB rows carry no line columns; enriched from
                        // Tantivy via enrich_hits_from_store on final results.
                        start_line: 0,
                        end_line: 0,
                        snippet_truncated: false,
                        // Enriched from Tantivy (with the timestamp) in
                        // enrich_hits_from_store — lance rows carry no line cols.
                        timestamp: None,
                    });
                }
            }

            Ok(hits)
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = q;
            Ok(Vec::new())
        }
    }

    /// Pure vector search with oversampling and optional MMR reranking.
    ///
    /// Unlike `vector_search()` (which is called as one half of hybrid search),
    /// this method oversamples by 3x the requested `top_k` to compensate for
    /// post-filter loss, wraps execution in a configurable timeout, and
    /// optionally applies MMR diversity reranking.
    pub async fn pure_vector_search(
        &self,
        q: &HybridQuery,
        timeout_ms: u64,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<HybridHit>> {
        if self.is_lexical_only() {
            return Ok(Vec::new());
        }
        let _snapshot = if engram_core::get_policy(&q.namespace)
            .is_ok_and(|policy| policy.versioning == engram_core::NamespaceVersioning::Snapshot)
        {
            Some(
                self.snapshots
                    .pin(&q.project_id, &q.namespace, q.generation)?,
            )
        } else {
            None
        };
        #[cfg(feature = "vector")]
        let vector_epoch = self.pin_vector_epoch(&q.project_id)?;
        let oversample_factor = if q.use_mmr {
            self.mmr_oversampling.max(3)
        } else {
            3
        };
        let mut q_oversampled = q.clone();
        // FTS2: cap the oversampled fetch size so large top_k + multiplier combos
        // cannot allocate unbounded intermediate buffers or cause OOM under the engine.
        q_oversampled.top_k = (q.top_k.saturating_mul(oversample_factor)).min(10_000);

        let timeout_dur = std::time::Duration::from_millis(timeout_ms);
        #[cfg(feature = "vector")]
        let search = self.vector_search_with_epoch(&q_oversampled, cancel, vector_epoch.clone());
        #[cfg(not(feature = "vector"))]
        let search = self.vector_search(&q_oversampled, cancel);
        let mut hits = match tokio::time::timeout(timeout_dur, search).await {
            Ok(result) => result?,
            Err(_) => {
                // ENG-AUD-2026-S05-0002: return a typed Err rather than
                // Ok(Vec::new()) so callers can distinguish "backend timed
                // out" (infra failure) from "no matching documents" (correct
                // empty result).  Masking as empty silently degrades ADP
                // evidence consumers and any retry/circuit-breaker logic.
                tracing::warn!(
                    timeout_ms = timeout_ms,
                    "vector search timed out after {}ms — returning error (not empty result)",
                    timeout_ms
                );
                return Err(anyhow::anyhow!(
                    "ENG-AUD-2026-S05-0002: vector search infrastructure timeout after \
                         {timeout_ms}ms; backend may be unavailable — treat as transient failure, \
                         not an empty result set"
                ));
            }
        };

        #[cfg(feature = "vector")]
        if q.use_mmr && hits.len() > q.top_k {
            let chunk_ids: Vec<u64> = hits.iter().map(|h| h.chunk_id).collect();
            // M-4 fix: skip MMR and log a warning on LanceDB failure instead
            // of silently running reranking with an empty vector map (which
            // produces degenerate rankings where all cosine similarities are
            // 0.0, defeating the diversity objective entirely).
            match self
                .get_vectors_by_chunk_ids_with_epoch(
                    &q.project_id,
                    &chunk_ids,
                    vector_epoch.clone(),
                    _snapshot.clone(),
                )
                .await
            {
                Ok(vectors) => {
                    hits = self.mmr_rerank(hits, &vectors, q.top_k, 0.7);
                }
                Err(e) => {
                    tracing::warn!(
                        project_id = %q.project_id,
                        "MMR reranking skipped — failed to load vectors from LanceDB: {e:#}"
                    );
                    hits.truncate(q.top_k);
                }
            }
        }

        hits.truncate(q.top_k);

        // Vector rows carry no line columns; backfill line range + snippet
        // from Tantivy on the final, truncated list.
        self.enrich_hits_from_store(&mut hits);

        Ok(hits)
    }

    pub async fn get_vectors_by_chunk_ids(
        &self,
        project_id: &str,
        chunk_ids: &[u64],
    ) -> anyhow::Result<std::collections::HashMap<u64, Vec<f32>>> {
        #[cfg(feature = "vector")]
        {
            self.get_vectors_by_chunk_ids_with_epoch(
                project_id,
                chunk_ids,
                self.pin_vector_epoch(project_id)?,
                None,
            )
            .await
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = (project_id, chunk_ids);
            Ok(std::collections::HashMap::new())
        }
    }

    #[cfg(feature = "vector")]
    async fn get_vectors_by_chunk_ids_with_epoch(
        &self,
        project_id: &str,
        chunk_ids: &[u64],
        _vector_epoch: Arc<vector_epochs::Epoch>,
        source: Option<snapshots::Lease>,
    ) -> anyhow::Result<std::collections::HashMap<u64, Vec<f32>>> {
        // Hybrid ranking can combine disjoint 10,000-row lexical and vector
        // candidate sets before selecting its MMR prefix.
        anyhow::ensure!(chunk_ids.len() <= 20_000, "Native vector lookup exceeds 20000 chunk IDs");
        anyhow::ensure!(_vector_epoch.record.as_ref().is_none_or(|record| record.project == project_id),
            "Vector lease belongs to another project");
        self.ensure_managed_vector_table_present(&_vector_epoch)?;
        let worker = self.clone();
        let project = project_id.to_owned();
        let ids = chunk_ids.to_vec();
        self.run_native_vector_query(_vector_epoch.clone(), &CancellationToken::new(), async move {
            let _source = source;
            worker.get_vectors_by_chunk_ids_backend(&project, &ids, _vector_epoch).await
        }).await
    }

    #[cfg(feature = "vector")]
    async fn get_vectors_by_chunk_ids_backend(
        &self,
        project_id: &str,
        chunk_ids: &[u64],
        _vector_epoch: Arc<vector_epochs::Epoch>,
    ) -> anyhow::Result<std::collections::HashMap<u64, Vec<f32>>> {
        anyhow::ensure!(
            _vector_epoch
                .record
                .as_ref()
                .is_none_or(|r| r.project == project_id),
            "Vector lease belongs to another project"
        );
        #[cfg(feature = "vector")]
        {
            self.ensure_managed_vector_table_present(&_vector_epoch)?;
            let table_name = _vector_epoch.table.clone();
            if !self
                .native_store.capture()?.connection
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                return Ok(std::collections::HashMap::new());
            }
            let table = self.open_vector_epoch_table(&_vector_epoch).await?;

            let id_list = chunk_ids
                .iter()
                .map(|id| format!("CAST({} AS BIGINT UNSIGNED)", id))
                .collect::<Vec<_>>()
                .join(", ");
            let filter = format!("chunk_id IN ({})", id_list);

            let mut results = table.query().only_if(filter)
                .select(lancedb::query::Select::Columns(vec!["chunk_id".into(), "vector".into()]))
                .limit(1_000_001).execute().await?;

            let mut map = std::collections::HashMap::new();
            let mut rows = 0usize;
            use futures::TryStreamExt;
            while let Some(batch) = TryStreamExt::try_next(&mut results).await? {
                rows += batch.num_rows();
                anyhow::ensure!(rows <= 1_000_000, "Native vector lookup exceeds 1000000 rows");
                let chunk_id_arr = batch
                    .column_by_name("chunk_id")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt64Array>())
                    .ok_or_else(|| anyhow::anyhow!("missing chunk_id"))?;
                let vector_arr = batch
                    .column_by_name("vector")
                    .and_then(|c| c.as_any().downcast_ref::<arrow_array::FixedSizeListArray>())
                    .ok_or_else(|| anyhow::anyhow!("missing vector"))?;

                for i in 0..batch.num_rows() {
                    let id = chunk_id_arr.value(i);
                    let vec_view = vector_arr.value(i);
                    let vec_f32 = vec_view
                        .as_any()
                        .downcast_ref::<arrow_array::Float32Array>()
                        .ok_or_else(|| anyhow::anyhow!("vector is not f32"))?;
                    map.insert(id, vec_f32.values().to_vec());
                }
            }

            Ok(map)
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = project_id;
            let _ = chunk_ids;
            Ok(std::collections::HashMap::new())
        }
    }

    /// MMR diversity rerank.
    #[cfg(feature = "vector")]
    pub fn mmr_rerank(
        &self,
        candidates: Vec<HybridHit>,
        vectors: &std::collections::HashMap<u64, Vec<f32>>,
        top_k: usize,
        lambda: f32,
    ) -> Vec<HybridHit> {
        if candidates.is_empty() || top_k == 0 {
            return Vec::new();
        }

        let mut selected: Vec<HybridHit> = Vec::new();
        let mut remaining = candidates;

        selected.push(remaining.remove(0));
        if selected.len() == top_k || remaining.is_empty() { return selected; }

        // Keep one maximum per candidate rather than recomputing all earlier
        // pair similarities on every selection. Reserve this bounded buffer
        // before allocating; memory pressure retains the exact uncached rule.
        let cache_bytes = remaining.len().checked_mul(std::mem::size_of::<f32>())
            .and_then(|bytes| u64::try_from(bytes).ok());
        let (cache_admitted, _similarity_cache_allocation) = if remaining.len() <= 20_000 {
            match (self.memory_budget.as_ref(), cache_bytes) {
                (None, Some(_)) => (true, None),
                (Some(budget), Some(bytes)) => match AllocationGuard::try_new(budget, bytes, Subsystem::Misc, "MMR similarity maxima") {
                    Ok(guard) => (true, Some(guard)),
                    Err(_) => (false, None),
                },
                _ => (false, None),
            }
        } else { (false, None) };
        let mut max_similarities = cache_admitted.then(|| vec![0.0f32; remaining.len()]);

        // Pre-allocate a single fallback zero-vector outside the hot loop
        let zero_vec = vec![0.0f32; self.embedder.dimension()];

        while selected.len() < top_k && !remaining.is_empty() {
            let mut best_mmr_score = f32::MIN;
            let mut best_idx = 0;

            for (idx, cand) in remaining.iter().enumerate() {
                let cand_vec = vectors.get(&cand.chunk_id).unwrap_or(&zero_vec);

                let mut max_sim = 0.0f32;
                if let Some(maxima) = max_similarities.as_mut() {
                    if let Some(sel_vec) = selected.last().and_then(|sel| vectors.get(&sel.chunk_id)) {
                        let sim = dot_product(cand_vec, sel_vec).clamp(0.0, 1.0);
                        if sim > maxima[idx] { maxima[idx] = sim; }
                    }
                    max_sim = maxima[idx];
                } else { for sel in &selected {
                    let sel_vec = match vectors.get(&sel.chunk_id) {
                        Some(v) => v,
                        None => continue,
                    };
                    // Cosine similarity via dot product of (assumed) unit vectors.
                    // Clamp to [0, 1] to handle non-cosine embedders (e.g., Euclidean
                    // distance converted to `1 - dist`, which can produce negative
                    // values for distant points). Without clamping, a negative max_sim
                    // would flip the diversity penalty into a reward.
                    let sim = dot_product(cand_vec, sel_vec).clamp(0.0, 1.0);
                    if sim > max_sim {
                        max_sim = sim;
                    }
                } }

                // Standard MMR formula: λ * relevance - (1-λ) * max_similarity.
                // cand.score is the RRF relevance; max_sim is clamped to [0, 1].
                let mmr_score = lambda * cand.score - (1.0 - lambda) * max_sim;
                if mmr_score > best_mmr_score {
                    best_mmr_score = mmr_score;
                    best_idx = idx;
                }
            }

            selected.push(remaining.remove(best_idx));
            if let Some(maxima) = max_similarities.as_mut() { maxima.remove(best_idx); }
        }

        selected
    }

    /// Hybrid fuse (RRF).
    pub async fn search(
        &self,
        q: &HybridQuery,
        centrality_boost: Option<&std::collections::HashMap<String, f32>>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<HybridHit>> {
        self.search_ranked_prefix(q, centrality_boost, cancel, q.top_k).await
    }

    /// Return an exact prefix of the complete ranking requested by `q`.
    /// Candidate retrieval, score fusion, filters and the decision to apply
    /// MMR retain `q.top_k`. Greedy MMR need only choose the required prefix.
    /// Callers fusing several namespace rankings still need the full lists.
    pub async fn search_ranked_prefix(
        &self,
        q: &HybridQuery,
        centrality_boost: Option<&std::collections::HashMap<String, f32>>,
        cancel: &CancellationToken,
        output_limit: usize,
    ) -> anyhow::Result<Vec<HybridHit>> {
        let output_limit = output_limit.min(q.top_k);
        let _snapshot = if engram_core::get_policy(&q.namespace)
            .is_ok_and(|policy| policy.versioning == engram_core::NamespaceVersioning::Snapshot)
        {
            Some(
                self.snapshots
                    .pin(&q.project_id, &q.namespace, q.generation)?,
            )
        } else {
            None
        };
        #[cfg(feature = "vector")]
        let vector_epoch = if self.is_lexical_only() {
            None
        } else {
            Some(self.pin_vector_epoch(&q.project_id)?)
        };
        let fetch_k = if q.use_mmr {
            // FTS2: saturating_mul + cap prevents OOM on large top_k × multiplier combos.
            (q.top_k.saturating_mul(self.mmr_oversampling.max(2))).min(10_000)
        } else {
            q.top_k
        };

        let mut q_modified = q.clone();
        q_modified.top_k = fetch_k;

        let lexical = self.lexical_search(&q_modified)?;
        #[cfg(feature = "vector")]
        let vector = match &vector_epoch {
            Some(epoch) => {
                self.vector_search_with_epoch(&q_modified, cancel, epoch.clone())
                    .await?
            }
            None => Vec::new(),
        };
        #[cfg(not(feature = "vector"))]
        let vector = self.vector_search(&q_modified, cancel).await?;

        use std::collections::HashMap;
        let capacity = lexical.len() + vector.len();
        let mut rrf_scores: HashMap<String, (f32, HybridHit)> = HashMap::with_capacity(capacity);
        let k = 60.0;

        // Reusable buffer for file_node_id lookups (avoids per-hit format!() allocation).
        let mut file_node_buf = String::with_capacity(128);

        for (rank, mut hit) in lexical.into_iter().enumerate() {
            if let Some(boosts) = centrality_boost {
                file_node_buf.clear();
                file_node_buf.push_str("file:");
                file_node_buf.push_str(hit.path.as_str());
                hit.centrality = *boosts.get(file_node_buf.as_str()).unwrap_or(&0.0);
            }
            let score = 1.0 / (k + (rank + 1) as f32);
            // Use pk as the merge key; move pk out of hit to avoid clone when pk is populated.
            // NS1: the fallback key for hits with empty pk uses a canonical separator ':' and
            // the three fields that together uniquely identify a chunk within the project.
            // build_pk() is not used here because it requires project_id + generation which
            // are not available in the merge context — this fallback key is only used for
            // RRF deduplication within a single query response, not stored to any index.
            let key = if hit.pk.is_empty() {
                format!("{}:{}:{}", hit.path.as_str(), hit.chunk_id, hit.doc_id)
            } else {
                std::mem::take(&mut hit.pk)
            };
            // Re-store pk in hit for downstream consumers.
            hit.pk = key.clone();
            let entry = rrf_scores.entry(key).or_insert((0.0, hit));
            entry.0 += score;
        }

        for (rank, mut hit) in vector.into_iter().enumerate() {
            if let Some(boosts) = centrality_boost {
                file_node_buf.clear();
                file_node_buf.push_str("file:");
                file_node_buf.push_str(hit.path.as_str());
                hit.centrality = *boosts.get(file_node_buf.as_str()).unwrap_or(&0.0);
            }
            let score = 1.0 / (k + (rank + 1) as f32);
            // NS1: same canonical fallback as the lexical path above.
            let key = if hit.pk.is_empty() {
                format!("{}:{}:{}", hit.path.as_str(), hit.chunk_id, hit.doc_id)
            } else {
                std::mem::take(&mut hit.pk)
            };
            hit.pk = key.clone();
            let entry = rrf_scores.entry(key).or_insert((0.0, hit));
            entry.0 += score;
        }

        let mut merged: Vec<HybridHit> = rrf_scores
            .into_values()
            .map(|(rrf_score, mut hit)| {
                let boost = (1.0 + hit.centrality).ln() * 0.05;
                hit.score = rrf_score + boost;
                // Vendored/minified assets (bower_components, node_modules,
                // *.min.js …) are already excluded from the graph, but they
                // stayed fully searchable and routinely outranked app code —
                // a font-awesome bundle placed top-5 for "user role and
                // permission management". Demote hard: still findable when
                // the query genuinely targets vendor code (those hits have
                // no app-code competition), never ahead of first-party code.
                if engram_core::is_vendor_path(hit.path.as_str()) {
                    hit.score *= 0.2;
                }
                hit
            })
            .collect();

        // ENG-AUD-2026-S06-001: stable tie-break after score to make ranking
        // deterministic across identical queries.  HashMap iteration order and
        // f32 NaN-equality can otherwise produce different orderings run-to-run,
        // which breaks ADP reproducibility and regression assertions.
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.as_str().cmp(b.path.as_str()))
                .then_with(|| a.doc_id.cmp(&b.doc_id))
                .then_with(|| a.chunk_id.cmp(&b.chunk_id))
        });

        #[cfg(feature = "vector")]
        if q.use_mmr && merged.len() > q.top_k && vector_epoch.is_some() {
            let chunk_ids: Vec<u64> = merged.iter().map(|h| h.chunk_id).collect();
            let vectors = self
                .get_vectors_by_chunk_ids_with_epoch(
                    &q.project_id,
                    &chunk_ids,
                    vector_epoch
                        .as_ref()
                        .expect("MMR requires a vector epoch")
                        .clone(),
                    _snapshot.clone(),
                )
                .await?;
            merged = self.mmr_rerank(merged, &vectors, output_limit, 0.5);
        } else if merged.len() > q.top_k {
            merged.truncate(q.top_k);
        }

        #[cfg(not(feature = "vector"))]
        if merged.len() > q.top_k {
            merged.truncate(q.top_k);
        }

        merged.truncate(output_limit);

        // Vector-sourced hits have no line info; backfill from Tantivy now
        // that the list is final and small.
        self.enrich_hits_from_store(&mut merged);

        Ok(merged)
    }
}

/// Escape special characters for Tantivy's query parser so a literal user
/// string does not accidentally invoke boolean operators, ranges, wildcards,
/// regex, or other query syntax.
///
/// Covers all Tantivy 0.22+ special characters including `<` and `>` (used in
/// range queries like `[A TO Z]` and `{A TO Z}`).
/// FTS1: Count unescaped top-level `|` alternations in a regex pattern.
/// Only counts alternations at parenthesis depth 0 — alternations inside
/// `(a|b)` groups or `[a|b]` character classes are NOT counted, because
/// they are bounded sub-expressions with predictable DFA size.
/// Only top-level alternatives like `a|b|c|...` cause unbounded DFA growth.
fn count_unescaped_alternations(pat: &str) -> usize {
    let mut count = 0usize;
    let mut in_class = false;
    let mut paren_depth: i32 = 0;
    let mut escaped = false;
    for c in pat.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => paren_depth += 1,
            ')' if !in_class && paren_depth > 0 => paren_depth -= 1,
            // Only count top-level alternations (paren_depth == 0).
            '|' if !in_class && paren_depth == 0 => count += 1,
            _ => {}
        }
    }
    count
}

/// Build a query that matches `text` as literal words: split on whitespace,
/// each word analysed by `field`'s own tokenizer exactly as a parsed word
/// would be, joined by `conjunction` (strict) or disjunction (loose).
///
/// No query grammar is involved, so no input can be a syntax error. Escaping
/// into the grammar could not guarantee that: Tantivy rejects a bare `AND`,
/// `OR`, `NOT` or `IN` and an unescaped backtick, and treats `NOT` in prose as
/// negation — ordinary user-story text hit all three, and the SyntaxError
/// aborted the whole hybrid search. Words that yield no tokens (punctuation,
/// or shorter than the trigram width) are dropped first: left in, they
/// become empty clauses the parser calls "only excluding terms", and its
/// lenient repair turns that into match-all.
pub fn literal_text_query(
    index: &tantivy::Index,
    field: tantivy::schema::Field,
    text: &str,
    conjunction: bool,
) -> anyhow::Result<Box<dyn tantivy::query::Query>> {
    use tantivy::query_grammar::{Delimiter, UserInputAst, UserInputLeaf, UserInputLiteral};
    let mut analyzer = index.tokenizer_for_field(field)?;
    let clauses: Vec<_> = text
        .split_whitespace()
        .filter(|word| analyzer.token_stream(word).advance())
        .map(|word| {
            let literal = UserInputLiteral {
                field_name: None,
                phrase: word.to_string(),
                delimiter: Delimiter::None,
                slop: 0,
                prefix: false,
            };
            (
                None,
                UserInputAst::Leaf(Box::new(UserInputLeaf::Literal(literal))),
            )
        })
        .collect();
    if clauses.is_empty() {
        return Ok(Box::new(tantivy::query::EmptyQuery));
    }
    let mut parser = QueryParser::for_index(index, vec![field]);
    if conjunction {
        parser.set_conjunction_by_default();
    }
    Ok(parser.build_query_from_user_input_ast(UserInputAst::Clause(clauses))?)
}

/// Literal words ranked on the word field, each word also matchable as a
/// case-sensitive substring through the trigram field. Word-only matching
/// dropped documents the trigram index always found: `restart` never
/// reaches `restarted`, nor a table name its LINQ plural. Per word, either
/// form satisfies it (strict still requires every word); the substring form
/// scores at `trigram_weight` so whole-word matches rank first.
/// Score of a word matched only as a substring, relative to a whole-word
/// match. 0.1 measured best on a real project's history (0.1 / 0.3 / words-only).
pub const SUBSTRING_MATCH_WEIGHT: f32 = 0.1;

/// Shortest word (without surrounding punctuation) also matched as a substring.
pub const SUBSTRING_MIN_CHARS: usize = 4;

pub fn literal_word_or_substring_query(
    index: &tantivy::Index,
    words: tantivy::schema::Field,
    trigrams: tantivy::schema::Field,
    text: &str,
    conjunction: bool,
    trigram_weight: f32,
) -> anyhow::Result<Box<dyn tantivy::query::Query>> {
    let occur = if conjunction {
        Occur::Must
    } else {
        Occur::Should
    };
    let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
    for word in text.split_whitespace() {
        let mut forms: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();
        let by_word = literal_text_query(index, words, word, false)?;
        if !by_word.is::<tantivy::query::EmptyQuery>() {
            forms.push((Occur::Should, by_word));
        }
        // The substring form takes the word without surrounding punctuation
        // (`projekt`, "missing", (1,2)) and only from four characters up: a
        // short common word's trigrams occur in nearly every document, which
        // cost a story-length query a second and ranked nothing.
        let core = word.trim_matches(|c: char| !c.is_alphanumeric());
        let by_substring = if core.chars().count() >= SUBSTRING_MIN_CHARS {
            literal_text_query(index, trigrams, core, false)?
        } else {
            Box::new(tantivy::query::EmptyQuery)
        };
        if !by_substring.is::<tantivy::query::EmptyQuery>() {
            forms.push((
                Occur::Should,
                Box::new(tantivy::query::BoostQuery::new(
                    by_substring,
                    trigram_weight,
                )),
            ));
        }
        if !forms.is_empty() {
            clauses.push((occur, Box::new(BooleanQuery::new(forms))));
        }
    }
    if clauses.is_empty() {
        return Ok(Box::new(tantivy::query::EmptyQuery));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

pub fn escape_tantivy_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '+' | '-' | '&' | '|' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '"' | '~'
            | '*' | '?' | ':' | '\\' | '/' | '<' | '>' | '\'' => {
                // ENG-2026-FTS-APOS: Tantivy's query parser treats `'` as a
                // grammar token, so an unescaped contraction (`doesn't`) makes
                // parse_query return Err(SyntaxError) — which aborts the whole
                // hybrid search() before the vector arm runs. Found via the
                // pilot eval: NL queries with apostrophes returned 0 hits.
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(feature = "vector")]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Derive a u64 chunk_id from the content_hash for backward compatibility.
/// Uses the first 8 bytes of the raw blake3 hash (not the hex string).
pub fn chunk_id_from_hash(hash: [u8; 32]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&hash[0..8]);
    u64::from_le_bytes(b)
}

/// Derive a u64 chunk_id from a ContentHash (hex string).
pub fn chunk_id_from_content_hash(h: &ContentHash) -> u64 {
    // Decode first 8 bytes of hex string
    let hex = &h.0;
    if hex.len() < 16 {
        return 0;
    }
    let mut bytes = [0u8; 8];
    for i in 0..8 {
        match u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
            Ok(b) => bytes[i] = b,
            // Any invalid hex digit makes the entire hash undecodable; return
            // the canonical "missing" sentinel (0) rather than a corrupt partial ID.
            Err(_) => return 0,
        }
    }
    u64::from_le_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Embedder factory (for HybridSearchEngine::new)
// ---------------------------------------------------------------------------

/// Check if a file is a web.config or app.config (ASP.NET configuration).
/// Extract the content of all inline `<script>` blocks from WebForms markup.
///
/// Concatenates the bodies of `<script ...>...</script>` tags (excluding `runat="server"`)
/// so the JS bridge extractor can find jQuery selectors, __doPostBack calls, fetch/XHR
/// calls, and PageMethods invocations embedded directly in .aspx/.ascx/.master files.
fn extract_inline_scripts(markup: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    // Match <script ...>...</script> blocks (case-insensitive, non-greedy).
    static SCRIPT_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)<script\b([^>]*)>(.*?)</script>"#).expect("valid regex literal")
    });

    let mut out = String::new();
    for cap in SCRIPT_RE.captures_iter(markup) {
        let attrs = cap.get(1).map_or("", |m| m.as_str());
        // Skip server-side scripts (e.g., <script runat="server"> which is C#/VB code)
        if attrs.to_lowercase().contains("runat") {
            continue;
        }
        let body = cap.get(2).map_or("", |m| m.as_str()).trim();
        if !body.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(body);
        }
    }
    out
}

fn is_web_config(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.eq_ignore_ascii_case("web.config") || s.eq_ignore_ascii_case("app.config"))
        .unwrap_or(false)
}

/// Build an embedder from the configured embedding backend.
/// Returns `Err` when a remote backend is configured but cannot be constructed,
/// so callers fail fast rather than silently degrading to ProjectionEmbedder.
/// EMB2: silent fallback was replaced with explicit fail-closed error to ensure
/// operators are notified when the configured backend is unavailable.
#[cfg(feature = "vector")]
fn build_embedder_for_backend(
    cfg: &engram_core::Config,
) -> anyhow::Result<Arc<dyn engram_ml::Embedder>> {
    match cfg.embedding_backend.as_str() {
        "openai" | "remote" => {
            let api_key = cfg.openai_api_key.clone().unwrap_or_default();
            let api_base = cfg
                .openai_api_base
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            let model = cfg
                .embedding_model
                .clone()
                .unwrap_or_else(|| "text-embedding-3-small".into());
            let embedder = engram_ml::embed::RemoteEmbedder::openai(model, api_key, api_base, 1536)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "ENG-AUD-2026-0007 EMB2: failed to build OpenAI embedder \
                         (embedding_backend=openai) — refusing to fall back to \
                         ProjectionEmbedder; fix the configuration or switch to \
                         a different backend: {e}"
                    )
                })?;
            Ok(Arc::new(embedder))
        }
        "ollama" => {
            let url = cfg
                .ollama_url
                .clone()
                .unwrap_or_else(|| "http://localhost:11434".into());
            let model = cfg
                .embedding_model
                .clone()
                .unwrap_or_else(|| "nomic-embed-text".into());
            let embedder =
                engram_ml::embed::RemoteEmbedder::ollama(model, url, 768).map_err(|e| {
                    anyhow::anyhow!(
                        "ENG-AUD-2026-0007 EMB2: failed to build Ollama embedder \
                         (embedding_backend=ollama) — refusing to fall back to \
                         ProjectionEmbedder; fix the configuration or switch to \
                         a different backend: {e}"
                    )
                })?;
            Ok(Arc::new(embedder))
        }
        "local" | "candle" => Ok(Arc::new(engram_ml::embed::LocalEmbedder)),
        _ => Ok(Arc::new(engram_ml::embed::ProjectionEmbedder::new(
            crate::vector::VECTOR_DIM,
        ))),
    }
}

#[cfg(test)]
mod p0_core_loop_tests {
    use super::{SNIPPET_MAX_CHARS, SemanticQuality, semantic_quality_for_backend, snippet_of};

    #[test]
    fn semantic_quality_maps_every_backend() {
        assert_eq!(
            semantic_quality_for_backend("ollama"),
            SemanticQuality::Semantic
        );
        assert_eq!(
            semantic_quality_for_backend("openai"),
            SemanticQuality::Semantic
        );
        assert_eq!(
            semantic_quality_for_backend("remote"),
            SemanticQuality::Semantic
        );
        assert_eq!(
            semantic_quality_for_backend("fts_only"),
            SemanticQuality::Off
        );
        assert_eq!(
            semantic_quality_for_backend("local"),
            SemanticQuality::DegradedTrigram
        );
        assert_eq!(
            semantic_quality_for_backend("candle"),
            SemanticQuality::DegradedTrigram
        );
        // Config::default() leaves the backend empty — that is the default
        // install and must be labeled degraded, not semantic.
        assert_eq!(
            semantic_quality_for_backend(""),
            SemanticQuality::DegradedTrigram
        );
    }

    #[test]
    fn snippet_short_content_is_untouched() {
        let (sn, truncated) = snippet_of("fn main() {}", SNIPPET_MAX_CHARS);
        assert_eq!(sn, "fn main() {}");
        assert!(!truncated);
    }

    #[test]
    fn snippet_cuts_at_line_boundary() {
        let line = "a".repeat(80);
        let content = vec![line.as_str(); 10].join("\n"); // 809 chars
        let (sn, truncated) = snippet_of(&content, 500);
        assert!(truncated);
        // 6 lines of 80 + 5 newlines = 485 ≤ 500; cut lands on a boundary.
        assert!(content.starts_with(&sn));
        assert_eq!(content.as_bytes()[sn.len()], b'\n');
        assert!(sn.chars().count() <= 500);
    }

    #[test]
    fn snippet_single_oversized_line_falls_back_to_char_cut() {
        let content = "x".repeat(900); // no newlines at all
        let (sn, truncated) = snippet_of(&content, 500);
        assert!(truncated);
        assert_eq!(sn.chars().count(), 500);
    }

    #[test]
    fn snippet_exact_budget_is_not_truncated() {
        let content = "y".repeat(500);
        let (sn, truncated) = snippet_of(&content, 500);
        assert_eq!(sn.len(), 500);
        assert!(!truncated);
    }

    #[test]
    fn snippet_multibyte_safe() {
        // 600 two-byte chars: a 500-char budget must not split a char.
        let content = "é".repeat(600);
        let (sn, truncated) = snippet_of(&content, 500);
        assert!(truncated);
        assert_eq!(sn.chars().count(), 500);
    }
}

/// All case spellings of a short token (each letter lower/upper), for the
/// case-insensitive trigram query. Letters without a distinct other case
/// contribute one spelling; the result is deduplicated and sorted.
pub(crate) fn case_variants(token: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![String::new()];
    for c in token.chars() {
        let lower: Vec<char> = c.to_lowercase().collect();
        let upper: Vec<char> = c.to_uppercase().collect();
        let mut forms: Vec<Vec<char>> = vec![lower.clone()];
        if upper != lower {
            forms.push(upper);
        }
        let mut next = Vec::with_capacity(out.len() * forms.len());
        for prefix in &out {
            for f in &forms {
                let mut s = prefix.clone();
                s.extend(f.iter());
                next.push(s);
            }
        }
        out = next;
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod case_variant_tests {
    use super::case_variants;

    #[test]
    fn three_letters_give_eight_spellings() {
        let v = case_variants("per");
        assert_eq!(v.len(), 8);
        assert!(v.contains(&"per".to_string()));
        assert!(v.contains(&"PER".to_string()));
        assert!(v.contains(&"pEr".to_string()));
    }

    #[test]
    fn caseless_characters_do_not_multiply() {
        assert_eq!(case_variants("_1."), vec!["_1.".to_string()]);
        assert_eq!(case_variants("a_1").len(), 2);
    }

    #[test]
    fn non_ascii_letters_get_both_cases() {
        let v = case_variants("åäö");
        assert_eq!(v.len(), 8);
        assert!(v.contains(&"ÅÄÖ".to_string()));
    }
}
