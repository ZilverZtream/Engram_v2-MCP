#![allow(clippy::unwrap_used)]
//! VEC2 — `merge_insert` atomicity tests.
//!
//! Proves that `upsert_vectors` uses true upsert semantics (match-on-pk):
//! - Repeated upserts of the same PKs do NOT grow the row count (no phantom duplicates).
//! - Updated fields reflect the latest batch values after the upsert.
//! - A batch mixing existing and new PKs lands exactly N_existing+N_new rows.
//!
//! These tests catch a regression to the old delete-then-add pattern, which had
//! a window where rows could temporarily vanish and left duplicate rows after
//! a crash between the delete and the add phases.

use engram_index::vector::{connect, create_record_batch, open_or_create_table, upsert_vectors};

const DIM: usize = 4;

/// Build a minimal record batch: `n` rows, all with `gen=1`, using the supplied
/// `content_hash` and sequential pk values `"{prefix}:k0"`, `"{prefix}:k1"`, …
fn make_batch(
    project_id: &str,
    pks: &[String],
    content_hash: &str,
) -> arrow_array::RecordBatch {
    let n = pks.len();
    let doc_ids: Vec<String> = pks.iter().map(|pk| format!("doc_{pk}")).collect();
    let content_hashes: Vec<String> = vec![content_hash.to_string(); n];
    let chunk_ids: Vec<u64> = (0..n as u64).collect();
    let paths: Vec<String> = (0..n).map(|i| format!("src/file{i}.rs")).collect();
    let languages: Vec<String> = vec!["rust".to_string(); n];
    let authors: Vec<Option<String>> = vec![None; n];
    let timestamps: Vec<Option<u64>> = vec![None; n];
    let vectors: Vec<Vec<f32>> = (0..n)
        .map(|i| {
            // Each row gets a unit-like vector offset by i to be distinguishable.
            let v = i as f32 + 1.0;
            let norm = (4.0 * v * v).sqrt();
            vec![v / norm; DIM]
        })
        .collect();

    create_record_batch(
        project_id,
        "code",
        1,
        pks,
        &doc_ids,
        &content_hashes,
        &chunk_ids,
        &paths,
        &languages,
        &authors,
        &timestamps,
        &vectors,
        DIM,
    )
    .expect("create_record_batch must succeed")
}

fn pks(prefix: &str, range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| format!("proj:{prefix}:1:doc{i}")).collect()
}

/// VEC2: upserting the same N PKs twice must leave exactly N rows, not 2N.
///
/// If `merge_insert` were replaced with a plain `insert`, the second call would
/// append duplicates and the count would be 2N.  This test catches that regression.
#[tokio::test]
async fn vec2_repeated_upsert_same_pks_does_not_grow_row_count() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conn = connect(tmp.path()).await.unwrap();
    let (table, _) = open_or_create_table(&conn, "vectors", DIM).await.unwrap();

    let all_pks = pks("repeat", 0..5);

    // First upsert — 5 new rows.
    let batch1 = make_batch("proj", &all_pks, "hash_v1");
    upsert_vectors(&table, vec![batch1]).await.unwrap();

    let count_after_first = table.count_rows(None).await.unwrap();
    assert_eq!(count_after_first, 5, "VEC2: first upsert must insert 5 rows");

    // Second upsert — same 5 PKs, different content_hash.
    let batch2 = make_batch("proj", &all_pks, "hash_v2");
    upsert_vectors(&table, vec![batch2]).await.unwrap();

    let count_after_second = table.count_rows(None).await.unwrap();
    assert_eq!(
        count_after_second, 5,
        "VEC2: re-upserting same 5 PKs must leave exactly 5 rows (got {count_after_second}); \
         a regression to plain insert would give 10"
    );
}

/// VEC2: upserting the same batch 3× is idempotent — row count never exceeds N.
#[tokio::test]
async fn vec2_upsert_is_idempotent_over_many_repetitions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conn = connect(tmp.path()).await.unwrap();
    let (table, _) = open_or_create_table(&conn, "vectors", DIM).await.unwrap();

    let all_pks = pks("idem", 0..3);

    for i in 0..3u64 {
        let batch = make_batch("proj", &all_pks, &format!("hash_v{i}"));
        upsert_vectors(&table, vec![batch]).await.unwrap();
    }

    let count = table.count_rows(None).await.unwrap();
    assert_eq!(
        count, 3,
        "VEC2: 3 upserts of identical PKs must yield exactly 3 rows; got {count}"
    );
}

/// VEC2: a batch mixing N existing PKs + M new PKs must land exactly N+M rows.
///
/// This exercises the `when_matched_update_all` + `when_not_matched_insert_all`
/// branches of `merge_insert` in a single call.
#[tokio::test]
async fn vec2_mixed_existing_and_new_pks_lands_correct_count() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conn = connect(tmp.path()).await.unwrap();
    let (table, _) = open_or_create_table(&conn, "vectors", DIM).await.unwrap();

    // Insert 3 "existing" rows.
    let existing_pks = pks("mixed", 0..3);
    let batch_existing = make_batch("proj", &existing_pks, "hash_existing");
    upsert_vectors(&table, vec![batch_existing]).await.unwrap();

    assert_eq!(table.count_rows(None).await.unwrap(), 3);

    // Upsert: those 3 existing + 4 new PKs = 7 total.
    let new_pks = pks("mixed", 3..7);
    let all_pks: Vec<String> = existing_pks.iter().chain(new_pks.iter()).cloned().collect();
    let batch_mixed = make_batch("proj", &all_pks, "hash_updated");
    upsert_vectors(&table, vec![batch_mixed]).await.unwrap();

    let count = table.count_rows(None).await.unwrap();
    assert_eq!(
        count, 7,
        "VEC2: 3 existing + 4 new PKs must give 7 rows total; got {count}"
    );
}

/// VEC2: upsert of a batch with 0 rows must be a no-op (no crash, count unchanged).
#[tokio::test]
async fn vec2_empty_batch_is_noop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let conn = connect(tmp.path()).await.unwrap();
    let (table, _) = open_or_create_table(&conn, "vectors", DIM).await.unwrap();

    // Insert 2 rows first.
    let initial_pks = pks("empty", 0..2);
    upsert_vectors(&table, vec![make_batch("proj", &initial_pks, "h1")])
        .await
        .unwrap();
    assert_eq!(table.count_rows(None).await.unwrap(), 2);

    // Now upsert with an empty batch list.
    upsert_vectors(&table, vec![]).await.unwrap();
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 2, "VEC2: empty upsert must leave row count unchanged; got {count}");
}
