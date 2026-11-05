//! Complete, bounded observations of selected vector rows and paths.
use std::collections::BTreeSet;
use arrow_array::{Array, RecordBatch, StringArray};
use futures::{TryStream, TryStreamExt};

pub(super) const MAX_ROWS: usize = 1_000_000;

pub(super) async fn observe<S, E>(batches: S) -> anyhow::Result<(usize, BTreeSet<String>)>
where
    S: TryStream<Ok = RecordBatch, Error = E> + Unpin,
    E: Into<anyhow::Error>,
{
    observe_with_limit(batches, MAX_ROWS).await
}

async fn observe_with_limit<S, E>(mut batches: S, limit: usize) -> anyhow::Result<(usize, BTreeSet<String>)>
where
    S: TryStream<Ok = RecordBatch, Error = E> + Unpin,
    E: Into<anyhow::Error>,
{
    let mut rows = 0usize;
    let mut paths = BTreeSet::new();
    while let Some(batch) = batches.try_next().await.map_err(|error| -> anyhow::Error { error.into() })? {
        rows = rows.checked_add(batch.num_rows())
            .ok_or_else(|| anyhow::anyhow!("Native path row count overflow"))?;
        anyhow::ensure!(rows <= limit, "Native path inspection exceeds {limit} rows");
        let values = batch.column_by_name("path")
            .and_then(|column| column.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| anyhow::anyhow!("missing path"))?;
        for i in 0..batch.num_rows() {
            anyhow::ensure!(!values.is_null(i), "Native path inspection encountered a null path");
            let path = values.value(i);
            if !paths.contains(path) {
                paths.insert(path.to_owned());
            }
        }
    }
    // EOF is part of the observation: a partial stream never certifies totals.
    Ok((rows, paths))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use arrow_schema::{DataType, Field, Schema};

    fn batch(paths: Vec<Option<&str>>) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("path", DataType::Utf8, true)])),
            vec![Arc::new(StringArray::from(paths))],
        ).unwrap()
    }

    #[tokio::test]
    async fn rows_count_chunks_while_paths_count_distinct_files() {
        let stream = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(batch(vec![Some("a.rs"), Some("a.rs")])),
            Ok(batch(vec![Some("a.rs"), Some("b.rs")])),
        ]);
        assert_eq!(observe_with_limit(stream, 4).await.unwrap(),
            (4, BTreeSet::from(["a.rs".into(), "b.rs".into()])));
    }

    #[tokio::test]
    async fn late_provider_failure_and_cap_plus_one_refuse_partial_inventory() {
        let stream = futures::stream::iter(vec![
            Ok(batch(vec![Some("a.rs")])),
            Err(std::io::Error::other("provider failed after first batch")),
        ]);
        assert!(observe_with_limit(stream, 4).await.unwrap_err().to_string().contains("provider failed"));
        let stream = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(batch(vec![Some("a.rs"); 4])),
            Ok(batch(vec![Some("b.rs")])),
        ]);
        assert!(observe_with_limit(stream, 4).await.unwrap_err().to_string().contains("exceeds 4"));
    }

    #[tokio::test]
    async fn invalid_path_column_does_not_certify_a_partial_inventory() {
        for invalid in [batch(vec![None]), RecordBatch::new_empty(Arc::new(Schema::empty()))] {
            let stream = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(batch(vec![Some("a.rs")])), Ok(invalid),
            ]);
            assert!(observe_with_limit(stream, 4).await.is_err());
        }
    }

    #[tokio::test]
    async fn observations_wait_for_stream_eof() {
        use futures::StreamExt;
        let (send, receive) = tokio::sync::oneshot::channel();
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(batch(vec![Some("a.rs")]))])
            .chain(futures::stream::once(async move {
                receive.await.expect("owned stream release")
            }));
        let mut observed = Box::pin(observe_with_limit(Box::pin(stream), 4));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(10), &mut observed).await.is_err());
        send.send(Err(std::io::Error::other("late read failed"))).unwrap();
        assert!(observed.await.unwrap_err().to_string().contains("late read failed"));
    }
}
