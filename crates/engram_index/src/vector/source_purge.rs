//! Managed source GC observes candidate metadata once, then deletes only exact
//! obsolete physical identities. A complete selection is never expanded into
//! a large negative native predicate on every edit.
use super::*;
use arrow_array::Array;
use futures::{TryStream, TryStreamExt};
use lancedb::query::{ExecutableQuery, QueryBase, QueryExecutionOptions, Select};
use std::collections::BTreeSet;

const MAX_ROWS: usize = 1_000_000;
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_PAIRS: usize = 100_000;
const MAX_PAIR_BYTES: usize = 64 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 64 * 1024;
const MAX_FILTER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    rows: usize,
    decoded_bytes: usize,
    pairs: usize,
    pair_bytes: usize,
    filter_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            rows: MAX_ROWS,
            decoded_bytes: MAX_DECODED_BYTES,
            pairs: MAX_PAIRS,
            pair_bytes: MAX_PAIR_BYTES,
            filter_bytes: MAX_FILTER_BYTES,
        }
    }
}

async fn plan<S, E, F>(
    mut batches: S,
    cutoff: u64,
    protected: &F,
    limits: Limits,
) -> anyhow::Result<Vec<String>>
where
    S: TryStream<Ok = RecordBatch, Error = E> + Unpin,
    E: Into<anyhow::Error>,
    F: Fn(&str, u64) -> bool + Sync,
{
    let mut rows = 0usize;
    let mut decoded_bytes = 0usize;
    let mut pair_bytes = 0usize;
    let mut obsolete = BTreeSet::new();
    while let Some(batch) = batches
        .try_next()
        .await
        .map_err(|error| -> anyhow::Error { error.into() })?
    {
        rows = rows
            .checked_add(batch.num_rows())
            .ok_or_else(|| anyhow::anyhow!("Source GC row count overflow"))?;
        anyhow::ensure!(
            rows <= limits.rows,
            "Source GC candidate row budget exceeded; no native delete started"
        );
        let paths = batch
            .column_by_name("path")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| {
                anyhow::anyhow!("Source GC candidate path column is missing or unsupported")
            })?;
        let revisions = batch
            .column_by_name("generation")
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| {
                anyhow::anyhow!("Source GC candidate generation column is missing or unsupported")
            })?;
        anyhow::ensure!(
            paths.null_count() == 0 && revisions.null_count() == 0,
            "Source GC candidate identity contains nulls; no native delete started"
        );
        for i in 0..batch.num_rows() {
            let path = paths.value(i);
            let revision = revisions.value(i);
            anyhow::ensure!(
                revision < cutoff && path.len() <= MAX_PATH_BYTES,
                "Source GC candidate identity exceeds its admitted scope or path budget"
            );
            decoded_bytes = decoded_bytes
                .checked_add(path.len())
                .and_then(|n| n.checked_add(8))
                .ok_or_else(|| anyhow::anyhow!("Source GC decoded-byte count overflow"))?;
            anyhow::ensure!(
                decoded_bytes <= limits.decoded_bytes,
                "Source GC candidate byte budget exceeded"
            );
            if !protected(path, revision) && !obsolete.contains(&(revision, path.to_owned())) {
                pair_bytes = pair_bytes
                    .checked_add(path.len())
                    .and_then(|n| n.checked_add(64))
                    .ok_or_else(|| anyhow::anyhow!("Source GC retained-byte count overflow"))?;
                anyhow::ensure!(
                    obsolete.len() < limits.pairs && pair_bytes <= limits.pair_bytes,
                    "Source GC obsolete-identity budget exceeded"
                );
                obsolete.insert((revision, path.to_owned()));
            }
        }
    }
    // A late provider error or any budget failure refuses the entire plan.
    // All filters are bounded and validated before the first native mutation.
    let prefix = format!(
        "namespace = 'memory' AND generation < {} AND (",
        crate::sql_literals::u64_literal(cutoff)
    );
    let mut filters = Vec::new();
    let mut terms = Vec::new();
    let mut bytes = prefix.len() + 1;
    fn finish(prefix: &str, terms: &[String]) -> String {
        fn balanced(terms: &[String]) -> String {
            if terms.len() == 1 {
                return terms[0].clone();
            }
            let (left, right) = terms.split_at(terms.len() / 2);
            format!("({} OR {})", balanced(left), balanced(right))
        }
        format!("{prefix}{})", balanced(terms))
    }
    for (revision, path) in obsolete {
        let term = format!(
            "(generation = {} AND path = '{}')",
            crate::sql_literals::u64_literal(revision),
            path.replace('\'', "''")
        );
        anyhow::ensure!(
            prefix.len() + term.len() + 1 <= limits.filter_bytes,
            "Source GC exact identity exceeds native filter budget"
        );
        let separator_bytes = if terms.is_empty() { 0 } else { 6 }; // balanced (A OR B)
        if bytes + separator_bytes + term.len() > limits.filter_bytes {
            filters.push(finish(&prefix, &terms));
            terms.clear();
            bytes = prefix.len() + 1;
        }
        bytes += term.len() + if terms.is_empty() { 0 } else { 6 };
        terms.push(term);
    }
    if !terms.is_empty() {
        filters.push(finish(&prefix, &terms));
    }
    Ok(filters)
}

