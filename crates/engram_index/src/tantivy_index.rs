use crate::word_tokenizer::{WORDS_TOKENIZER, words_analyzer};
use std::path::{Path, PathBuf};
use tantivy::schema::*;
use tantivy::tokenizer::{NgramTokenizer, TextAnalyzer};
use tantivy::{Index, Result as TantivyResult, TantivyDocument};

#[derive(Debug, Clone, Copy)]
pub struct Fields {
    /// Canonical primary key: `{project_id}:{namespace}:{generation}:{doc_id}`
    pub pk: Field,
    /// Per-instance document identity (stable across identical content at same location).
    pub doc_id: Field,
    /// blake3 hex hash of raw content bytes (deduplication identity).
    pub content_hash: Field,
    pub project_id: Field,
    pub namespace: Field,
    pub generation: Field,
    /// Legacy chunk_id field (kept for backward compat queries).
    pub chunk_id: Field,
    pub path: Field,
    pub language: Field,
    pub author: Field,
    pub timestamp: Field,
    pub start_line: Field,
    pub end_line: Field,
    /// Case-preserving character trigrams: substring grep and regex.
    pub content: Field,
    /// Words (see `word_tokenizer`): BM25 ranking for literal text queries.
    /// Not stored — `content` holds the text.
    pub content_words: Field,
}

