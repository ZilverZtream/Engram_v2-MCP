//! Maintenance must preserve logical and physical row identities, including
//! source readers that predate the compacted native manifest.
use super::*;
use arrow_array::{Array, FixedSizeListArray, Float32Array, StringArray, UInt64Array};
use futures::TryStreamExt;
use std::collections::BTreeMap;

type Rows = BTreeMap<String, (String, String, u64, u64, Vec<u32>)>;

#[test]
fn lexical_vector_compaction_without_a_table_does_not_initialize_vectors() -> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(async {
            let cfg = engram_core::Config { embedding_backend:"fts_only".into(),..Default::default() };
            let index = engine(owned.path(), &cfg).await?;
            index.index_docs("project",&[document("a.rs",1,"source","retained source")],&CancellationToken::new()).await?;
            anyhow::ensure!(index.native_store.capture().expect("owned native fixture").connection.table_names().execute().await?.is_empty());
            anyhow::ensure!(index.compact_vector_fragments("project").await?.is_none());
            anyhow::ensure!(index.native_store.capture().expect("owned native fixture").connection.table_names().execute().await?.is_empty(),"Maintenance must not bootstrap an unused vector store");
            anyhow::ensure!(index.count_docs("project")?==1,"Skipping native compaction must preserve lexical content");
            Ok::<_,anyhow::Error>(())
        })
    };
    owned.close()?;
    result
}

async fn rows(table: &lancedb::Table) -> anyhow::Result<Rows> {
    let batches: Vec<arrow_array::RecordBatch> = table
        .query()
        .limit(1000)
        .execute()
        .await?
        .try_collect()
        .await?;
    let mut rows = Rows::new();
    for batch in batches {
        let strings = |name| {
            batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
        };
        let integers = |name| {
            batch
                .column_by_name(name)
                .unwrap()
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap()
        };
        let vectors = batch
            .column_by_name("vector")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        for n in 0..batch.num_rows() {
            let vector = vectors.value(n);
            let vector = vector.as_any().downcast_ref::<Float32Array>().unwrap();
            anyhow::ensure!(
                rows.insert(
                    strings("pk").value(n).into(),
                    (
                        strings("content_hash").value(n).into(),
                        strings("path").value(n).into(),
                        integers("generation").value(n),
                        integers("chunk_id").value(n),
                        vector.values().iter().map(|v| v.to_bits()).collect()
                    )
                )
                .is_none()
            );
        }
    }
    anyhow::ensure!(table.count_rows(None).await? == rows.len());
    Ok(rows)
}