pub(crate) async fn purge_managed_source_generations_serialized<F>(
    connection: &Connection,
    table_name: &str,
    cutoff: u64,
    protected: F,
) -> anyhow::Result<()>
where
    F: Fn(&str, u64) -> bool + Send + Sync,
{
    let deadline = std::time::Duration::from_secs(30);
    let (table, filters) = tokio::time::timeout(deadline, async {
        // Reopen under the caller's existing commit lane; do not refresh a
        // shared/stale reader or infer native absence from lexical documents.
        let table = connection.open_table(table_name).execute().await?;
        let mut options = QueryExecutionOptions::default();
        options.max_batch_length = 1024;
        options.timeout = Some(deadline);
        let candidate = format!(
            "namespace = 'memory' AND generation < {}",
            crate::sql_literals::u64_literal(cutoff)
        );
        let query = table
            .query()
            .only_if(&candidate)
            .select(Select::Columns(vec!["path".into(), "generation".into()]))
            .limit(MAX_ROWS + 1);
        let stream = query.execute_with_options(options).await?;
        let filters = plan(stream, cutoff, &protected, Limits::default()).await?;
        Ok::<_, anyhow::Error>((table, filters))
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!("Source GC candidate observation exceeded30s; no native delete started")
    })??;
    for filter in filters {
        // Never cancel an actual started commit. The native worker retains
        // commit/epoch/protected-source ownership until all writes finish.
        prepare_managed_write(&table).await?;
        table.delete(&filter).await?;
    }
    for filter in super::namespace_purge_filters(
        cutoff,
        None,
        engram_core::KNOWN_NAMESPACES
            .iter()
            .copied()
            .filter(|namespace| *namespace != "memory"),
    ) {
        super::delete_rows_if_present(connection, table_name, &filter).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_schema::{DataType, Field, Schema};
    use futures::StreamExt;

    fn batch(paths: Vec<Option<&str>>, generations: Vec<Option<u64>>) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("path", DataType::Utf8, true),
                Field::new("generation", DataType::UInt64, true),
            ])),
            vec![
                Arc::new(StringArray::from(paths)),
                Arc::new(UInt64Array::from(generations)),
            ],
        )
        .unwrap()
    }
    fn unprotected(_: &str, _: u64) -> bool {
        false
    }

    #[tokio::test]
    async fn incomplete_candidate_stream_never_produces_a_deletion_plan() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(batch(
            vec![Some("obsolete")],
            vec![Some(1)],
        ))])
        .chain(futures::stream::once(async move { receive.await.unwrap() }));
        let mut planned = Box::pin(plan(Box::pin(stream), 2, &unprotected, Limits::default()));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut planned)
                .await
                .is_err()
        );
        send.send(Err(std::io::Error::other("late native provider failure")))
            .unwrap();
        assert!(
            planned
                .await
                .unwrap_err()
                .to_string()
                .contains("late native provider failure")
        );
    }

    #[tokio::test]
    async fn candidate_identity_failures_refuse_the_entire_observation() {
        let wrong_type = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("path", DataType::Utf8, false),
                Field::new("generation", DataType::Utf8, false),
            ])),
            vec![
                Arc::new(StringArray::from(vec!["a"])),
                Arc::new(StringArray::from(vec!["1"])),
            ],
        )
        .unwrap();
        let long = "x".repeat(MAX_PATH_BYTES + 1);
        for invalid in [
            batch(vec![None], vec![Some(1)]),
            batch(vec![Some("a")], vec![None]),
            batch(vec![Some("future")], vec![Some(2)]),
            batch(vec![Some(&long)], vec![Some(1)]),
            wrong_type,
            RecordBatch::new_empty(Arc::new(Schema::empty())),
        ] {
            let stream = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(batch(vec![Some("obsolete")], vec![Some(1)])),
                Ok(invalid),
            ]);
            assert!(
                plan(stream, 2, &unprotected, Limits::default())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn work_budgets_count_all_chunks_and_refuse_before_deletion() {
        let defaults = Limits::default();
        for limits in [
            Limits {
                rows: 1,
                ..defaults
            },
            Limits {
                decoded_bytes: 1,
                ..defaults
            },
            Limits {
                pairs: 0,
                ..defaults
            },
            Limits {
                pair_bytes: 1,
                ..defaults
            },
            Limits {
                filter_bytes: 1,
                ..defaults
            },
        ] {
            let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(batch(
                vec![Some("same"), Some("same")],
                vec![Some(1), Some(1)],
            ))]);
            assert!(plan(stream, 2, &unprotected, limits).await.is_err());
        }
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(batch(
            vec![Some("same"), Some("same")],
            vec![Some(1), Some(1)],
        ))]);
        assert!(
            plan(
                stream,
                2,
                &unprotected,
                Limits {
                    rows: 2,
                    ..defaults
                }
            )
            .await
            .is_ok()
        );
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(batch(
            vec![Some("held")],
            vec![Some(1)],
        ))]);
        assert!(
            plan(
                stream,
                2,
                &|_, _| true,
                Limits {
                    pairs: 0,
                    filter_bytes: 0,
                    ..defaults
                }
            )
            .await
            .unwrap()
            .is_empty(),
            "Protected identities must not schedule any delete, even with no obsolete-pair budget"
        );
    }

    #[test]
    fn bounded_deletion_plans_preserve_exact_revision_pairs_and_namespaces() -> anyhow::Result<()> {
        let owned = tempfile::tempdir()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            let connection = super::super::connect(owned.path()).await?;
            let mut rows = vec![
                ("memory".to_owned(), "shared".to_owned(), 1u64),
                ("memory".into(), "shared".into(), 2),
                ("memory".into(), "other".into(), 1),
                ("memory".into(), "other".into(), 2),
                ("memory".into(), "future".into(), u64::MAX),
                ("memory".into(), "high-held".into(), u64::MAX - 1),
                ("memory".into(), "high-obsolete".into(), u64::MAX - 2),
                ("memory_bank".into(), "shared".into(), 1),
            ];
            rows.extend((0..20).map(|i| ("memory".into(), format!("old/O'Brien-å-{i}"), 1)));
            let schema = Arc::new(Schema::new(vec![
                Field::new("namespace", DataType::Utf8, false),
                Field::new("path", DataType::Utf8, false),
                Field::new("generation", DataType::UInt64, false),
            ]));
            let data = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(
                        rows.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
                    )),
                    Arc::new(StringArray::from(
                        rows.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(),
                    )),
                    Arc::new(UInt64Array::from(
                        rows.iter().map(|x| x.2).collect::<Vec<_>>(),
                    )),
                ],
            )?;
            let table = connection
                .create_table(
                    "rows",
                    Box::new(RecordBatchIterator::new(vec![Ok(data)].into_iter(), schema)),
                )
                .write_options(super::super::auto_cleanup::write_options())
                .execute()
                .await?;
            let candidates: Vec<_> = rows
                .iter()
                .filter(|x| x.0 == "memory" && x.2 < u64::MAX)
                .collect();
            let candidate = batch(
                candidates.iter().map(|x| Some(x.1.as_str())).collect(),
                candidates.iter().map(|x| Some(x.2)).collect(),
            );
            let held = |path: &str, revision| {
                (path == "shared" && revision == 2)
                    || (path == "other" && revision == 1)
                    || (path == "high-held" && revision == u64::MAX - 1)
            };
            let filters = plan(
                futures::stream::iter(vec![Ok::<_, std::io::Error>(candidate)]),
                u64::MAX,
                &held,
                Limits {
                    filter_bytes: 256,
                    ..Limits::default()
                },
            )
            .await?;
            anyhow::ensure!(
                filters.len() > 1 && filters.iter().all(|f| f.len() <= 256),
                "Wide cleanup must split within its bounded native request size"
            );
            for filter in filters {
                prepare_managed_write(&table).await?;
                table.delete(&filter).await?;
            }
            let mut actual = BTreeSet::new();
            let mut stream = table
                .query()
                .select(Select::Columns(vec![
                    "namespace".into(),
                    "path".into(),
                    "generation".into(),
                ]))
                .execute()
                .await?;
            while let Some(batch) = stream.try_next().await? {
                let ns = batch
                    .column_by_name("namespace")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                let paths = batch
                    .column_by_name("path")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                let revisions = batch
                    .column_by_name("generation")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .unwrap();
                for i in 0..batch.num_rows() {
                    actual.insert((
                        ns.value(i).to_owned(),
                        paths.value(i).to_owned(),
                        revisions.value(i),
                    ));
                }
            }
            anyhow::ensure!(
                actual
                    == BTreeSet::from([
                        ("memory".into(), "shared".into(), 2),
                        ("memory".into(), "other".into(), 1),
                        ("memory".into(), "future".into(), u64::MAX),
                        ("memory".into(), "high-held".into(), u64::MAX - 1),
                        ("memory_bank".into(), "shared".into(), 1),
                    ]),
                "Only exact obsolete physical pairs may be removed"
            );
            Ok::<_, anyhow::Error>(())
        });
        drop(runtime);
        owned.close()?;
        result
    }
}
