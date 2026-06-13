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
    pub language_filters: Option<Vec<String>>,
    pub author_filter: Option<String>,
    pub date_after: Option<u64>,
    pub date_before: Option<u64>,
    pub use_mmr: bool,
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

#[derive(Debug, Clone, Default)]
pub struct IngestStats {
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

/// Cancel-safe scope guard for a long-lived Tantivy IndexWriter.
///
/// On drop (including panic/cancel), commits any pending docs and waits
/// for merge threads so segments are never left orphaned.
pub struct BulkWriterGuard {
    writer: Option<tantivy::IndexWriter<tantivy::TantivyDocument>>,
    docs_since_commit: usize,
}

impl BulkWriterGuard {
    /// Commit the current batch. Cheap (no merge wait).
    pub fn commit(&mut self) -> anyhow::Result<()> {
        if let Some(ref mut w) = self.writer {
            w.commit()?;
            self.docs_since_commit = 0;
        }
        Ok(())
    }

    /// Commit if `docs_since_commit >= threshold`.
    pub fn maybe_commit(&mut self, threshold: usize) -> anyhow::Result<()> {
        if self.docs_since_commit >= threshold {
            self.commit()?;
        }
        Ok(())
    }

    /// Final shutdown: commit + wait for all merge threads.
    /// This is the expensive call — run it once at the end.
    pub fn finish(mut self) -> anyhow::Result<()> {
        if let Some(mut w) = self.writer.take() {
            w.commit()?;
            w.wait_merging_threads()?;
        }
        Ok(())
    }
}

impl Drop for BulkWriterGuard {
    fn drop(&mut self) {
        if let Some(mut w) = self.writer.take() {
            // Best-effort commit + merge wait on cancel/panic.
            if w.commit().is_ok() {
                let _ = w.wait_merging_threads();
            }
        }
    }
}

pub struct HybridSearchEngine {
    tantivy_index: tantivy::Index,
    fields: Fields,
    extractor: Arc<crate::parsing::SymbolExtractor>,
    #[cfg(feature = "vector")]
    embedder: Arc<dyn engram_ml::Embedder>,
    #[cfg(feature = "vector")]
    lance_conn: lancedb::Connection,
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
        let (index, fields) = open_or_create(&tantivy_dir)?;

        #[cfg(feature = "vector")]
        let lance_conn = crate::vector::connect(&lance_dir).await?;

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
            // "fts_only" and empty-string (Config::default()) signal that vector
            // embeddings are intentionally disabled. Use a no-op stub embedder.
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

        // 16b: wrap remote embedders in the cross-project content-hash cache.
        // Reindex cycles, copy-forward, and history re-runs become cache hits
        // instead of full re-embeds. Local/deterministic embedders are cheap
        // enough that caching only adds I/O. Cache-open failure falls back to
        // the uncached embedder (warn, never fail engine construction).
        #[cfg(feature = "vector")]
        let embedder: Arc<dyn engram_ml::Embedder> =
            if matches!(embedding_backend.as_str(), "openai" | "remote" | "ollama")
                && !cfg.data_dir.as_os_str().is_empty()
            {
                let cache_path = cfg.data_dir.join("embed_cache.redb");
                match crate::embed_cache::CachedEmbedder::new(embedder.clone(), &cache_path) {
                    Ok(cached) => Arc::new(cached),
                    Err(e) => {
                        tracing::warn!("embed cache unavailable, continuing uncached: {e:#}");
                        embedder
                    }
                }
            } else {
                embedder
            };

