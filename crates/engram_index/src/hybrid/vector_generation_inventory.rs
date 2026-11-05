//! Complete, bounded observations of selected vector rows and paths.
use std::collections::BTreeSet;
use arrow_array::{Array, RecordBatch, StringArray, UInt64Array};
use futures::{TryStream, TryStreamExt};

pub(super) const MAX_ROWS: usize = 1_000_000;

pub(super) async fn observe_selected<S, E, F>(batches: S, selected: F) -> anyhow::Result<(usize, BTreeSet<String>)>
where S: TryStream<Ok = RecordBatch, Error = E> + Unpin, E: Into<anyhow::Error>,
      F: Fn(&str, &str, u64) -> bool,
{
    observe_selected_with_limits(batches, selected, MAX_ROWS, 256 * 1024 * 1024, 64 * 1024 * 1024).await
}

async fn observe_selected_with_limits<S, E, F>(mut batches: S, selected: F, row_limit: usize,
    byte_limit: usize, retained_limit: usize) -> anyhow::Result<(usize, BTreeSet<String>)>
where S: TryStream<Ok = RecordBatch, Error = E> + Unpin, E: Into<anyhow::Error>,
      F: Fn(&str, &str, u64) -> bool,
{
    let mut candidates = 0usize;
    let mut decoded_bytes = 0usize;
    let mut retained_bytes = 0usize;
    let mut rows = 0usize;
    let mut paths = BTreeSet::new();
    while let Some(batch) = batches.try_next().await.map_err(|error| -> anyhow::Error { error.into() })? {
        candidates = candidates.checked_add(batch.num_rows()).ok_or_else(|| anyhow::anyhow!("Native candidate count overflow"))?;
        anyhow::ensure!(candidates <= row_limit, "Native candidate inspection exceeds {row_limit} rows");
        let values = batch.column_by_name("path").and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| anyhow::anyhow!("Native candidate path column is missing or unsupported"))?;
        let generations = batch.column_by_name("generation").and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| anyhow::anyhow!("Native candidate generation column is missing or unsupported"))?;
        let namespaces = batch.column_by_name("namespace").and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| anyhow::anyhow!("Native candidate namespace column is missing or unsupported"))?;
        anyhow::ensure!(values.null_count() == 0 && generations.null_count() == 0 && namespaces.null_count() == 0,
            "Native candidate identity contains nulls");
        for i in 0..batch.num_rows() {
            let path = values.value(i);
            let namespace = namespaces.value(i);
            decoded_bytes = decoded_bytes.checked_add(path.len()).and_then(|n| n.checked_add(namespace.len()))
                .and_then(|n| n.checked_add(8)).ok_or_else(|| anyhow::anyhow!("Native candidate byte count overflow"))?;
            anyhow::ensure!(decoded_bytes <= byte_limit, "Native candidate byte budget exceeded");
            if selected(namespace, path, generations.value(i)) {
                rows += 1; // bounded by the checked raw candidate count above
                if !paths.contains(path) {
                    retained_bytes = retained_bytes.checked_add(path.len()).and_then(|n| n.checked_add(32))
                        .ok_or_else(|| anyhow::anyhow!("Native selected-path byte count overflow"))?;
                    anyhow::ensure!(retained_bytes <= retained_limit, "Native selected-path byte budget exceeded");
                    paths.insert(path.to_owned());
                }
            }
        }
    }
    Ok((rows, paths)) // No selected totals escape before complete EOF.
}

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

    fn candidates(rows: &[(&str, &str, u64)]) -> RecordBatch {
        RecordBatch::try_new(Arc::new(Schema::new(vec![
            Field::new("namespace", DataType::Utf8, false), Field::new("path", DataType::Utf8, false),
            Field::new("generation", DataType::UInt64, false),
        ])), vec![Arc::new(StringArray::from(rows.iter().map(|r| r.0).collect::<Vec<_>>())),
            Arc::new(StringArray::from(rows.iter().map(|r| r.1).collect::<Vec<_>>())),
            Arc::new(UInt64Array::from(rows.iter().map(|r| r.2).collect::<Vec<_>>()))]).unwrap()
    }

    fn selected(namespace: &str, path: &str, revision: u64) -> bool {
        if namespace == "memory" { (path == "O'Brien-å.rs" && revision == 2) || (path == "stable.rs" && revision == 1) }
        else { revision == 2 }
    }

    #[tokio::test]
    async fn candidate_rows_require_exact_membership_while_chunks_and_paths_remain_distinct() {
        let stream = futures::stream::iter(vec![
            Ok::<_, std::io::Error>(candidates(&[("memory", "O'Brien-å.rs", 1),
                ("memory", "O'Brien-å.rs", 2), ("memory", "stable.rs", 1)])),
            Ok(candidates(&[("memory", "O'Brien-å.rs", 2), ("memory", "O'Brien-å.rs", 3),
                ("memory", "unselected.rs", 2), ("custom", "O'Brien-å.rs", 2), ("custom", "wrong-generation", 3)])),
        ]);
        assert_eq!(observe_selected_with_limits(stream, selected, 8, 4096, 4096).await.unwrap(),
            (4, BTreeSet::from(["O'Brien-å.rs".into(), "stable.rs".into()])));
    }

    #[tokio::test]
    async fn rejected_candidates_still_consume_work_and_byte_budgets() {
        for (rows, bytes, retained) in [(1, 4096, 4096), (2, 1, 4096), (2, 4096, 1)] {
            let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(candidates(&[
                ("memory", "unselected.rs", 1), ("memory", "O'Brien-å.rs", 2)]))]);
            assert!(observe_selected_with_limits(stream, selected, rows, bytes, retained).await.is_err());
        }
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(candidates(&[
            ("memory", "unselected.rs", 1), ("memory", "O'Brien-å.rs", 2)]))]);
        assert_eq!(observe_selected_with_limits(stream, selected, 2, 4096, 4096).await.unwrap().0, 1);
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(candidates(&[("memory", "unselected.rs", 1)]))]);
        assert_eq!(observe_selected_with_limits(stream, selected, 1, 4096, 0).await.unwrap(), (0, BTreeSet::new()));
    }

    #[tokio::test]
    async fn selected_inventory_never_returns_partial_totals_before_eof_or_on_invalid_identity() {
        use futures::StreamExt;
        let (send, receive) = tokio::sync::oneshot::channel();
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(candidates(&[("memory", "O'Brien-å.rs", 2)]))])
            .chain(futures::stream::once(async move { receive.await.unwrap() }));
        let mut observed = Box::pin(observe_selected(Box::pin(stream), selected));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(10), &mut observed).await.is_err());
        send.send(Err(std::io::Error::other("late candidate failure"))).unwrap();
        assert!(observed.await.unwrap_err().to_string().contains("late candidate failure"));
        let missing = RecordBatch::new_empty(Arc::new(Schema::empty()));
        let wrong = RecordBatch::try_new(Arc::new(Schema::new(vec![
            Field::new("namespace", DataType::Utf8, true), Field::new("path", DataType::Utf8, false),
            Field::new("generation", DataType::UInt64, false),
        ])), vec![Arc::new(StringArray::from(vec![None::<&str>])), Arc::new(StringArray::from(vec!["x"])),
            Arc::new(UInt64Array::from(vec![1]))]).unwrap();
        for invalid in [missing, wrong, batch(vec![Some("no generation or namespace")])] {
            let stream = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(candidates(&[("memory", "O'Brien-å.rs", 2)])), Ok(invalid),
            ]);
            assert!(observe_selected(stream, selected).await.is_err());
        }
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
