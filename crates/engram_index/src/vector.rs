use arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
    UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use lance_arrow::FixedSizeListArrayExt;
use lancedb::{Connection, Table};
use rayon::prelude::*;
use std::path::Path;
use std::sync::Arc;

/// Default vector dimension for the ProjectionEmbedder / LocalEmbedder (all-MiniLM-L6-v2 style).
/// External callers should use the embedder's `dimension()` method instead of this constant.
pub const VECTOR_DIM: usize = 384;

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
pub async fn open_or_create_table(
    conn: &Connection,
    name: &str,
    dim: usize,
) -> anyhow::Result<Table> {
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
            conn.drop_table(name, &[]).await?;
            let batch = RecordBatch::new_empty(schema.clone());
            let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
            Ok(conn.create_table(name, reader).execute().await?)
        } else {
            Ok(table)
        }
    } else {
        let batch = RecordBatch::new_empty(schema.clone());
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        Ok(conn.create_table(name, reader).execute().await?)
    }
}

/// Upsert vectors keyed by `pk`. Deletes existing rows with matching pks before inserting.
pub async fn upsert_vectors(table: &Table, batches: Vec<RecordBatch>) -> anyhow::Result<()> {
    if batches.is_empty() {
        return Ok(());
    }

    // Collect all pks to delete before inserting (raw, un-escaped values)
    let mut all_pks: Vec<String> = Vec::new();
    for batch in &batches {
        if let Some(pk_col) = batch
            .column_by_name("pk")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        {
            for i in 0..pk_col.len() {
                all_pks.push(pk_col.value(i).to_string());
            }
        }
    }

    // Delete in batches of 500 to avoid overly long SQL; escaping handled in build_pk_filter.
    for chunk in all_pks.chunks(500) {
        let filter = build_pk_filter(chunk);
        table.delete(&filter).await?;
    }

    // Now insert new rows
    let schema = batches[0].schema();
    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), schema);
    table.add(reader).execute().await?;
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

    let pk_arr = StringArray::from(pks.to_vec());
    let doc_id_arr = StringArray::from(doc_ids.to_vec());
    let content_hash_arr = StringArray::from(content_hashes.to_vec());
    let chunk_id_arr = UInt64Array::from(chunk_ids.to_vec());
    let project_id_arr = StringArray::from(vec![project_id; n]);
    let namespace_arr = StringArray::from(vec![namespace; n]);
    let generation_arr = UInt64Array::from(generations.to_vec());
    let path_arr = StringArray::from(paths.to_vec());
    let language_arr = StringArray::from(languages.to_vec());
    let author_arr = StringArray::from(
        authors
            .iter()
            .map(|a| a.as_deref().unwrap_or(""))
            .collect::<Vec<_>>(),
    );
    let timestamp_arr = UInt64Array::from(
        timestamps
            .iter()
            .map(|t| t.unwrap_or(0))
            .collect::<Vec<_>>(),
    );

    // Validate that all vectors match the expected dimension. Log a warning for
    // any mismatches (which get silently padded/truncated to `dim`). A persistent
    // mismatch indicates a misconfigured embedder and will degrade search quality.
    let mismatch_count = vectors.iter().filter(|v| v.len() != dim).count();
    if mismatch_count > 0 {
        tracing::warn!(
            expected_dim = dim,
            mismatched = mismatch_count,
            total = vectors.len(),
            "Vector dimension mismatch detected — {} of {} vectors have incorrect \
             dimension. Vectors will be padded/truncated to {}. Check embedder config.",
            mismatch_count,
            vectors.len(),
            dim
        );
    }

    // Parallelize vector flattening; truncate/pad to `dim` to match the schema.
    let flat_vectors: Vec<f32> = vectors
        .par_iter()
        .flat_map(|v| {
            let mut v_resized = v.clone();
            v_resized.resize(dim, 0.0);
            v_resized
        })
        .collect();

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
pub fn build_pk_filter(pks: &[String]) -> String {
    let pk_list = pks
        .iter()
        .map(|pk| format!("'{}'", pk.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("pk IN ({})", pk_list)
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
}