pub fn open_or_create(index_dir: &Path) -> TantivyResult<(Index, Fields)> {
    let io = |e: std::io::Error| tantivy::TantivyError::IoError(std::sync::Arc::new(e));
    finish_interrupted_migration(index_dir).map_err(io)?;
    std::fs::create_dir_all(index_dir).map_err(io)?;

    let mut schema_builder = Schema::builder();

    // Primary key for upsert: delete-by-term before add.
    let pk = schema_builder.add_text_field("pk", STRING | STORED);
    let doc_id = schema_builder.add_text_field("doc_id", STRING | STORED);
    let content_hash = schema_builder.add_text_field("content_hash", STRING | STORED);

    let project_id = schema_builder.add_text_field("project_id", STRING | STORED);
    let namespace = schema_builder.add_text_field("namespace", STRING | STORED);
    let generation = schema_builder.add_u64_field("generation", INDEXED | STORED);
    let chunk_id = schema_builder.add_u64_field("chunk_id", INDEXED | STORED);
    let path = schema_builder.add_text_field("path", STRING | STORED);
    let language = schema_builder.add_text_field("language", STRING | STORED);
    let author = schema_builder.add_text_field("author", STRING | STORED);
    let timestamp = schema_builder.add_u64_field("timestamp", INDEXED | STORED);
    let start_line = schema_builder.add_u64_field("start_line", STORED);
    let end_line = schema_builder.add_u64_field("end_line", STORED);

    // For Sourcegraph-style substring matching, index `content` with trigram tokenizer.
    let text_indexing = TextFieldIndexing::default()
        .set_tokenizer("trigram")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text_options = TextOptions::default()
        .set_indexing_options(text_indexing)
        .set_stored();
    let content = schema_builder.add_text_field("content", text_options);
    // Added last: an index written before it is migrated, not wiped.
    let words_indexing = TextFieldIndexing::default()
        .set_tokenizer(WORDS_TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let content_words = schema_builder.add_text_field(
        "content_words",
        TextOptions::default().set_indexing_options(words_indexing),
    );

    let schema = schema_builder.build();

    let index = if index_dir.join("meta.json").exists() {
        // Attempt to open existing index; if schema mismatch occurs we must recreate.
        // Fix #5: only wipe the directory on confirmed schema incompatibility.
        // Transient errors (e.g. antivirus lock, read timeout) must propagate as
        // errors rather than silently destroying the entire index.
        match Index::open_in_dir(index_dir) {
            Ok(idx) => {
                // Check if pk field exists - if not, the index predates this schema.
                if idx.schema().get_field("pk").is_err() {
                    // Schema is stale; wipe and recreate.
                    std::fs::remove_dir_all(index_dir)?;
                    std::fs::create_dir_all(index_dir)?;
                    Index::create_in_dir(index_dir, schema)?
                } else if idx.schema().get_field("content_words").is_err() {
                    migrate_adding_fields(index_dir, idx, schema)?
                } else {
                    idx
                }
            }
            Err(tantivy::TantivyError::SchemaError(_)) => {
                // Explicit schema incompatibility — safe to wipe.
                std::fs::remove_dir_all(index_dir)?;
                std::fs::create_dir_all(index_dir)?;
                Index::create_in_dir(index_dir, schema)?
            }
            Err(e) => {
                // Transient IO or other error — propagate rather than destroying
                // a potentially good index.
                return Err(e);
            }
        }
    } else {
        Index::create_in_dir(index_dir, schema)?
    };

    register_tokenizers(&index)?;

    Ok((
        index,
        Fields {
            pk,
            doc_id,
            content_hash,
            project_id,
            namespace,
            generation,
            chunk_id,
            path,
            language,
            author,
            timestamp,
            start_line,
            end_line,
            content,
            content_words,
        },
    ))
}

fn register_tokenizers(index: &Index) -> TantivyResult<()> {
    let trigram = TextAnalyzer::builder(NgramTokenizer::new(3, 3, false)?).build();
    index.tokenizers().register("trigram", trigram);
    index
        .tokenizers()
        .register(WORDS_TOKENIZER, words_analyzer());
    Ok(())
}

fn sibling(index_dir: &Path, suffix: &str) -> PathBuf {
    let mut name = index_dir.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    index_dir.with_file_name(name)
}

const MIGRATION_COMPLETE: &str = "MIGRATION_COMPLETE";

/// Rebuild an index under `schema` from its stored documents. Every field is
/// stored except the derived `content_words`, which is re-analysed from
/// `content`, so no project needs re-indexing and no knowledge-only
/// namespace (memory bank, merged PRs, history) is lost. The old index stays
/// in place until the new one holds exactly as many documents.
fn migrate_adding_fields(index_dir: &Path, old: Index, schema: Schema) -> TantivyResult<Index> {
    let staging = sibling(index_dir, ".migrating");
    let backup = sibling(index_dir, ".premigration");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;
    let expected = {
        let new = Index::create_in_dir(&staging, schema.clone())?;
        register_tokenizers(&new)?;
        let content_words = schema.get_field("content_words")?;
        let old_schema = old.schema();
        let reader = old.reader()?;
        let searcher = reader.searcher();
        let mut writer: tantivy::IndexWriter = new.writer(200_000_000)?;
        for segment in searcher.segment_readers() {
            let store = segment.get_store_reader(64)?;
            for doc in segment.doc_ids_alive() {
                let named = store.get::<TantivyDocument>(doc)?.to_named_doc(&old_schema);
                let text = match named.0.get("content").and_then(|values| values.first()) {
                    Some(OwnedValue::Str(text)) => Some(text.clone()),
                    _ => None,
                };
                let mut migrated = TantivyDocument::convert_named_doc(&schema, named)
                    .map_err(|e| tantivy::TantivyError::InvalidArgument(e.to_string()))?;
                if let Some(text) = text {
                    migrated.add_text(content_words, &text);
                }
                writer.add_document(migrated)?;
            }
        }
        writer.commit()?;
        writer.wait_merging_threads()?;
        let migrated = new.reader()?.searcher().num_docs();
        if migrated != searcher.num_docs() {
            return Err(tantivy::TantivyError::InternalError(format!(
                "index migration copied {migrated} of {} documents; kept the original",
                searcher.num_docs()
            )));
        }
        migrated
    };
    // Every handle on both directories is closed here; Windows cannot rename
    // a directory with open memory maps.
    drop(old);
    std::fs::write(staging.join(MIGRATION_COMPLETE), expected.to_string())?;
    std::fs::rename(index_dir, &backup)?;
    std::fs::rename(&staging, index_dir)?;
    std::fs::remove_file(index_dir.join(MIGRATION_COMPLETE))?;
    std::fs::remove_dir_all(&backup)?;
    Index::open_in_dir(index_dir)
}

/// Resume a migration a crash interrupted between its directory renames.
fn finish_interrupted_migration(index_dir: &Path) -> std::io::Result<()> {
    let staging = sibling(index_dir, ".migrating");
    let backup = sibling(index_dir, ".premigration");
    let staged_complete = staging.join(MIGRATION_COMPLETE).exists();
    if !index_dir.join("meta.json").exists() && backup.exists() {
        if staged_complete {
            std::fs::rename(&staging, index_dir)?;
        } else {
            std::fs::rename(&backup, index_dir)?;
        }
    }
    let _ = std::fs::remove_file(index_dir.join(MIGRATION_COMPLETE));
    if backup.exists() && index_dir.join("meta.json").exists() {
        std::fs::remove_dir_all(&backup)?;
    }
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    Ok(())
}
