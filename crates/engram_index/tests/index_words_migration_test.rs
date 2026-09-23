//! Adding the `content_words` field must not cost any stored knowledge. An
//! index written before the field existed is rebuilt from its stored
//! documents on open: every document survives with every stored value, the
//! new field is populated from `content`, and a crash between the migration's
//! directory renames resumes instead of losing the index.

use engram_index::literal_text_query;
use engram_index::tantivy_index::open_or_create;
use tantivy::collector::Count;
use tantivy::schema::*;
use tantivy::tokenizer::{NgramTokenizer, TextAnalyzer};
use tantivy::{Index, TantivyDocument, doc};

/// The schema as it was before `content_words`.
fn write_legacy_index(dir: &std::path::Path, docs: usize) {
    let mut sb = Schema::builder();
    let pk = sb.add_text_field("pk", STRING | STORED);
    let doc_id = sb.add_text_field("doc_id", STRING | STORED);
    let content_hash = sb.add_text_field("content_hash", STRING | STORED);
    let project_id = sb.add_text_field("project_id", STRING | STORED);
    let namespace = sb.add_text_field("namespace", STRING | STORED);
    let generation = sb.add_u64_field("generation", INDEXED | STORED);
    let chunk_id = sb.add_u64_field("chunk_id", INDEXED | STORED);
    let path = sb.add_text_field("path", STRING | STORED);
    let language = sb.add_text_field("language", STRING | STORED);
    let author = sb.add_text_field("author", STRING | STORED);
    let timestamp = sb.add_u64_field("timestamp", INDEXED | STORED);
    let start_line = sb.add_u64_field("start_line", STORED);
    let end_line = sb.add_u64_field("end_line", STORED);
    let indexing = TextFieldIndexing::default()
        .set_tokenizer("trigram")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let content = sb.add_text_field(
        "content",
        TextOptions::default()
            .set_indexing_options(indexing)
            .set_stored(),
    );
    std::fs::create_dir_all(dir).unwrap();
    let index = Index::create_in_dir(dir, sb.build()).unwrap();
    index.tokenizers().register(
        "trigram",
        TextAnalyzer::builder(NgramTokenizer::new(3, 3, false).unwrap()).build(),
    );
    let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    for n in 0..docs {
        writer
            .add_document(doc!(
                pk => format!("p:history:0:d{n}"),
                doc_id => format!("d{n}"),
                content_hash => format!("h{n}"),
                project_id => "p",
                namespace => "history",
                generation => 0u64,
                chunk_id => n as u64,
                path => format!("commit:{n:040}"),
                language => "text",
                author => "Dev",
                timestamp => 1_700_000_000u64 + n as u64,
                start_line => 0u64,
                end_line => 0u64,
                content => format!("Merged PR {n}: GetAllowedProjectIds filters Missing photos"),
            ))
            .unwrap();
    }
    // A deleted document must stay deleted.
    writer.delete_term(Term::from_field_text(pk, "p:history:0:d0"));
    writer.commit().unwrap();
}

fn count(index: &Index, field: Field, text: &str) -> usize {
    let q = literal_text_query(index, field, text, true).unwrap();
    index
        .reader()
        .unwrap()
        .searcher()
        .search(&q, &Count)
        .unwrap()
}

#[test]
fn legacy_index_is_migrated_without_losing_documents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tantivy");
    write_legacy_index(&dir, 50);

    let (index, fields) = open_or_create(&dir).unwrap();
    let searcher = index.reader().unwrap().searcher();
    assert_eq!(
        searcher.num_docs(),
        49,
        "all live documents, the deleted one still gone"
    );
    // Stored values survive intact.
    let stored: TantivyDocument = searcher
        .search(
            &tantivy::query::TermQuery::new(
                Term::from_field_text(fields.pk, "p:history:0:d7"),
                IndexRecordOption::Basic,
            ),
            &tantivy::collector::TopDocs::with_limit(1),
        )
        .unwrap()
        .first()
        .map(|(_, addr)| searcher.doc(*addr).unwrap())
        .unwrap();
    let text = |f: Field| {
        stored
            .get_first(f)
            .and_then(|v| v.as_str().map(str::to_owned))
    };
    assert_eq!(
        text(fields.path).as_deref(),
        Some(format!("commit:{:040}", 7).as_str())
    );
    assert_eq!(
        stored.get_first(fields.timestamp).and_then(|v| v.as_u64()),
        Some(1_700_000_007)
    );
    // Word-level search finds lower-case prose and identifier parts; trigram still works.
    assert_eq!(count(&index, fields.content_words, "missing photos"), 49);
    assert_eq!(
        count(&index, fields.content_words, "allowed project ids"),
        49
    );
    assert_eq!(count(&index, fields.content, "Missing"), 49);
    // No migration debris; a second open is a plain open.
    assert!(!tmp.path().join("tantivy.migrating").exists());
    assert!(!tmp.path().join("tantivy.premigration").exists());
    drop((searcher, index));
    let (again, _) = open_or_create(&dir).unwrap();
    assert_eq!(again.reader().unwrap().searcher().num_docs(), 49);
}

#[test]
fn crash_between_renames_restores_the_original_index() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tantivy");
    write_legacy_index(&dir, 5);
    // Simulate a crash after the original moved aside, before the new one landed.
    std::fs::rename(&dir, tmp.path().join("tantivy.premigration")).unwrap();
    std::fs::create_dir_all(tmp.path().join("tantivy.migrating")).unwrap();

    let (index, fields) = open_or_create(&dir).unwrap();
    // Five written, one deleted.
    assert_eq!(index.reader().unwrap().searcher().num_docs(), 4);
    assert_eq!(count(&index, fields.content_words, "missing"), 4);
    assert!(!tmp.path().join("tantivy.premigration").exists());
}
