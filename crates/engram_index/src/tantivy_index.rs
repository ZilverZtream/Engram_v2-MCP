use std::path::Path;
use tantivy::schema::*;
use tantivy::tokenizer::{NgramTokenizer, TextAnalyzer};
use tantivy::{Index, Result as TantivyResult};

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
    pub content: Field,
}

pub fn open_or_create(index_dir: &Path) -> TantivyResult<(Index, Fields)> {
    std::fs::create_dir_all(index_dir).ok();

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

    let schema = schema_builder.build();

    let index = if index_dir.join("meta.json").exists() {
        // Attempt to open existing index; if schema mismatch occurs we must recreate.
        match Index::open_in_dir(index_dir) {
            Ok(idx) => {
                // Check if pk field exists - if not, the index predates this schema.
                if idx.schema().get_field("pk").is_err() {
                    // Schema is stale; wipe and recreate.
                    std::fs::remove_dir_all(index_dir).ok();
                    std::fs::create_dir_all(index_dir).ok();
                    Index::create_in_dir(index_dir, schema)?
                } else {
                    idx
                }
            }
            Err(_) => {
                std::fs::remove_dir_all(index_dir).ok();
                std::fs::create_dir_all(index_dir).ok();
                Index::create_in_dir(index_dir, schema)?
            }
        }
    } else {
        Index::create_in_dir(index_dir, schema)?
    };

    // Register trigram tokenizer.
    let trigram = TextAnalyzer::builder(NgramTokenizer::new(3, 3, false)?).build();
    index.tokenizers().register("trigram", trigram);

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
        },
    ))
}