#[test]
fn cancelled_vector_compaction_preserves_old_readers_source_identity_and_next_edit()
-> anyhow::Result<()> {
    let owned = tempfile::tempdir()?;
    let result = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let cfg = engram_core::Config::default();
            let cancel = CancellationToken::new();
            let index = engine(owned.path(), &cfg).await?;
            index.index_docs("project", &[
                document("a.rs",1,"old-a","obsolete source"),
                document("b.rs",1,"stable-b","stable source")],&cancel).await?;
            let source_reader = index.pin_source_generation("project",1)?;
            let epoch = index.pin_vector_epoch("project")?;
            let old = index.open_vector_epoch_table(&epoch).await?;
            old.checkout(old.version().await?).await?;
            let old_rows = rows(&old).await?;
            index.prepare_source_snapshot("project",1,2,&[RelPath::new("a.rs")],&[],&cancel).await?;
            index.index_docs("project",&[document("a.rs",2,"new-a","replacement source")],&cancel).await?;
            index.seal_source_snapshot("project",2)?;
            for n in 0..6 {
                let mut doc = document("history.rs",0,&format!("history-{n}"),"historical source");
                doc.namespace = "history".into();
                index.index_docs("project",&[doc],&cancel).await?;
            }
            index.purge_old_generations("project",2).await?;
            let before_table = index.open_vector_epoch_table(&epoch).await?;
            let before = rows(&before_table).await?;
            let fragments_before = before_table.dataset().unwrap().get().await?.get_fragments().len();
            anyhow::ensure!(fragments_before>=6,"Fixture must contain real fragmentation");
            let before_hits = index.vector_search(&query(2,"source"),&cancel).await?;
            let before_pks: BTreeSet<_> = before_hits.iter().map(|h|h.pk.clone()).collect();
            anyhow::ensure!(before_hits.iter().map(|h|h.doc_id.as_str()).collect::<BTreeSet<_>>()==BTreeSet::from(["new-a","stable-b"]));
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let completed = Arc::new(tokio::sync::Notify::new());
            let mut request_engine = index.clone();
            request_engine.native_write_pause = Some(Arc::new(super::super::vector_mutations::NativeWritePause {
                entered:entered.clone(), release:release.clone(), started:Default::default(),
            }));
            request_engine.native_commit_completed = Some(completed.clone());
            let request = tokio::spawn(async move { request_engine.compact_vector_fragments("project").await });
            let reached = tokio::time::timeout(std::time::Duration::from_secs(10),entered.notified()).await.is_ok();
            request.abort();
            let disconnected = request.await.is_err();
            let (still_excluded, settled) = {
            let probe = index.acquire_commit(&cancel);
            tokio::pin!(probe);
            let (still_excluded, pending) = match futures::poll!(&mut probe) {
                std::task::Poll::Pending => (true, true),
                std::task::Poll::Ready(Err(error)) => (
                    matches!(error.downcast_ref::<CoordinationError>(), Some(CoordinationError::Busy)), false
                ),
                std::task::Poll::Ready(Ok(guard)) => {drop(guard);(false, false)},
            };
            // Always release the owned worker before assertions can unwind.
            release.notify_one();
            let settled = tokio::time::timeout(std::time::Duration::from_secs(10),completed.notified()).await.is_ok();
            if pending { drop(probe.await?); }
            (still_excluded, settled)
            };
            anyhow::ensure!(reached && disconnected && still_excluded && settled,"Cancelled caller must leave native compaction owning its write lane until completion");
            let after_table = index.open_vector_epoch_table(&epoch).await?;
            let fragments_after = after_table.dataset().unwrap().get().await?.get_fragments().len();
            anyhow::ensure!(fragments_after<fragments_before,"Explicit maintenance must actually combine fragments");
            anyhow::ensure!(rows(&after_table).await?==before,"Compaction must preserve all physical PKs, paths, hashes, generations, chunk IDs and vector bits");
            anyhow::ensure!(rows(&old).await?==old_rows,"Held native version must remain readable");
            anyhow::ensure!(index.vector_search(&query(1,"source"),&cancel).await?.iter().map(|h|h.doc_id.as_str()).collect::<BTreeSet<_>>()==BTreeSet::from(["old-a","stable-b"]));
            anyhow::ensure!(index.vector_search(&query(2,"source"),&cancel).await?.iter().map(|h|h.pk.clone()).collect::<BTreeSet<_>>()==before_pks);
            drop(after_table);drop(before_table);drop(old);drop(epoch);drop(source_reader);drop(index);
            let reopened = engine(owned.path(),&cfg).await?;
            anyhow::ensure!(reopened.vector_search(&query(2,"source"),&cancel).await?.iter().map(|h|h.pk.clone()).collect::<BTreeSet<_>>()==before_pks);
            reopened.prepare_source_snapshot("project",2,3,&[RelPath::new("a.rs")],&[],&cancel).await?;
            reopened.index_docs("project",&[document("a.rs",3,"next-a","subsequent source")],&cancel).await?;
            reopened.seal_source_snapshot("project",3)?;
            anyhow::ensure!(reopened.vector_search(&query(3,"source"),&cancel).await?.iter().map(|h|h.doc_id.as_str()).collect::<BTreeSet<_>>()==BTreeSet::from(["next-a","stable-b"]));
            println!("COMPACTION_BEHAVIOR fragments_before={fragments_before} fragments_after={fragments_after} rows_preserved={}",before.len());
            Ok::<_,anyhow::Error>(())
        })
    };
    owned.close()?;
    result
}