        Ok(Self {
            tantivy_index: index,
            fields,
            extractor: Arc::new(crate::parsing::SymbolExtractor::new()),
            #[cfg(feature = "vector")]
            embedder,
            #[cfg(feature = "vector")]
            lance_conn,
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
        semantic_quality_for_backend(&self.embedding_backend)
    }

    /// Create a long-lived Tantivy writer for bulk indexing.
    ///
    /// The returned guard commits + waits on drop (cancel-safe).
    /// Call `write_docs_to_writer` to add documents, then `finish()`.
    pub fn create_bulk_writer(&self) -> anyhow::Result<BulkWriterGuard> {
        let writer = self.tantivy_index.writer(self.tantivy_writer_memory)?;
        Ok(BulkWriterGuard {
            writer: Some(writer),
            docs_since_commit: 0,
        })
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
    pub async fn embed_and_upsert_vectors(
        &self,
        project_id: &str,
        docs: &[IndexDoc],
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        if docs.is_empty() || cancel.is_cancelled() {
            return Ok(());
        }

        #[cfg(feature = "vector")]
        if self.embedding_backend != "fts_only" {
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

            let table_name = format!("project_{}", project_id.replace('-', "_"));
            let (table, open_outcome) = crate::vector::open_or_create_table(
                &self.lance_conn,
                &table_name,
                self.embedder.dimension(),
            )
            .await?;
            if let crate::vector::TableOpenOutcome::Recreated {
                ref reason,
                prior_row_count,
            } = open_outcome
            {
                let loss_str = match prior_row_count {
                    Some(n) => format!("{n} historical vectors were lost"),
                    None => "historical vector count unknown".to_string(),
                };
                anyhow::bail!(
                    "VEC1: vector table '{table_name}' recreated ({reason}) — {loss_str}. \
                     Full re-index required for project '{project_id}'."
                );
            }

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
                    // X1-7f9b / X2-embmem: same admission-control protocol as the
                    // index_docs embed path — per-batch budget headroom check,
                    // released BEFORE the await so the guard never spans a
                    // suspension point (a slow remote embedder must not starve
                    // other allocations for the round-trip duration).
                    let estimated_embed_bytes = chunk.iter().map(|t| t.len() as u64).sum::<u64>()
                        + (chunk.len() as u64 * self.embedder.dimension() as u64 * 4);
                    let _embed_guard = self
                        .memory_budget
                        .as_ref()
                        .map(|budget| {
                            AllocationGuard::try_new(
                                budget,
                                estimated_embed_bytes,
                                Subsystem::LanceDb,
                                "embedding batch",
                            )
                        })
                        .transpose()?;
                    drop(_embed_guard);
                    let batch_vecs = self.embedder.embed_batch_cancellable(chunk, cancel).await?;
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
                crate::vector::upsert_vectors(&table, vec![batch])
                    .await
                    .map_err(|e| {
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

        // 1. Lexical index (Tantivy) — pk-based upsert
        {
            let _tantivy_guard = self
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        self.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "tantivy index writer",
                    )
                })
                .transpose()?;

            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
                self.tantivy_index.writer(self.tantivy_writer_memory)?;
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
                writer.delete_term(Term::from_field_text(self.fields.pk, &pk));

                let tdoc = doc!(
                    self.fields.pk => pk.as_str(),
                    self.fields.doc_id => d.doc_id.as_str(),
                    self.fields.content_hash => d.content_hash.as_str(),
                    self.fields.project_id => project_id,
                    self.fields.namespace => d.namespace.as_str(),
                    self.fields.generation => effective_gen,
                    self.fields.chunk_id => d.chunk_id,
                    self.fields.path => d.path.as_str(),
                    self.fields.language => d.language.as_str(),
                    self.fields.author => d.author.as_deref().unwrap_or(""),
                    self.fields.timestamp => d.timestamp.unwrap_or(0),
                    self.fields.start_line => d.start_line as u64,
                    self.fields.end_line => d.end_line as u64,
                    self.fields.content => d.content.as_str(),
                );
                writer.add_document(tdoc)?;
            }
            writer.commit()?;
            // IMPORTANT: wait for Tantivy merge workers before dropping the writer.
            // Dropping an IndexWriter cancels in-flight/background merges, which can
            // leave thousands of tiny segments across batched indexing runs.
            // This call may block for seconds-to-minutes on large indexes and is
            // expected for correctness/consolidation.
            writer.wait_merging_threads()?;
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        // 2. Vector index (LanceDB) — pk-based upsert
        #[cfg(feature = "vector")]
        if self.embedding_backend != "fts_only" {
            let table_name = format!("project_{}", project_id.replace('-', "_"));
            let (table, open_outcome) = crate::vector::open_or_create_table(
                &self.lance_conn,
                &table_name,
                self.embedder.dimension(),
            )
            .await?;
            if let crate::vector::TableOpenOutcome::Recreated {
                ref reason,
                prior_row_count,
            } = open_outcome
            {
                // VEC1/X1: fail-closed so the caller knows a full re-index is required.
                // `prior_row_count` gives operators the data-loss metric: Some(n) = exact
                // loss, None = count failed so magnitude is unknown (treat as non-zero).
                // Returning Err here propagates up to the job runner, which must
                // schedule a full project reindex to repopulate the vector store.
                // The Tantivy write committed above is idempotent — retrying the
                // full batch is safe and will repair both stores.
                let loss_str = match prior_row_count {
                    Some(n) => format!("{n} historical vectors were lost"),
                    None => "historical vector count unknown (count_rows failed before drop)"
                        .to_string(),
                };
                anyhow::bail!(
                    "VEC1: vector table '{table_name}' was recreated due to schema mismatch \
                     ({reason}) — {loss_str}. A full re-index is required to restore semantic \
                     search quality. Schedule a reindex job for project '{project_id}' and retry."
                );
            }

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

            // Pre-compute effective generations and collect metadata before embedding.
            // This allows batch embedding (single API call for all docs).
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

            // Batch embed: single API call for remote backends (5-10x faster).
            // For ProjectionEmbedder the default sequential fallback is used.
            if !cancel.is_cancelled() && !contents_for_embed.is_empty() {
                // Process in sub-batches of 64 to bound per-request payload size.
                for chunk in contents_for_embed.chunks(64) {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let estimated_embed_bytes = chunk.iter().map(|t| t.len() as u64).sum::<u64>()
                        + (chunk.len() as u64 * self.embedder.dimension() as u64 * 4);
                    let _embed_guard = self
                        .memory_budget
                        .as_ref()
                        .map(|budget| {
                            AllocationGuard::try_new(
                                budget,
                                estimated_embed_bytes,
                                Subsystem::LanceDb,
                                "embedding batch",
                            )
                        })
                        .transpose()?;
                    // X1-7f9b: release the allocation budget before awaiting the remote
                    // embed call. Holding AllocationGuard across an async .await ties the
                    // memory budget to network latency — a slow remote embedder starves all
                    // other concurrent allocations for the full round-trip duration.
                    drop(_embed_guard);
                    // EMB1: use cancellable batch embed so in-flight remote HTTP
                    // calls can be preempted when the cancellation token fires.
                    let batch_vecs = self.embedder.embed_batch_cancellable(chunk, cancel).await?;
                    vectors.extend(batch_vecs);
                }
            }

            if !cancel.is_cancelled() && !pks.is_empty() {
                let estimated_vector_bytes =
                    vectors.iter().map(|v| (v.len() as u64) * 4).sum::<u64>()
                        + pks.iter().map(|pk| pk.len() as u64).sum::<u64>();
                let _vector_guard = self
                    .memory_budget
                    .as_ref()
                    .map(|budget| {
                        AllocationGuard::try_new(
                            budget,
                            estimated_vector_bytes,
                            Subsystem::LanceDb,
                            "vector ingestion",
                        )
                    })
                    .transpose()?;

                // Namespace homogeneity is guaranteed by the entry-point check above.
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

                // Propagate LanceDB errors so the caller knows indexing is
                // incomplete and can retry.  Tantivy pk-based upserts are
                // idempotent, so retrying the full batch repairs both stores.
                // Silently swallowing this error (the previous behaviour) left
                // Tantivy and LanceDB permanently diverged with no recovery
                // signal to the caller (C-2 fix).
                crate::vector::upsert_vectors(&table, vec![batch])
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "LanceDB vector upsert failed; lexical index was committed \
                         but vector index was not updated — retry to repair: {e:#}"
                        )
                    })?;
            }
        }

