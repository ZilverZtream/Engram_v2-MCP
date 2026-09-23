//! `strict` / `loose` search promises that ANY user string is matched as
//! literal words. Raw user-story text routinely carries markdown backticks
//! and upper-case AND / OR / NOT / IN; escaping into Tantivy's grammar left
//! each of those a SyntaxError, which aborts the whole hybrid search — so the
//! caller got an error (search_history) or a silently empty evidence arm
//! (get_change_set history / kb_bridge). These tests pin the property, not
//! the instances: every input builds a query, operator words match as the
//! prose they are, and all-punctuation input matches nothing (never all).

use engram_index::literal_text_query;
use tantivy::collector::Count;
use tantivy::schema::{Field, IndexRecordOption, Schema, TEXT, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{NgramTokenizer, TextAnalyzer};
use tantivy::{Index, doc};

/// Same content-field analysis as production (`tantivy_index::open_or_create`).
fn trigram_index() -> (Index, Field) {
    let mut sb = Schema::builder();
    let indexing = TextFieldIndexing::default()
        .set_tokenizer("trigram")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let content = sb.add_text_field(
        "content",
        TextOptions::default().set_indexing_options(indexing),
    );
    let index = Index::create_in_ram(sb.build());
    index.tokenizers().register(
        "trigram",
        TextAnalyzer::builder(NgramTokenizer::new(3, 3, false).unwrap()).build(),
    );
    (index, content)
}

fn word_index() -> (Index, Field) {
    let mut sb = Schema::builder();
    let content = sb.add_text_field("content", TEXT);
    (Index::create_in_ram(sb.build()), content)
}

fn hits(index: &Index, field: Field, text: &str, conjunction: bool) -> usize {
    let q = literal_text_query(index, field, text, conjunction).unwrap();
    index
        .reader()
        .unwrap()
        .searcher()
        .search(&q, &Count)
        .unwrap()
}

#[test]
fn every_token_combination_builds_a_query() {
    let mut tokens: Vec<String> = (0x21u8..0x7f)
        .map(|b| b as char)
        .filter(|c| !c.is_ascii_alphanumeric())
        .map(|c| c.to_string())
        .collect();
    for word in [
        "AND", "OR", "NOT", "IN", "TO", "and", "Not", "x", "a-b", "é", "🎉", "`code`", "\\", " ",
        "\t", "\n", "2,100", "missing", "*",
    ] {
        tokens.push(word.to_string());
    }
    let mut failures = Vec::new();
    for (name, (index, field)) in [("trigram", trigram_index()), ("word", word_index())] {
        let mut check = |text: String| {
            for conjunction in [true, false] {
                if let Err(error) = literal_text_query(&index, field, &text, conjunction) {
                    failures.push(format!("{name} conj={conjunction} {text:?}: {error}"));
                }
            }
        };
        for a in &tokens {
            check(a.clone());
            for b in &tokens {
                check(format!("{a} {b}"));
                check(format!("{a}{b}"));
                for c in ["AND", "missing", "`", "NOT"] {
                    check(format!("{a} {b} {c}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} inputs failed, e.g. {:#?}",
        failures.len(),
        &failures[..failures.len().min(10)]
    );
}

#[test]
fn story_text_with_backticks_and_sql_keywords_finds_its_document() {
    let (index, content) = trigram_index();
    let mut writer = index.writer(15_000_000).unwrap();
    writer
        .add_document(doc!(content => "Filter photos NOT in the projekt list"))
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(
        hits(
            &index,
            content,
            "Filter photos NOT in the `projekt` list",
            false
        ),
        1
    );
    assert_eq!(
        hits(&index, content, "WHERE photos IN (1,2) OR Filter", false),
        1
    );
}

#[test]
fn operator_words_match_as_prose_not_as_operators() {
    let (index, content) = word_index();
    let mut writer = index.writer(15_000_000).unwrap();
    writer
        .add_document(doc!(content => "the export must not fail on photos"))
        .unwrap();
    writer
        .add_document(doc!(content => "unrelated text"))
        .unwrap();
    writer.commit().unwrap();
    // As an operator, NOT would EXCLUDE this document; as prose it must match it.
    assert_eq!(hits(&index, content, "must NOT fail", false), 1);
    assert_eq!(hits(&index, content, "must NOT fail", true), 1);
    // strict still means every word; loose still means any word.
    assert_eq!(hits(&index, content, "must fail absent", true), 0);
    assert_eq!(hits(&index, content, "must fail absent", false), 1);
}

#[test]
fn input_with_no_indexable_words_matches_nothing_not_everything() {
    let (index, content) = word_index();
    let mut writer = index.writer(15_000_000).unwrap();
    writer
        .add_document(doc!(content => "any document"))
        .unwrap();
    writer.commit().unwrap();
    for text in ["", "   ", "! ! ?", "` \" '"] {
        assert_eq!(hits(&index, content, text, false), 0, "{text:?}");
        assert_eq!(hits(&index, content, text, true), 0, "{text:?}");
    }
}
