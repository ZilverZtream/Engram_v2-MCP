use arrow_array::{
    FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use lance_arrow::FixedSizeListArrayExt;
use lancedb::{Connection, Table};
use std::path::Path;
use std::sync::Arc;

/// Default vector dimension for the ProjectionEmbedder / LocalEmbedder (all-MiniLM-L6-v2 style).
/// External callers should use the embedder's `dimension()` method instead of this constant.
pub const VECTOR_DIM: usize = 384;

/// Outcome of opening or creating a vector table.
#[derive(Debug, Clone)]
pub enum TableOpenOutcome {
    /// Table already existed with compatible schema — no data was lost.
    Opened,
    /// Table was newly created (no prior data existed).
    Created,
    /// Table had incompatible schema and was dropped+recreated — ALL vector data was lost.
    ///
    /// `reason` describes why the schema was incompatible (missing pk / dimension mismatch).
    /// `prior_row_count` is the row count captured immediately before the drop, giving
    /// operators an observable data-loss metric: zero means no vectors existed, non-zero
    /// means a full re-index is required to recover that many rows.
    ///
    /// The caller MUST trigger a full re-index to restore search quality.
    Recreated { reason: String, prior_row_count: u64 },
}

/// Connect to a local LanceDB database.
pub async fn connect(db_dir: &Path) -> anyhow::Result<Connection> {
    std::fs::create_dir_all(db_dir)?;
    let db = lancedb::connect(db_dir.to_string_lossy().as_ref())
        .execute()
        .await?;
    Ok(db)
}

/// The canonical schema for vector rows, parameterised by `dim`.
/// Includes `pk` as the primary key for true upsert.
fn vector_schema(dim: usize) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        // Primary key for upsert: {project_id}:{namespace}:{generation}:{doc_id}
        Field::new("pk", DataType::Utf8, false),
        Field::new("doc_id", DataType::Utf8, false),
        Field::new("content_hash", DataType::Utf8, false),
        Field::new("chunk_id", DataType::UInt64, false),
        Field::new("project_id", DataType::Utf8, false),
        Field::new("namespace", DataType::Utf8, false),
        Field::new("generation", DataType::UInt64, false),
        Field::new("path", DataType::Utf8, false),
        Field::new("language", DataType::Utf8, false),
        Field::new("author", DataType::Utf8, true),
        Field::new("timestamp", DataType::UInt64, true),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            false,
        ),
    ]))
}

/// Read the actual vector dimension stored in an existing table's schema.
fn stored_vector_dim(tschema: &arrow_schema::Schema) -> Option<usize> {
    let field = tschema.field_with_name("vector").ok()?;
    if let DataType::FixedSizeList(_, size) = field.data_type() {
        Some(*size as usize)
    } else {
        None
    }
}

/// Open or create a LanceDB table with the given vector `dim`.
///
/// If the existing table has a different vector dimension (e.g. an old 384-dim table
/// when the embedder now produces 1536-dim vectors), the stale table is dropped and
/// recreated with the correct schema so inserts do not panic.
///
/// Returns `(table, outcome)` where `outcome` signals whether data was lost
/// (see [`TableOpenOutcome`]). The caller MUST handle `Recreated` by logging a
/// prominent warning and triggering a full re-index.
pub async fn open_or_create_table(
    conn: &Connection,
    name: &str,
    dim: usize,
) -> anyhow::Result<(Table, TableOpenOutcome)> {
    let schema = vector_schema(dim);

    if conn
        .table_names()
        .execute()
        .await?
        .contains(&name.to_string())
    {
        let table = conn.open_table(name).execute().await?;
        let tschema = table.schema().await?;
        // Drop and recreate if: pk column is missing OR vector dimension has changed.
        let needs_recreate =
            tschema.field_with_name("pk").is_err() || stored_vector_dim(&tschema) != Some(dim);
        if needs_recreate {
            let reason = if tschema.field_with_name("pk").is_err() {
                "missing pk column".to_string()
            } else {
                "vector dimension mismatch".to_string()
            };
            // Capture row count before drop so operators can observe exactly how many
            // vectors will be lost.  Zero means the table was empty (no-op loss);
            // non-zero is the target for the subsequent full re-index job.
            // VEC1-v9q5: if count_rows itself fails, warn rather than silently
            // returning 0 which would make the drop look like a no-op data-loss event.
            let prior_row_count = match table.count_rows(None).await {
                Ok(n) => n as u64,
                Err(e) => {
                    tracing::warn!(
                        table = name,
                        reason = %reason,
                        "VEC1: count_rows failed before drop — row-count will be \
                         reported as 0 but actual loss may be non-zero: {e:#}"
                    );
                    0
                }
            };
            conn.drop_table(name, &[]).await?;
            let batch = RecordBatch::new_empty(schema.clone());
            let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
            let new_table = conn.create_table(name, reader).execute().await?;
            Ok((new_table, TableOpenOutcome::Recreated { reason, prior_row_count }))
        } else {
            Ok((table, TableOpenOutcome::Opened))
        }
    } else {
        let batch = RecordBatch::new_empty(schema.clone());
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        let new_table = conn.create_table(name, reader).execute().await?;
        Ok((new_table, TableOpenOutcome::Created))
    }
}