        Ok(())
    }

    pub async fn purge_old_generations(
        &self,
        project_id: &str,
        active_generation: u64,
    ) -> anyhow::Result<()> {
        // 1. Tantivy purge
        {
            // MEM3-7d30af: acquire AllocationGuard before opening writer so the
            // memory budget is informed of the write-buffer reservation even during
            // purge/GC operations (same as index_docs does for the write path).
            let _tantivy_guard = self
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        self.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "tantivy purge writer",
                    )
                })
                .transpose()?;
            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
                self.tantivy_index.writer(self.tantivy_writer_memory)?;

            for ns in engram_core::KNOWN_NAMESPACES {
                if let Ok(policy) = engram_core::get_policy(ns) {
                    let pid_term = Term::from_field_text(self.fields.project_id, project_id);
                    let ns_term = Term::from_field_text(self.fields.namespace, ns);

                    match policy.retention {
                        engram_core::NamespaceRetention::KeepLatestOnly => {
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
                                    Occur::MustNot,
                                    Box::new(TermQuery::new(
                                        Term::from_field_u64(
                                            self.fields.generation,
                                            active_generation,
                                        ),
                                        IndexRecordOption::Basic,
                                    )),
                                ),
                            ]);
                            writer.delete_query(Box::new(query))?;
                        }
                        engram_core::NamespaceRetention::KeepLastGenerations(n) => {
                            let min_keep = active_generation.saturating_sub(n as u64 - 1);
                            if min_keep > 0 {
                                let parser = QueryParser::for_index(
                                    &self.tantivy_index,
                                    vec![self.fields.generation],
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
            writer.commit()?;

            // Trigger segment merge to reclaim disk space from deleted documents.
            // Without this, deleted docs remain as tombstones in segments indefinitely.
            drop(writer.garbage_collect_files());
            // IMPORTANT: consume the writer only after commit/GC side effects are
            // queued so merge threads can run to completion before writer teardown.
            // This may block for seconds-to-minutes on heavily fragmented indexes.
            writer.wait_merging_threads()?;
        }

        // 2. LanceDB purge
        #[cfg(feature = "vector")]
        {
            let table_name = format!("project_{}", project_id.replace('-', "_"));
            if self
                .lance_conn
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                let table = self.lance_conn.open_table(&table_name).execute().await?;
                crate::vector::purge_old_generations(&table, active_generation).await?;
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
        if unchanged_paths.is_empty() {
            return Ok(());
        }

        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let mut all_docs: Vec<IndexDoc> = Vec::new();

        for path in unchanged_paths {
            if cancel.is_cancelled() {
                break;
            }

            let pid_term = Term::from_field_text(self.fields.project_id, project_id);
            let pid_q = TermQuery::new(pid_term, IndexRecordOption::Basic);
            let ns_term = Term::from_field_text(self.fields.namespace, namespace);
            let ns_q = TermQuery::new(ns_term, IndexRecordOption::Basic);
            let path_term = Term::from_field_text(self.fields.path, path.as_str());
            let path_q = TermQuery::new(path_term, IndexRecordOption::Basic);
            let gen_term = Term::from_field_u64(self.fields.generation, old_generation);
            let gen_q = TermQuery::new(gen_term, IndexRecordOption::Basic);

            let query = BooleanQuery::new(vec![
                (
                    Occur::Must,
                    Box::new(pid_q) as Box<dyn tantivy::query::Query>,
                ),
                (
                    Occur::Must,
                    Box::new(ns_q) as Box<dyn tantivy::query::Query>,
                ),
                (
                    Occur::Must,
                    Box::new(path_q) as Box<dyn tantivy::query::Query>,
                ),
                (
                    Occur::Must,
                    Box::new(gen_q) as Box<dyn tantivy::query::Query>,
                ),
            ]);

            const PAGE_SIZE: usize = 2000;
            let mut offset = 0usize;
            loop {
                let page: Vec<(Score, DocAddress)> =
                    searcher.search(&query, &TopDocs::with_limit(PAGE_SIZE).and_offset(offset))?;
                if page.is_empty() {
                    break;
                }

                for (_, addr) in page.iter().copied() {
                    let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
                    let get_str = |f| {
                        doc.get_first(f)
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    let get_u64 = |f| doc.get_first(f).and_then(|v| v.as_u64()).unwrap_or(0);

                    let content_hash = get_str(self.fields.content_hash);
                    let path_str = get_str(self.fields.path);
                    let language = get_str(self.fields.language);
                    let content = get_str(self.fields.content);
                    let author = get_str(self.fields.author);
                    let timestamp = get_u64(self.fields.timestamp);
                    let chunk_id = get_u64(self.fields.chunk_id);
                    let start_line = get_u64(self.fields.start_line) as u32;
                    let end_line = get_u64(self.fields.end_line) as u32;

                    // doc_id is location-based so it is stable across generations for same content at same place
                    let ch = ContentHash(content_hash.clone());
                    let new_doc_id = DocIdStr::compute(&path_str, start_line, end_line, &ch);

                    all_docs.push(IndexDoc {
                        generation: new_generation,
                        chunk_id,
                        path: RelPath::new(&path_str),
                        language,
                        content,
                        namespace: namespace.to_string(),
                        author: if author.is_empty() {
                            None
                        } else {
                            Some(author)
                        },
                        timestamp: if timestamp == 0 {
                            None
                        } else {
                            Some(timestamp)
                        },
                        start_line,
                        end_line,
                        doc_id: new_doc_id.0,
                        content_hash,
                    });
                }

                offset = offset.saturating_add(PAGE_SIZE);
                if page.len() < PAGE_SIZE {
                    break;
                }
            }

            if all_docs.len() >= 512 {
                self.index_docs(project_id, &all_docs, cancel).await?;
                all_docs.clear();
            }
        }

        if !all_docs.is_empty() && !cancel.is_cancelled() {
            self.index_docs(project_id, &all_docs, cancel).await?;
        }

        Ok(())
    }

    /// Delete all docs for given paths from GlobalMutable/AppendOnly namespaces.
    /// For Snapshot namespaces, use copy-forward instead; this is a no-op for those.
    pub async fn delete_files(
        &self,
        project_id: &str,
        namespace: &str,
        paths: &[RelPath],
    ) -> anyhow::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        // 1. LanceDB first — attempt before Tantivy commits so that a LanceDB
        //    failure leaves Tantivy in its pre-delete state (no cross-store divergence).
        #[cfg(feature = "vector")]
        {
            let table_name = format!("project_{}", project_id.replace('-', "_"));
            if self
                .lance_conn
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                let table = self.lance_conn.open_table(&table_name).execute().await?;
                for p in paths {
                    // Escape single quotes in path (SQL injection safety)
                    let safe_path = p.as_str().replace('\'', "''");
                    let safe_ns = namespace.replace('\'', "''");
                    let filter = format!("namespace = '{}' AND path = '{}'", safe_ns, safe_path);
                    table.delete(&filter).await?;
                }
            }
        }

        // 2. Tantivy — commit only after LanceDB succeeds (or the vector feature is absent).
        {
            // MEM3-7d30af: acquire AllocationGuard before opening writer so the
            // memory budget is informed of the write-buffer reservation during
            // delete operations (mirrors index_docs guard pattern).
            let _tantivy_guard = self
                .memory_budget
                .as_ref()
                .map(|budget| {
                    AllocationGuard::try_new(
                        budget,
                        self.tantivy_writer_memory as u64,
                        Subsystem::Tantivy,
                        "tantivy delete writer",
                    )
                })
                .transpose()?;
            let mut writer: tantivy::IndexWriter<tantivy::TantivyDocument> =
                self.tantivy_index.writer(self.tantivy_writer_memory)?;
            for p in paths {
                let pid_term = Term::from_field_text(self.fields.project_id, project_id);
                let ns_term = Term::from_field_text(self.fields.namespace, namespace);
                let path_term = Term::from_field_text(self.fields.path, p.as_str());
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
            writer.commit()?;
            // IMPORTANT: wait for Tantivy merge workers before dropping the writer.
            // Dropping an IndexWriter cancels in-flight/background merges, which can
            // leave the index in a highly fragmented segment state over time.
            // This may block for seconds-to-minutes on large indexes and is expected.
            writer.wait_merging_threads()?;
        }

        Ok(())
    }

    /// Return lightweight metadata for all Tantivy docs in a project.
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
    pub fn count_docs_by_namespace(
        &self,
        project_id: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, usize>> {
        let reader = self.tantivy_index.reader()?;
        let searcher = reader.searcher();
        let mut counts = std::collections::HashMap::new();
        for ns in &["memory", "history", "antipattern", "vfs"] {
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

    pub async fn count_vectors(&self, project_id: &str) -> anyhow::Result<usize> {
        #[cfg(feature = "vector")]
        {
            let table_name = format!("project_{}", project_id.replace('-', "_"));
            if !self
                .lance_conn
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                return Ok(0);
            }
            let table = self.lance_conn.open_table(&table_name).execute().await?;
            Ok(table.count_rows(None).await?)
        }
        #[cfg(not(feature = "vector"))]
        {
            let _ = project_id;
            Ok(0)
        }
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
        use rayon::prelude::*;

        let mut stats = IngestStats::default();
        let total_files = files.len();
        stats.files = total_files;

        crate::vb_extractor::begin_project(root);

        // Single Tantivy writer for the entire index_files run.
        // BulkWriterGuard commits + waits on Drop (cancel-safe).
        let mut guard = self.create_bulk_writer()?;
        let fields = self.fields();

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
            let _chunking_guard = self
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
            let extractor = self.extractor.clone();
            let root_buf = root.to_path_buf();
            let namespace_str = namespace.to_string();

            let (chunk_stats, chunk_docs) = tokio::task::spawn_blocking(move || {
                chunk_paths
                    .par_iter()
                    .map(|p| {
                        let mut local_stats = IngestStats::default();
                        let mut local_docs = Vec::new();
                        let rel_path = RelPath::from_relative(&root_buf, p)
                            .unwrap_or_else(|| RelPath::new(&p.to_string_lossy()));

                        let Ok(meta) = std::fs::metadata(p) else {
                            local_stats
                                .skipped_files
                                .push((rel_path, "Could not read metadata".into()));
                            return (local_stats, local_docs);
                        };
                        if meta.len() > ingest::MAX_FILE_SIZE {
                            local_stats
                                .skipped_files
                                .push((rel_path, format!("File too large ({} bytes)", meta.len())));
                            return (local_stats, local_docs);
                        }

                        if ingest::is_binary(p) {
                            local_stats
                                .skipped_files
                                .push((rel_path, "Binary file".into()));
                            return (local_stats, local_docs);
                        }

                        let Ok(bytes) = std::fs::read(p) else {
                            // DS1/D5: log unexpected read failures so operators can see
                            // them in structured logs, not just in the indexing report.
                            // Policy: fail-open (skip file, continue job).
                            tracing::warn!(
                                path = %p.display(),
                                "DS1/D5: file read failed during indexing — file skipped (fail-open)"
                            );
                            local_stats
                                .skipped_files
                                .push((rel_path, "Could not read file content".into()));
                            return (local_stats, local_docs);
                        };
                        let size = bytes.len() as u64;
                        local_stats.bytes += size;

                        let file_hash = blake3::hash(&bytes).to_hex().to_string();
                        // Reuse the metadata we already read (avoids second stat syscall)
                        let mtime_ms = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);

                        let Ok(text) = String::from_utf8(bytes) else {
                            // DS1/D5: log unexpected UTF-8 failures (fail-open: skip file).
                            tracing::warn!(
                                path = %p.display(),
                                "DS1/D5: file UTF-8 decode failed during indexing — file skipped (fail-open)"
                            );
                            local_stats
                                .skipped_files
                                .push((rel_path, "Invalid UTF-8 encoding".into()));
                            return (local_stats, local_docs);
                        };
                        let language = engram_core::guess_language(p);
                        *local_stats
                            .languages
                            .entry(language.to_string())
                            .or_insert(0) += 1;

                        // Wrap rel_path in Arc so all symbols/edges/chunks from this file
                        // share one allocation. O(1) clone instead of O(N) string copies.
                        let arc_rel = Arc::new(rel_path);

                        local_stats.all_files.push((*arc_rel).clone());

                        local_stats
                            .fingerprints
                            .push(crate::docstore::FileFingerprint {
                                rel_path: arc_rel.as_str().to_string(),
                                size,
                                mtime_ms,
                                file_hash,
                            });

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
                                crate::vb_extractor::extract_vb(p, &text)
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
                            crate::vb_extractor::extract_vb(p, &text)
                        } else if ext_lower.as_deref() == Some("cs") {
                            crate::cs_extractor::extract_cs(p, &text)
                        } else if ext_lower.as_deref() == Some("asp") {
                            crate::asp_classic_extractor::extract_classic_asp(&arc_rel, &text)
                        } else if matches!(ext_lower.as_deref(), Some("rdlc" | "rdl")) {
                            crate::report_extractor::extract_ssrs_report(&arc_rel, &text)
                        } else {
                            extractor.extract(p, &text)
                        };
                        for s in &syms {
                            local_stats.symbols.push((arc_rel.clone(), s.clone()));
                        }
                        for e in edges {
                            local_stats.edges.push((arc_rel.clone(), e));
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
                                crate::state_extractor::extract_state_accesses(
                                    &arc_rel, &text, language,
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

                        (local_stats, local_docs)
                    })
                    .reduce(
                        || (IngestStats::default(), Vec::new()),
                        |mut a, b| {
                            a.0.chunks += b.0.chunks;
                            a.0.bytes += b.0.bytes;
                            a.0.all_files.extend(b.0.all_files);
                            a.0.fingerprints.extend(b.0.fingerprints);
                            a.0.symbols.extend(b.0.symbols);
                            a.0.edges.extend(b.0.edges);
                            a.0.skipped_files.extend(b.0.skipped_files);
                            for (lang, count) in b.0.languages {
                                *a.0.languages.entry(lang).or_insert(0) += count;
                            }
                            a.0.warnings.extend(b.0.warnings);
                            a.1.extend(b.1);
                            a
                        },
                    )
            })
            .await?;

            stats.chunks += chunk_stats.chunks;
            stats.bytes += chunk_stats.bytes;
            stats.all_files.extend(chunk_stats.all_files);
            stats.fingerprints.extend(chunk_stats.fingerprints);
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
                self.embed_and_upsert_vectors(project_id, &batch, cancel)
                    .await?;
                batch.clear();
            }

            processed_count += file_chunk.len();
            progress_cb(processed_count, total_files);
        }

        if !batch.is_empty() && !cancel.is_cancelled() {
            Self::write_docs_to_writer(&fields, &mut guard, project_id, &batch)?;
            self.embed_and_upsert_vectors(project_id, &batch, cancel)
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
            "loose" => {
                let parser = QueryParser::for_index(&self.tantivy_index, vec![self.fields.content]);
                parser.parse_query(&escape_tantivy_literal(&q.text))?
            }
            "strict" => {
                let mut parser =
                    QueryParser::for_index(&self.tantivy_index, vec![self.fields.content]);
                parser.set_conjunction_by_default();
                parser.parse_query(&escape_tantivy_literal(&q.text))?
            }
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

        if let Ok(policy) = engram_core::get_policy(&q.namespace) {
            match policy.versioning {
                engram_core::NamespaceVersioning::Snapshot => {
                    let gen_term = Term::from_field_u64(self.fields.generation, q.generation);
                    let gen_q = TermQuery::new(gen_term, IndexRecordOption::Basic);
                    must_clauses.push((Occur::Must, Box::new(gen_q)));
                }
                engram_core::NamespaceVersioning::AppendOnly => {
                    let parser =
                        QueryParser::for_index(&self.tantivy_index, vec![self.fields.generation]);
                    if let Ok(query) =
                        parser.parse_query(&format!("generation:[* TO {}]", q.generation))
                    {
                        must_clauses.push((Occur::Must, query));
                    }
                }
                engram_core::NamespaceVersioning::GlobalMutable => {}
            }
        }

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

        if let Some(author) = &q.author_filter {
            must_clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.author, author),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if q.date_after.is_some() || q.date_before.is_some() {
            let after = q.date_after.unwrap_or(0);
            let before = q.date_before.unwrap_or(u64::MAX);
            let parser = QueryParser::for_index(&self.tantivy_index, vec![self.fields.timestamp]);
            if let Ok(query) = parser.parse_query(&format!("timestamp:[{} TO {}]", after, before)) {
                must_clauses.push((Occur::Must, query));
            }
        }

        let query = BooleanQuery::new(must_clauses);

        let top_docs: Vec<(Score, DocAddress)> =
            searcher.search(&query, &TopDocs::with_limit(q.top_k))?;

        let mut out = Vec::with_capacity(top_docs.len());
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
                snippet_truncated,
            });
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
            "loose" => {
                let parser = QueryParser::for_index(&self.tantivy_index, vec![self.fields.content]);
                parser.parse_query(&escape_tantivy_literal(&q.text))?
            }
            "strict" => {
                let mut parser =
                    QueryParser::for_index(&self.tantivy_index, vec![self.fields.content]);
                parser.set_conjunction_by_default();
                parser.parse_query(&escape_tantivy_literal(&q.text))?
            }
            unknown => {
                anyhow::bail!("unknown fts_mode '{unknown}': must be strict, loose, or regex")
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

        Ok(Some((path, language, content, start_line, end_line)))
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
        }
    }

    #[allow(dead_code)]
    fn add_generation_filter(
        &self,
        namespace: &str,
        generation: u64,
        clauses: &mut Vec<(Occur, Box<dyn tantivy::query::Query>)>,
    ) -> anyhow::Result<()> {
        if let Ok(policy) = engram_core::get_policy(namespace) {
            match policy.versioning {
                engram_core::NamespaceVersioning::Snapshot => {
                    let gen_term = Term::from_field_u64(self.fields.generation, generation);
                    clauses.push((
                        Occur::Must,
                        Box::new(TermQuery::new(gen_term, IndexRecordOption::Basic)),
                    ));
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
        Ok(())
    }

    pub async fn vector_search(
        &self,
        q: &HybridQuery,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<HybridHit>> {
        if self.embedding_backend == "fts_only" {
            return Ok(Vec::new());
        }
        #[cfg(feature = "vector")]
        {
            let table_name = format!("project_{}", q.project_id.replace('-', "_"));
            if !self
                .lance_conn
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                return Ok(Vec::new());
            }
            let table = self.lance_conn.open_table(&table_name).execute().await?;

            // EMB1-x8q2: use embed_cancellable so in-flight embed can be cooperatively
            // interrupted if the job or request is cancelled before the remote returns.
            let query_vec = self.embedder.embed_cancellable(&q.text, cancel).await?;

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
                            let _ = write!(wc, " AND generation = {}", q.generation);
                        }
                        engram_core::NamespaceVersioning::AppendOnly => {
                            let _ = write!(wc, " AND generation <= {}", q.generation);
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
                    let _ = write!(wc, " AND timestamp >= {}", after);
                }
                if let Some(before) = q.date_before {
                    let _ = write!(wc, " AND timestamp < {}", before);
                }
                wc
            };

            let mut results = table
                .query()
                .nearest_to(query_vec)?
                .only_if(where_clause)
                .limit(q.top_k)
                .execute()
                .await?;

            let mut hits = Vec::new();
            use futures::TryStreamExt;
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
                        score: 1.0 - score_arr.value(i),
                        centrality: 0.0,
                        snippet: None,
                        doc_id: doc_id_val,
                        // LanceDB rows carry no line columns; enriched from
                        // Tantivy via enrich_hits_from_store on final results.
                        start_line: 0,
                        end_line: 0,
                        snippet_truncated: false,
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
        let mut hits =
            match tokio::time::timeout(timeout_dur, self.vector_search(&q_oversampled, cancel))
                .await
            {
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
                .get_vectors_by_chunk_ids(&q.project_id, &chunk_ids)
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
            let table_name = format!("project_{}", project_id.replace('-', "_"));
            if !self
                .lance_conn
                .table_names()
                .execute()
                .await?
                .contains(&table_name)
            {
                return Ok(std::collections::HashMap::new());
            }
            let table = self.lance_conn.open_table(&table_name).execute().await?;

            let id_list = chunk_ids
                .iter()
                .map(|id| format!("CAST({} AS BIGINT UNSIGNED)", id))
                .collect::<Vec<_>>()
                .join(", ");
            let filter = format!("chunk_id IN ({})", id_list);

            let mut results = table.query().only_if(filter).execute().await?;

            let mut map = std::collections::HashMap::new();
            use futures::TryStreamExt;
            while let Some(batch) = TryStreamExt::try_next(&mut results).await? {
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

        if let Some(first) = remaining.first().cloned() {
            selected.push(first);
            remaining.remove(0);
        }

        // Pre-allocate a single fallback zero-vector outside the hot loop
        let zero_vec = vec![0.0f32; self.embedder.dimension()];

        while selected.len() < top_k && !remaining.is_empty() {
            let mut best_mmr_score = f32::MIN;
            let mut best_idx = 0;

            for (idx, cand) in remaining.iter().enumerate() {
                let cand_vec = vectors.get(&cand.chunk_id).unwrap_or(&zero_vec);

                let mut max_sim = 0.0f32;
                for sel in &selected {
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
                }

                // Standard MMR formula: λ * relevance - (1-λ) * max_similarity.
                // cand.score is the RRF relevance; max_sim is clamped to [0, 1].
                let mmr_score = lambda * cand.score - (1.0 - lambda) * max_sim;
                if mmr_score > best_mmr_score {
                    best_mmr_score = mmr_score;
                    best_idx = idx;
                }
            }

            selected.push(remaining.remove(best_idx));
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
        let fetch_k = if q.use_mmr {
            // FTS2: saturating_mul + cap prevents OOM on large top_k × multiplier combos.
            (q.top_k.saturating_mul(self.mmr_oversampling.max(2))).min(10_000)
        } else {
            q.top_k
        };

        let mut q_modified = q.clone();
        q_modified.top_k = fetch_k;

        let lexical = self.lexical_search(&q_modified)?;
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
        if q.use_mmr && merged.len() > q.top_k {
            let chunk_ids: Vec<u64> = merged.iter().map(|h| h.chunk_id).collect();
            let vectors = self
                .get_vectors_by_chunk_ids(&q.project_id, &chunk_ids)
                .await?;
            merged = self.mmr_rerank(merged, &vectors, q.top_k, 0.5);
        } else if merged.len() > q.top_k {
            merged.truncate(q.top_k);
        }

        #[cfg(not(feature = "vector"))]
        if merged.len() > q.top_k {
            merged.truncate(q.top_k);
        }

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
                // OciusX eval: NL queries with apostrophes returned 0 hits.
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