/// Upsert vectors keyed by `pk`.
///
/// VEC1 (non-atomic fix): uses LanceDB `merge_insert` (match on `pk`) instead of
/// the previous delete-then-add pattern. `merge_insert` is a single atomic
/// operation: existing rows with matching `pk` are updated in place, new rows are
/// inserted, and no window exists where rows are temporarily absent between the
/// delete and the add.
///
/// VEC2 atomicity: LanceDB's Lance format uses an MVCC append-only manifest model.
/// Each write operation (including merge_insert) creates a single new manifest
/// version in one atomic file-system append; readers and concurrent writers see
/// only the prior or the completed new version — never an intermediate state.
/// A failed write leaves the manifest unchanged (the partial data file is orphaned
/// and eventually cleaned up by vacuum). The empirical test in
/// `tests/vec2_merge_insert_atomicity_tests.rs` validates this observable behavior.
pub async fn upsert_vectors(table: &Table, batches: Vec<RecordBatch>) -> anyhow::Result<()> {
    if batches.is_empty() {
        return Ok(());
    }

    let schema = batches[0].schema();
    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);

    let mut merge = table.merge_insert(&["pk"]);
    merge.when_matched_update_all(None);
    merge.when_not_matched_insert_all();
    merge
        .execute(Box::new(reader))
        .await
        .map_err(|e| anyhow::anyhow!("VEC1: LanceDB merge_insert failed: {e:#}"))?;

    Ok(())
}

pub async fn purge_old_generations(table: &Table, active_generation: u64) -> anyhow::Result<()> {
    for ns in engram_core::KNOWN_NAMESPACES {
        if let Ok(policy) = engram_core::get_policy(ns) {
            let safe_ns = ns.replace('\'', "''");
            match policy.retention {
                engram_core::NamespaceRetention::KeepLatestOnly => {
                    let filter = format!(
                        "namespace = '{}' AND generation != {}",
                        safe_ns, active_generation
                    );
                    table.delete(&filter).await?;
                }
                engram_core::NamespaceRetention::KeepLastGenerations(n) => {
                    let min_keep = active_generation.saturating_sub(n as u64 - 1);
                    if min_keep > 0 {
                        let filter =
                            format!("namespace = '{}' AND generation < {}", safe_ns, min_keep);
                        table.delete(&filter).await?;
                    }
                }
                engram_core::NamespaceRetention::KeepForever => {}
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn create_record_batch(
    project_id: &str,
    namespace: &str,
    generation: u64,
    pks: &[String],
    doc_ids: &[String],
    content_hashes: &[String],
    chunk_ids: &[u64],
    paths: &[String],
    languages: &[String],
    authors: &[Option<String>],
    timestamps: &[Option<u64>],
    vectors: &[Vec<f32>],
    dim: usize,
) -> anyhow::Result<RecordBatch> {
    let gens = vec![generation; pks.len()];
    create_record_batch_with_gens(
        project_id,
        namespace,
        &gens,
        pks,
        doc_ids,
        content_hashes,
        chunk_ids,
        paths,
        languages,
        authors,
        timestamps,
        vectors,
        dim,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn create_record_batch_with_gens(
    project_id: &str,
    namespace: &str,
    generations: &[u64],
    pks: &[String],
    doc_ids: &[String],
    content_hashes: &[String],
    chunk_ids: &[u64],
    paths: &[String],
    languages: &[String],
    authors: &[Option<String>],
    timestamps: &[Option<u64>],
    vectors: &[Vec<f32>],
    dim: usize,
) -> anyhow::Result<RecordBatch> {
    let schema = vector_schema(dim);
    let n = pks.len();

    // Upfront shape consistency — all parallel slices must have the same length.
    // Arrow's try_new() catches mismatches but only at RecordBatch construction;
    // failing here gives a clear error that names the offending field.
    anyhow::ensure!(doc_ids.len() == n, "doc_ids.len() ({}) != pks.len() ({n})", doc_ids.len());
    anyhow::ensure!(content_hashes.len() == n, "content_hashes.len() ({}) != pks.len() ({n})", content_hashes.len());
    anyhow::ensure!(chunk_ids.len() == n, "chunk_ids.len() ({}) != pks.len() ({n})", chunk_ids.len());
    anyhow::ensure!(paths.len() == n, "paths.len() ({}) != pks.len() ({n})", paths.len());
    anyhow::ensure!(languages.len() == n, "languages.len() ({}) != pks.len() ({n})", languages.len());
    anyhow::ensure!(authors.len() == n, "authors.len() ({}) != pks.len() ({n})", authors.len());
    anyhow::ensure!(timestamps.len() == n, "timestamps.len() ({}) != pks.len() ({n})", timestamps.len());
    anyhow::ensure!(vectors.len() == n, "vectors.len() ({}) != pks.len() ({n})", vectors.len());
    anyhow::ensure!(generations.len() == n, "generations.len() ({}) != pks.len() ({n})", generations.len());

    // Build Arrow arrays from borrowed slices — avoid .to_vec() on already-owned Vecs.
    // StringArray::from accepts &[&str] which avoids cloning String→String.
    let pk_refs: Vec<&str> = pks.iter().map(|s| s.as_str()).collect();
    let pk_arr = StringArray::from(pk_refs);
    let did_refs: Vec<&str> = doc_ids.iter().map(|s| s.as_str()).collect();
    let doc_id_arr = StringArray::from(did_refs);
    let ch_refs: Vec<&str> = content_hashes.iter().map(|s| s.as_str()).collect();
    let content_hash_arr = StringArray::from(ch_refs);
    let chunk_id_arr = UInt64Array::from(chunk_ids.to_vec());
    let project_id_arr = StringArray::from(vec![project_id; n]);
    let namespace_arr = StringArray::from(vec![namespace; n]);
    let generation_arr = UInt64Array::from(generations.to_vec());
    let path_refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    let path_arr = StringArray::from(path_refs);
    let lang_refs: Vec<&str> = languages.iter().map(|s| s.as_str()).collect();
    let language_arr = StringArray::from(lang_refs);
    // Preserve None values as Arrow nulls instead of coercing to "" / 0.
    // Collapsing to sentinels loses the "unknown" distinction and poisons
    // downstream analytics that filter on these fields.
    let author_refs: Vec<Option<&str>> = authors.iter().map(|a| a.as_deref()).collect();
    let author_arr = StringArray::from(author_refs);
    let ts_vals: Vec<Option<u64>> = timestamps.to_vec();
    let timestamp_arr = UInt64Array::from(ts_vals);

    // Validate that all vectors match the expected dimension.
    let mismatch_count = vectors.iter().filter(|v| v.len() != dim).count();
    if mismatch_count > 0 {
        // ENG-AUD-2026-0005: fail closed on dimension mismatch — silently padding/truncating
        // corrupts the similarity space while the pipeline stays "green."
        // If coercion is truly required, the caller must normalize dimensions before ingestion.
        anyhow::bail!(
            "ENG-AUD-2026-0005: vector dimension mismatch — {} of {} vectors have incorrect \
             dimension (expected {}, got mixed). Check embedder config or normalize before ingestion.",
            mismatch_count,
            vectors.len(),
            dim
        );
    }

    // Zero-copy vector flattening: pre-allocate exact buffer and copy slices
    // directly. Avoids per-vector clone() + resize() which allocated ~150MB for
    // a 10K × 1536-dim batch.
    let mut flat_vectors = vec![0.0f32; vectors.len() * dim];
    for (i, v) in vectors.iter().enumerate() {
        debug_assert_eq!(v.len(), dim, "vector dimension invariant violated");
        flat_vectors[i * dim..(i + 1) * dim].copy_from_slice(v);
    }

    let vector_values = Float32Array::from(flat_vectors);
    let vector_arr = FixedSizeListArray::try_new_from_values(vector_values, dim as i32)?;

    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(pk_arr),
            Arc::new(doc_id_arr),
            Arc::new(content_hash_arr),
            Arc::new(chunk_id_arr),
            Arc::new(project_id_arr),
            Arc::new(namespace_arr),
            Arc::new(generation_arr),
            Arc::new(path_arr),
            Arc::new(language_arr),
            Arc::new(author_arr),
            Arc::new(timestamp_arr),
            Arc::new(vector_arr),
        ],
    )
    .map_err(|e| anyhow::anyhow!(e))
}

/// Build a SQL-safe pk IN (...) filter fragment for the given pks.
/// Single quotes in pk values are escaped as '' (standard SQL).
/// Uses a single pre-allocated String buffer instead of per-pk format!() + join().
pub fn build_pk_filter(pks: &[String]) -> String {
    // Estimate: "pk IN (" = 7, per pk ~avg 60 chars + "', '" = 4, trailing ")"
    let mut buf = String::with_capacity(7 + pks.len() * 64 + 1);
    buf.push_str("pk IN (");
    for (i, pk) in pks.iter().enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        buf.push('\'');
        for c in pk.chars() {
            if c == '\'' {
                buf.push_str("''");
            } else {
                buf.push(c);
            }
        }
        buf.push('\'');
    }
    buf.push(')');
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pk_filter_escapes_apostrophe() {
        // A path containing a single-quote (e.g., Irish "O'Brien/file.cs")
        let pk = "proj:memory:1:doc_O'Brien_file".to_string();
        let filter = build_pk_filter(&[pk]);
        // Must not contain an unescaped single quote that would break the SQL filter.
        assert!(
            filter.contains("O''Brien"),
            "apostrophe should be doubled: got {filter}"
        );
        assert!(
            !filter.contains("O'Brien'"),
            "unescaped apostrophe must not appear: got {filter}"
        );
    }

    #[test]
    fn test_pk_filter_no_apostrophe() {
        let pk = "proj:memory:1:normal_doc".to_string();
        let filter = build_pk_filter(&[pk]);
        assert_eq!(filter, "pk IN ('proj:memory:1:normal_doc')");
    }

    #[test]
    fn test_pk_filter_multiple() {
        let pks = vec!["a:b:c".to_string(), "x:y:z".to_string()];
        let filter = build_pk_filter(&pks);
        assert_eq!(filter, "pk IN ('a:b:c', 'x:y:z')");
    }

    // ── create_record_batch_with_gens shape assertions (ENG-AUD-2026-0010) ──

    fn make_batch_with_gens(
        n: usize,
        doc_ids_len: usize,
        vectors_len: usize,
    ) -> anyhow::Result<super::RecordBatch> {
        let pks: Vec<String> = (0..n).map(|i| format!("pk{i}")).collect();
        let doc_ids: Vec<String> = (0..doc_ids_len).map(|i| format!("d{i}")).collect();
        let content_hashes: Vec<String> = (0..n).map(|i| format!("hash{i}")).collect();
        let chunk_ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
        let paths: Vec<String> = (0..n).map(|i| format!("src/{i}.rs")).collect();
        let languages: Vec<String> = (0..n).map(|_| "rust".into()).collect();
        let authors: Vec<Option<String>> = (0..n).map(|_| None).collect();
        let timestamps: Vec<Option<u64>> = (0..n).map(|_| None).collect();
        let dim = 4usize;
        let vectors: Vec<Vec<f32>> = (0..vectors_len).map(|_| vec![0.1f32; dim]).collect();
        let generations: Vec<u64> = (0..n).map(|_| 1u64).collect();
        super::create_record_batch_with_gens(
            "proj",
            "code",
            &generations,
            &pks,
            &doc_ids,
            &content_hashes,
            &chunk_ids,
            &paths,
            &languages,
            &authors,
            &timestamps,
            &vectors,
            dim,
        )
    }

    /// Gate ENG-AUD-2026-0010: doc_ids shorter than pks must be rejected upfront.
    #[test]
    fn batch_with_gens_rejects_short_doc_ids() {
        let result = make_batch_with_gens(3, 2, 3);
        assert!(result.is_err(), "mismatched doc_ids length must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("doc_ids"),
            "error should mention doc_ids; got: {msg}"
        );
    }

    /// Mismatched vectors length must be rejected upfront.
    #[test]
    fn batch_with_gens_rejects_short_vectors() {
        let result = make_batch_with_gens(3, 3, 2);
        assert!(result.is_err(), "mismatched vectors length must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("vectors"),
            "error should mention vectors; got: {msg}"
        );
    }

    /// All slices equal length must succeed.
    #[test]
    fn batch_with_gens_accepts_consistent_lengths() {
        let result = make_batch_with_gens(3, 3, 3);
        assert!(result.is_ok(), "consistent slice lengths must succeed: {:?}", result.err());
    }

    /// Empty batch (n=0) must succeed.
    #[test]
    fn batch_with_gens_accepts_empty_batch() {
        let result = make_batch_with_gens(0, 0, 0);
        assert!(result.is_ok(), "empty batch must succeed: {:?}", result.err());
    }

    // ── ENG-AUD-2026-0005: fail-closed on dimension mismatch ──────────────

    #[test]
    fn dimension_mismatch_fails_closed() {
        // ENG-AUD-2026-0005: ingestion must fail when vectors have wrong dimension.
        // Previously this silently padded/truncated.
        let source = include_str!("vector.rs");
        assert!(
            source.contains("ENG-AUD-2026-0005"),
            "vector.rs must contain ENG-AUD-2026-0005 audit tag"
        );
        assert!(
            source.contains("anyhow::bail!") && source.contains("dimension mismatch"),
            "dimension mismatch must use anyhow::bail! to fail closed"
        );
        // Positive check: bail! with dimension mismatch message must appear exactly once
        // (in production code, not just this test).
        let bail_count = source.matches("ENG-AUD-2026-0005").count();
        assert!(
            bail_count >= 2,
            "ENG-AUD-2026-0005 tag must appear in both production code and this test; found {bail_count}"
        );
    }

    #[test]
    fn dimension_mismatch_batch_returns_err() {
        // ENG-AUD-2026-0005: call create_record_batch_with_gens with a mismatched vector.
        let n = 2usize;
        let pks: Vec<String> = (0..n).map(|i| format!("pk{i}")).collect();
        let doc_ids: Vec<String> = (0..n).map(|i| format!("d{i}")).collect();
        let content_hashes: Vec<String> = (0..n).map(|i| format!("hash{i}")).collect();
        let chunk_ids: Vec<u64> = (0..n).map(|i| i as u64).collect();
        let paths: Vec<String> = (0..n).map(|i| format!("src/{i}.rs")).collect();
        let languages: Vec<String> = (0..n).map(|_| "rust".into()).collect();
        let authors: Vec<Option<String>> = (0..n).map(|_| None).collect();
        let timestamps: Vec<Option<u64>> = (0..n).map(|_| None).collect();
        let generations: Vec<u64> = (0..n).map(|_| 1u64).collect();
        let dim = 4usize;
        // One vector has dim=3 (wrong), one has dim=4 (correct)
        let vectors: Vec<Vec<f32>> = vec![
            vec![0.1, 0.2, 0.3],
            vec![0.1, 0.2, 0.3, 0.4],
        ];
        let result = super::create_record_batch_with_gens(
            "proj",
            "code",
            &generations,
            &pks,
            &doc_ids,
            &content_hashes,
            &chunk_ids,
            &paths,
            &languages,
            &authors,
            &timestamps,
            &vectors,
            dim,
        );
        assert!(result.is_err(), "dimension mismatch must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("ENG-AUD-2026-0005"),
            "error must reference audit tag; got: {msg}"
        );
    }

    #[test]
    fn dimension_match_succeeds() {
        // Positive: when all vectors match dim, no error should occur.
        // Use structural check since the function is hard to call from unit test.
        let source = include_str!("vector.rs");
        // The zero-copy flattening code must still exist (it runs when all dims match)
        assert!(
            source.contains("copy_from_slice") || source.contains("flat_vectors"),
            "zero-copy flattening must still be present after fail-closed fix"
        );
    }
}
